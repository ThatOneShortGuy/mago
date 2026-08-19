//! Capability smoke tests; one short test per LSP request, exercising
//! the happy path. Built on [`super::harness::Harness`] helpers to keep
//! each case ~10 lines.

use serde_json::Value;
use serde_json::json;

use super::harness::Harness;

/// Completion responses serialize either as a bare array or as a
/// `CompletionList` object; normalize both to the item array.
fn completion_array(result: &Value) -> Value {
    if result.is_array() {
        result.clone()
    } else {
        result.get("items").cloned().unwrap_or_else(|| Value::Array(Vec::new()))
    }
}

const SAMPLE: &str = "<?php
namespace App;

final class Greeter
{
    public function hello(string $name): string
    {
        return 'hi ' . $name;
    }
}

function farewell(string $name): string {
    return 'bye ' . $name;
}
";

#[tokio::test]
async fn folding_range() {
    let mut h = Harness::start(&[("a.php", SAMPLE)]).await;
    let result = h.for_doc("textDocument/foldingRange", "a.php").await;
    assert!(!result.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn bootstrap_reports_work_done_progress() {
    // A client that advertises work-done progress should see the bootstrap
    // report a begin/end progress pair for its indexing token.
    let mut h =
        Harness::start_with_capabilities(&[("a.php", SAMPLE)], json!({ "window": { "workDoneProgress": true } })).await;

    // The `end` is emitted right around readiness; drain briefly to catch it.
    h.client.drain_notifications(1).await;

    let kinds: Vec<String> = h
        .client
        .progress_events()
        .iter()
        .filter(|e| e["params"]["token"] == json!("mago/indexing"))
        .filter_map(|e| e["params"]["value"]["kind"].as_str().map(String::from))
        .collect();

    assert!(kinds.iter().any(|k| k == "begin"), "expected a progress `begin`, got {kinds:?}");
    assert!(kinds.iter().any(|k| k == "end"), "expected a progress `end`, got {kinds:?}");
}

#[tokio::test]
async fn hover_class() {
    let mut h = Harness::start(&[("a.php", SAMPLE)]).await;
    let result = h.at("textDocument/hover", "a.php", 3, 13).await;
    let value = result["contents"]["value"].as_str().unwrap_or("");
    assert!(value.contains("class") && value.contains("Greeter"), "got {value:?}");
}

#[tokio::test]
async fn hover_variable_shows_type() {
    let code = "<?php\nfunction greet(string $name): void {\n    echo $name;\n}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;

    let result = h.at("textDocument/hover", "a.php", 2, 11).await;
    let value = result["contents"]["value"].as_str().unwrap_or("");
    assert!(value.contains("string") && value.contains("$name"), "got {value:?}");
}

#[tokio::test]
async fn hover_assigned_local_shows_inferred_type() {
    // Hovering the assignment target of an inferred local (not a declared
    // param) should still surface the variable's type.
    let code = "<?php\nnamespace App\\Models;\nclass Warehouse {\n    public static function nearest(): Warehouse { return new Warehouse(); }\n}\n$nearestWarehouse = Warehouse::nearest();\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;

    let result = h.at("textDocument/hover", "a.php", 5, 2).await;
    let value = result["contents"]["value"].as_str().unwrap_or("");
    assert!(value.contains("Warehouse") && value.contains("$nearestWarehouse"), "got {value:?}");
}

#[tokio::test]
async fn goto_definition() {
    let code = "<?php\nclass Greeter {}\n\n$g = new Greeter();\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    let result = h.at("textDocument/definition", "a.php", 3, 9).await;
    assert_eq!(result["range"]["start"]["line"], 1);
}

#[tokio::test]
async fn goto_definition_on_use_statement() {
    let lib = "<?php\nnamespace Bar;\nclass G {}\n";
    let consumer = "<?php\nnamespace Foo;\nuse Bar\\G;\n";
    let mut h = Harness::start(&[("lib.php", lib), ("c.php", consumer)]).await;
    h.open("lib.php", lib).await;
    h.open("c.php", consumer).await;

    let result = h.at("textDocument/definition", "c.php", 2, 8).await;
    assert!(result["uri"].as_str().unwrap_or("").ends_with("lib.php"), "got {result:?}");
}

#[tokio::test]
async fn hover_on_use_statement() {
    let lib = "<?php\nnamespace Bar;\nclass G {}\n";
    let consumer = "<?php\nnamespace Foo;\nuse Bar\\G;\n";
    let mut h = Harness::start(&[("lib.php", lib), ("c.php", consumer)]).await;
    h.open("lib.php", lib).await;
    h.open("c.php", consumer).await;

    let result = h.at("textDocument/hover", "c.php", 2, 8).await;
    let value = result["contents"]["value"].as_str().unwrap_or("");
    assert!(value.contains("Bar") && value.contains('G'), "got {value:?}");
}

#[tokio::test]
async fn formatting() {
    let mut h = Harness::start(&[("a.php", "<?php\n$x   =   1   ;  \n")]).await;
    let result = h
        .request(
            "textDocument/formatting",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "options": { "tabSize": 4, "insertSpaces": true }
            }),
        )
        .await;
    let new_text = result[0]["newText"].as_str().unwrap_or("");
    assert!(new_text.contains("$x = 1;"), "got {new_text:?}");
}

#[tokio::test]
async fn semantic_tokens() {
    let mut h = Harness::start(&[("a.php", SAMPLE)]).await;
    let result = h.for_doc("textDocument/semanticTokens/full", "a.php").await;
    let data = result["data"].as_array().unwrap();
    assert!(!data.is_empty());
    assert_eq!(data.len() % 5, 0);
}

/// Legend indices, mirroring the order in [`crate::language_server::capabilities`].
const TOKEN_KEYWORD: u64 = 0;
const TOKEN_FUNCTION: u64 = 6;

