//! LSP-backed query tools: diagnostics, definition, references.

pub mod client;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolOutcome};
use client::{Client, DIAGNOSTIC_TIMEOUT, from_uri, project_root, server_for};

pub use client::{language_id, to_uri};

/// Returns a process-wide client for this file's language server.
async fn client_for(ctx: &ToolContext, path: &Path) -> Result<Arc<Client>> {
    let server = server_for(path)
        .with_context(|| format!("没有为 {} 配置语言服务器", path.display()))?;
    let root = project_root(path);
    let key = format!("{server}|{}", root.display());

    let existing = {
        let cache = ctx.lsp.lock().await;
        cache.get(&key).cloned()
    };
    if let Some(client) = existing {
        return Ok(client);
    }

    let client = Arc::new(Client::start(server, &root).await?);
    ctx.lsp.lock().await.insert(key, client.clone());
    Ok(client)
}

fn resolve(ctx: &ToolContext, raw: &str) -> PathBuf {
    let path = ctx.resolve(raw);
    std::fs::canonicalize(&path).unwrap_or(path)
}

async fn open_file(client: &Client, path: &Path) -> Result<String, ToolOutcome> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| ToolOutcome::error(format!("无法读取 {}：{error}", path.display())))?;
    client
        .open(path, &text)
        .await
        .map_err(|error| ToolOutcome::error(format!("打开文件失败：{error:#}")))?;
    Ok(text)
}

fn severity_name(value: &Value) -> &'static str {
    match value.as_u64().unwrap_or(1) {
        1 => "error",
        2 => "warning",
        3 => "info",
        _ => "hint",
    }
}

fn format_diagnostics(path: &Path, items: &[Value]) -> String {
    if items.is_empty() {
        return format!("{}：没有诊断信息", path.display());
    }

    let mut lines = Vec::with_capacity(items.len());
    for item in items {
        let line = item["range"]["start"]["line"].as_u64().unwrap_or(0) + 1;
        let column = item["range"]["start"]["character"].as_u64().unwrap_or(0) + 1;
        let message = item["message"].as_str().unwrap_or("");
        lines.push(format!(
            "{}:{line}:{column} {}: {message}",
            path.display(),
            severity_name(&item["severity"])
        ));
    }
    lines.join("\n")
}

fn format_locations(items: &[Value]) -> String {
    if items.is_empty() {
        return "没有找到位置".to_string();
    }

    let mut lines = Vec::with_capacity(items.len());
    for item in items {
        let uri = item
            .get("uri")
            .or_else(|| item.get("targetUri"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let range = item
            .get("range")
            .or_else(|| item.get("targetSelectionRange"))
            .or_else(|| item.get("targetRange"));
        let Some(range) = range else {
            continue;
        };
        let line = range["start"]["line"].as_u64().unwrap_or(0) + 1;
        let column = range["start"]["character"].as_u64().unwrap_or(0) + 1;
        let path = from_uri(uri)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| uri.to_string());
        lines.push(format!("{path}:{line}:{column}"));
    }

    if lines.is_empty() {
        "没有找到位置".to_string()
    } else {
        lines.join("\n")
    }
}

fn position_from(args: &Value) -> Result<(u64, u64), ToolOutcome> {
    let line = args["line"]
        .as_u64()
        .ok_or_else(|| ToolOutcome::error("缺少 line 参数"))?;
    let column = args["column"].as_u64().unwrap_or(1);
    if line == 0 || column == 0 {
        return Err(ToolOutcome::error("line 与 column 从 1 开始"));
    }
    Ok((line, column))
}

fn position_params(path: &Path, line: u64, column: u64) -> Value {
    json!({
        "textDocument": { "uri": to_uri(path) },
        "position": { "line": line - 1, "character": column - 1 }
    })
}

pub struct DiagnosticsTool;

#[async_trait::async_trait]
impl Tool for DiagnosticsTool {
    fn name(&self) -> &'static str {
        "diagnostics"
    }

    fn description(&self) -> &'static str {
        "返回指定文件的编译错误和警告，带从 1 开始的行列号。支持 Go、Lua、Rust。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolOutcome {
        let Some(raw) = args["path"].as_str() else {
            return ToolOutcome::error("缺少 path 参数");
        };
        let path = resolve(ctx, raw);

        let client = match client_for(ctx, &path).await {
            Ok(client) => client,
            Err(error) => return ToolOutcome::error(format!("{error:#}")),
        };

        if let Err(outcome) = open_file(&client, &path).await {
            return outcome;
        }

        let items = client.wait_diagnostics(&path, DIAGNOSTIC_TIMEOUT).await;
        ToolOutcome::ok(format_diagnostics(&path, &items))
    }
}

