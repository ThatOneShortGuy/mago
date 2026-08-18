//! Helpers attached to [`super::Backend`] that don't belong in the
//! `tower-lsp` trait dispatch: lifecycle (`bootstrap`), workspace mutation
//! (`apply_change_atomic`), and shared utilities.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use tokio::task;
use tower_lsp_server::jsonrpc::Result as JsonRpcResult;
use tower_lsp_server::ls_types::Diagnostic;
use tower_lsp_server::ls_types::MessageType;
use tower_lsp_server::ls_types::ProgressToken;
use tower_lsp_server::ls_types::Uri;

use mago_database::DatabaseReader;
use mago_database::file::File as MagoFile;
use mago_database::file::FileId;
use mago_database::file::FileType;
use mago_reporting::CompiledIgnoreSet;
use mago_server::ServerError;
use mago_server::lookup;

use crate::language_server::diagnostics::build_diagnostics;
use crate::language_server::document::OpenDocument;
use crate::language_server::state::BackendState;
use crate::language_server::state::WorkspaceRegistry;
use crate::language_server::state::WorkspaceState;
use crate::language_server::state::build_workspace;
use crate::language_server::workspace::file_id_for;
use crate::language_server::workspace::logical_name_for;

use super::Backend;

/// How long to wait after the last `didChange` in a burst before running the
/// (expensive) incremental analysis. Short enough to feel immediate on a pause,
/// long enough to coalesce a run of keystrokes into one analysis.
const CHANGE_DEBOUNCE: Duration = Duration::from_millis(150);

/// The publishable result of an incremental analysis: `(changed_file_count,
/// URIs whose diagnostics went stale, per-URI diagnostics that changed)`.
type DiagnosticsOutcome = (usize, Vec<Uri>, Vec<(Uri, Vec<Diagnostic>)>);

impl Backend {
    pub(super) async fn bootstrap(&self, roots: Vec<PathBuf>, progress_supported: bool) {
        let started = Instant::now();

        // Optional work-done progress so the editor shows an "indexing"
        // indicator while the (background) bootstrap runs. The `create`
        // handshake is bounded by a short timeout: a client that advertised
        // support but never answers must not be able to stall the analysis.
        let progress = if progress_supported {
            let token = ProgressToken::String("mago/indexing".into());
            match tokio::time::timeout(Duration::from_secs(2), self.client.create_work_done_progress(token.clone()))
                .await
            {
                Ok(Ok(())) => Some(self.client.progress(token, "Mago").begin().await),
                _ => None,
            }
        } else {
            None
        };

        let mut workspaces = Vec::new();

        for root in roots {
            tracing::info!(root = %root.display(), "bootstrap starting");
            if let Some(progress) = &progress {
                progress.report(format!("Analyzing {}", root.display())).await;
            }

            let config = Arc::clone(&self.config);
            let root_label = root.display().to_string();
            let outcome = task::spawn_blocking(move || build_workspace(root, config)).await;

            match outcome {
                Ok(Ok((mut workspace, analysis_result))) => {
                    tracing::info!(
                        root = %root_label,
                        elapsed = ?started.elapsed(),
                        issues = analysis_result.issues.len(),
                        "analyzer pass complete",
                    );

                    if workspace.features.linter {
                        if let Some(progress) = &progress {
                            progress.report(format!("Linting {root_label}")).await;
                        }
                        let lint_started = Instant::now();
                        analyze_all_workspace_files(&mut workspace);
                        tracing::info!(elapsed = ?lint_started.elapsed(), "file analysis pass complete");
                    }

                    workspaces.push(workspace);
                }
                Ok(Err(err)) => {
                    tracing::error!(root = %root_label, error = %err, "bootstrap failed");
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            format!("mago-server: bootstrap failed for {root_label}: {err}"),
                        )
                        .await;
                }
                Err(err) => {
                    tracing::error!(root = %root_label, error = %err, "bootstrap task panicked");
                    self.client
                        .log_message(MessageType::ERROR, format!("mago-server: bootstrap task panicked: {err}"))
                        .await;
                }
            }
        }