/// Decode the delta-encoded wire format into `(line, character, length, kind)`.
fn decoded_tokens(result: &Value) -> Vec<(u64, u64, u64, u64)> {
    let data: Vec<u64> = result["data"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
    let (mut line, mut character) = (0, 0);
    data.chunks(5)
        .map(|chunk| {
            line += chunk[0];
            character = if chunk[0] == 0 { character + chunk[1] } else { chunk[1] };
            (line, character, chunk[2], chunk[3])
        })
        .collect()
}

/// Semantic tokens override the client's own highlighting wherever they land,
/// so the server must stay out of a heredoc body: a blanket `string` token
/// there flattens the SQL grammar the editor injects into it.
#[tokio::test]
async fn semantic_tokens_leave_heredoc_bodies_to_the_client() {
    let source = "<?php\n$query = <<<'SQL'\n    SELECT id\n    FROM orders\n    SQL;\n";
    let mut h = Harness::start(&[("a.php", source)]).await;
    let result = h.for_doc("textDocument/semanticTokens/full", "a.php").await;

    let body_lines: Vec<_> = decoded_tokens(&result).into_iter().filter(|(line, ..)| (2..=3).contains(line)).collect();
    assert!(body_lines.is_empty(), "heredoc body was tokenized: {body_lines:?}");
}

/// A name after `::` or `->` is a member even when it lexes as a reserved
/// keyword, and a bare name that is not a call (a `void` return type, a
/// constant) is left alone rather than guessed at from its capitalization.
#[tokio::test]
async fn semantic_tokens_classify_names_by_position() {
    let source = "<?php\nfunction run(): void\n{\n    return Builder::new();\n}\n";
    let mut h = Harness::start(&[("a.php", source)]).await;
    let result = h.for_doc("textDocument/semanticTokens/full", "a.php").await;
    let tokens = decoded_tokens(&result);

    // `void` on line 1 at character 15 must not be claimed at all.
    assert!(
        !tokens.iter().any(|(line, character, ..)| *line == 1 && *character == 15),
        "return type was tokenized: {tokens:?}"
    );
    // `new` on line 3 is a static method call, not the `new` operator.
    let new_call = tokens.iter().find(|(line, character, ..)| *line == 3 && *character == 20);
    assert_eq!(new_call.map(|(.., kind)| *kind), Some(TOKEN_FUNCTION), "got {tokens:?}");
    // ... while `function` on line 1 is still a keyword.
    let keyword = tokens.iter().find(|(line, character, ..)| *line == 1 && *character == 0);
    assert_eq!(keyword.map(|(.., kind)| *kind), Some(TOKEN_KEYWORD), "got {tokens:?}");
}

#[tokio::test]
async fn references_cross_file() {
    let mut h =
        Harness::start(&[("lib.php", "<?php\nclass Greeter {}\n"), ("c.php", "<?php\n$g = new Greeter();\n")]).await;
    h.open("lib.php", "<?php\nclass Greeter {}\n").await;
    h.open("c.php", "<?php\n$g = new Greeter();\n").await;
    let result = h
        .request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": h.url("lib.php") },
                "position": { "line": 1, "character": 6 },
                "context": { "includeDeclaration": true },
            }),
        )
        .await;
    let uris: Vec<&str> = result.as_array().unwrap().iter().map(|l| l["uri"].as_str().unwrap_or("")).collect();
    assert!(uris.iter().any(|u| u.ends_with("lib.php")));
    assert!(uris.iter().any(|u| u.ends_with("c.php")));
}

#[tokio::test]
async fn references_follows_use_alias() {
    let lib = "<?php\nnamespace Bar;\nclass G {}\n";
    let consumer = "<?php\nnamespace Foo;\nuse Bar\\G as Qux;\n$x = new Qux();\n";
    let mut h = Harness::start(&[("lib.php", lib), ("c.php", consumer)]).await;
    h.open("lib.php", lib).await;
    h.open("c.php", consumer).await;

    let result = h
        .request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": h.url("c.php") },
                "position": { "line": 3, "character": 10 },
                "context": { "includeDeclaration": true },
            }),
        )
        .await;
    let uris: Vec<&str> = result.as_array().unwrap().iter().map(|l| l["uri"].as_str().unwrap_or("")).collect();
    assert!(uris.iter().any(|u| u.ends_with("lib.php")), "missing declaration in lib.php, got {uris:?}");
    assert!(uris.iter().any(|u| u.ends_with("c.php")), "missing alias usage in c.php, got {uris:?}");
}

#[tokio::test]
async fn workspace_symbol() {
    let mut h = Harness::start(&[("a.php", SAMPLE)]).await;
    let result = h.request("workspace/symbol", json!({ "query": "Greet" })).await;
    let names: Vec<&str> = result.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap_or("")).collect();
    assert!(names.iter().any(|n| n.ends_with("Greeter")), "got {names:?}");
}

#[tokio::test]
async fn signature_help() {
    let code = "<?php\nfunction add(int $left, int $right): int { return $left + $right; }\n\nadd(1, ";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h.at("textDocument/signatureHelp", "a.php", 3, 7).await;
    assert!(result["signatures"][0]["label"].as_str().unwrap_or("").contains("add"));
    assert_eq!(result["activeParameter"], 1);
}

#[tokio::test]
async fn inlay_hints() {
    let code = "<?php\nfunction add(int $left, int $right): int { return $left + $right; }\n\nadd(1, 2);\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h
        .request(
            "textDocument/inlayHint",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 5, "character": 0 }
                }
            }),
        )
        .await;
    let labels: Vec<&str> = result.as_array().unwrap().iter().map(|h| h["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"left:") && labels.contains(&"right:"), "got {labels:?}");
}

#[tokio::test]
async fn rename() {
    let code = "<?php\nclass Greeter {}\n$g = new Greeter();\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "position": { "line": 1, "character": 6 },
                "newName": "Speaker",
            }),
        )
        .await;
    let edits = result["changes"][&h.url("a.php")].as_array().unwrap();
    let texts: Vec<&str> = edits.iter().map(|e| e["newText"].as_str().unwrap_or("")).collect();
    assert!(texts.iter().all(|t| *t == "Speaker") && texts.len() >= 2);
}

#[tokio::test]
async fn rename_variable_accepts_dollar_prefixed_name() {
    let code = "<?php\n$name = 'Ada';\necho $name;\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;

    let prepare = h.at("textDocument/prepareRename", "a.php", 1, 2).await;
    assert_eq!(prepare["placeholder"], "$name");

    let result = h
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "position": { "line": 1, "character": 2 },
                "newName": "$label",
            }),
        )
        .await;

    let edits = result["changes"][&h.url("a.php")].as_array().unwrap();
    let texts: Vec<&str> = edits.iter().map(|e| e["newText"].as_str().unwrap_or("")).collect();
    assert_eq!(texts, vec!["$label", "$label"]);
}

