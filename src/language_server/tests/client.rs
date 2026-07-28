//! Tiny LSP client used by integration tests.
//!
//! Speaks JSON-RPC over a `tokio::io::DuplexStream` half; pair the other half
//! with [`mago_server::serve`] to drive the server in-process.

use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::DuplexStream;
use tokio::time::Duration;
use tokio::time::timeout;

pub struct LspClient {
    stream: DuplexStream,
    next_id: Arc<AtomicI64>,
    /// Persistent receive buffer; multiple LSP messages may arrive in a
    /// single read, so we accumulate and parse out one at a time.
    rx: Vec<u8>,
    /// Notifications received while waiting for a request response. Tests
    /// can drain this with [`Self::take_pending_notifications`] to inspect
    /// publishDiagnostics, log messages, etc., without racing the request
    /// loop.
    pending_notifications: Vec<Value>,
}

impl LspClient {
    pub fn new(stream: DuplexStream) -> Self {
        Self {
            stream,
            next_id: Arc::new(AtomicI64::new(1)),
            rx: Vec::with_capacity(8192),
            pending_notifications: Vec::new(),
        }
    }

    pub async fn send_request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        self.write(request).await;

        id
    }

    pub async fn send_notification(&mut self, method: &str, params: Value) {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        self.write(notification).await;
    }

    async fn write(&mut self, msg: Value) {
        let body = serde_json::to_string(&msg).expect("json encode");
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        self.stream.write_all(framed.as_bytes()).await.expect("lsp write");
    }

    pub async fn read_message(&mut self, timeout_secs: u64) -> Value {
        timeout(Duration::from_secs(timeout_secs), async {
            loop {
                if let Some(msg) = self.try_pop_message() {
                    return msg;
                }

                let mut chunk = [0u8; 4096];
                let n = self.stream.read(&mut chunk).await.expect("lsp read");
                if n == 0 {
                    panic!("lsp stream closed before a complete message arrived");
                }
                self.rx.extend_from_slice(&chunk[..n]);
            }
        })
        .await
        .expect("lsp read timed out")
    }

    /// Try to extract a single complete LSP message from `self.rx`.
    /// Returns `None` if more bytes are needed.
    fn try_pop_message(&mut self) -> Option<Value> {
        const SEPARATOR: &[u8] = b"\r\n\r\n";
        let separator_pos = self.rx.windows(SEPARATOR.len()).position(|w| w == SEPARATOR)?;
        let header = std::str::from_utf8(&self.rx[..separator_pos]).expect("lsp header utf-8");

        let content_length: usize = header
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length:"))
            .map(|v| v.trim())
            .and_then(|v| v.parse().ok())
            .expect("Content-Length header");

        let body_start = separator_pos + SEPARATOR.len();
        if self.rx.len() < body_start + content_length {
            return None;
        }

        let body_end = body_start + content_length;
        let value: Value = serde_json::from_slice(&self.rx[body_start..body_end]).expect("lsp body json");

        self.rx.drain(..body_end);

        Some(value)
    }

    /// Read messages until one matches the given response id. Any
    /// notifications observed while waiting are stashed in
    /// `pending_notifications` so tests don't accidentally drop them.
    pub async fn await_response(&mut self, expected_id: i64, timeout_secs: u64) -> Value {
        loop {
            let msg = self.read_message(timeout_secs).await;
            let has_method = msg.get("method").is_some();
            let msg_id = msg.get("id").cloned();

            if !has_method && msg_id.as_ref().and_then(|v| v.as_i64()) == Some(expected_id) {
                return msg;
            }
            if has_method && msg_id.is_none() {
                // Notification; keep it for later inspection.
                self.pending_notifications.push(msg);
                continue;
            }
            if has_method && msg_id.is_some() {
                // Server-initiated request (e.g. window/workDoneProgress/create):
                // reply with a null result so the server's request future
                // resolves instead of blocking, mirroring a real editor.
                self.reply_ok(msg_id.unwrap()).await;
            }
        }
    }

    /// Reply to a server-initiated request with an empty successful result.
    async fn reply_ok(&mut self, id: Value) {
        self.write(json!({ "jsonrpc": "2.0", "id": id, "result": null })).await;
    }

    /// Read and stash any messages that arrive within `timeout_secs`, replying
    /// to server-initiated requests, until the stream goes quiet. Used to
    /// collect trailing notifications (e.g. a `$/progress` end) after a request.
    pub async fn drain_notifications(&mut self, timeout_secs: u64) {
        while let Ok(msg) = timeout(Duration::from_secs(timeout_secs), self.read_message(timeout_secs + 1)).await {
            let has_method = msg.get("method").is_some();
            let msg_id = msg.get("id").cloned();
            if has_method && msg_id.is_none() {
                self.pending_notifications.push(msg);
            } else if has_method && msg_id.is_some() {
                self.reply_ok(msg_id.unwrap()).await;
            }
        }
    }

    /// All `$/progress` notifications observed so far.
    pub fn progress_events(&self) -> Vec<Value> {
        self.pending_notifications
            .iter()
            .filter(|m| m.get("method").and_then(|v| v.as_str()) == Some("$/progress"))
            .cloned()
            .collect()
    }
}