        let count = workspaces.len();
        *self.state.lock().unwrap() = BackendState::Ready(WorkspaceRegistry::new(workspaces));
        tracing::info!(elapsed = ?started.elapsed(), workspaces = count, "ready");

        if let Some(progress) = progress {
            progress.finish().await;
        }
    }

    /// Apply a database mutation under the workspace mutex AND immediately
    /// run incremental analysis on the affected files; all without
    /// dropping the lock in between.
    ///
    /// This prevents capability handlers (which also acquire the mutex)
    /// from observing a database with new contents but stale analysis
    /// while the analyzer thread is in flight.
    pub(super) async fn apply_change_atomic<F>(&self, uri: Uri, mutate: F)
    where
        F: FnOnce(&mut WorkspaceState) -> Vec<FileId> + Send + 'static,
    {
        // Wait out the background bootstrap so an edit that arrives mid-bootstrap
        // is applied against the ready workspace instead of being dropped.
        self.ensure_ready().await;

        let started = Instant::now();
        let state = Arc::clone(&self.state);
        let outcome = task::spawn_blocking(move || -> Result<Option<DiagnosticsOutcome>, ServerError> {
            let Some(path) = uri.to_file_path() else {
                return Ok(None);
            };

            let mut guard = state.lock().unwrap();
            let BackendState::Ready(registry) = &mut *guard else {
                return Ok(None);
            };
            let Some(workspace) = registry.for_path_mut(path.as_ref()) else {
                return Ok(None);
            };

            let changed = mutate(workspace);
            if changed.is_empty() {
                return Ok(None);
            }

            Ok(Some(analyze_and_collect(workspace, &changed)?))
        })
        .await;

        self.publish_outcome(started, outcome).await;
    }

    /// Apply a database mutation immediately (no analysis) and eagerly drop the
    /// changed files' per-file caches, so content-hash-keyed capabilities
    /// (hover/completion) recompute against fresh text right away. Returns the
    /// changed file ids. Pairs with [`analyze_and_publish`](Self::analyze_and_publish)
    /// for the debounced change path.
    pub(super) async fn mutate_only<F>(&self, uri: Uri, mutate: F) -> Vec<FileId>
    where
        F: FnOnce(&mut WorkspaceState) -> Vec<FileId> + Send + 'static,
    {
        self.ensure_ready().await;

        let state = Arc::clone(&self.state);
        let changed = task::spawn_blocking(move || -> Vec<FileId> {
            let Some(path) = uri.to_file_path() else {
                return Vec::new();
            };
            let mut guard = state.lock().unwrap();
            let BackendState::Ready(registry) = &mut *guard else {
                return Vec::new();
            };
            let Some(workspace) = registry.for_path_mut(path.as_ref()) else {
                return Vec::new();
            };

            let changed = mutate(workspace);
            if !changed.is_empty() {
                workspace.invalidate_artifacts(&changed);
            }
            changed
        })
        .await
        .unwrap_or_default();

        if !changed.is_empty() {
            lookup::invalidate(&changed);
        }
        changed
    }

    /// Run incremental analysis for already-applied changes to `uri` and publish
    /// the resulting diagnostics. The database mutation is expected to have
    /// happened already (see [`mutate_only`](Self::mutate_only)).
    pub(super) async fn analyze_and_publish(&self, uri: Uri, changed: Vec<FileId>) {
        if changed.is_empty() {
            return;
        }

        let started = Instant::now();
        let state = Arc::clone(&self.state);
        let outcome = task::spawn_blocking(move || -> Result<Option<DiagnosticsOutcome>, ServerError> {
            let Some(path) = uri.to_file_path() else {
                return Ok(None);
            };
            let mut guard = state.lock().unwrap();
            let BackendState::Ready(registry) = &mut *guard else {
                return Ok(None);
            };
            let Some(workspace) = registry.for_path_mut(path.as_ref()) else {
                return Ok(None);
            };

            Ok(Some(analyze_and_collect(workspace, &changed)?))
        })
        .await;

        self.publish_outcome(started, outcome).await;
    }

    /// Publish the diagnostics produced by an incremental analysis task.
    async fn publish_outcome(
        &self,
        started: Instant,
        outcome: Result<Result<Option<DiagnosticsOutcome>, ServerError>, task::JoinError>,
    ) {
        match outcome {
            Ok(Ok(Some((count, stale, changed_diags)))) => {
                tracing::debug!(files = count, elapsed = ?started.elapsed(), "incremental analysis");
                for url in stale {
                    self.client.publish_diagnostics(url, vec![], None).await;
                }
                for (url, diags) in changed_diags {
                    self.client.publish_diagnostics(url, diags, None).await;
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(err)) => {
                tracing::error!(error = %err, "incremental analysis failed");
                self.client
                    .log_message(MessageType::ERROR, format!("mago-server: incremental analysis failed: {err}"))
                    .await;
            }
            Err(err) => {
                tracing::error!(error = %err, "analysis task panicked");
                self.client
                    .log_message(MessageType::ERROR, format!("mago-server: analysis task panicked: {err}"))
                    .await;
            }
        }
    }

    pub(super) async fn apply_buffer_open(&self, uri: Uri, text: String, version: i32) {
        let Some(path) = uri.to_file_path() else {
            return;
        };
        let path = path.into_owned();
        self.apply_change_atomic(uri.clone(), move |workspace| {
            let logical = logical_name_for(&workspace.root, &path);
            let virtual_file = workspace.database().get_id(logical.as_bytes()).is_none();
            let file_id = if virtual_file {
                let file = MagoFile::new(
                    Cow::Owned(logical.into_bytes()),
                    FileType::Host,
                    Some(path),
                    Cow::Owned(text.into_bytes()),
                );
                workspace.database_mut().add(file)
            } else {
                let id = FileId::new(logical.as_bytes());
                workspace.database_mut().update(id, Cow::Owned(text.into_bytes()));
                id
            };

            workspace.open_documents.insert(uri, OpenDocument { file_id, virtual_file, version });
            vec![file_id]
        })
        .await;
    }

    pub(super) async fn apply_buffer_change(&self, uri: Uri, text: String, version: i32) {
        // Apply the edit to the database right away so content-hash-keyed
        // capabilities (hover, completion) see fresh text immediately...
        let mutate_uri = uri.clone();
        let changed = self
            .mutate_only(uri.clone(), move |workspace| {
                let file_id = {
                    let Some(open) = workspace.open_documents.get_mut(&mutate_uri) else {
                        return Vec::new();
                    };
                    open.version = version;
                    open.file_id
                };

                workspace.database_mut().update(file_id, Cow::Owned(text.into_bytes()));
                vec![file_id]
            })
            .await;

        if changed.is_empty() {
            return;
        }

        // ...but debounce the expensive incremental analysis + diagnostics: a
        // burst of keystrokes coalesces into one analysis of the final text.
        let key = uri.to_string();
        self.pending_change_versions.lock().unwrap().insert(key.clone(), version);

        let backend = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(CHANGE_DEBOUNCE).await;

            // Only proceed if no newer edit superseded this one in the meantime.
            let superseded = backend.pending_change_versions.lock().unwrap().get(&key).copied() != Some(version);
            if superseded {
                return;
            }
            backend.pending_change_versions.lock().unwrap().remove(&key);
            backend.analyze_and_publish(uri, changed).await;
        });
    }

    pub(super) async fn apply_buffer_close(&self, uri: Uri) {
        let Some(path) = uri.to_file_path() else {
            return;
        };

        // Drop any pending debounced analysis for this document.
        self.pending_change_versions.lock().unwrap().remove(&uri.to_string());

        let path = path.into_owned();
        self.apply_change_atomic(uri.clone(), move |workspace| {
            let Some(open) = workspace.open_documents.remove(&uri) else {
                return Vec::new();
            };

            if open.virtual_file {
                workspace.database_mut().delete(open.file_id);
            } else if let Ok(file) = MagoFile::read(&workspace.root, &path, FileType::Host) {
                workspace.database_mut().update(open.file_id, file.contents);
            }
            vec![open.file_id]
        })
        .await;
    }

    pub(super) async fn apply_disk_change(&self, uri: Uri) {
        let Some(path) = uri.to_file_path() else {
            return;
        };
        let path = path.into_owned();
        self.apply_change_atomic(uri.clone(), move |workspace| {
            if workspace.open_documents.contains_key(&uri) {
                return Vec::new();
            }

            let Ok(file) = MagoFile::read(&workspace.root, &path, FileType::Host) else {
                return Vec::new();
            };

            let id = file.id;
            if workspace.database().get(&id).is_ok() {
                workspace.database_mut().update(id, file.contents);
            } else {
                workspace.database_mut().add(file);
            }
            vec![id]
        })
        .await;
    }

    pub(super) async fn apply_disk_delete(&self, uri: Uri) {
        let Some(path) = uri.to_file_path() else {
            return;
        };

        let path = path.into_owned();
        self.apply_change_atomic(uri, move |workspace| {
            let id = file_id_for(&workspace.root, &path);
            if workspace.database_mut().delete(id) { vec![id] } else { Vec::new() }
        })
        .await;
    }

    /// Run `f` with the file at `uri` and its owning workspace, routing by
    /// longest-prefix root match. `None` if uninitialized or unrouted.
    pub(super) fn with_file<F, R>(&self, uri: &Uri, f: F) -> Option<R>
    where
        F: FnOnce(&MagoFile, &WorkspaceState) -> R,
    {
        let path = uri.to_file_path()?;
        let guard = self.state.lock().unwrap();
        let BackendState::Ready(registry) = &*guard else {
            return None;
        };
        let workspace = registry.for_path(path.as_ref())?;
        let file = file_for_uri(workspace, uri)?;
        Some(f(&file, workspace))
    }

    /// Run `f` against the workspace owning `uri`.
    pub(super) fn with_workspace_for_uri<F, R>(&self, uri: &Uri, f: F) -> Option<R>
    where
        F: FnOnce(&WorkspaceState) -> R,
    {
        let path = uri.to_file_path()?;
        let guard = self.state.lock().unwrap();
        let BackendState::Ready(registry) = &*guard else {
            return None;
        };
        let workspace = registry.for_path(path.as_ref())?;
        Some(f(workspace))
    }

    /// Run `f` against the workspace owning `uri`, mutably.
    pub(super) fn with_workspace_mut_for_uri<F, R>(&self, uri: &Uri, f: F) -> Option<R>
    where
        F: FnOnce(&mut WorkspaceState) -> R,
    {
        let path = uri.to_file_path()?;
        let mut guard = self.state.lock().unwrap();
        let BackendState::Ready(registry) = &mut *guard else {
            return None;
        };
        let workspace = registry.for_path_mut(path.as_ref())?;
        Some(f(workspace))
    }

    /// Run `f` against the whole registry (e.g. cross-workspace operations).
    pub(super) fn with_registry<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&WorkspaceRegistry) -> R,
    {
        let guard = self.state.lock().unwrap();
        let BackendState::Ready(registry) = &*guard else {
            return None;
        };
        Some(f(registry))
    }

    /// Run `f` against the whole registry, mutably.
    pub(super) fn with_registry_mut<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut WorkspaceRegistry) -> R,
    {
        let mut guard = self.state.lock().unwrap();
        let BackendState::Ready(registry) = &mut *guard else {
            return None;
        };
        Some(f(registry))
    }

    /// Bootstrap a newly-added workspace folder and insert it into the
    /// registry. No-op if the server isn't ready or the folder is already
    /// tracked.
    pub(super) async fn add_workspace_folder(&self, root: PathBuf) {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
        if self.with_registry(|registry| registry.contains_root(&canonical)).unwrap_or(true) {
            return;
        }

        let config = Arc::clone(&self.config);
        let root_label = root.display().to_string();
        let outcome = task::spawn_blocking(move || build_workspace(root, config)).await;

        match outcome {
            Ok(Ok((mut workspace, _analysis_result))) => {
                if workspace.features.linter {
                    analyze_all_workspace_files(&mut workspace);
                }
                self.with_registry_mut(|registry| registry.add(workspace));
                tracing::info!(root = %root_label, "workspace folder added");
            }
            Ok(Err(err)) => {
                tracing::error!(root = %root_label, error = %err, "adding workspace folder failed");
            }
            Err(err) => {
                tracing::error!(root = %root_label, error = %err, "add-workspace task panicked");
            }
        }
    }

    /// Remove a workspace folder from the registry and clear the diagnostics it
    /// had published.
    pub(super) async fn remove_workspace_folder(&self, root: PathBuf) {
        let canonical = root.canonicalize().unwrap_or(root);
        let stale: Vec<Uri> = self
            .with_registry_mut(|registry| {
                registry.remove(&canonical).map(|ws| ws.last_diagnostics.into_keys().collect()).unwrap_or_default()
            })
            .unwrap_or_default();

        for uri in stale {
            self.client.publish_diagnostics(uri, vec![], None).await;
        }
    }
}