#[tokio::test]
async fn rename_variable_only_renames_enclosing_scope() {
    let code = "<?php\n$name = 'global';\nfunction greet(string $name): void {\n    echo $name;\n}\necho $name;\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;

    let result = h
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "position": { "line": 3, "character": 11 },
                "newName": "$label",
            }),
        )
        .await;

    let edits = result["changes"][&h.url("a.php")].as_array().unwrap();
    let lines: Vec<u64> = edits.iter().map(|e| e["range"]["start"]["line"].as_u64().unwrap()).collect();
    let texts: Vec<&str> = edits.iter().map(|e| e["newText"].as_str().unwrap_or("")).collect();
    assert_eq!(texts, vec!["$label", "$label"]);
    assert_eq!(lines, vec![2, 3]);
}

#[tokio::test]
async fn document_link() {
    let lib = "<?php\nnamespace App;\nclass Greeter {}\n";
    let consumer = "<?php\nuse App\\Greeter;\n\n$g = new Greeter();\n";
    let mut h = Harness::start(&[("lib.php", lib), ("c.php", consumer)]).await;
    h.open("c.php", consumer).await;
    let result = h.for_doc("textDocument/documentLink", "c.php").await;
    let targets: Vec<&str> = result.as_array().unwrap().iter().filter_map(|l| l["target"].as_str()).collect();
    assert!(targets.iter().any(|t| t.ends_with("lib.php")), "got {targets:?}");
}

#[tokio::test]
async fn code_lens() {
    let mut h = Harness::start(&[("a.php", SAMPLE)]).await;
    h.open("a.php", SAMPLE).await;
    let result = h.for_doc("textDocument/codeLens", "a.php").await;
    let titles: Vec<&str> = result.as_array().unwrap().iter().filter_map(|l| l["command"]["title"].as_str()).collect();
    assert!(titles.iter().any(|t| t.contains("reference")), "got {titles:?}");
}

#[tokio::test]
async fn completion_variables() {
    let code = "<?php\nfunction demo(): void {\n    $alpha = 1;\n    $alphabet = 2;\n    $a\n}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h.at("textDocument/completion", "a.php", 4, 6).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"$alpha") && labels.contains(&"$alphabet"));
}

#[tokio::test]
async fn completion_classes_bare_prefix() {
    let mut h = Harness::start(&[
        ("lib.php", "<?php\nnamespace App;\nclass Greeter {}\nclass Goodbye {}\n"),
        ("c.php", "<?php\nnamespace App;\n\n$g = new G\n"),
    ])
    .await;
    h.open("c.php", "<?php\nnamespace App;\n\n$g = new G\n").await;
    let result = h.at("textDocument/completion", "c.php", 3, 11).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"Greeter") && labels.contains(&"Goodbye"), "got {labels:?}");
}

#[tokio::test]
async fn completion_methods_on_this() {
    let code = "<?php\nclass Greeter {\n    public function hello(string $n): string { return ''; }\n    public function howdy(string $n): string { return ''; }\n\n    public function dispatch(): void {\n        $this->h\n    }\n}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h.at("textDocument/completion", "a.php", 6, 16).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"hello") && labels.contains(&"howdy"), "got {labels:?}");
}

#[tokio::test]
async fn completion_methods_typed_param() {
    let code = "<?php\nclass Greeter {\n    public function hello(string $n): string { return ''; }\n    public function howdy(string $n): string { return ''; }\n}\n\nfunction dispatch(Greeter $g): void {\n    $g->h\n}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h.at("textDocument/completion", "a.php", 7, 9).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"hello") && labels.contains(&"howdy"), "got {labels:?}");
}

#[tokio::test]
async fn completion_properties_typed_param() {
    let code = "<?php\nclass Bag { public string $alpha = ''; public int $beta = 0; }\n\nfunction open(Bag $b): void {\n    $b->a\n}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h.at("textDocument/completion", "a.php", 4, 9).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"alpha"), "got {labels:?}");
}

#[tokio::test]
async fn completion_static_constants() {
    let code = "<?php\nfinal class Status {\n    public const string ACTIVE = 'active';\n    public const string ARCHIVED = 'archived';\n}\n\n$x = Status::A\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h.at("textDocument/completion", "a.php", 6, 14).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"ACTIVE") && labels.contains(&"ARCHIVED"), "got {labels:?}");
}

#[tokio::test]
async fn completion_static_offers_static_members_only() {
    let code = "<?php\nclass Box {\n    public static function make(): void {}\n    public function open(): void {}\n    public const string TAG = 'x';\n}\n\nBox::\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h.at("textDocument/completion", "a.php", 7, 5).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"make") && labels.contains(&"TAG"), "expected static members, got {labels:?}");
    assert!(!labels.contains(&"open"), "instance method must not appear after `::`, got {labels:?}");
}

#[tokio::test]
async fn completion_instance_offers_instance_members_only() {
    let code = "<?php\nclass Box {\n    public static function make(): void {}\n    public function open(): void {}\n}\n\nfunction run(Box $b): void {\n    $b->\n}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h.at("textDocument/completion", "a.php", 7, 8).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"open"), "expected instance method, got {labels:?}");
    assert!(!labels.contains(&"make"), "static method must not appear after `->`, got {labels:?}");
}

#[tokio::test]
async fn completion_does_not_offer_anonymous_classes() {
    let code = "<?php\nclass Real {}\n$x = new class {};\n$y = new R\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h.at("textDocument/completion", "a.php", 3, 10).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"Real"), "expected the named class, got {labels:?}");
    assert!(!labels.iter().any(|l| l.starts_with('{')), "anonymous classes must not appear, got {labels:?}");
}

#[tokio::test]
async fn completion_variables_skip_the_partial_being_typed() {
    let code = "<?php\nfunction demo(): void {\n    $table = 1;\n    $t\n}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h.at("textDocument/completion", "a.php", 3, 6).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"$table"), "expected the in-scope variable, got {labels:?}");
    assert!(!labels.contains(&"$t"), "the partial being typed must not be offered, got {labels:?}");
}

#[tokio::test]
async fn completion_variable_edit_preserves_the_dollar_sign() {
    let code = "<?php\nfunction demo(): void {\n    $table = 1;\n    $t\n}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h.at("textDocument/completion", "a.php", 3, 6).await;
    let r = completion_array(&result);
    let item = r
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["label"].as_str() == Some("$table"))
        .expect("expected $table completion");
    // The edit rewrites the whole `$table`, including the `$`, over the typed range.
    assert_eq!(item["textEdit"]["newText"].as_str(), Some("$table"), "got {item:?}");
    assert_eq!(item["textEdit"]["range"]["start"]["character"].as_u64(), Some(4), "should replace from the `$`");
    assert_eq!(item["textEdit"]["range"]["end"]["character"].as_u64(), Some(6));
}

