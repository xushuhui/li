//! Token estimation and transcript rendering for context compaction.

use crate::message::{Content, Message, Role};

/// Rough token estimate: one per CJK/fullwidth character, one per four others.
///
/// This follows design.md D8 — no tokenizer dependency. Accuracy is corrected
/// after each request with the provider's reported `usage.input`.
pub fn estimate_text(text: &str) -> usize {
    let mut wide = 0usize;
    let mut narrow = 0usize;
    for ch in text.chars() {
        if is_wide(ch) {
            wide += 1;
        } else {
            narrow += 1;
        }
    }
    wide + narrow.div_ceil(4)
}

fn is_wide(ch: char) -> bool {
    matches!(
        ch as u32,
        0x1100..=0x115F
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x20000..=0x3FFFD
    )
}

/// Estimated tokens for one message, including role markers and tool payloads.
pub fn estimate_message(message: &Message) -> usize {
    let mut tokens = 4;
    for block in &message.content {
        match block {
            Content::Text { text } => tokens += estimate_text(text),
            Content::ToolCall {
                name, arguments, ..
            } => {
                tokens += estimate_text(name) + estimate_text(&arguments.to_string());
            }
        }
    }
    tokens
}

const MESSAGE_CHARS: usize = 2000;
const TOOL_CHARS: usize = 1200;
const CALL_CHARS: usize = 600;

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push_str("…（截断）");
    out
}

fn render_message(message: &Message) -> String {
    let mut out = String::new();
    match message.role {
        Role::User => out.push_str("用户："),
        Role::Assistant => out.push_str("助手："),
        Role::ToolResult => {
            out.push_str("工具 ");
            out.push_str(message.tool_name.as_deref().unwrap_or("?"));
            out.push('：');
        }
    }

    let limit = if message.role == Role::ToolResult {
        TOOL_CHARS
    } else {
        MESSAGE_CHARS
    };
    out.push_str(&truncate(&message.text(), limit));

    for (_, name, arguments) in message.tool_calls() {
        out.push_str("\n[调用 ");
        out.push_str(name);
        out.push_str("] ");
        out.push_str(&truncate(&arguments.to_string(), CALL_CHARS));
    }
    out
}

/// Renders the visible conversation into plain text for the summary request.
///
/// When the text exceeds `char_budget`, the earliest user message is kept and
/// the newest messages fill the rest — the task goal survives, the middle goes.
pub fn render_transcript(
    summary: Option<&str>,
    messages: &[&Message],
    char_budget: usize,
) -> String {
    let mut header = String::new();
    if let Some(summary) = summary {
        header.push_str("## 已有摘要\n");
        header.push_str(summary);
        header.push_str("\n\n");
    }
    let mut used = header.chars().count();

    let rendered: Vec<String> = messages.iter().map(|message| render_message(message)).collect();
    let total: usize = rendered.iter().map(|block| block.chars().count() + 1).sum();

    if used + total <= char_budget {
        let mut out = header;
        for block in rendered {
            out.push_str(&block);
            out.push('\n');
        }
        return out;
    }

    let mut start = rendered.len();
    while start > 0 {
        let size = rendered[start - 1].chars().count() + 1;
        if used + size > char_budget {
            break;
        }
        used += size;
        start -= 1;
    }

    let mut out = header;
    let first_is_user = messages.first().map(|m| m.role == Role::User).unwrap_or(false);
    if start > 0 && first_is_user {
        out.push_str(&rendered[0]);
        out.push_str("\n（……中间省略……）\n");
    } else if start > 0 {
        out.push_str(&format!("（省略前 {start} 条消息）\n"));
    }

    for block in &rendered[start..] {
        out.push_str(block);
        out.push('\n');
    }
    out
}

/// Fixed five-section summary template (design.md R2).
pub const SUMMARY_PROMPT: &str = "\
你正在压缩一段编码对话，供同一个 agent 之后继续工作使用。
只输出下面五节，不要寒暄，不要解释，不要复述对话原文。

## 目标
用户最初要完成什么，一到两句话。

## 约束
用户明确要求或禁止的事；没有就写“无”。

## 已完成
已经完成的工作，包括改动过的文件路径。

## 待办
还没完成的工作，按优先级排列。

## 关键文件
路径加一句话作用。

以下是对话记录：

";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_is_one_token_per_character() {
        assert_eq!(estimate_text("白泽"), 2);
        assert_eq!(estimate_text("白泽助手"), 4);
    }

    #[test]
    fn ascii_is_one_token_per_four_characters() {
        assert_eq!(estimate_text("abcd"), 1);
        assert_eq!(estimate_text("abcdefgh"), 2);
        assert_eq!(estimate_text("a"), 1);
    }

    #[test]
    fn mixed_text_adds_both_halves() {
        // 2 个汉字 + 4 个 ASCII 字符
        assert_eq!(estimate_text("白泽abcd"), 3);
    }

    #[test]
    fn message_estimate_includes_tool_arguments() {
        let message = Message::assistant_turn(
            String::new(),
            vec![Content::ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: serde_json::json!({ "path": "abcdefgh" }),
            }],
            None,
        );
        assert!(estimate_message(&message) > 4);
    }

    #[test]
    fn transcript_keeps_first_user_message_when_over_budget() {
        let first = Message::user("最初的目标：修复登录");
        let second = Message::user("中间的一段很长的细节".repeat(50));
        let third = Message::user("最近的进展");
        let messages = vec![&first, &second, &third];

        let text = render_transcript(None, &messages, 120);

        assert!(text.contains("最初的目标：修复登录"));
        assert!(text.contains("最近的进展"));
        assert!(text.contains("中间省略"));
    }

    #[test]
    fn transcript_is_complete_when_under_budget() {
        let message = Message::user("短对话");
        let text = render_transcript(None, &[&message], 10_000);
        assert!(text.contains("短对话"));
        assert!(!text.contains("省略"));
    }
}
