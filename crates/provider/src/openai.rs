//! OpenAI-compatible streaming client with bounded retries.

use std::time::Duration;

use baize_core::Usage;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{retry_delay, ProviderError};
use crate::sse::{data_payload, drain_lines};

const MAX_ATTEMPTS: usize = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delta {
    Text(String),
    Usage(Usage),
    ToolCalls(Vec<ToolCall>),
}

pub struct Stream {
    pub rx: mpsc::Receiver<Result<Delta, ProviderError>>,
    pub handle: JoinHandle<()>,
}

impl Stream {
    pub fn abort(&self) {
        self.handle.abort();
    }
}

#[derive(Debug, Clone)]
pub struct WireMessage {
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
}

impl WireMessage {
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(text: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: text.filter(|value| !value.is_empty()),
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

impl Serialize for WireMessage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("WireMessage", 4)?;
        state.serialize_field("role", &self.role)?;
        if let Some(content) = &self.content {
            state.serialize_field("content", content)?;
        }
        if !self.tool_calls.is_empty() {
            let calls: Vec<WireToolCall> = self
                .tool_calls
                .iter()
                .map(|call| WireToolCall {
                    id: call.id.clone(),
                    kind: "function",
                    function: WireFunction {
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    },
                })
                .collect();
            state.serialize_field("tool_calls", &calls)?;
        }
        if let Some(call_id) = &self.tool_call_id {
            state.serialize_field("tool_call_id", call_id)?;
        }
        state.end()
    }
}

#[derive(Serialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction,
}

#[derive(Serialize)]
struct WireFunction {
    name: String,
    arguments: String,
}

pub struct OpenAiProvider {
    client: Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiProvider {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> anyhow::Result<Self> {
        let client = Client::builder().connect_timeout(CONNECT_TIMEOUT).build()?;
        Ok(Self {
            client,
            base_url: base_url.into(),
            api_key,
        })
    }

    pub fn stream(
        &self,
        model: &str,
        system: &str,
        messages: Vec<WireMessage>,
        tools: Vec<Value>,
    ) -> Stream {
        let (tx, rx) = mpsc::channel(128);
        let provider = Self {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
        };
        let model = model.to_string();
        let system = system.to_string();
        let handle = tokio::spawn(async move {
            provider.run(model, system, messages, tools, tx).await;
        });
        Stream { rx, handle }
    }

    async fn run(
        &self,
        model: String,
        system: String,
        messages: Vec<WireMessage>,
        tools: Vec<Value>,
        tx: mpsc::Sender<Result<Delta, ProviderError>>,
    ) {
        let mut attempt = 0;
        let mut delivered = false;
        loop {
            attempt += 1;
            match self
                .attempt(&model, &system, &messages, &tools, &tx, &mut delivered)
                .await
            {
                Ok(()) => return,
                Err(error) => {
                    if delivered || attempt >= MAX_ATTEMPTS || !error.is_retryable() {
                        let _ = tx.send(Err(error)).await;
                        return;
                    }
                    tokio::time::sleep(retry_delay(attempt)).await;
                }
            }
        }
    }

    async fn attempt(
        &self,
        model: &str,
        system: &str,
        messages: &[WireMessage],
        tools: &[Value],
        tx: &mpsc::Sender<Result<Delta, ProviderError>>,
        delivered: &mut bool,
    ) -> Result<(), ProviderError> {
        let mut wire = Vec::with_capacity(messages.len() + 1);
        wire.push(WireMessage::text("system", system));
        wire.extend(messages.iter().cloned());

        let body = RequestBody {
            model: model.to_string(),
            messages: wire,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            tools: if tools.is_empty() {
                None
            } else {
                Some(tools.to_vec())
            },
        };

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut request = self.client.post(url).json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }

        let response = request
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Http {
                status: status.as_u16(),
                body,
            });
        }

        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        let mut calls = CallAccumulator::default();

        loop {
            let next = tokio::time::timeout(READ_TIMEOUT, stream.next()).await;
            let chunk = match next {
                Ok(Some(Ok(bytes))) => bytes,
                Ok(Some(Err(error))) => {
                    return Err(ProviderError::Transport(error.to_string()));
                }
                Ok(None) => break,
                Err(_) => return Err(ProviderError::Transport("读取响应超时".to_string())),
            };
            buffer.extend_from_slice(&chunk);
            for line in drain_lines(&mut buffer) {
                if handle_line(&line, tx, delivered, &mut calls).await? == LineAction::Done {
                    emit_calls(&mut calls, tx).await;
                    return Ok(());
                }
            }
        }

        emit_calls(&mut calls, tx).await;
        Ok(())
    }
}

async fn emit_calls(
    calls: &mut CallAccumulator,
    tx: &mpsc::Sender<Result<Delta, ProviderError>>,
) {
    if calls.is_empty() {
        return;
    }
    let _ = tx.send(Ok(Delta::ToolCalls(calls.take()))).await;
}