#[tokio::test]
async fn completion_qualified_includes_sub_namespace_classes() {
    let lib = "<?php\nnamespace Foo\\Bar;\nclass Qux {}\n";
    let consumer = "<?php\n\\Foo\\\n";
    let mut h = Harness::start(&[("lib.php", lib), ("c.php", consumer)]).await;
    h.open("lib.php", lib).await;
    h.open("c.php", consumer).await;
    let result = h.at("textDocument/completion", "c.php", 1, 5).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"Bar\\Qux"), "expected the sub-namespace class, got {labels:?}");
}

#[tokio::test]
async fn selection_range() {
    let code = "<?php\nclass A {\n    public function f(): void {\n        echo 1;\n    }\n}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h
        .request(
            "textDocument/selectionRange",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "positions": [{ "line": 3, "character": 13 }]
            }),
        )
        .await;
    let mut depth = 0;
    let mut node: &Value = &result[0];
    while !node["parent"].is_null() && depth < 16 {
        depth += 1;
        node = &node["parent"];
    }
    assert!(depth >= 2);
}

#[tokio::test]
async fn completion_after_lone_dollar_offers_local_variables() {
    let code = "<?php\n\nfunction demo(): void {\n    $alpha = 1;\n    $beta = 2;\n    $\n}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h.at("textDocument/completion", "a.php", 5, 5).await;
    let r = completion_array(&result);
    let labels: Vec<String> =
        r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("").to_string()).collect();
    assert!(labels.iter().any(|l| l == "$alpha"), "expected $alpha in {labels:?}");
    assert!(labels.iter().any(|l| l == "$beta"), "expected $beta in {labels:?}");
}

#[tokio::test]
async fn completion_after_arrow_offers_instance_members() {
    let code = "<?php\n\nclass Greeter {\n    public string $name = '';\n    public function hello(): string { return ''; }\n}\n\nfunction demo(Greeter $activity): void {\n    $activity->\n}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h.at("textDocument/completion", "a.php", 8, 15).await;
    let r = completion_array(&result);
    let labels: Vec<String> =
        r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("").to_string()).collect();
    assert!(labels.iter().any(|l| l == "name"), "expected `name` property in {labels:?}");
    assert!(labels.iter().any(|l| l == "hello"), "expected `hello` method in {labels:?}");
    assert!(!labels.iter().any(|l| l.starts_with('$')), "did not expect variables in {labels:?}");
}

#[tokio::test]
async fn linter_quickfixes() {
    let code = "<?php\nfunction check(mixed $x): bool { return $x === null; }\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 4, "character": 0 } },
                "context": { "diagnostics": [] }
            }),
        )
        .await;
    if let Some(arr) = result.as_array() {
        for action in arr {
            assert!(action["kind"].as_str().unwrap_or("").contains("quickfix"));
        }
    }
}

#[tokio::test]
async fn code_action_removes_all_unused_imports_in_file() {
    let code = "<?php\nnamespace App;\n\nuse Foo\\Bar;\nuse Foo\\Baz;\n\nfinal class Demo {}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;

    let result = h
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "range": { "start": { "line": 6, "character": 12 }, "end": { "line": 6, "character": 12 } },
                "context": { "diagnostics": [] }
            }),
        )
        .await;

    let actions = result.as_array().unwrap();
    let action = actions
        .iter()
        .find(|action| action["title"] == "Fix all `no-redundant-use` issues in file")
        .unwrap_or_else(|| panic!("missing remove-all action in {actions:?}"));
    let edits = action["edit"]["changes"][&h.url("a.php")].as_array().unwrap();
    assert_eq!(edits.len(), 2, "got {edits:?}");
}

#[tokio::test]
async fn code_action_wraps_all_interpolated_variables_in_braces() {
    let code = "<?php\n$name = 'Ada';\necho \"Hello, $name!\";\necho \"Bye, $name!\";\nfinal class Demo {}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;

    let result = h
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "range": { "start": { "line": 4, "character": 12 }, "end": { "line": 4, "character": 12 } },
                "context": { "diagnostics": [] }
            }),
        )
        .await;

    let actions = result.as_array().unwrap();
    let action = actions
        .iter()
        .find(|action| action["title"] == "Fix all `braced-string-interpolation` issues in file")
        .unwrap_or_else(|| panic!("missing interpolation action in {actions:?}"));
    let edits = action["edit"]["changes"][&h.url("a.php")].as_array().unwrap();
    let new_texts: Vec<&str> = edits.iter().map(|edit| edit["newText"].as_str().unwrap_or("")).collect();
    assert_eq!(new_texts, vec!["{", "}", "{", "}"]);
}

#[tokio::test]
async fn code_action_fix_all_groups_any_repeated_fixable_issue() {
    let code = "<?php\nIF (true) {\n    ECHO \"ok\";\n}\nfinal class Demo {}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;

    let result = h
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "range": { "start": { "line": 4, "character": 12 }, "end": { "line": 4, "character": 12 } },
                "context": { "diagnostics": [] }
            }),
        )
        .await;

    let actions = result.as_array().unwrap();
    let action = actions
        .iter()
        .find(|action| action["title"] == "Fix all `lowercase-keyword` issues in file")
        .unwrap_or_else(|| panic!("missing lowercase-keyword fix-all action in {actions:?}"));
    let edits = action["edit"]["changes"][&h.url("a.php")].as_array().unwrap();
    let new_texts: Vec<&str> = edits.iter().map(|edit| edit["newText"].as_str().unwrap_or("")).collect();
    assert_eq!(new_texts, vec!["if", "echo"]);
}

#[tokio::test]
async fn code_action_orders_direct_fix_then_expect_then_fix_all() {
    let code = "<?php\nIF (true) {\n    ECHO \"ok\";\n}\nfinal class Demo {}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;

    let result = h
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 2 } },
                "context": { "diagnostics": [] }
            }),
        )
        .await;

    let actions = result.as_array().unwrap();
    let titles: Vec<&str> = actions.iter().map(|action| action["title"].as_str().unwrap_or("")).collect();

    let expect_at = titles.iter().position(|title| title.starts_with("Add @mago-expect"));
    let fix_all_at = titles.iter().position(|title| title.starts_with("Fix all "));
    let direct_at =
        titles.iter().position(|title| !title.starts_with("Add @mago-expect") && !title.starts_with("Fix all "));

    let direct_at = direct_at.unwrap_or_else(|| panic!("missing direct fix in {titles:?}"));
    let expect_at = expect_at.unwrap_or_else(|| panic!("missing @mago-expect action in {titles:?}"));
    let fix_all_at = fix_all_at.unwrap_or_else(|| panic!("missing fix-all action in {titles:?}"));

    assert!(direct_at < expect_at, "direct fix must precede @mago-expect in {titles:?}");
    assert!(expect_at < fix_all_at, "@mago-expect must precede fix-all in {titles:?}");
}

