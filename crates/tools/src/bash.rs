//! `bash`: run a shell command with timeout, approval, and output truncation.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolOutcome};

pub struct BashTool;

const DEFAULT_TIMEOUT: u64 = 120;
const MAX_TIMEOUT: u64 = 600;
const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 200 * 1024;
const KEEP: usize = 100;

#[async_trait::async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "在工作目录执行 shell 命令，返回合并后的标准输出与标准错误。只读命令直接执行，其他命令先询问用户。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TIMEOUT,
                    "description": "超时秒数，默认 120"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolOutcome {
        let Some(command) = args["command"].as_str() else {
            return ToolOutcome::error("缺少 command 参数");
        };
        if command.trim().is_empty() {
            return ToolOutcome::error("command 为空");
        }
        let timeout = args["timeout"]
            .as_u64()
            .unwrap_or(DEFAULT_TIMEOUT)
            .clamp(1, MAX_TIMEOUT);

        if !is_read_only(command) && !ctx.approver.approve(command) {
            return ToolOutcome::error("用户拒绝执行该命令");
        }

        let child = match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&ctx.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // SIMPLIFICATION: 只杀直接子进程。`sh -c 'a & b'` 里的孙进程在超时后可能残留；
            // 升级路径是 spawn 时用 pre_exec 建独立进程组，超时后 killpg 整组。
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(error) => return ToolOutcome::error(format!("无法启动命令：{error}")),
        };

        let output = match tokio::time::timeout(
            Duration::from_secs(timeout),
            child.wait_with_output(),
        )
        .await
        {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => return ToolOutcome::error(format!("命令执行失败：{error}")),
            Err(_) => return ToolOutcome::error(format!("命令超时（{timeout} 秒）")),
        };

        let success = output.status.success();
        let code = output.status.code().unwrap_or(-1);

        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&stderr);
        }
        if text.trim().is_empty() {
            text = format!("[退出码 {code}，无输出]");
        } else if !success {
            text.push_str(&format!("\n[退出码 {code}]"));
        }

        ToolOutcome {
            text: truncate(&text),
            is_error: !success,
        }
    }
}

/// Commands that can destroy work or escalate privileges.
///
/// Checked per segment so `cd dir && rm -rf build` is still caught.
pub fn is_dangerous(command: &str) -> bool {
    command
        .split(['&', '|', ';', '\n'])
        .any(segment_is_dangerous)
}

fn segment_is_dangerous(segment: &str) -> bool {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let mut index = 0;
    while index < tokens.len() && is_assignment(tokens[index]) {
        index += 1;
    }
    let Some(word) = tokens.get(index) else {
        return false;
    };
    let name = word.rsplit('/').next().unwrap_or(word);

    if name.starts_with("mkfs") {
        return true;
    }

    match name {
        "rm" | "rmdir" | "unlink" | "shred" | "dd" | "sudo" | "doas" | "killall" | "fdisk"
        | "diskutil" => true,
        "chmod" | "chown" => tokens[index + 1..]
            .iter()
            .any(|token| token.starts_with('-') && token.contains('R')),
        "xargs" | "parallel" => tokens[index + 1..]
            .iter()
            .any(|token| matches!(*token, "rm" | "rmdir" | "shred" | "dd")),
        "find" => tokens[index + 1..].contains(&"-delete"),
        "git" => {
            let mut cursor = index + 1;
            while cursor < tokens.len() && tokens[cursor].starts_with('-') {
                cursor += 1;
            }
            let sub = tokens.get(cursor).copied().unwrap_or("");
            let flags = &tokens[cursor..];
            match sub {
                "reset" => flags.contains(&"--hard"),
                "clean" => flags
                    .iter()
                    .any(|flag| flag.starts_with('-') && flag.contains('f')),
                "push" => flags.iter().any(|flag| {
                    *flag == "--force" || *flag == "-f" || flag.starts_with("--force-with-lease")
                }),
                "checkout" | "restore" => flags.contains(&"--"),
                "branch" => flags.contains(&"-D"),
                _ => false,
            }
        }
        _ => false,
    }
}

fn is_read_only(command: &str) -> bool {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let mut index = 0;
    while index < tokens.len() && is_assignment(tokens[index]) {
        index += 1;
    }
    let Some(word) = tokens.get(index) else {
        return false;
    };
    let name = word.rsplit('/').next().unwrap_or(word);
    match name {
        "ls" | "cat" | "head" | "tail" | "wc" | "pwd" | "which" | "stat" | "file" | "grep" | "rg"
        | "fd" | "find" => true,
        "git" => matches!(
            tokens.get(index + 1),
            Some(&("status" | "diff" | "log" | "show" | "rev-parse"))
        ),
        _ => false,
    }
}

