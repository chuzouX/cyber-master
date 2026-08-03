//! Agent 层错误。
//!
//! 刻意不扩展 `cyber-core::CoreError`：HTTP/流式错误属 agent 层职责，且 core 不应
//! 引入 reqwest 依赖（保持 `core → storage` 纯净）。既有 38 个 core/tui 测试不受影响。

use cyber_core::CoreError;

pub type Result<T> = std::result::Result<T, AgentError>;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("stream: {0}")]
    Stream(String),

    #[error("provider: {0}")]
    Provider(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("core: {0}")]
    Core(#[from] CoreError),
}