#[tokio::test]
async fn code_action_lowercases_self_keyword() {
    let code = "<?php\nenum InvoiceStatus {\n    case New;\n\n    public function label(): string {\n        return match ($this) {\n            Self::New => 'New',\n        };\n    }\n}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;

    let result = h
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "range": { "start": { "line": 6, "character": 12 }, "end": { "line": 6, "character": 16 } },
                "context": { "diagnostics": [] }
            }),
        )
        .await;

    let actions = result.as_array().unwrap();
    let action = actions
        .iter()
        .find(|action| action["title"].as_str().unwrap_or("").contains("self` instead of `Self"))
        .unwrap_or_else(|| panic!("missing Self lowercase action in {actions:?}"));
    let edits = action["edit"]["changes"][&h.url("a.php")].as_array().unwrap();
    let new_texts: Vec<&str> = edits.iter().map(|edit| edit["newText"].as_str().unwrap_or("")).collect();
    assert_eq!(new_texts, vec!["self"]);
}

#[tokio::test]
async fn code_action_adds_mago_expect_for_diagnostic() {
    let code = "<?php\nenum InvoiceStatus {\n    case New;\n\n    public function label(): string {\n        return match ($this) {\n            Self::New => 'New',\n        };\n    }\n}\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;

    let result = h
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "range": { "start": { "line": 6, "character": 12 }, "end": { "line": 6, "character": 16 } },
                "context": { "diagnostics": [] }
            }),
        )
        .await;

    let actions = result.as_array().unwrap();
    let action = actions
        .iter()
        .find(|action| action["title"] == "Add @mago-expect lint:lowercase-keyword")
        .unwrap_or_else(|| panic!("missing @mago-expect action in {actions:?}"));
    let edits = action["edit"]["changes"][&h.url("a.php")].as_array().unwrap();
    assert_eq!(edits.len(), 1, "got {edits:?}");
    assert_eq!(edits[0]["newText"], "            /** @mago-expect lint:lowercase-keyword */\n");
    assert_eq!(edits[0]["range"]["start"]["line"], 6);
}

#[tokio::test]
async fn completion_ranks_acronym_above_substring() {
    let lib = "<?php\nnamespace App;\nclass GetAllTransactionsQueryHandler {}\nclass Gauge {}\n";
    let consumer = "<?php\nnamespace App;\n\n$x = new GATQH\n";
    let mut h = Harness::start(&[("lib.php", lib), ("c.php", consumer)]).await;
    h.open("c.php", consumer).await;
    let result = h.at("textDocument/completion", "c.php", 3, 14).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert_eq!(labels.first().copied(), Some("GetAllTransactionsQueryHandler"), "got {labels:?}");
}

#[tokio::test]
async fn completion_after_new_offers_classes_not_functions() {
    let lib = "<?php\nnamespace App;\nclass Allocator {}\nfunction all_things(): void {}\n";
    let consumer = "<?php\nnamespace App;\n\n$x = new All\n";
    let mut h = Harness::start(&[("lib.php", lib), ("c.php", consumer)]).await;
    h.open("c.php", consumer).await;
    let result = h.at("textDocument/completion", "c.php", 3, 12).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"Allocator"), "expected the class, got {labels:?}");
    assert!(!labels.contains(&"all_things"), "functions must not appear after `new`, got {labels:?}");
}

#[tokio::test]
async fn completion_inserts_fqcn_for_out_of_namespace_class() {
    let lib = "<?php\nnamespace App\\Models;\nclass User {}\n";
    let consumer = "<?php\nnamespace App;\n\n$x = new User\n";
    let mut h = Harness::start(&[("lib.php", lib), ("c.php", consumer)]).await;
    h.open("c.php", consumer).await;
    let result = h.at("textDocument/completion", "c.php", 3, 13).await;
    let r = completion_array(&result);
    let item =
        r.as_array().unwrap().iter().find(|i| i["label"].as_str() == Some("User")).expect("expected User completion");
    assert_eq!(item["insertText"].as_str(), Some("\\App\\Models\\User"), "got {item:?}");
}

#[tokio::test]
async fn completion_static_member_substring_matches() {
    let code = "<?php\nenum InvoiceStatus {\n    case Draft;\n    case Finalized;\n    case Uncollectible;\n}\n\n$x = InvoiceStatus::a\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;
    let result = h.at("textDocument/completion", "a.php", 7, 20).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"Draft"), "expected Draft (contains a), got {labels:?}");
    assert!(labels.contains(&"Finalized"), "expected Finalized (contains a), got {labels:?}");
    assert!(labels.contains(&"cases"), "expected cases() method (contains a), got {labels:?}");
}

#[tokio::test]
async fn completion_imported_class_inserts_short_name() {
    let lib = "<?php\nnamespace App\\Enum;\nenum InvoiceStatus {\n    case Draft;\n}\n";
    let consumer = "<?php\nnamespace App\\Service;\nuse App\\Enum\\InvoiceStatus;\n\nfunction demo(): void {\n    $x = InvoiceS\n}\n";
    let mut h = Harness::start(&[("lib.php", lib), ("c.php", consumer)]).await;
    h.open("lib.php", lib).await;
    h.open("c.php", consumer).await;
    let result = h.at("textDocument/completion", "c.php", 5, 17).await;
    let r = completion_array(&result);
    let item = r
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["label"].as_str() == Some("InvoiceStatus"))
        .expect("expected InvoiceStatus completion");
    assert!(item["insertText"].is_null(), "imported class should insert its short name, got {item:?}");
}

#[tokio::test]
async fn completion_classifies_identifier_touching_closing_paren() {
    let lib = "<?php\nnamespace App;\nclass InvoiceStatus {}\n";
    let consumer = "<?php\nnamespace App;\n\nfunction demo(): void {\n    find(InvoiceS)\n}\n";
    let mut h = Harness::start(&[("lib.php", lib), ("c.php", consumer)]).await;
    h.open("lib.php", lib).await;
    h.open("c.php", consumer).await;
    let result = h.at("textDocument/completion", "c.php", 4, 17).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"InvoiceStatus"), "identifier touching `)` should still complete, got {labels:?}");
}

