//! cyber-agent: LLM provider、流式对话、工具调用、agent loop。
//!
//! P2 实现：Provider trait（OpenAI / Anthropic / Ollama 三家）、流式对话、上下文注入。
//! 工具调用 / agent loop / 斜杠命令留 P2.2。

pub mod agent;
pub mod anthropic;
pub mod error;
pub mod mock;
pub mod ollama;
pub mod openai;
pub mod prompt;
pub mod provider;
pub mod sse;
pub mod types;

pub use agent::run_stream;
pub use error::{AgentError, Result};
pub use provider::{provider_factory, Provider};
pub use types::{AgentEvent, Message, Role, StreamEvent};