fn is_assignment(token: &str) -> bool {
    match token.split_once('=') {
        Some((key, _)) => {
            !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !key.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

fn truncate(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= MAX_LINES && text.len() <= MAX_BYTES {
        return text.to_string();
    }

    let head = lines.iter().take(KEEP).copied().collect::<Vec<_>>().join("\n");
    let tail = lines
        .iter()
        .rev()
        .take(KEEP)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let omitted = lines.len().saturating_sub(KEEP * 2);
    format!("{head}\n[...省略 {omitted} 行...]\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Rig;
    use crate::Approver;
    use std::sync::Arc;

    struct Fixed(bool);

    impl Approver for Fixed {
        fn approve(&self, _command: &str) -> bool {
            self.0
        }
    }

    #[test]
    fn flags_destructive_commands() {
        for command in [
            "rm -rf build",
            "rm file.txt",
            "sudo ls",
            "git reset --hard HEAD~1",
            "git clean -fd",
            "git push --force origin main",
            "git branch -D old",
            "chmod -R 777 .",
            "find . -name '*.o' -delete",
            "mkfs.ext4 /dev/sdz",
            "cd sub && rm -rf out",
            "echo hi | xargs rm",
        ] {
            assert!(is_dangerous(command), "missed: {command}");
        }
    }

    #[test]
    fn leaves_ordinary_commands_alone() {
        for command in [
            "ls -la",
            "cargo test",
            "git status",
            "git commit -m x",
            "git checkout main",
            "chmod 644 file",
            "find . -name '*.rs'",
            "npm run build && echo done",
        ] {
            assert!(!is_dangerous(command), "false positive: {command}");
        }
    }

    #[tokio::test]
    async fn denied_command_never_runs() {
        let rig = Rig::new();
        let ctx = rig.context_with(Arc::new(Fixed(false)));

        let outcome = BashTool
            .execute(json!({ "command": "touch made.txt" }), &ctx)
            .await;

        assert!(outcome.is_error);
        assert!(outcome.text.contains("拒绝"));
        assert!(!rig.path().join("made.txt").exists());
    }

    #[tokio::test]
    async fn approved_command_runs() {
        let rig = Rig::new();
        let ctx = rig.context_with(Arc::new(Fixed(true)));

        let outcome = BashTool
            .execute(json!({ "command": "echo hi" }), &ctx)
            .await;

        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(outcome.text.contains("hi"));
    }

    #[tokio::test]
    async fn allowlisted_command_skips_approval() {
        let rig = Rig::new();
        rig.write("a.txt", "content\n");
        let ctx = rig.context_with(Arc::new(Fixed(false)));

        let outcome = BashTool
            .execute(json!({ "command": "cat a.txt" }), &ctx)
            .await;

        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(outcome.text.contains("content"));
    }

    #[tokio::test]
    async fn merges_streams_and_reports_failure() {
        let rig = Rig::new();
        let ctx = rig.context_with(Arc::new(Fixed(true)));

        let outcome = BashTool
            .execute(
                json!({ "command": "sh -c 'echo out; echo err >&2; exit 3'" }),
                &ctx,
            )
            .await;

        assert!(outcome.is_error);
        assert!(outcome.text.contains("out"));
        assert!(outcome.text.contains("err"));
        assert!(outcome.text.contains("退出码 3"));
    }

    #[tokio::test]
    async fn timeout_kills_process() {
        let rig = Rig::new();
        let ctx = rig.context_with(Arc::new(Fixed(true)));
        let started = std::time::Instant::now();

        let outcome = BashTool
            .execute(json!({ "command": "sleep 5", "timeout": 1 }), &ctx)
            .await;

        assert!(outcome.is_error);
        assert!(outcome.text.contains("超时"));
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
    }

    #[test]
    fn whitelist_is_narrow() {
        assert!(is_read_only("ls -la"));
        assert!(is_read_only("git status --short"));
        assert!(is_read_only("git diff HEAD"));
        assert!(!is_read_only("git push"));
        assert!(!is_read_only("rm -rf target"));
        assert!(!is_read_only("echo hi | sh"));
    }

    #[test]
    fn whitelist_skips_env_and_path_prefixes() {
        assert!(is_read_only("FOO=1 BAR=2 cat file"));
        assert!(is_read_only("/bin/cat file"));
        assert!(is_read_only("/usr/bin/git status"));
        assert!(!is_read_only("FOO=1 rm file"));
        assert!(!is_read_only("1BAD=1 ls"));
    }

    #[test]
    fn truncates_long_output() {
        let text = (0..3000)
            .map(|index| index.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate(&text);
        assert!(result.contains("省略"));
        assert!(result.starts_with("0\n"));
        assert!(result.ends_with("2999"));
    }

    #[test]
    fn short_output_unchanged() {
        assert_eq!(truncate("a\nb"), "a\nb");
    }
}
