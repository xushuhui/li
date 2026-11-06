//! `read`: numbered lines plus a content tag that `edit` later validates.

use serde_json::{Value, json};

use crate::snapshot::{normalize, tag_of};
use crate::{Tool, ToolContext, ToolOutcome};

pub struct ReadTool;

const MAX_LINES: usize = 2000;

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "读取文本文件。输出以 [路径#标签] 开头，随后每行是 行号:内容。标签供 edit 校验。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "相对工作目录或绝对路径" },
                "start": { "type": "integer", "minimum": 1, "description": "起始行，默认 1" },
                "limit": { "type": "integer", "minimum": 1, "description": "最多读取多少行，默认 2000" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolOutcome {
        let Some(path) = args["path"].as_str() else {
            return ToolOutcome::error("缺少 path 参数");
        };
        let full = ctx.resolve(path);

        let bytes = match std::fs::read(&full) {
            Ok(bytes) => bytes,
            Err(error) => {
                return ToolOutcome::error(format!("无法读取 {path}：{error}"));
            }
        };
        if bytes.contains(&0) {
            return ToolOutcome::error(format!("{path} 是二进制文件，read 只处理文本"));
        }

        let content = String::from_utf8_lossy(&bytes).into_owned();
        let tag = tag_of(&normalize(&content));
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let start = args["start"].as_u64().unwrap_or(1).max(1) as usize;
        if start > total && total > 0 {
            return ToolOutcome::error(format!("起始行 {start} 超过文件总行数 {total}"));
        }
        let limit = args["limit"].as_u64().unwrap_or(MAX_LINES as u64).max(1) as usize;
        let end = (start + limit - 1).min(total);

        let mut output = format!("[{}#{tag}]\n", ctx.display(&full));
        for (offset, line) in lines
            .get(start.saturating_sub(1)..end)
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            output.push_str(&format!("{}:{line}\n", start + offset));
        }
        if end < total {
            output.push_str(&format!("[已显示 {start}-{end} 行，文件共 {total} 行]\n"));
        }

        ctx.snapshots
            .lock()
            .expect("snapshot lock")
            .record(&full, tag, content, vec![(start, end.max(start))]);

        ToolOutcome::ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Rig, tag_from};

    #[tokio::test]
    async fn numbers_lines_and_records_snapshot() {
        let rig = Rig::new();
        rig.write("a.txt", "one\ntwo\n");
        let ctx = rig.context();

        let outcome = ReadTool.execute(json!({ "path": "a.txt" }), &ctx).await;

        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(outcome.text.starts_with("[a.txt#"));
        assert!(outcome.text.contains("1:one"));
        assert!(outcome.text.contains("2:two"));

        let tag = tag_from(&outcome.text);
        assert_eq!(tag.len(), 4);
        let store = ctx.snapshots.lock().unwrap();
        let snapshot = store
            .find(&rig.path().join("a.txt"), &tag)
            .expect("snapshot recorded");
        assert_eq!(snapshot.visible, vec![(1, 2)]);
    }

    #[tokio::test]
    async fn honours_start_and_limit() {
        let rig = Rig::new();
        rig.write("a.txt", "one\ntwo\nthree\nfour\n");
        let ctx = rig.context();

        let outcome = ReadTool
            .execute(json!({ "path": "a.txt", "start": 2, "limit": 2 }), &ctx)
            .await;

        assert!(outcome.text.contains("2:two"));
        assert!(outcome.text.contains("3:three"));
        assert!(!outcome.text.contains("1:one"));
        assert!(!outcome.text.contains("4:four"));
        assert!(outcome.text.contains("文件共 4 行"));
    }

    #[tokio::test]
    async fn truncates_after_two_thousand_lines() {
        let rig = Rig::new();
        let body = (1..=2001)
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        rig.write("big.txt", &body);
        let ctx = rig.context();

        let outcome = ReadTool.execute(json!({ "path": "big.txt" }), &ctx).await;

        assert!(!outcome.is_error);
        assert!(outcome.text.contains("2000:2000"));
        assert!(!outcome.text.contains("2001:2001"));
        assert!(outcome.text.contains("文件共 2001 行"));
    }

    #[tokio::test]
    async fn rejects_binary_and_missing_files() {
        let rig = Rig::new();
        rig.write("bin.dat", "a\0b");
        let ctx = rig.context();

        let binary = ReadTool.execute(json!({ "path": "bin.dat" }), &ctx).await;
        assert!(binary.is_error);
        assert!(binary.text.contains("二进制"));

        let missing = ReadTool.execute(json!({ "path": "nope.txt" }), &ctx).await;
        assert!(missing.is_error);
        assert!(missing.text.contains("无法读取"));
    }

    #[tokio::test]
    async fn rejects_start_beyond_end() {
        let rig = Rig::new();
        rig.write("a.txt", "one\n");
        let ctx = rig.context();

        let outcome = ReadTool
            .execute(json!({ "path": "a.txt", "start": 9 }), &ctx)
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("超过文件总行数"));
    }
}
