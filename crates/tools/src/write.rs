//! `write`: full-file replacement, no snapshot required.

use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolOutcome};

pub struct WriteTool;

#[async_trait::async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &'static str {
        "创建或覆盖整个文件，返回写入字节数。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string", "description": "完整文件内容" }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolOutcome {
        let Some(path) = args["path"].as_str() else {
            return ToolOutcome::error("缺少 path 参数");
        };
        let Some(content) = args["content"].as_str() else {
            return ToolOutcome::error("缺少 content 参数");
        };

        let full = ctx.resolve(path);
        if let Some(parent) = full.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            return ToolOutcome::error(format!("无法创建目录 {}：{error}", parent.display()));
        }

        match std::fs::write(&full, content) {
            Ok(()) => ToolOutcome::ok(format!("已写入 {} 字节：{path}", content.len())),
            Err(error) => ToolOutcome::error(format!("无法写入 {path}：{error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Rig;

    #[tokio::test]
    async fn creates_missing_directories() {
        let rig = Rig::new();
        let ctx = rig.context();

        let outcome = WriteTool
            .execute(json!({ "path": "nested/deep/a.txt", "content": "hi\n" }), &ctx)
            .await;

        assert!(!outcome.is_error, "{}", outcome.text);
        assert_eq!(rig.read("nested/deep/a.txt"), "hi\n");
        assert!(outcome.text.contains("3"));
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let rig = Rig::new();
        rig.write("a.txt", "old\n");
        let ctx = rig.context();

        let outcome = WriteTool
            .execute(json!({ "path": "a.txt", "content": "new\n" }), &ctx)
            .await;

        assert!(!outcome.is_error);
        assert_eq!(rig.read("a.txt"), "new\n");
    }

    #[tokio::test]
    async fn rejects_missing_content() {
        let rig = Rig::new();
        let ctx = rig.context();

        let outcome = WriteTool.execute(json!({ "path": "a.txt" }), &ctx).await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("content"));
    }
}
