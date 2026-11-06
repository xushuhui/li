mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use support::{sse_body, FakeServer, Reply};

fn setup(port: u16) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join(".config/baize");
    fs::create_dir_all(&config).expect("config dir");
    fs::write(config.join("config.toml"), config_text(port)).expect("config file");
    dir
}

fn config_text(port: u16) -> String {
    format!(
        "[model]\ndefault = \"test-model\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\ncontext_window = 200000\n"
    )
}

fn run(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_baize"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("BAIZE_API_KEY")
        .output()
        .expect("run baize")
}

fn session_file(home: &Path) -> PathBuf {
    let root = home.join(".baize/sessions");
    let mut dirs: Vec<PathBuf> = fs::read_dir(&root)
        .expect("sessions dir")
        .map(|entry| entry.expect("entry").path())
        .collect();
    dirs.sort();

    let dir = dirs.first().expect("one session dir");
    fs::read_dir(dir)
        .expect("session dir")
        .map(|entry| entry.expect("entry").path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .expect("session file")
}

fn session_lines(home: &Path) -> Vec<Value> {
    fs::read_to_string(session_file(home))
        .expect("read session")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid json line"))
        .collect()
}

fn request_body(server: &FakeServer, index: usize) -> Value {
    let requests = server.requests.lock().expect("requests");
    serde_json::from_str(&requests[index]).expect("json body")
}

#[test]
fn single_turn_streams_and_writes_session() {
    let server = support::start(vec![Reply::Sse {
        body: sse_body(&["你", "好"], Some((3, 2))),
        chunk: 5,
    }]);
    let home = setup(server.port);

    let output = run(home.path(), home.path(), &["你好"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "你好");

    let lines = session_lines(home.path());
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[1]["type"], "message");
    assert_eq!(lines[1]["message"]["role"], "user");
    assert_eq!(lines[2]["message"]["role"], "assistant");
    assert_eq!(lines[2]["message"]["content"][0]["text"], "你好");
    assert_eq!(lines[2]["message"]["usage"]["input"], 3);
    assert_eq!(lines[2]["parentId"], lines[1]["id"]);
}

#[test]
fn system_prompt_includes_global_and_project_rules() {
    let server = support::start(vec![Reply::Sse {
        body: sse_body(&["ok"], None),
        chunk: 0,
    }]);
    let home = setup(server.port);
    fs::write(home.path().join(".config/baize/AGENTS.md"), "GLOBAL-RULE").expect("global rules");

    let project = home.path().join("project");
    fs::create_dir_all(&project).expect("project dir");
    fs::write(project.join("AGENTS.md"), "PROJECT-RULE").expect("project rules");

    let output = run(home.path(), &project, &["hi"]);
    assert!(output.status.success());

    let body = request_body(&server, 0);
    let system = body["messages"][0]["content"].as_str().expect("system text");
    assert!(system.contains("GLOBAL-RULE"));
    assert!(system.contains("PROJECT-RULE"));
    assert!(system.find("GLOBAL-RULE") < system.find("PROJECT-RULE"));
}

#[test]
fn finds_project_rules_from_nested_directory() {
    let server = support::start(vec![Reply::Sse {
        body: sse_body(&["ok"], None),
        chunk: 0,
    }]);
    let home = setup(server.port);

    let project = home.path().join("project");
    let nested = project.join("src/deep");
    fs::create_dir_all(&nested).expect("nested dir");
    fs::write(project.join("AGENTS.md"), "NESTED-RULE").expect("rules");

    let output = run(home.path(), &nested, &["hi"]);
    assert!(output.status.success());

    let body = request_body(&server, 0);
    let system = body["messages"][0]["content"].as_str().expect("system text");
    assert!(system.contains("NESTED-RULE"));
}

#[test]
fn missing_base_url_exits_2() {
    let home = tempfile::tempdir().expect("temp dir");
    let config = home.path().join(".config/baize");
    fs::create_dir_all(&config).expect("config dir");
    fs::write(
        config.join("config.toml"),
        "[model]\ndefault = \"m\"\ncontext_window = 200000\n",
    )
    .expect("config");

    let output = run(home.path(), home.path(), &["hi"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("base_url"));
}

#[test]
fn zero_context_window_exits_2() {
    let home = tempfile::tempdir().expect("temp dir");
    let config = home.path().join(".config/baize");
    fs::create_dir_all(&config).expect("config dir");
    fs::write(
        config.join("config.toml"),
        "[model]\ndefault = \"m\"\nbase_url = \"http://127.0.0.1:1/v1\"\ncontext_window = 0\n",
    )
    .expect("config");

    let output = run(home.path(), home.path(), &["hi"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("context_window"));
}

#[test]
fn unauthorized_exits_1_without_assistant_message() {
    let server = support::start(vec![Reply::Status {
        code: 401,
        body: "{\"error\":\"nope\"}".to_string(),
    }]);
    let home = setup(server.port);

    let output = run(home.path(), home.path(), &["hi"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("BAIZE_API_KEY"));

    let lines = session_lines(home.path());
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["message"]["role"], "user");
}

#[test]
fn retries_after_429() {
    let server = support::start(vec![
        Reply::Status {
            code: 429,
            body: "{}".to_string(),
        },
        Reply::Sse {
            body: sse_body(&["ok"], None),
            chunk: 0,
        },
    ]);
    let home = setup(server.port);

    let output = run(home.path(), home.path(), &["hi"]);
    assert!(output.status.success());
    assert_eq!(server.requests.lock().expect("requests").len(), 2);
}

#[test]
fn gives_up_after_three_failures() {
    let server = support::start(vec![Reply::Status {
        code: 429,
        body: "{}".to_string(),
    }]);
    let home = setup(server.port);

    let output = run(home.path(), home.path(), &["hi"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(server.requests.lock().expect("requests").len(), 3);
}

#[test]
fn does_not_retry_400() {
    let server = support::start(vec![Reply::Status {
        code: 400,
        body: "{}".to_string(),
    }]);
    let home = setup(server.port);

    let output = run(home.path(), home.path(), &["hi"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(server.requests.lock().expect("requests").len(), 1);
}

#[test]
fn split_bytes_reassemble_text() {
    let server = support::start(vec![Reply::Sse {
        body: sse_body(&["中文", "测试"], None),
        chunk: 1,
    }]);
    let home = setup(server.port);

    let output = run(home.path(), home.path(), &["hi"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "中文测试");
}

#[test]
fn done_only_yields_empty_assistant() {
    let server = support::start(vec![Reply::Sse {
        body: "data: [DONE]\n\n".to_string(),
        chunk: 0,
    }]);
    let home = setup(server.port);

    let output = run(home.path(), home.path(), &["hi"]);
    assert!(output.status.success());

    let lines = session_lines(home.path());
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[2]["message"]["content"][0]["text"], "");
}

#[test]
fn blank_prompt_sends_nothing() {
    let server = support::start(vec![Reply::Close]);
    let home = setup(server.port);

    let output = run(home.path(), home.path(), &["   "]);
    assert!(output.status.success());
    assert!(server.requests.lock().expect("requests").is_empty());

    let lines = session_lines(home.path());
    assert_eq!(lines.len(), 1);
}

#[test]
fn version_matches_crate() {
    let output = Command::new(env!("CARGO_BIN_EXE_baize"))
        .arg("--version")
        .output()
        .expect("run baize");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn model_override_is_requested_and_recorded() {
    let server = support::start(vec![Reply::Sse {
        body: sse_body(&["ok"], None),
        chunk: 0,
    }]);
    let home = setup(server.port);

    let output = run(home.path(), home.path(), &["--model", "other-model", "hi"]);
    assert!(output.status.success());

    assert_eq!(request_body(&server, 0)["model"], "other-model");
    let lines = session_lines(home.path());
    assert_eq!(lines[1]["type"], "model_change");
    assert_eq!(lines[1]["model"], "other-model");
}

#[test]
fn nested_rules_include_every_layer() {
    let server = support::start(vec![Reply::Sse {
        body: sse_body(&["ok"], None),
        chunk: 0,
    }]);
    let home = setup(server.port);

    let repo = home.path().join("repo");
    let deep = repo.join("pkg/src");
    fs::create_dir_all(&deep).expect("deep dir");
    fs::create_dir_all(repo.join(".git")).expect("git dir");
    fs::write(repo.join("AGENTS.md"), "ROOT-RULE").expect("root");
    fs::write(repo.join("pkg/AGENTS.md"), "PKG-RULE").expect("pkg");
    fs::write(deep.join("AGENTS.md"), "DEEP-RULE").expect("deep");

    let output = run(home.path(), &deep, &["hi"]);
    assert!(output.status.success());

    let body = request_body(&server, 0);
    let system = body["messages"][0]["content"].as_str().expect("system text");
    let root = system.find("ROOT-RULE").expect("root rule");
    let pkg = system.find("PKG-RULE").expect("pkg rule");
    let deep = system.find("DEEP-RULE").expect("deep rule");
    assert!(root < pkg && pkg < deep, "{system}");
}

#[test]
fn rules_stop_at_repository_root() {
    let server = support::start(vec![Reply::Sse {
        body: sse_body(&["ok"], None),
        chunk: 0,
    }]);
    let home = setup(server.port);

    let outside = home.path().join("outside");
    let repo = outside.join("repo/sub");
    fs::create_dir_all(&repo).expect("repo");
    fs::create_dir_all(outside.join(".git")).expect("outer git");
    fs::create_dir_all(outside.join("repo/.git")).expect("inner git");
    fs::write(outside.join("AGENTS.md"), "OUTSIDE-RULE").expect("outside");
    fs::write(outside.join("repo/AGENTS.md"), "INNER-RULE").expect("inner");

    let output = run(home.path(), &repo, &["hi"]);
    assert!(output.status.success());

    let body = request_body(&server, 0);
    let system = body["messages"][0]["content"].as_str().expect("system text");
    assert!(system.contains("INNER-RULE"));
    assert!(!system.contains("OUTSIDE-RULE"), "{system}");
}

#[test]
fn empty_rules_file_is_skipped() {
    let server = support::start(vec![Reply::Sse {
        body: sse_body(&["ok"], None),
        chunk: 0,
    }]);
    let home = setup(server.port);

    let project = home.path().join("project");
    fs::create_dir_all(&project).expect("project");
    fs::write(project.join("AGENTS.md"), "   \n\n").expect("empty rules");

    let output = run(home.path(), &project, &["hi"]);
    assert!(output.status.success());

    let body = request_body(&server, 0);
    let system = body["messages"][0]["content"].as_str().expect("system text");
    assert!(!system.contains("项目规则"), "{system}");
}
