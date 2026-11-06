mod support;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use li_core::{Content, Message, Session, encode_dir};
use support::{reply, request_body, setup, sse_body};

fn run_cli(home: &Path, cwd: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_li"))
        .args(args)
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
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait")
}

fn sessions_dir(home: &Path, cwd: &Path) -> PathBuf {
    let canonical = fs::canonicalize(cwd).expect("canonical cwd");
    home.join(".li/sessions")
        .join(encode_dir(&canonical.to_string_lossy()))
}

fn session_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect()
}

fn line_count(path: &Path) -> usize {
    fs::read_to_string(path)
        .expect("read session")
        .lines()
        .count()
}

#[test]
fn continue_without_session_exits_1() {
    let server = support::start(vec![]);
    let (home, cwd) = setup(server.port);

    let output = run_cli(home.path(), &cwd, &["--continue", "hi"], "");

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("还没有会话"));
}

#[test]
fn continue_reuses_history() {
    let server = support::start(vec![
        reply(sse_body(&["第一轮回答"], None)),
        reply(sse_body(&["第二轮回答"], None)),
    ]);
    let (home, cwd) = setup(server.port);

    let first = run_cli(home.path(), &cwd, &["第一个问题"], "");
    assert!(first.status.success(), "stderr: {:?}", first.stderr);

    let second = run_cli(home.path(), &cwd, &["--continue", "第二个问题"], "");
    assert!(second.status.success(), "stderr: {:?}", second.stderr);

    let body = request_body(&server, 1);
    let texts: Vec<&str> = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect();

    assert!(texts.iter().any(|text| text.contains("第一个问题")));
    assert!(texts.iter().any(|text| text.contains("第一轮回答")));
    assert!(texts.iter().any(|text| text.contains("第二个问题")));
}

#[test]
fn continue_appends_to_the_same_file() {
    let server = support::start(vec![
        reply(sse_body(&["一"], None)),
        reply(sse_body(&["二"], None)),
    ]);
    let (home, cwd) = setup(server.port);

    let first = run_cli(home.path(), &cwd, &["第一句"], "");
    assert!(first.status.success(), "stderr: {:?}", first.stderr);

    let dir = sessions_dir(home.path(), &cwd);
    let files = session_files(&dir);
    assert_eq!(files.len(), 1);
    let before = line_count(&files[0]);

    let second = run_cli(home.path(), &cwd, &["--continue", "第二句"], "");
    assert!(second.status.success(), "stderr: {:?}", second.stderr);

    let files = session_files(&dir);
    assert_eq!(files.len(), 1, "should append, not create");
    assert!(line_count(&files[0]) > before);
}

#[test]
fn resume_lists_sessions_and_selects_one() {
    let server = support::start(vec![reply(sse_body(&["好的"], None))]);
    let (home, cwd) = setup(server.port);

    let dir = sessions_dir(home.path(), &cwd);
    fs::create_dir_all(&dir).expect("sessions dir");

    let mut older = Session::create(&dir, "/old", "0.1.0").expect("older session");
    older.append_message(Message::user("旧的会话")).expect("older msg");
    drop(older);

    std::thread::sleep(std::time::Duration::from_millis(1100));

    let mut newer = Session::create(&dir, "/new", "0.1.0").expect("newer session");
    newer.append_message(Message::user("新的会话")).expect("newer msg");
    drop(newer);

    let output = run_cli(home.path(), &cwd, &["--resume", "接着聊"], "1\n");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("新的会话"), "stderr: {stderr}");

    let body = request_body(&server, 0);
    let texts: Vec<&str> = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect();
    assert!(texts.iter().any(|text| text.contains("新的会话")));
    assert!(!texts.iter().any(|text| text.contains("旧的会话")));
}

#[test]
fn resume_with_blank_line_cancels() {
    let server = support::start(vec![]);
    let (home, cwd) = setup(server.port);

    let dir = sessions_dir(home.path(), &cwd);
    fs::create_dir_all(&dir).expect("sessions dir");
    let mut session = Session::create(&dir, "/work", "0.1.0").expect("session");
    session.append_message(Message::user("某个会话")).expect("msg");
    drop(session);

    let output = run_cli(home.path(), &cwd, &["--resume"], "\n");

    assert_eq!(output.status.code(), Some(0));
    let requests = server.requests.lock().expect("requests").len();
    assert_eq!(requests, 0);
}

#[test]
fn continue_repairs_dangling_tool_call() {
    let server = support::start(vec![reply(sse_body(&["继续了"], None))]);
    let (home, cwd) = setup(server.port);

    let dir = sessions_dir(home.path(), &cwd);
    fs::create_dir_all(&dir).expect("sessions dir");
    let mut session = Session::create(&dir, "/work", "0.1.0").expect("session");
    session.append_message(Message::user("跑一下")).expect("user");
    session
        .append_message(Message::assistant_turn(
            String::new(),
            vec![Content::ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: serde_json::json!({ "path": "a.txt" }),
            }],
            None,
        ))
        .expect("assistant");
    drop(session);

    let output = run_cli(home.path(), &cwd, &["--continue", "继续"], "");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    let body = request_body(&server, 0);
    let tool_messages: Vec<&serde_json::Value> = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect();

    assert_eq!(tool_messages.len(), 1);
    assert_eq!(tool_messages[0]["tool_call_id"], "call_1");
    assert!(
        tool_messages[0]["content"]
            .as_str()
            .expect("content")
            .contains("执行被中断")
    );
}