pub struct DefinitionTool;

#[async_trait::async_trait]
impl Tool for DefinitionTool {
    fn name(&self) -> &'static str {
        "definition"
    }

    fn description(&self) -> &'static str {
        "返回指定行列符号的定义位置。行列从 1 开始，与 read 输出一致。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "line": { "type": "integer", "minimum": 1 },
                "column": { "type": "integer", "minimum": 1, "description": "默认 1" }
            },
            "required": ["path", "line"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolOutcome {
        let Some(raw) = args["path"].as_str() else {
            return ToolOutcome::error("缺少 path 参数");
        };
        let (line, column) = match position_from(&args) {
            Ok(position) => position,
            Err(outcome) => return outcome,
        };
        let path = resolve(ctx, raw);

        let client = match client_for(ctx, &path).await {
            Ok(client) => client,
            Err(error) => return ToolOutcome::error(format!("{error:#}")),
        };
        if let Err(outcome) = open_file(&client, &path).await {
            return outcome;
        }

        match client
            .request("textDocument/definition", position_params(&path, line, column))
            .await
        {
            Ok(value) => {
                let items = value.as_array().cloned().unwrap_or_default();
                ToolOutcome::ok(format_locations(&items))
            }
            Err(error) => ToolOutcome::error(format!("{error:#}")),
        }
    }
}

pub struct ReferencesTool;

#[async_trait::async_trait]
impl Tool for ReferencesTool {
    fn name(&self) -> &'static str {
        "references"
    }

    fn description(&self) -> &'static str {
        "返回指定行列符号被引用的所有位置。行列从 1 开始。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "line": { "type": "integer", "minimum": 1 },
                "column": { "type": "integer", "minimum": 1, "description": "默认 1" }
            },
            "required": ["path", "line"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolOutcome {
        let Some(raw) = args["path"].as_str() else {
            return ToolOutcome::error("缺少 path 参数");
        };
        let (line, column) = match position_from(&args) {
            Ok(position) => position,
            Err(outcome) => return outcome,
        };
        let path = resolve(ctx, raw);

        let client = match client_for(ctx, &path).await {
            Ok(client) => client,
            Err(error) => return ToolOutcome::error(format!("{error:#}")),
        };
        if let Err(outcome) = open_file(&client, &path).await {
            return outcome;
        }

        let mut params = position_params(&path, line, column);
        params["context"] = json!({ "includeDeclaration": true });

        match client.request("textDocument/references", params).await {
            Ok(value) => {
                let items = value.as_array().cloned().unwrap_or_default();
                ToolOutcome::ok(format_locations(&items))
            }
            Err(error) => ToolOutcome::error(format!("{error:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_report_one_based_positions() {
        let item = json!({
            "range": { "start": { "line": 11, "character": 4 } },
            "severity": 1,
            "message": "undefined: foo"
        });
        let text = format_diagnostics(Path::new("main.go"), &[item]);
        assert_eq!(text, "main.go:12:5 error: undefined: foo");
    }

    #[test]
    fn diagnostics_report_empty_state() {
        let text = format_diagnostics(Path::new("main.go"), &[]);
        assert!(text.contains("没有诊断信息"));
    }

    #[test]
    fn locations_decode_file_uris() {
        let item = json!({
            "uri": "file:///tmp/a.go",
            "range": { "start": { "line": 41, "character": 5 } }
        });
        assert_eq!(format_locations(&[item]), "/tmp/a.go:42:6");
    }

    #[test]
    fn locations_accept_location_links() {
        let item = json!({
            "targetUri": "file:///tmp/b.go",
            "targetSelectionRange": { "start": { "line": 0, "character": 0 } }
        });
        assert_eq!(format_locations(&[item]), "/tmp/b.go:1:1");
    }

    #[test]
    fn positions_start_at_one() {
        assert!(position_from(&json!({ "line": 0 })).is_err());
        assert!(position_from(&json!({ "line": 1, "column": 0 })).is_err());
        match position_from(&json!({ "line": 3 })) {
            Ok((line, column)) => assert_eq!((line, column), (3, 1)),
            Err(_) => panic!("valid position rejected"),
        }
    }
}
