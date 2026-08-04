//! MCP 层错误。
//!
//! 刻意不扩展 `cyber-core::CoreError`：MCP 子进程 / JSON-RPC 错误属 mcp 层职责，
//! 且 core 不应引入 tokio 子进程语义。既有 core/tui 测试不受影响。

pub type Result<T> = std::result::Result<T, McpError>;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("config: {0}")]
    Config(String),

    #[error("core: {0}")]
    Core(#[from] cyber_core::CoreError),

    #[error("server '{server}' 网络请求失败: {detail}")]
    Network { server: String, detail: String },

    #[error("server '{server}' 响应解析失败: {detail}")]
    BadResponse { server: String, detail: String },

    #[error("server '{server}' 启动失败: {detail}")]
    SpawnFailed { server: String, detail: String },

    #[error("server '{server}' initialize 握手失败: {detail}")]
    InitFailed { server: String, detail: String },

    #[error("server '{server}' 超时（{secs}s 无响应）")]
    Timeout { server: String, secs: u64 },

    #[error("JSON-RPC 错误 {code}: {message}")]
    Rpc { code: i32, message: String },

    #[error("server '{server}' 无工具 '{tool}'")]
    ToolNotFound { server: String, tool: String },

    #[error("MCP 通道已关闭（server actor 已退出）")]
    ChannelClosed,
}
