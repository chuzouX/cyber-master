//! cyber-mcp: MCP 客户端（stdio / Streamable HTTP / legacy SSE）。
//!
//! P3 实现：MCP server 生命周期管理（actor 模式）、工具发现（`tools/list`）、
//! 工具调用（`tools/call`），通过实现 `cyber_agent::Tool` trait 注入统一工具表。
//!
//! P3.2 扩展：补齐 Streamable HTTP（`spawn_http`，每次 call 一个 POST + `Mcp-Session-Id`
//! 回带）与 legacy SSE（`spawn_sse`，长连 GET event-stream 收响应 + POST endpoint）。
//! `McpConnection` 传输无关（仅持 channel + id + tools），三种传输各起独立 actor。
//!
//! 依赖方向不变量：cyber-agent 不反向依赖本 crate（本 crate 依赖 cyber-agent
//! 实现 `Tool` trait + cyber-core 的 `Paths` / `read_utf8`）。

pub mod config;
pub mod connection;
pub mod error;
pub mod proto;
pub mod registry;
pub mod sse;
pub mod tool;
pub mod transport;

pub use config::{McpServerSpec, McpServersConfig};
pub use connection::McpConnection;
pub use error::{McpError, Result};
pub use registry::McpRegistry;
pub use tool::McpTool;
pub use transport::McpTransport;
