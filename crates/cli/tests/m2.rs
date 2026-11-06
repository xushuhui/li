mod support;

use std::fs;
use std::path::Path;

use support::{
    multi_tool_frame, reply, request_body, run, run_with_stdin, setup, sse_body, tool_frame,
    tool_messages,
};

#[test]
fn read_call_result_feeds_back_into_second_request() {
    let server = support::start(vec![
        reply(tool_frame("call_1", "read", r#"{"path":"data.txt"}"#)),
        reply(sse_body(&["读完了"], None)),
    ]);
    let (home, cwd) = setup(server.port);
    fs::write(cwd.join("data.txt"), "alpha\nbeta\n").expect("seed file");

    let output = run(home.path(), &cwd, "看看 data.txt");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    let second = request_body(&server, 1);
    let tools = second["tools"].as_array().expect("tools array");
    assert!(tools.iter().any(|tool| tool["function"]["name"] == "read"));

    let tool_messages = tool_messages(&second);
    assert_eq!(tool_messages.len(), 1);
    assert_eq!(tool_messages[0]["tool_call_id"], "call_1");
    let content = tool_messages[0]["content"].as_str().expect("content");
    assert!(content.contains("alpha"));
    assert!(content.contains("1:alpha"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("读完了"));
}

#[test]
fn write_call_creates_file() {
    let server = support::start(vec![
        reply(tool_frame(
            "call_1",
            "write",
            r#"{"path":"out.txt","content":"hello\nworld\n"}"#,
        )),
        reply(sse_body(&["已写入"], None)),
    ]);
    let (home, cwd) = setup(server.port);

    let output = run(home.path(), &cwd, "写个文件");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    let written = fs::read_to_string(cwd.join("out.txt")).expect("written file");
    assert_eq!(written, "hello\nworld\n");

    let body = request_body(&server, 1);
    let tool_messages = tool_messages(&body);
    assert!(tool_messages[0]["content"].as_str().unwrap().contains("已写入"));
}

#[test]
fn unknown_tool_reports_error_and_loop_continues() {
    let server = support::start(vec![
        reply(tool_frame("call_1", "nope", "{}")),
        reply(sse_body(&["好的"], None)),
    ]);
    let (home, cwd) = setup(server.port);

    let output = run(home.path(), &cwd, "试试");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    let body = request_body(&server, 1);
    let tool_messages = tool_messages(&body);
    let content = tool_messages[0]["content"].as_str().expect("content");
    assert!(content.contains("未知工具"));
}

#[test]
fn denied_bash_does_not_run() {
    let server = support::start(vec![
        reply(tool_frame("call_1", "bash", r#"{"command":"rm -rf victim"}"#)),
        reply(sse_body(&["命令被拒绝"], None)),
    ]);
    let (home, cwd) = setup(server.port);
    fs::create_dir_all(cwd.join("victim")).expect("victim");

    let output = run_with_stdin(home.path(), &cwd, "删掉 victim", "n\n");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(cwd.join("victim").exists(), "denied command ran");

    let body = request_body(&server, 1);
    let tool_messages = tool_messages(&body);
    assert!(
        tool_messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("拒绝")
    );
}

#[test]
fn allowlisted_bash_runs_without_prompt() {
    let server = support::start(vec![
        reply(tool_frame("call_1", "bash", r#"{"command":"cat data.txt"}"#)),
        reply(sse_body(&["看完了"], None)),
    ]);
    let (home, cwd) = setup(server.port);
    fs::write(cwd.join("data.txt"), "content\n").expect("seed");

    let output = run(home.path(), &cwd, "读一下");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    let body = request_body(&server, 1);
    let tool_messages = tool_messages(&body);
    assert!(
        tool_messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("content")
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("[y/N]"));
}

#[test]
fn parallel_reads_all_return_in_one_round() {
    let server = support::start(vec![
        reply(multi_tool_frame(&[
            ("call_a", "read", r#"{"path":"a.txt"}"#),
            ("call_b", "read", r#"{"path":"b.txt"}"#),
        ])),
        reply(sse_body(&["都读完了"], None)),
    ]);
    let (home, cwd) = setup(server.port);
    fs::write(cwd.join("a.txt"), "alpha\n").expect("seed a");
    fs::write(cwd.join("b.txt"), "beta\n").expect("seed b");

    let output = run(home.path(), &cwd, "读两个");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    let body = request_body(&server, 1);
    let tool_messages = tool_messages(&body);
    assert_eq!(tool_messages.len(), 2);

    let contents: Vec<&str> = tool_messages
        .iter()
        .map(|message| message["content"].as_str().expect("content"))
        .collect();
    assert!(contents.iter().any(|text| text.contains("alpha")));
    assert!(contents.iter().any(|text| text.contains("beta")));

    let ids: Vec<&str> = tool_messages
        .iter()
        .map(|message| message["tool_call_id"].as_str().expect("id"))
        .collect();
    assert!(ids.contains(&"call_a"));
    assert!(ids.contains(&"call_b"));
}

#[test]
fn read_then_edit_closes_the_loop() {
    let original = "one\ntwo\nthree\n";
    let tag = li_tools::tag_of(original);
    let patch = format!("[a.txt#{tag}]\nPUT 2.=2:\n+TWO\n");
    let edit_args = serde_json::json!({ "input": patch }).to_string();

    let server = support::start(vec![
        reply(tool_frame("call_1", "read", r#"{"path":"a.txt"}"#)),
        reply(tool_frame("call_2", "edit", &edit_args)),
        reply(sse_body(&["改好了"], None)),
    ]);
    let (home, cwd) = setup(server.port);
    fs::write(cwd.join("a.txt"), original).expect("seed");

    let output = run(home.path(), &cwd, "把第二行改成大写");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    assert_eq!(
        fs::read_to_string(cwd.join("a.txt")).expect("read file"),
        "one\nTWO\nthree\n"
    );

    let body = request_body(&server, 2);
    let tool_messages = tool_messages(&body);
    assert_eq!(tool_messages.len(), 2);
    assert!(tool_messages[1]["content"].as_str().unwrap().contains("a.txt#"));
}

#[test]
fn tool_round_limit_stops_the_loop() {
    let server = support::start(vec![reply(tool_frame(
        "call_1",
        "read",
        r#"{"path":"data.txt"}"#,
    ))]);
    let (home, cwd) = setup(server.port);
    fs::write(cwd.join("data.txt"), "x\n").expect("seed");

    let output = run(home.path(), &cwd, "无限读");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    let requests = server.requests.lock().expect("requests").len();
    assert_eq!(requests, 64);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("超过 64 轮"), "stderr: {stderr}");
}

#[test]
fn auto_approval_runs_without_prompt() {
    let server = support::start(vec![
        reply(tool_frame("call_1", "bash", r#"{"command":"touch made.txt"}"#)),
        reply(sse_body(&["做完了"], None)),
    ]);
    let (home, cwd) = setup(server.port);
    fs::write(
        home.path().join(".config/li/config.toml"),
        format!(
            "[model]\ndefault = \"test-model\"\nbase_url = \"http://127.0.0.1:{}/v1\"\ncontext_window = 200000\n\n[tools]\nbash_approval = \"auto\"\n",
            server.port
        ),
    )
    .expect("config");

    let output = run(home.path(), &cwd, "建个文件");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(cwd.join("made.txt").exists(), "auto mode still prompted");
}

fn set_approval(home: &Path, port: u16, mode: &str) {
    fs::write(
        home.join(".config/li/config.toml"),
        format!(
            "[model]\ndefault = \"test-model\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\ncontext_window = 200000\n\n[tools]\nbash_approval = \"{mode}\"\n"
        ),
    )
    .expect("config");
}

#[test]
fn danger_mode_runs_safe_commands_without_prompt() {
    let server = support::start(vec![
        reply(tool_frame("call_1", "bash", r#"{"command":"touch made.txt"}"#)),
        reply(sse_body(&["做完了"], None)),
    ]);
    let (home, cwd) = setup(server.port);
    set_approval(home.path(), server.port, "danger");

    let output = run(home.path(), &cwd, "建个文件");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(cwd.join("made.txt").exists());
}

#[test]
fn danger_mode_asks_before_rm() {
    let server = support::start(vec![
        reply(tool_frame("call_1", "bash", r#"{"command":"rm -rf victim"}"#)),
        reply(sse_body(&["没删"], None)),
    ]);
    let (home, cwd) = setup(server.port);
    fs::create_dir_all(cwd.join("victim")).expect("victim");
    set_approval(home.path(), server.port, "danger");

    let output = run_with_stdin(home.path(), &cwd, "删掉 victim", "n\n");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(cwd.join("victim").exists(), "rm ran without approval");
}

#[test]
fn danger_mode_runs_rm_when_confirmed() {
    let server = support::start(vec![
        reply(tool_frame("call_1", "bash", r#"{"command":"rm -rf victim"}"#)),
        reply(sse_body(&["删了"], None)),
    ]);
    let (home, cwd) = setup(server.port);
    fs::create_dir_all(cwd.join("victim")).expect("victim");
    set_approval(home.path(), server.port, "danger");

    let output = run_with_stdin(home.path(), &cwd, "删掉 victim", "y\n");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(!cwd.join("victim").exists());
}