#[tokio::test]
async fn completion_ranks_local_namespace_before_distant() {
    let near = "<?php\nnamespace App\\Service;\nclass Invoicer {}\n";
    let far = "<?php\nnamespace Other\\Deep\\Place;\nclass Invoicer {}\n";
    let consumer = "<?php\nnamespace App\\Service;\n\nfunction demo(): void {\n    $x = Invoice\n}\n";
    let mut h = Harness::start(&[("near.php", near), ("far.php", far), ("c.php", consumer)]).await;
    h.open("near.php", near).await;
    h.open("far.php", far).await;
    h.open("c.php", consumer).await;
    let result = h.at("textDocument/completion", "c.php", 4, 15).await;
    let r = completion_array(&result);
    let items = r.as_array().unwrap();
    let first = items
        .iter()
        .find(|i| i["label"].as_str() == Some("Invoicer"))
        .and_then(|i| i["documentation"]["value"].as_str())
        .expect("expected an Invoicer completion");
    assert_eq!(first, "App\\Service\\Invoicer", "same-namespace Invoicer should rank first, got {first:?}");
}

#[tokio::test]
async fn completion_static_member_on_imported_short_name() {
    let lib =
        "<?php\nnamespace App\\Enum;\nenum InvoiceStatus {\n    case Draft;\n    case Paid;\n    case Finalized;\n}\n";
    let consumer = "<?php\nnamespace App\\Service;\nuse App\\Enum\\InvoiceStatus;\n\nfunction demo(): void {\n    $x = InvoiceStatus::Pa\n}\n";
    let mut h = Harness::start(&[("lib.php", lib), ("c.php", consumer)]).await;
    h.open("lib.php", lib).await;
    h.open("c.php", consumer).await;
    let result = h.at("textDocument/completion", "c.php", 5, 26).await;
    let r = completion_array(&result);
    let labels: Vec<&str> = r.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap_or("")).collect();
    assert!(labels.contains(&"Paid"), "imported short name should resolve enum cases, got {labels:?}");
    assert!(!labels.is_empty(), "imported short name receiver must resolve, got {labels:?}");
}

#[tokio::test]
async fn multi_workspace_symbol_spans_every_folder() {
    let mut h = Harness::start_multi(&[
        ("alpha", &[("a.php", "<?php\nnamespace Alpha;\nclass AlphaService {}\n")]),
        ("beta", &[("b.php", "<?php\nnamespace Beta;\nclass BetaService {}\n")]),
    ])
    .await;
    let result = h.request("workspace/symbol", json!({ "query": "Service" })).await;
    let names: Vec<&str> = result.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap_or("")).collect();
    assert!(names.iter().any(|n| n.ends_with("AlphaService")), "missing symbol from first folder, got {names:?}");
    assert!(names.iter().any(|n| n.ends_with("BetaService")), "missing symbol from second folder, got {names:?}");
}

#[tokio::test]
async fn multi_workspace_hover_routes_to_owning_folder() {
    let mut h = Harness::start_multi(&[
        ("alpha", &[("a.php", "<?php\nnamespace Alpha;\nfinal class AlphaThing {}\n")]),
        ("beta", &[("b.php", "<?php\nnamespace Beta;\nfinal class BetaThing {}\n")]),
    ])
    .await;

    let alpha = h.at_uri("textDocument/hover", &h.url_in("alpha", "a.php"), 2, 13).await;
    let alpha_value = alpha["contents"]["value"].as_str().unwrap_or("");
    assert!(alpha_value.contains("AlphaThing"), "hover in first folder failed, got {alpha_value:?}");

    let beta = h.at_uri("textDocument/hover", &h.url_in("beta", "b.php"), 2, 13).await;
    let beta_value = beta["contents"]["value"].as_str().unwrap_or("");
    assert!(beta_value.contains("BetaThing"), "hover in second folder failed, got {beta_value:?}");
}

#[tokio::test]
async fn multi_workspace_applies_per_folder_config() {
    // The `tabs` folder ships its own mago.toml turning on tab indentation;
    // the `spaces` folder has none and inherits the (spaces) default. Each
    // workspace must format according to its own discovered config.
    let messy = "<?php\n\nfunction demo(): void {\necho 1;\n}\n";
    let mut h = Harness::start_multi(&[
        ("tabs", &[("a.php", messy), ("mago.toml", "[formatter]\nuse-tabs = true\n")]),
        ("spaces", &[("a.php", messy)]),
    ])
    .await;

    let tabs_uri = h.url_in("tabs", "a.php");
    let tabs = h
        .request(
            "textDocument/formatting",
            json!({ "textDocument": { "uri": tabs_uri }, "options": { "tabSize": 4, "insertSpaces": true } }),
        )
        .await;
    let tabs_text = tabs[0]["newText"].as_str().unwrap_or("");
    assert!(tabs_text.contains('\t'), "the `tabs` workspace config should format with tabs, got {tabs_text:?}");

    let spaces_uri = h.url_in("spaces", "a.php");
    let spaces = h
        .request(
            "textDocument/formatting",
            json!({ "textDocument": { "uri": spaces_uri }, "options": { "tabSize": 4, "insertSpaces": true } }),
        )
        .await;
    let spaces_text = spaces[0]["newText"].as_str().unwrap_or("");
    assert!(!spaces_text.contains('\t'), "the `spaces` workspace should not use tabs, got {spaces_text:?}");
}

#[tokio::test]
async fn multi_workspace_handles_dynamic_folder_add() {
    let mut h =
        Harness::start_multi(&[("alpha", &[("a.php", "<?php\nnamespace Alpha;\nclass AlphaThing {}\n")])]).await;

    let before = h.request("workspace/symbol", json!({ "query": "BetaThing" })).await;
    let before_names: Vec<&str> = before.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap_or("")).collect();
    assert!(!before_names.iter().any(|n| n.ends_with("BetaThing")), "beta should not exist yet, got {before_names:?}");

    h.add_folder("beta", &[("b.php", "<?php\nnamespace Beta;\nclass BetaThing {}\n")]).await;

    let after = h.request("workspace/symbol", json!({ "query": "BetaThing" })).await;
    let after_names: Vec<&str> = after.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap_or("")).collect();
    assert!(
        after_names.iter().any(|n| n.ends_with("BetaThing")),
        "beta should be added dynamically, got {after_names:?}"
    );
}

