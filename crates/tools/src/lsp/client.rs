//! Minimal LSP client: spawn a server and speak `Content-Length` framed JSON-RPC.
//!
//! Only the subset needed by the three query tools is implemented: initialize,
//! didOpen, request/response matching, and `publishDiagnostics` caching.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Client {
    stdin: Arc<Mutex<ChildStdin>>,
    child: Child,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    diagnostics: Arc<Mutex<HashMap<String, Vec<Value>>>>,
    next_id: AtomicI64,
    reader: JoinHandle<()>,
}

impl Client {
    pub async fn start(command: &str, root: &Path) -> Result<Self> {
        let mut child = Command::new(command)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("无法启动语言服务器 {command}"))?;

        let stdin = child
            .stdin
            .take()
            .context("语言服务器没有标准输入")?;
        let stdout = child
            .stdout
            .take()
            .context("语言服务器没有标准输出")?;

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let diagnostics: Arc<Mutex<HashMap<String, Vec<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let reader = tokio::spawn(read_loop(
            BufReader::new(stdout),
            stdin.clone(),
            pending.clone(),
            diagnostics.clone(),
        ));

        let client = Self {
            stdin,
            child,
            pending,
            diagnostics,
            next_id: AtomicI64::new(1),
            reader,
        };

        let root_uri = to_uri(root);
        let init = json!({
            "processId": null,
            "rootUri": root_uri,
            "workspaceFolders": [{ "uri": root_uri, "name": "workspace" }],
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {},
                    "definition": {},
                    "references": {}
                }
            }
        });
        client.request("initialize", init).await?;
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(value)) => {
                if let Some(error) = value.get("error") {
                    bail!("{method} 失败：{error}");
                }
                Ok(value.get("result").cloned().unwrap_or(Value::Null))
            }
            Ok(Err(_)) => bail!("语言服务器在响应 {method} 前退出"),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("{method} 超过 {} 秒未响应", REQUEST_TIMEOUT.as_secs())
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    pub async fn open(&self, path: &Path, text: &str) -> Result<()> {
        self.diagnostics
            .lock()
            .await
            .remove(&to_uri(path));

        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": to_uri(path),
                    "languageId": language_id(path),
                    "version": 1,
                    "text": text,
                }
            }),
        )
        .await
    }

    pub async fn wait_diagnostics(&self, path: &Path, timeout: Duration) -> Vec<Value> {
        let uri = to_uri(path);
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            if let Some(items) = self.diagnostics.lock().await.get(&uri) {
                return items.clone();
            }
            if tokio::time::Instant::now() >= deadline {
                return Vec::new();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn write_message(&self, value: &Value) -> Result<()> {
        let body = serde_json::to_string(value)?;
        let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(frame.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
        let _ = self.child.start_kill();
    }
}

async fn read_loop(
    mut stdout: BufReader<ChildStdout>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    diagnostics: Arc<Mutex<HashMap<String, Vec<Value>>>>,
) {
    while let Ok(Some(message)) = read_message(&mut stdout).await {
        let Some(id) = message.get("id").and_then(Value::as_i64) else {
            if message.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics")
            {
                let uri = message["params"]["uri"].as_str().unwrap_or_default().to_string();
                let items = message["params"]["diagnostics"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                diagnostics.lock().await.insert(uri, items);
            }
            continue;
        };

        if message.get("method").is_some() {
            // Server-initiated request: answer with null so it does not block.
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null });
            if let Ok(body) = serde_json::to_string(&reply) {
                let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
                let mut guard = stdin.lock().await;
                let _ = guard.write_all(frame.as_bytes()).await;
                let _ = guard.flush().await;
            }
            continue;
        }

        if let Some(tx) = pending.lock().await.remove(&id) {
            let _ = tx.send(message);
        }
    }
}

async fn read_message<R>(reader: &mut R) -> Result<Option<Value>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            length = rest.trim().parse().ok();
        }
    }

    let Some(length) = length else {
        bail!("语言服务器返回的消息缺少 Content-Length");
    };
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

/// File URI with percent-encoding for anything outside the unreserved set.
pub fn to_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Inverse of [`to_uri`] for the paths we generate ourselves.
pub fn from_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(rest.len());
    let mut chars = rest.bytes();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let high = chars.next()?;
            let low = chars.next()?;
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            bytes.push(hex(high)? * 16 + hex(low)?);
        } else {
            bytes.push(byte);
        }
    }
    Some(PathBuf::from(String::from_utf8(bytes).ok()?))
}

pub fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("go") => "go",
        Some("lua") => "lua",
        Some("rs") => "rust",
        _ => "plaintext",
    }
}

/// The server binary for a file, if one is configured.
pub fn server_for(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("go") => Some("gopls"),
        Some("lua") => Some("lua-language-server"),
        Some("rs") => Some("rust-analyzer"),
        _ => None,
    }
}

/// Walks up from `start` to the directory that contains `.git`.
pub fn project_root(start: &Path) -> PathBuf {
    let mut cursor = Some(start);
    while let Some(dir) = cursor {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        cursor = dir.parent();
    }
    start.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_round_trips_plain_paths() {
        let path = Path::new("/tmp/baize/main.go");
        let uri = to_uri(path);
        assert_eq!(uri, "file:///tmp/baize/main.go");
        assert_eq!(from_uri(&uri).as_deref(), Some(path));
    }

    #[test]
    fn uri_escapes_spaces_and_unicode() {
        let uri = to_uri(Path::new("/tmp/a b/c.go"));
        assert_eq!(uri, "file:///tmp/a%20b/c.go");
        assert_eq!(
            from_uri(&uri).as_deref(),
            Some(Path::new("/tmp/a b/c.go"))
        );
    }

    #[test]
    fn maps_extensions_to_servers() {
        assert_eq!(server_for(Path::new("a.go")), Some("gopls"));
        assert_eq!(server_for(Path::new("a.lua")), Some("lua-language-server"));
        assert_eq!(server_for(Path::new("a.rs")), Some("rust-analyzer"));
        assert_eq!(server_for(Path::new("a.txt")), None);
    }
}
