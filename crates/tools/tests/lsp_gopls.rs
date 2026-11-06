//! Real-gopls integration tests. Skipped when the Go toolchain is absent.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use li_tools::lsp::{DefinitionTool, DiagnosticsTool, ReferencesTool};
use li_tools::{AllowAll, SnapshotStore, Tool, ToolContext};
use serde_json::json;

fn context(dir: &Path) -> ToolContext {
    ToolContext {
        cwd: dir.to_path_buf(),
        snapshots: Arc::new(Mutex::new(SnapshotStore::default())),
        approver: Arc::new(AllowAll),
        lsp: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
    }
}

fn gopls_ready() -> bool {
    std::process::Command::new("gopls")
        .arg("version")
        .output()
        .is_ok()
        && std::process::Command::new("go")
            .arg("version")
            .output()
            .is_ok()
}

fn write_module(dir: &Path, source: &str) {
    std::fs::write(dir.join("go.mod"), "module example.com/demo\n\ngo 1.21\n").expect("go.mod");
    std::fs::write(dir.join("main.go"), source).expect("main.go");
    std::fs::create_dir_all(dir.join(".git")).expect(".git");
}

const SOURCE: &str = "package main\n\nfunc helper() int { return 1 }\n\nfunc main() {\n\t_ = helper()\n\t_ = missing\n}\n";

#[tokio::test]
async fn gopls_reports_undefined_identifier() {
    if !gopls_ready() {
        eprintln!("skipping: gopls or go not installed");
        return;
    }

    let dir = tempfile::tempdir().expect("temp");
    write_module(dir.path(), SOURCE);

    let ctx = context(dir.path());
    let outcome = DiagnosticsTool
        .execute(json!({ "path": "main.go" }), &ctx)
        .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert!(outcome.text.contains("undefined"), "{}", outcome.text);
    assert!(outcome.text.contains(":7:"), "{}", outcome.text);
}

#[tokio::test]
async fn gopls_finds_definition_of_a_call() {
    if !gopls_ready() {
        eprintln!("skipping: gopls or go not installed");
        return;
    }

    let dir = tempfile::tempdir().expect("temp");
    write_module(dir.path(), SOURCE);

    let ctx = context(dir.path());
    let outcome = DefinitionTool
        .execute(json!({ "path": "main.go", "line": 6, "column": 7 }), &ctx)
        .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert!(outcome.text.contains("main.go:3:"), "{}", outcome.text);
}

#[tokio::test]
async fn gopls_finds_references_and_reuses_the_server() {
    if !gopls_ready() {
        eprintln!("skipping: gopls or go not installed");
        return;
    }

    let dir = tempfile::tempdir().expect("temp");
    write_module(dir.path(), SOURCE);

    let ctx = context(dir.path());
    let outcome = ReferencesTool
        .execute(json!({ "path": "main.go", "line": 3, "column": 6 }), &ctx)
        .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert!(outcome.text.contains(":6:"), "{}", outcome.text);

    let second = DiagnosticsTool
        .execute(json!({ "path": "main.go" }), &ctx)
        .await;
    assert!(!second.is_error, "{}", second.text);
    assert_eq!(ctx.lsp.lock().await.len(), 1, "server should be reused");
}