/// The enum from the bug report: a `case` declaration plus a `self::` use in a
/// `match`. Both sites must rename together.
const SERIES_ENUM: &str = "<?php\nenum Series: string {\n    case FrenchRailEC = 'french-rail-ec';\n    case EnergyPrime = 'energy-prime';\n\n    public function label(): string {\n        return match ($this) {\n            self::FrenchRailEC => 'French Rail EC',\n            self::EnergyPrime => 'Energy Prime',\n        };\n    }\n}\n";

/// `(line, newText)` for every edit the rename produced in `name`.
fn edits_in(result: &Value, uri: &str) -> Vec<(u64, String)> {
    let mut edits: Vec<(u64, String)> = result["changes"][uri]
        .as_array()
        .unwrap_or_else(|| panic!("no edits for {uri} in {result}"))
        .iter()
        .map(|e| (e["range"]["start"]["line"].as_u64().unwrap(), e["newText"].as_str().unwrap_or("").to_owned()))
        .collect();
    edits.sort();
    edits
}

#[tokio::test]
async fn rename_enum_case_from_usage() {
    let mut h = Harness::start(&[("a.php", SERIES_ENUM)]).await;
    h.open("a.php", SERIES_ENUM).await;

    // `self::EnergyPrime` in the match arm.
    let result = h
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "position": { "line": 8, "character": 20 },
                "newName": "EnergyPremium",
            }),
        )
        .await;

    assert_eq!(
        edits_in(&result, &h.url("a.php")),
        vec![(3, "EnergyPremium".to_owned()), (8, "EnergyPremium".to_owned())],
    );
}

#[tokio::test]
async fn rename_enum_case_from_declaration() {
    let mut h = Harness::start(&[("a.php", SERIES_ENUM)]).await;
    h.open("a.php", SERIES_ENUM).await;

    // The `case EnergyPrime` declaration itself.
    let result = h
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "position": { "line": 3, "character": 12 },
                "newName": "EnergyPremium",
            }),
        )
        .await;

    assert_eq!(
        edits_in(&result, &h.url("a.php")),
        vec![(3, "EnergyPremium".to_owned()), (8, "EnergyPremium".to_owned())],
    );
}

#[tokio::test]
async fn prepare_rename_offers_enum_case() {
    let mut h = Harness::start(&[("a.php", SERIES_ENUM)]).await;
    h.open("a.php", SERIES_ENUM).await;

    let prepare = h.at("textDocument/prepareRename", "a.php", 8, 20).await;
    assert_eq!(prepare["placeholder"], "EnergyPrime");
    assert_eq!(prepare["range"]["start"], json!({ "line": 8, "character": 18 }));
    assert_eq!(prepare["range"]["end"], json!({ "line": 8, "character": 29 }));
}

#[tokio::test]
async fn rename_enum_case_leaves_same_name_on_another_enum_alone() {
    let a = "<?php\nenum Alpha: string {\n    case Same = 'a';\n}\n";
    let b = "<?php\nenum Beta: string {\n    case Same = 'b';\n}\necho Beta::Same->value;\n";
    let mut h = Harness::start(&[("a.php", a), ("b.php", b)]).await;
    h.open("a.php", a).await;
    h.open("b.php", b).await;

    let result = h
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "position": { "line": 2, "character": 10 },
                "newName": "Renamed",
            }),
        )
        .await;

    assert_eq!(edits_in(&result, &h.url("a.php")), vec![(2, "Renamed".to_owned())]);
    assert!(result["changes"][&h.url("b.php")].is_null(), "Beta::Same must be untouched, got {result}");
}

#[tokio::test]
async fn rename_class_constant_follows_inheritance_across_files() {
    let a = "<?php\nclass Base { public const LIMIT = 10; }\n";
    let b = "<?php\nclass Child extends Base {\n    public function get(): int { return parent::LIMIT; }\n}\necho Child::LIMIT;\necho Base::LIMIT;\n";
    let mut h = Harness::start(&[("a.php", a), ("b.php", b)]).await;
    h.open("a.php", a).await;
    h.open("b.php", b).await;

    // From `Child::LIMIT`, which resolves to the declaration on `Base`.
    let result = h
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": h.url("b.php") },
                "position": { "line": 4, "character": 13 },
                "newName": "CEILING",
            }),
        )
        .await;

    assert_eq!(edits_in(&result, &h.url("a.php")), vec![(1, "CEILING".to_owned())]);
    assert_eq!(
        edits_in(&result, &h.url("b.php")),
        vec![(2, "CEILING".to_owned()), (4, "CEILING".to_owned()), (5, "CEILING".to_owned())],
    );
}

#[tokio::test]
async fn rename_static_method() {
    let code = "<?php\nclass Util { public static function make(): void {} }\nUtil::make();\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;

    let result = h
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "position": { "line": 2, "character": 7 },
                "newName": "build",
            }),
        )
        .await;

    assert_eq!(edits_in(&result, &h.url("a.php")), vec![(1, "build".to_owned()), (2, "build".to_owned())]);
}

#[tokio::test]
async fn rename_static_property_edits_the_name_not_the_dollar() {
    let code = "<?php\nclass Registry { public static array $items = []; }\nRegistry::$items[] = 1;\n";
    let mut h = Harness::start(&[("a.php", code)]).await;
    h.open("a.php", code).await;

    let prepare = h.at("textDocument/prepareRename", "a.php", 2, 12).await;
    assert_eq!(prepare["placeholder"], "items");
    assert_eq!(prepare["range"]["start"], json!({ "line": 2, "character": 11 }));

    let result = h
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "position": { "line": 2, "character": 12 },
                "newName": "entries",
            }),
        )
        .await;

    assert_eq!(edits_in(&result, &h.url("a.php")), vec![(1, "entries".to_owned()), (2, "entries".to_owned())]);
}

#[tokio::test]
async fn references_finds_enum_case_uses() {
    let mut h = Harness::start(&[("a.php", SERIES_ENUM)]).await;
    h.open("a.php", SERIES_ENUM).await;

    let result = h
        .request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": h.url("a.php") },
                "position": { "line": 8, "character": 20 },
                "context": { "includeDeclaration": false },
            }),
        )
        .await;

    let lines: Vec<u64> =
        result.as_array().unwrap().iter().map(|l| l["range"]["start"]["line"].as_u64().unwrap()).collect();
    assert_eq!(lines, vec![8], "declaration should be excluded, got {result}");
}