/// Run a synchronous LSP capability handler with a tracing span attached
/// for telemetry. Slow handlers (≥50ms) log at `debug`; the rest at `trace`.
pub(super) fn traced<T, F>(name: &'static str, f: F) -> JsonRpcResult<T>
where
    F: FnOnce() -> JsonRpcResult<T>,
{
    let started = Instant::now();
    let result = f();
    let elapsed = started.elapsed();
    if elapsed.as_millis() >= 50 {
        tracing::debug!(handler = name, elapsed = ?elapsed, "lsp handler");
    } else {
        tracing::trace!(handler = name, elapsed = ?elapsed, "lsp handler");
    }
    result
}

pub(super) fn file_for_uri(workspace: &WorkspaceState, uri: &Uri) -> Option<Arc<MagoFile>> {
    let path = uri.to_file_path()?;
    let id = file_id_for(&workspace.root, &path);
    workspace.database().get(&id).ok()
}

fn analyze_all_workspace_files(workspace: &mut WorkspaceState) {
    workspace.server.refresh_all_host_analyses();
    let count = workspace.server.analyses().count();
    let total_issues: usize = workspace.server.lint_issues().map(|issues| issues.len()).sum();
    tracing::info!("mago-server file analysis: {count} files, {total_issues} lint issues");
}

/// Run incremental analysis for `changed` against `workspace`, refresh lint
/// state and per-file caches, then diff the resulting diagnostics against the
/// last-published set. Returns `(changed_file_count, stale_uris, changed_diags)`
/// ready to publish. Assumes `changed` is non-empty. Shared by the immediate
/// ([`apply_change_atomic`](Backend::apply_change_atomic)) and debounced
/// ([`analyze_and_publish`](Backend::analyze_and_publish)) paths.
fn analyze_and_collect(workspace: &mut WorkspaceState, changed: &[FileId]) -> Result<DiagnosticsOutcome, ServerError> {
    let mut result = workspace.server.analyze_incremental(changed)?;

    let ignore_set = CompiledIgnoreSet::compile(
        &workspace.configuration.analyzer.ignore,
        workspace.configuration.source.glob.to_database_settings(),
    );
    result.issues.filter_out_ignored(&ignore_set, |file_id| {
        workspace.database().get_ref(&file_id).ok().map(|f| String::from_utf8_lossy(&f.name).into_owned())
    });

    workspace.invalidate_artifacts(changed);
    lookup::invalidate(changed);

    if workspace.features.linter {
        workspace.refresh_analyses(changed);
    }

    let lint_issues = workspace.server.lint_issues();
    let diagnostics = build_diagnostics(workspace.database(), &result, lint_issues);

    // Diff against this workspace's last-published set so we only emit changed
    // URIs and clear ones that went quiet.
    let stale: Vec<Uri> =
        workspace.last_diagnostics.keys().filter(|url| !diagnostics.contains_key(*url)).cloned().collect();
    let changed_diags: Vec<(Uri, Vec<Diagnostic>)> =
        diagnostics.into_iter().filter(|(url, diags)| workspace.last_diagnostics.get(url) != Some(diags)).collect();
    for url in &stale {
        workspace.last_diagnostics.remove(url);
    }
    for (url, diags) in &changed_diags {
        workspace.last_diagnostics.insert(url.clone(), diags.clone());
    }

    Ok((changed.len(), stale, changed_diags))
}
