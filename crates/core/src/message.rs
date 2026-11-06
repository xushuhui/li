//! Message model shared by the session file and the provider request.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "toolResult")]
    ToolResult,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Content {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "toolCall")]
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub interrupted: bool,
    #[serde(
        rename = "toolCallId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_call_id: Option<String>,
    #[serde(rename = "toolName", default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(rename = "isError", default, skip_serializing_if = "is_false")]
    pub is_error: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self::with_text(Role::User, text)
    }

    pub fn assistant(text: impl Into<String>, usage: Option<Usage>) -> Self {
        let mut message = Self::with_text(Role::Assistant, text);
        message.usage = usage;
        message
    }

    pub fn assistant_turn(text: String, calls: Vec<Content>, usage: Option<Usage>) -> Self {
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(Content::Text { text });
        }
        content.extend(calls);
        Self {
            role: Role::Assistant,
            content,
            usage,
            interrupted: false,
            tool_call_id: None,
            tool_name: None,
            is_error: false,
        }
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        text: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: Role::ToolResult,
            content: vec![Content::Text { text: text.into() }],
            usage: None,
            interrupted: false,
            tool_call_id: Some(call_id.into()),
            tool_name: Some(tool_name.into()),
            is_error,
        }
    }

    fn with_text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![Content::Text { text: text.into() }],
            usage: None,
            interrupted: false,
            tool_call_id: None,
            tool_name: None,
            is_error: false,
        }
    }

    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                Content::Text { text } => Some(text.as_str()),
                Content::ToolCall { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn tool_calls(&self) -> Vec<(&str, &str, &Value)> {
        self.content
            .iter()
            .filter_map(|block| match block {
                Content::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some((id.as_str(), name.as_str(), arguments)),
                Content::Text { .. } => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_message_serializes_with_tag() {
        let value = serde_json::to_value(Message::user("hi")).unwrap();
        assert_eq!(value["role"], "user");
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][0]["text"], "hi");
        assert!(value.get("usage").is_none());
    }

    #[test]
    fn assistant_turn_keeps_text_then_calls() {
        let message = Message::assistant_turn(
            "thinking".into(),
            vec![Content::ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: json!({"path": "a.rs"}),
            }],
            None,
        );
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][1]["type"], "toolCall");
        assert_eq!(value["content"][1]["arguments"]["path"], "a.rs");
    }

    #[test]
    fn tool_result_serializes_role_and_error() {
        let message = Message::tool_result("call_1", "read", "boom", true);
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["role"], "toolResult");
        assert_eq!(value["toolCallId"], "call_1");
        assert_eq!(value["toolName"], "read");
        assert_eq!(value["isError"], true);
    }

    #[test]
    fn text_skips_tool_call_blocks() {
        let message = Message::assistant_turn(
            "answer".into(),
            vec![Content::ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: json!({}),
            }],
            None,
        );
        assert_eq!(message.text(), "answer");
        assert_eq!(message.tool_calls().len(), 1);
    }
}