#[tokio::test]
async fn definition_jumps_to_enum_case_declaration() {
    let mut h = Harness::start(&[("a.php", SERIES_ENUM)]).await;
    h.open("a.php", SERIES_ENUM).await;

    let result = h.at("textDocument/definition", "a.php", 8, 20).await;
    let location = if result.is_array() { result[0].clone() } else { result };
    assert_eq!(location["range"]["start"], json!({ "line": 3, "character": 9 }), "got {location}");
}

#[tokio::test]
async fn hover_describes_enum_case() {
    let mut h = Harness::start(&[("a.php", SERIES_ENUM)]).await;
    h.open("a.php", SERIES_ENUM).await;

    let result = h.at("textDocument/hover", "a.php", 8, 20).await;
    let markdown = result["contents"]["value"].as_str().unwrap_or_default();
    assert!(markdown.contains("Series::EnergyPrime"), "got {markdown:?}");
}

/// The real `App\\Enums\\Series` from the bug report. Its case names are riddled
/// with shared prefixes (`EnergyPro` / `EnergyProEC` / `EnergyPrime`), which a
/// substring-based search would happily confuse.
const REAL_SERIES_ENUM: &str = r#"<?php

namespace App\Enums;

use Exception;

enum Series: int
{
    case EnergyPro = 1;
    case EnergyPlus = 2;
    case Standard = 3;
    case MiniBlinds = 4;
    case FrenchRail = 5;
    case MiniBlindsFrenchRail = 6;
    case EnergyProEC = 7;
    case MiniBlindsFrenchRailEC = 8;
    case GardenWindow = 9;
    case StandardEC = 10;
    case FrenchRailEC = 11;
    case EnergyPlusEC = 12;
    case DogDoor = 13;
    case EnergyPrime = 14;

    public static function fromSalesforce(string $seriesName): self
    {
        $seriesName = match ($seriesName) {
            'Plus Special' => 'Energy Plus',
            'Pro Special' => 'Energy Pro',
            'Pro Special EC' => 'Energy Pro EC',
            default => $seriesName,
        };

        return match (strtolower($seriesName)) {
            'energy pro' => self::EnergyPro,
            'energy plus' => self::EnergyPlus,
            'standard' => self::Standard,
            'mini blinds' => self::MiniBlinds,
            'french rail' => self::FrenchRail,
            'mini blinds/french rail', 'mini blinds/ french rail' => self::MiniBlindsFrenchRail,
            'energy pro ec' => self::EnergyProEC,
            'mini blinds/ french rail ec' => self::MiniBlindsFrenchRailEC,
            'garden window' => self::GardenWindow,
            'standard ec' => self::StandardEC,
            'french rail ec' => self::FrenchRailEC,
            'energy plus ec' => self::EnergyPlusEC,
            'dog door' => self::DogDoor,
            default => throw new Exception("Unknown Series enum variant `{$seriesName}`"),
        };
    }

    public function name(): string
    {
        return match ($this) {
            self::EnergyPro => 'Energy Pro',
            self::EnergyPlus => 'Energy Plus',
            self::Standard => 'Standard',
            self::MiniBlinds => 'Mini Blinds',
            self::FrenchRail => 'French Rail',
            self::MiniBlindsFrenchRail => 'Mini Blinds/French Rail',
            self::EnergyProEC => 'Energy Pro EC',
            self::MiniBlindsFrenchRailEC => 'Mini Blinds/French Rail EC',
            self::GardenWindow => 'Garden Window',
            self::StandardEC => 'Standard EC',
            self::FrenchRailEC => 'French Rail EC',
            self::EnergyPlusEC => 'Energy Plus EC',
            self::DogDoor => 'Dog Door',
            self::EnergyPrime => 'Energy Prime',
        };
    }
}
"#;

#[tokio::test]
async fn rename_enum_case_ignores_cases_sharing_a_prefix() {
    let mut h = Harness::start(&[("Series.php", REAL_SERIES_ENUM)]).await;
    h.open("Series.php", REAL_SERIES_ENUM).await;

    // The `case EnergyPro = 1;` declaration.
    let result = h
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": h.url("Series.php") },
                "position": { "line": 8, "character": 12 },
                "newName": "EnergyProfessional",
            }),
        )
        .await;

    // Only the declaration and its two `self::` uses; never `EnergyProEC`
    // (line 14) or `EnergyPrime` (lines 21 and 66).
    assert_eq!(
        edits_in(&result, &h.url("Series.php")),
        vec![
            (8, "EnergyProfessional".to_owned()),
            (33, "EnergyProfessional".to_owned()),
            (53, "EnergyProfessional".to_owned()),
        ],
    );
}

#[tokio::test]
async fn rename_enum_case_in_namespaced_enum() {
    let mut h = Harness::start(&[("Series.php", REAL_SERIES_ENUM)]).await;
    h.open("Series.php", REAL_SERIES_ENUM).await;

    // `self::EnergyPrime` in the `name()` match, exactly where the cursor was.
    let result = h
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": h.url("Series.php") },
                "position": { "line": 66, "character": 22 },
                "newName": "EnergyPremium",
            }),
        )
        .await;

    assert_eq!(
        edits_in(&result, &h.url("Series.php")),
        vec![(21, "EnergyPremium".to_owned()), (66, "EnergyPremium".to_owned())],
    );
}

#[tokio::test]
async fn rename_enum_case_reaches_importing_files() {
    // Mirrors the real layout: the enum in one file, an unrelated file that
    // imports it and uses `Series::Case->value`.
    let series = "<?php\n\nnamespace App\\Enums;\n\nenum Series: int\n{\n    case DogDoor = 13;\n    case EnergyPrime = 14;\n}\n";
    let seeder = "<?php\n\nnamespace Database\\Seeders;\n\nuse App\\Enums\\Series;\n\nclass ColorSeeder\n{\n    public function run(): array\n    {\n        return [\n            Series::EnergyPrime->value => ['black'],\n        ];\n    }\n}\n";

    let mut h = Harness::start(&[("Series.php", series), ("ColorSeeder.php", seeder)]).await;
    h.open("Series.php", series).await;

    // Rename from the declaration, with the seeder never opened in the editor.
    let result = h
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": h.url("Series.php") },
                "position": { "line": 7, "character": 12 },
                "newName": "EnergyPlusElite",
            }),
        )
        .await;

    assert_eq!(edits_in(&result, &h.url("Series.php")), vec![(7, "EnergyPlusElite".to_owned())]);
    assert_eq!(edits_in(&result, &h.url("ColorSeeder.php")), vec![(11, "EnergyPlusElite".to_owned())]);
}
