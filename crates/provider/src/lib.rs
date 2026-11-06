//! OpenAI-compatible streaming client.

mod error;
mod openai;
mod sse;

pub use error::ProviderError;
pub use openai::{Delta, OpenAiProvider, Stream, ToolCall, WireMessage};
