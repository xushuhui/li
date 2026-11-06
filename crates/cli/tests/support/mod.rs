//! Minimal HTTP server that answers with scripted SSE bodies.
//!
//! Written by hand so tests can control the exact byte boundaries of each write,
//! which is what exercises the incremental SSE parser.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Clone)]
pub enum Reply {
    /// 200 with an SSE body, written in pieces of `chunk` bytes.
    Sse { body: String, chunk: usize },
    /// Arbitrary status with a plain body.
    Status { code: u16, body: String },
    /// Accept the connection, read the request, then close without answering.
    Close,
}

pub struct FakeServer {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<String>>>,
}

pub fn start(replies: Vec<Reply>) -> FakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
    let port = listener.local_addr().expect("local addr").port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);

    thread::spawn(move || {
        for (index, incoming) in listener.incoming().enumerate() {
            let Ok(mut stream) = incoming else { continue };
            let body = read_request(&mut stream);
            seen.lock().expect("request lock").push(body);

            let reply = replies
                .get(index)
                .or_else(|| replies.last())
                .cloned()
                .unwrap_or(Reply::Close);

            match reply {
                Reply::Sse { body, chunk } => write_sse(&mut stream, &body, chunk),
                Reply::Status { code, body } => write_status(&mut stream, code, &body),
                Reply::Close => {}
            }
        }
    });

    FakeServer { port, requests }
}

pub fn sse_body(parts: &[&str], usage: Option<(u64, u64)>) -> String {
    let mut body = String::new();
    for part in parts {
        let encoded = serde_json::to_string(part).expect("encode text");
        body.push_str(&format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":{encoded}}}}}]}}\n\n"
        ));
    }
    if let Some((input, output)) = usage {
        body.push_str(&format!(
            "data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{input},\"completion_tokens\":{output}}}}}\n\n"
        ));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut temp = [0u8; 1024];

    loop {
        let read = stream.read(&mut temp).unwrap_or(0);
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&temp[..read]);

        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buffer[..position]).to_string();
            let length = headers
                .lines()
                .filter_map(|line| {
                    let lower = line.to_ascii_lowercase();
                    lower
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .next()
                .unwrap_or(0);

            let body_start = position + 4;
            while buffer.len() < body_start + length {
                let read = stream.read(&mut temp).unwrap_or(0);
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&temp[..read]);
            }
            return String::from_utf8_lossy(&buffer[body_start..]).to_string();
        }
    }
    String::new()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn write_sse(stream: &mut TcpStream, body: &str, chunk: usize) {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    for piece in body.as_bytes().chunks(chunk.max(1)) {
        if stream.write_all(piece).is_err() {
            return;
        }
        let _ = stream.flush();
        thread::sleep(Duration::from_millis(1));
    }
}

fn write_status(stream: &mut TcpStream, code: u16, body: &str) {
    let header = format!(
        "HTTP/1.1 {code} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

// --- shared helpers for binary-level tests ---------------------------------

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

/// Temp home plus a separate workspace directory, both under one tempdir.
pub fn setup(port: u16) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join(".config/li");
    fs::create_dir_all(&config).expect("config dir");
    fs::write(
        config.join("config.toml"),
        format!(
            "[model]\ndefault = \"test-model\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\ncontext_window = 200000\n"
        ),
    )
    .expect("config file");
    let workspace = dir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    (dir, workspace)
}

pub fn run(home: &Path, cwd: &Path, prompt: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_li"))
        .arg(prompt)
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("LI_API_KEY")
        .stdin(Stdio::null())
        .output()
        .expect("run li")
}

pub fn run_with_stdin(home: &Path, cwd: &Path, prompt: &str, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_li"))
        .arg(prompt)
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("LI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn li");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait")
}

/// One SSE body carrying a single tool call.
pub fn tool_frame(id: &str, name: &str, arguments: &str) -> String {
    multi_tool_frame(&[(id, name, arguments)])
}

/// One SSE body carrying several tool calls in the same assistant turn.
pub fn multi_tool_frame(calls: &[(&str, &str, &str)]) -> String {
    let calls: Vec<String> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            let arguments = serde_json::to_string(arguments).expect("encode arguments");
            format!(
                "{{\"index\":{index},\"id\":\"{id}\",\"function\":{{\"name\":\"{name}\",\"arguments\":{arguments}}}}}"
            )
        })
        .collect();
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{}]}}}}]}}\n\ndata: [DONE]\n\n",
        calls.join(",")
    )
}

pub fn reply(body: String) -> Reply {
    Reply::Sse { body, chunk: 7 }
}

pub fn request_body(server: &FakeServer, index: usize) -> Value {
    let requests = server.requests.lock().expect("requests");
    let raw = requests.get(index).expect("request");
    serde_json::from_str(raw).expect("json body")
}

pub fn tool_messages(body: &Value) -> Vec<&Value> {
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect()
}
