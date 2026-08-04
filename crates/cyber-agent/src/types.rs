//! 对话消息与流式事件类型。

use serde::{Deserialize, Serialize};

/// 消息角色。`serde(rename_all = "lowercase")` 保证与三家 API 的 `role` 字段一致。
///
/// `Tool` 角色用于工具执行结果回灌（OpenAI 用 `role:"tool"`；Anthropic 翻译为
/// `role:"user"` + `tool_result` content 块，由 provider 层处理）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    User,
    System,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// 一次工具调用（assistant 请求执行的工具）。
///
/// `arguments` 是 JSON 字符串（与 OpenAI/Anthropic 的 `arguments`/`input` 一致），
/// 由 agent loop 在执行前 `serde_json::from_str` 解析为 `Value`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 工具调用流式 delta 片段（流内）。
///
/// 三家协议的 tool-call 参数都是分片到达的（OpenAI 的 `delta.tool_calls[].function.arguments`，
/// Anthropic 的 `input_json_delta.partial_json`）。parser 每行产出本片段，agent loop
/// 按 `index` 累积合并：首个片段带 `id`+`name`，后续片段只带 `arguments_fragment`。
#[derive(Debug, Clone)]
pub struct ToolCallDelta {
    /// 工具调用序号（OpenAI 的 `tool_calls[].index`；Anthropic 的 content block `index`）。
    pub index: u32,
    /// 仅首个片段携带（工具调用 id）。
    pub id: Option<String>,
    /// 仅首个片段携带（工具名）。
    pub name: Option<String>,
    /// 参数 JSON 的部分字符串（可能为空）。
    pub arguments_fragment: String,
}

/// 一条对话消息。
///
/// - assistant 消息可带 `tool_calls`（请求执行工具）
/// - `role == Tool` 的消息带 `tool_call_id`（对应哪次调用的结果）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            ..Default::default()
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            ..Default::default()
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            ..Default::default()
        }
    }

    /// 工具执行结果消息（role=Tool）。
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
            ..Default::default()
        }
    }
}

/// Provider 流式输出的事件（流内）。
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// 增量 token（已是完整 UTF-8 字符，见 `sse::LineBuf` 的字节边界处理）。
    Delta(String),
    /// 工具调用 delta 片段（参数分片到达，由 agent loop 累积）。
    ToolCallDelta(ToolCallDelta),
    /// 流末尾的 token 用量（需请求 `stream_options: include_usage`）。
    Usage(Usage),
    /// 流正常结束。
    Done,
    /// 流内可恢复错误（HTTP/stream/解析失败），不终止 agent 任务，仅展示。
    Error(String),
}

/// 单次 API 调用的 token 用量（从流式响应末尾的 usage 字段提取）。
///
/// DeepSeek/OpenAI 的流式 usage chunk 含 `prompt_cache_hit_tokens` /
/// `prompt_cache_miss_tokens`（prefix cache 命中分解）。Anthropic/Ollama
/// 暂不返回缓存分解，相应字段为 0。
#[derive(Debug, Clone, Default)]
pub struct Usage {
    /// 输入 token 总数（含缓存命中 + 缓存未命中）。
    pub prompt_tokens: u64,
    /// 输出 token 数。
    pub completion_tokens: u64,
    /// 缓存命中的输入 token 数（DeepSeek/OpenAI prefix cache）。
    pub cache_hit_tokens: u64,
    /// 缓存未命中的输入 token 数。
    pub cache_miss_tokens: u64,
}

/// agent 任务 → TUI 的事件。
///
/// `ToolCall`/`ToolResult` 是 agent loop 在执行工具**前后**发出的完整事件
/// （非流式片段）；TUI 据此展示 `[tool] name(args) → result`。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 任务已启动（TUI 据此切到 streaming 态）。
    Started,
    Token(String),
    /// agent 即将执行一次工具调用（已累积完整的 name+arguments）。
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// 工具执行完成（含结果或错误）。
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    /// 单轮 API 调用的 token 用量（TUI 据此显示缓存命中率 + 成本）。
    Usage(Usage),
    Done,
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
        assert_eq!(serde_json::to_string(&Role::Assistant).unwrap(), "\"assistant\"");
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), "\"tool\"");
    }

    #[test]
    fn role_default_is_user() {
        assert_eq!(Role::default(), Role::User);
    }

    #[test]
    fn message_roundtrip() {
        let m = Message::user("hi");
        let s = serde_json::to_string(&m).unwrap();
        // tool_calls/tool_call_id 被 skip_serializing_if 跳过
        assert!(!s.contains("tool_calls"));
        assert!(!s.contains("tool_call_id"));
        let m2: Message = serde_json::from_str(&s).unwrap();
        assert_eq!(m2.role, Role::User);
        assert_eq!(m2.content, "hi");
        assert!(m2.tool_calls.is_empty());
        assert!(m2.tool_call_id.is_none());
    }

    #[test]
    fn message_with_tool_calls_serializes() {
        let m = Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "list_dir".into(),
                arguments: "{\"path\":\".\"}".into(),
            }],
            tool_call_id: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"tool_calls\""));
        assert!(s.contains("list_dir"));
    }

    #[test]
    fn message_tool_constructor() {
        let m = Message::tool("call_1", "结果");
        assert_eq!(m.role, Role::Tool);
        assert_eq!(m.content, "结果");
        assert_eq!(m.tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn stream_event_variants() {
        // 确保新变体可构造
        let _ = StreamEvent::ToolCallDelta(ToolCallDelta {
            index: 0,
            id: Some("x".into()),
            name: Some("y".into()),
            arguments_fragment: "{\"".into(),
        });
        let _ = StreamEvent::Delta("t".into());
        let _ = StreamEvent::Done;
        let _ = StreamEvent::Error("e".into());
    }

    #[test]
    fn agent_event_tool_variants() {
        let _ = AgentEvent::ToolCall {
            id: "1".into(),
            name: "n".into(),
            arguments: "{}".into(),
        };
        let _ = AgentEvent::ToolResult {
            id: "1".into(),
            name: "n".into(),
            output: "ok".into(),
            is_error: false,
        };
    }
}
