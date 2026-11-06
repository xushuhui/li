mod support;

use std::fs;

use support::{
    multi_tool_frame, reply, request_body, run, setup, sse_body, tool_frame, tool_messages,
};

#[test]
fn grep_call_feeds_matches_back() {
    let server = support::start(vec![
        reply(tool_frame("call_1", "grep", r#"{"pattern":"fn main"}"#)),
        reply(sse_body(&["找到了"], None)),
    ]);
    let (home, cwd) = setup(server.port);
    fs::write(cwd.join("main.rs"), "fn main() {\n}\n").expect("seed");

    let output = run(home.path(), &cwd, "找 main");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    let body = request_body(&server, 1);
    let tools = body["tools"].as_array().expect("tools");
    assert!(tools.iter().any(|tool| tool["function"]["name"] == "grep"));

    let messages = tool_messages(&body);
    assert_eq!(messages.len(), 1);
    let content = messages[0]["content"].as_str().expect("content");
    assert!(content.contains("main.rs:1:fn main() {"), "content: {content}");
}

#[test]
fn glob_call_feeds_paths_back() {
    let server = support::start(vec![
        reply(tool_frame("call_1", "glob", r#"{"pattern":"*.txt"}"#)),
        reply(sse_body(&["列出来了"], None)),
    ]);
    let (home, cwd) = setup(server.port);
    fs::write(cwd.join("notes.txt"), "x").expect("seed");

    let output = run(home.path(), &cwd, "有哪些文本文件");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    let body = request_body(&server, 1);
    let tools = body["tools"].as_array().expect("tools");
    assert!(tools.iter().any(|tool| tool["function"]["name"] == "glob"));

    let messages = tool_messages(&body);
    let content = messages[0]["content"].as_str().expect("content");
    assert!(content.contains("notes.txt"), "content: {content}");
}

#[test]
fn grep_and_glob_run_in_the_same_round() {
    let server = support::start(vec![
        reply(multi_tool_frame(&[
            ("call_a", "grep", r#"{"pattern":"MARK"}"#),
            ("call_b", "glob", r#"{"pattern":"*.txt"}"#),
        ])),
        reply(sse_body(&["都好了"], None)),
    ]);
    let (home, cwd) = setup(server.port);
    fs::write(cwd.join("a.txt"), "MARK here\n").expect("seed grep target");

    let output = run(home.path(), &cwd, "同时搜");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);

    let body = request_body(&server, 1);
    let messages = tool_messages(&body);
    assert_eq!(messages.len(), 2);

    let contents: Vec<&str> = messages
        .iter()
        .map(|message| message["content"].as_str().expect("content"))
        .collect();
    assert!(contents.iter().any(|text| text.contains("MARK")));
    assert!(contents.iter().any(|text| text.contains("a.txt")));
}
