//! cyber-agent: LLM provider、流式对话、工具调用、agent loop。
//!
//! P2 实现：Provider trait（OpenAI / Anthropic / Ollama 三家）、流式对话、上下文注入。
//! P2.2 实现：Tool trait + 内置工具 + agent loop（max_steps 循环）+ 工具调用协议 +
//! generation 计数器 + Mock 双模（echo / tool-loop）。

pub mod agent;
pub mod anthropic;
pub mod error;
pub mod mock;
pub mod models;
pub mod ollama;
pub mod openai;
pub mod prompt;
pub mod provider;
pub mod sse;
pub mod tool;
pub mod tools;
pub mod types;

pub use agent::run_stream;
pub use error::{AgentError, Result};
pub use models::{extract_model_ids, fetch_models};
pub use provider::{provider_factory, Provider, StreamRequest};
pub use tool::{Tool, ToolCtx, ToolOutput, ToolRegistry, ToolSchema};
pub use tools::builtin_tool_names;
pub use types::{AgentEvent, Message, Role, StreamEvent, ToolCall, ToolCallDelta};
