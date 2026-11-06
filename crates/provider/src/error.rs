use std::time::Duration;

/// A provider failure that the caller may present to the user or retry.
#[derive(Debug, Clone)]
pub enum ProviderError {
    /// The server answered with a non-success status.
    Http { status: u16, body: String },
    /// Connection, timeout, or body read failure.
    Transport(String),
    /// The response was not valid SSE or JSON.
    Parse(String),
}

impl ProviderError {
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Transport(_) => true,
            ProviderError::Http { status, .. } => {
                *status == 408 || *status == 429 || *status >= 500
            }
            ProviderError::Parse(_) => false,
        }
    }

    pub fn user_message(&self) -> String {
        match self {
            ProviderError::Http { status: 401, .. } => {
                "鉴权失败（401）。检查 LI_API_KEY 环境变量是否设置且有效。".to_string()
            }
            ProviderError::Http { status, body } => {
                format!("模型网关返回 {status}：{}", body.trim())
            }
            ProviderError::Transport(message) => format!("网络请求失败：{message}"),
            ProviderError::Parse(message) => format!("无法解析模型响应：{message}"),
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.user_message())
    }
}

impl std::error::Error for ProviderError {}

pub fn retry_delay(attempt: usize) -> Duration {
    Duration::from_secs(1 << (attempt - 1).min(2))
}