#[derive(Default)]
struct CallAccumulator {
    calls: Vec<PartialCall>,
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

impl CallAccumulator {
    fn apply(&mut self, delta: ToolCallDelta) {
        while self.calls.len() <= delta.index {
            self.calls.push(PartialCall::default());
        }
        let slot = &mut self.calls[delta.index];
        if let Some(id) = delta.id {
            slot.id = id;
        }
        if let Some(function) = delta.function {
            if let Some(name) = function.name {
                slot.name = name;
            }
            if let Some(arguments) = function.arguments {
                slot.arguments.push_str(&arguments);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    fn take(&mut self) -> Vec<ToolCall> {
        std::mem::take(&mut self.calls)
            .into_iter()
            .enumerate()
            .filter(|(_, call)| !call.name.is_empty())
            .map(|(index, call)| ToolCall {
                id: if call.id.is_empty() {
                    format!("call_{index}")
                } else {
                    call.id
                },
                name: call.name,
                arguments: call.arguments,
            })
            .collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum LineAction {
    Continue,
    Done,
}

async fn handle_line(
    line: &str,
    tx: &mpsc::Sender<Result<Delta, ProviderError>>,
    delivered: &mut bool,
    calls: &mut CallAccumulator,
) -> Result<LineAction, ProviderError> {
    let Some(data) = data_payload(line) else {
        return Ok(LineAction::Continue);
    };
    if data.is_empty() {
        return Ok(LineAction::Continue);
    }
    if data == "[DONE]" {
        return Ok(LineAction::Done);
    }

    let chunk: Chunk = serde_json::from_str(data)
        .map_err(|error| ProviderError::Parse(format!("{error}: {data}")))?;

    if let Some(usage) = chunk.usage {
        let _ = tx.send(Ok(Delta::Usage(usage.into_usage()))).await;
    }
    for choice in chunk.choices {
        if let Some(text) = choice.delta.content
            && !text.is_empty()
        {
            *delivered = true;
            let _ = tx.send(Ok(Delta::Text(text))).await;
        }
        for call in choice.delta.tool_calls {
            calls.apply(call);
        }
    }
    Ok(LineAction::Continue)
}

#[derive(Serialize)]
struct RequestBody {
    model: String,
    messages: Vec<WireMessage>,
    stream: bool,
    stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: DeltaBody,
}

#[derive(Deserialize, Default)]
struct DeltaBody {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

impl WireUsage {
    fn into_usage(self) -> Usage {
        Usage {
            input: self.prompt_tokens,
            output: self.completion_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parses_text_and_usage_chunks() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut delivered = false;
        let mut calls = CallAccumulator::default();

        let line = r#"data: {"choices":[{"delta":{"content":"你"}}]}"#;
        assert!(handle_line(line, &tx, &mut delivered, &mut calls).await.is_ok());
        assert!(delivered);

        let line = r#"data: {"choices":[],"usage":{"prompt_tokens":4,"completion_tokens":2}}"#;
        assert!(handle_line(line, &tx, &mut delivered, &mut calls).await.is_ok());

        assert_eq!(rx.recv().await.unwrap().unwrap(), Delta::Text("你".into()));
        assert_eq!(
            rx.recv().await.unwrap().unwrap(),
            Delta::Usage(Usage { input: 4, output: 2 })
        );
    }

    #[tokio::test]
    async fn accumulates_tool_call_fragments() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut delivered = false;
        let mut calls = CallAccumulator::default();

        let first = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"path\""}}]}}]}"#;
        let second = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"a.rs\"}"}}]}}]}"#;
        handle_line(first, &tx, &mut delivered, &mut calls)
            .await
            .unwrap();
        handle_line(second, &tx, &mut delivered, &mut calls)
            .await
            .unwrap();

        emit_calls(&mut calls, &tx).await;
        match rx.recv().await.unwrap().unwrap() {
            Delta::ToolCalls(parsed) => {
                assert_eq!(parsed.len(), 1);
                assert_eq!(parsed[0].id, "call_1");
                assert_eq!(parsed[0].name, "read");
                assert_eq!(parsed[0].arguments, r#"{"path":"a.rs"}"#);
            }
            other => panic!("unexpected delta: {other:?}"),
        }
    }

    #[tokio::test]
    async fn assigns_ids_when_upstream_omits_them() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut delivered = false;
        let mut calls = CallAccumulator::default();

        let line = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"read","arguments":"{}"}}]}}]}"#;
        handle_line(line, &tx, &mut delivered, &mut calls)
            .await
            .unwrap();
        emit_calls(&mut calls, &tx).await;

        match rx.recv().await.unwrap().unwrap() {
            Delta::ToolCalls(parsed) => assert_eq!(parsed[0].id, "call_0"),
            other => panic!("unexpected delta: {other:?}"),
        }
    }

    #[tokio::test]
    async fn stops_on_done_marker() {
        let (tx, _rx) = mpsc::channel(8);
        let mut delivered = false;
        let mut calls = CallAccumulator::default();
        let action = handle_line("data: [DONE]", &tx, &mut delivered, &mut calls)
            .await
            .unwrap();
        assert_eq!(action, LineAction::Done);
    }

    #[test]
    fn retry_delays_grow_then_cap() {
        assert_eq!(retry_delay(1).as_secs(), 1);
        assert_eq!(retry_delay(2).as_secs(), 2);
        assert_eq!(retry_delay(3).as_secs(), 4);
        assert_eq!(retry_delay(9).as_secs(), 4);
    }

    #[test]
    fn tool_result_wire_shape() {
        let message = WireMessage::tool_result("call_1", "out");
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["role"], "tool");
        assert_eq!(value["tool_call_id"], "call_1");
        assert_eq!(value["content"], "out");
    }
}
