//! 对话消息与流式事件类型。

use serde::{Deserialize, Serialize};

/// 消息角色。`serde(rename_all = "lowercase")` 保证与三家 API 的 `role` 字段一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// 一条对话消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

/// Provider 流式输出的事件（流内）。
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// 增量 token（已是完整 UTF-8 字符，见 `sse::LineBuf` 的字节边界处理）。
    Delta(String),
    /// 流正常结束。
    Done,
    /// 流内可恢复错误（HTTP/stream/解析失败），不终止 agent 任务，仅展示。
    Error(String),
}

/// agent 任务 → TUI 的事件（`StreamEvent` 的超集，多一个 `Started`）。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 任务已启动（TUI 据此切到 streaming 态）。
    Started,
    Token(String),
    Done,
    Error(String),
}

impl From<StreamEvent> for AgentEvent {
    fn from(e: StreamEvent) -> Self {
        match e {
            StreamEvent::Delta(t) => Self::Token(t),
            StreamEvent::Done => Self::Done,
            StreamEvent::Error(m) => Self::Error(m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
        assert_eq!(serde_json::to_string(&Role::Assistant).unwrap(), "\"assistant\"");
    }

    #[test]
    fn message_roundtrip() {
        let m = Message { role: Role::User, content: "hi".into() };
        let s = serde_json::to_string(&m).unwrap();
        let m2: Message = serde_json::from_str(&s).unwrap();
        assert_eq!(m2.role, Role::User);
        assert_eq!(m2.content, "hi");
    }

    #[test]
    fn stream_event_to_agent_event() {
        assert!(matches!(AgentEvent::from(StreamEvent::Delta("x".into())), AgentEvent::Token(_)));
        assert!(matches!(AgentEvent::from(StreamEvent::Done), AgentEvent::Done));
        assert!(matches!(AgentEvent::from(StreamEvent::Error("e".into())), AgentEvent::Error(_)));
    }
}
