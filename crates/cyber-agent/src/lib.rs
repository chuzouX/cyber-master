//! cyber-agent: LLM provider、流式对话、工具调用、agent loop。
//!
//! P2 实现：Provider trait（OpenAI / Anthropic / Ollama 三家）、流式对话、上下文注入。
//! P2.2 实现：Tool trait + 内置工具 + agent loop（max_steps 循环）+ 工具调用协议 +
//! generation 计数器 + Mock 双模（echo / tool-loop）。

pub mod agent;
pub mod anthropic;
pub mod background;
pub mod compact;
pub mod error;
pub mod mock;
pub mod models;
pub mod ollama;
pub mod openai;
pub mod permission;
pub mod prompt;
pub mod provider;
pub mod responses;
pub mod sse;
pub mod subagent;
pub mod tool;
pub mod tools;
pub mod types;
pub mod vision;

pub use agent::{
    is_retryable_error, run_compact_stream, run_stream, run_stream_with_permissions,
    run_writeup_stream, steering_channel, SteeringReceiver, SteeringSender,
};
pub use background::{BackgroundJob, BackgroundRegistry, JobKind, JobStatus};
pub use compact::{
    auto_compact_threshold, compact_messages, compact_messages_with_retry, compact_prompt,
    context_remaining_percent, estimate_messages_tokens, estimate_tokens,
    AUTOCOMPACT_BUFFER_TOKENS, COMPACT_MAX_OUTPUT_TOKENS,
};
pub use error::{AgentError, Result};
pub use models::{extract_model_ids, fetch_models};
pub use permission::{
    assess_tool_safety, check_extreme_danger, is_high_risk_tool, ApprovalChoice, PermissionBroker,
    PermissionDecision, PermissionMode, PermissionRequest, SafetyAssessment,
    AUTO_APPROVE_CONFIDENCE_THRESHOLD,
};
pub use provider::{provider_factory, Provider, StreamRequest};
pub use subagent::{SubagentArchive, SubagentRun, SubagentStatus};
pub use tool::{Tool, ToolCatalog, ToolCtx, ToolOutput, ToolRegistry, ToolSchema};
pub use tools::{
    builtin_tool_names, CtfChallengeTool, CustomTool, CustomToolsListTool, DelegateTasksTool,
    InspectImageTool, SaveMemoryTool, SearchToolsTool, TodoTool,
};
pub use types::{
    AgentEvent, ImageContent, Message, Role, StreamEvent, ToolCall, ToolCallDelta, Usage,
};
pub use vision::{
    detect_image_mime, format_injected_vision_description, get_model_vision_capability,
    is_deepseek_provider, is_deepseek_vision_model, is_image_extension, prepare_image_for_deepseek,
    probe_model_vision, resolve_prompt_placeholders, save_model_vision_capability, AttachedImage,
    CapabilityStore, VisionCapability, VisionConfig, VisionEngine, DEFAULT_IMAGE_TOKENS,
    MAX_IMAGE_BYTES, TINY_PROBE_PNG_BASE64, TINY_PROBE_PNG_DATA_URI,
};
