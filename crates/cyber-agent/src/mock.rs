//! Mock provider：离线回放，无需联网/API key。
//!
//! 双模（P2.2 Step 6）：
//! - **echo 模式**（`req.tools` 为空）：逐字符（20ms 间隔）流式回放 `收到：{最后一条
//!   user 消息}`，再发 `Done`。用于纯流式冒烟。
//! - **tool-loop 模式**（`req.tools` 非空）：
//!   - 第一步（消息中无 Tool 角色）：发文本 + `ToolCallDelta(list_dir, {"path":"."})` + `Done`，
//!     驱动 agent loop 执行 `list_dir`。
//!   - 第二步（消息中已有 Tool 结果）：发最终文本 + `Done`，结束循环。
//!
//! 用于 `--mock` / `CYBER_MOCK_PROVIDER=1` 下的端到端冒烟测试，覆盖 agent loop 全链路。

use std::pin::Pin;
use std::time::Duration;

use futures::stream::{self, Stream};

use crate::provider::{Provider, StreamRequest};
use crate::types::{Role, StreamEvent, ToolCallDelta};

pub struct MockProvider;

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MockProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for MockProvider {
    fn stream(&self, req: StreamRequest) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let has_tool_result = req.messages.iter().any(|m| m.role == Role::Tool);

        // tool-loop 模式：tools 非空
        if !req.tools.is_empty() {
            if !has_tool_result {
                // 第一步：文本 + 工具调用 + Done（无延迟，测试用）
                return Box::pin(stream::iter(vec![
                    StreamEvent::Delta("让我查看当前目录。".into()),
                    StreamEvent::ToolCallDelta(ToolCallDelta {
                        index: 0,
                        id: Some("mock_call_1".into()),
                        name: Some("list_dir".into()),
                        arguments_fragment: "{\"path\":\".\"}".into(),
                    }),
                    StreamEvent::Done,
                ]));
            }
            // 第二步：工具结果已回灌，发最终文本 + Done
            return Box::pin(stream::iter(vec![
                StreamEvent::Delta("（mock）已获取目录信息，任务完成。".into()),
                StreamEvent::Done,
            ]));
        }

        // echo 模式（tools 为空）：逐字符 20ms 流式回放
        let last = req
            .messages
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default();
        let text = format!("收到：{last}");
        let chars: Vec<char> = text.chars().collect();

        // state: (chars, idx)。idx ∈ [0, len) 发 Delta；idx == len 发 Done 并前进；
        // idx > len 返回 None 终止流。
        let s = stream::unfold((chars, 0usize), |(chars, idx)| async move {
            if idx < chars.len() {
                let c = chars[idx];
                tokio::time::sleep(Duration::from_millis(20)).await;
                Some((StreamEvent::Delta(c.to_string()), (chars, idx + 1)))
            } else if idx == chars.len() {
                Some((StreamEvent::Done, (chars, idx + 1)))
            } else {
                None
            }
        });
        Box::pin(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;
    use crate::ToolSchema;
    use futures::StreamExt;

    /// 构造一个带单个工具 schema 的 StreamRequest（触发 tool-loop 模式）。
    fn req_with_tools(messages: Vec<Message>) -> StreamRequest {
        StreamRequest::new(messages).with_tools(vec![ToolSchema {
            name: "list_dir".into(),
            description: "list directory".into(),
            parameters: serde_json::json!({"type": "object"}),
        }])
    }

    #[tokio::test]
    async fn mock_echo_streams_then_done() {
        let p = MockProvider::new();
        let req = StreamRequest::new(vec![Message::user("你好")]);
        let mut s = p.stream(req);
        let mut out = String::new();
        let mut got_done = false;
        while let Some(ev) = s.next().await {
            match ev {
                StreamEvent::Delta(t) => out.push_str(&t),
                StreamEvent::Done => got_done = true,
                StreamEvent::Error(_) => panic!("mock 不应产生错误"),
                StreamEvent::Usage(_) => {}
                StreamEvent::Reasoning(_) => {}
                StreamEvent::ToolCallDelta(_) => panic!("echo 模式不应产生 tool call"),
            }
        }
        assert_eq!(out, "收到：你好");
        assert!(got_done, "应以 Done 结束");
    }

    #[tokio::test]
    async fn mock_tool_loop_first_step_emits_text_and_tool_call() {
        let p = MockProvider::new();
        let req = req_with_tools(vec![Message::user("查看目录")]);
        let mut s = p.stream(req);
        let mut text = String::new();
        let mut got_tool_call = false;
        let mut got_done = false;
        while let Some(ev) = s.next().await {
            match ev {
                StreamEvent::Delta(t) => text.push_str(&t),
                StreamEvent::ToolCallDelta(d) => {
                    got_tool_call = true;
                    assert_eq!(d.index, 0);
                    assert_eq!(d.id.as_deref(), Some("mock_call_1"));
                    assert_eq!(d.name.as_deref(), Some("list_dir"));
                    assert_eq!(d.arguments_fragment, "{\"path\":\".\"}");
                }
                StreamEvent::Done => got_done = true,
                StreamEvent::Error(_) => panic!("mock 不应产生错误"),
                StreamEvent::Usage(_) => {}
                StreamEvent::Reasoning(_) => {}
            }
        }
        assert!(!text.is_empty(), "第一步应先发文本");
        assert!(got_tool_call, "第一步应发 tool call delta");
        assert!(got_done, "应以 Done 结束");
    }

    #[tokio::test]
    async fn mock_tool_loop_second_step_emits_final_text() {
        let p = MockProvider::new();
        // 第二步：消息含 Tool 角色结果
        let req = req_with_tools(vec![
            Message::user("查看目录"),
            Message::assistant("让我查看当前目录。"),
            Message::tool("mock_call_1", "a.txt\nb.txt"),
        ]);
        let mut s = p.stream(req);
        let mut text = String::new();
        let mut got_done = false;
        let mut got_tool_call = false;
        while let Some(ev) = s.next().await {
            match ev {
                StreamEvent::Delta(t) => text.push_str(&t),
                StreamEvent::ToolCallDelta(_) => got_tool_call = true,
                StreamEvent::Done => got_done = true,
                StreamEvent::Error(_) => panic!("mock 不应产生错误"),
                StreamEvent::Usage(_) => {}
                StreamEvent::Reasoning(_) => {}
            }
        }
        assert!(!text.is_empty(), "第二步应发最终文本");
        assert!(!got_tool_call, "第二步不应再发 tool call");
        assert!(got_done, "应以 Done 结束");
    }
}
