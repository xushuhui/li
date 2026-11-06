mod support;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use baize_core::encode_dir;
use support::{reply, request_body, setup, sse_body};

fn set_window(home: &Path, window: u32, percent: u8) {
    let path = home.join(".config/baize/config.toml");
    let text = fs::read_to_string(&path).expect("config");

    let mut out: Vec<String> = Vec::new();
    let mut in_context = false;
    for line in text.lines() {
        if line.trim() == "[context]" {
            in_context = true;
            continue;
        }
        if line.trim_start().starts_with('[') {
            in_context = false;
        }
        if in_context {
            continue;
        }
        if line.trim_start().starts_with("context_window = ") {
            out.push(format!("context_window = {window}"));
        } else {
            out.push(line.to_string());
        }
    }
    out.push("[context]".to_string());
    out.push(format!("compact_at_percent = {percent}"));
    fs::write(&path, out.join("\n") + "\n").expect("write config");
}

fn sessions_dir(home: &Path, cwd: &Path) -> PathBuf {
    let canonical = fs::canonicalize(cwd).expect("canonical cwd");
    home.join(".baize/sessions")
        .join(encode_dir(&canonical.to_string_lossy()))
}

fn repl(home: &Path, cwd: &Path, lines: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_baize"))
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("BAIZE_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn baize");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for line in lines {
            writeln!(stdin, "{line}").expect("write line");
        }
    }
    drop(child.stdin.take());

    child.wait_with_output().expect("wait")
}

fn message_texts(body: &serde_json::Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .map(|text| text.to_string())
        .collect()
}

#[test]
fn long_conversation_triggers_compaction() {
    let server = support::start(vec![
        reply(sse_body(&["收到，第一轮"], None)),
        reply(sse_body(&["## 目标\n修复登录\n\n## 约束\n无"], None)),
        reply(sse_body(&["压缩之后回答"], None)),
    ]);
    let (home, cwd) = setup(server.port);
    set_window(home.path(), 4000, 50);

    let goal = "最初的目标：修复登录流程".to_string();
    let filler = "细节".repeat(600); // 约 1200 token

    let output = repl(home.path(), &cwd, &[&goal, &filler]);
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("开始压缩"), "stderr: {stderr}");

    // 第二次请求是摘要请求，必须带着最初的目标。
    let summary_request = request_body(&server, 1);
    let summary_texts = message_texts(&summary_request);
    assert!(
        summary_texts.iter().any(|text| text.contains("修复登录流程")),
        "texts: {summary_texts:?}"
    );

    // 第三次请求：system + 摘要前缀 + 最新一条消息。
    let after = request_body(&server, 2);
    let after_texts = message_texts(&after);
    assert_eq!(after_texts.len(), 3, "texts: {after_texts:?}");
    assert!(
        after_texts
            .iter()
            .any(|text| text.contains("压缩摘要") && text.contains("修复登录")),
        "texts: {after_texts:?}"
    );
    assert!(
        !after_texts.iter().any(|text| text.contains("最初的目标")),
        "原始消息不应再出现在上下文里: {after_texts:?}"
    );

    let dir = sessions_dir(home.path(), &cwd);
    let session = fs::read_dir(&dir)
        .expect("session dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .expect("session file");

    let raw = fs::read_to_string(&session).expect("session text");
    assert!(raw.contains("\"type\":\"compaction\""));
    assert!(raw.contains("修复登录流程"), "原消息必须留在文件里");
}

#[test]
fn short_conversation_does_not_compact() {
    let server = support::start(vec![
        reply(sse_body(&["第一轮"], None)),
        reply(sse_body(&["第二轮"], None)),
    ]);
    let (home, cwd) = setup(server.port);
    set_window(home.path(), 4000, 50);

    let output = repl(home.path(), &cwd, &["短问题一", "短问题二"]);
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("开始压缩"), "stderr: {stderr}");

    assert_eq!(server.requests.lock().expect("requests").len(), 2);
}

fn run_cli(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_baize"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("BAIZE_API_KEY")
        .stdin(Stdio::null())
        .output()
        .expect("run baize")
}

#[test]
fn continue_after_compaction_carries_summary() {
    let server = support::start(vec![
        reply(sse_body(&["第一轮"], None)),
        reply(sse_body(&["## 目标\n修复登录"], None)),
        reply(sse_body(&["压缩后"], None)),
        reply(sse_body(&["继续后"], None)),
    ]);
    let (home, cwd) = setup(server.port);
    set_window(home.path(), 4000, 50);

    let output = repl(
        home.path(),
        &cwd,
        &["最初的目标：修复登录流程", &"细节".repeat(600)],
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    // 放大窗口，让这一轮不再触发压缩，专注验证摘要是否被加载。
    set_window(home.path(), 20_000, 50);

    let output = run_cli(home.path(), &cwd, &["--continue", "再问一句"]);
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    let body = request_body(&server, 3);
    let texts = message_texts(&body);
    assert!(
        texts
            .iter()
            .any(|text| text.contains("压缩摘要") && text.contains("修复登录")),
        "texts: {texts:?}"
    );
}
