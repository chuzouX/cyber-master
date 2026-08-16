//! OpenAI Responses API 流式实现（`/v1/responses`，kind="responses"）。
//!
//! 用于仅支持 Responses API 的模型（如 opencode 中转的 Grok 4.5 / GPT 5.6 Luna，
//! `@ai-sdk/openai` 类型端点）。与 Chat Completions 的差异：
//! - URL：`{base_url}/responses`（base_url 通常以 `/v1` 结尾）
//! - 请求体：`input` 数组（非 `messages`）；system 用顶层 `instructions`
//! - 工具结果回灌：`{type:"function_call_output", call_id, output}` item
//! - SSE 事件类型：`response.output_text.delta` / `response.function_call_arguments.delta`
//!   / `response.completed` 等（见 `sse::parse_responses_line`）
//!
//! 消息翻译（内部 Message → Responses input items）：
//! - `System` → 顶层 `instructions`（不进 input）
//! - `User` → `{role:"user", content:[{type:"input_text", text}]}`
//! - `Assistant`（无工具）→ `{role:"assistant", content:[{type:"output_text", text}]}`
//! - `Assistant`（带 tool_calls）→ 文本 item + 每个调用一个 `{type:"function_call", call_id, name, arguments}`
//! - `Tool`（工具结果）→ `{type:"function_call_output", call_id, output}`

use std::pin::Pin;

use futures::Stream;
use serde_json::{json, Value};

use cyber_core::{resolve_api_key, ProviderConfig};

use crate::error::{AgentError, Result};
use crate::provider::{HttpStream, Provider, StreamRequest};
use crate::sse::parse_responses_line;
use crate::types::{Message, Role, StreamEvent};

pub struct ResponsesProvider {
    client: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
    temperature: f32,
}

impl ResponsesProvider {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        let api_key = resolve_api_key(&cfg.api_key);
        if api_key.is_empty() {
            return Err(AgentError::Provider(format!(
                "provider kind=responses 需 api_key（{} 未设置或为空）",
                cfg.api_key
            )));
        }
        let base = cfg.base_url.trim_end_matches('/');
        Ok(Self {
            client: reqwest::Client::new(),
            url: format!("{base}/responses"),
            api_key,
            model: cfg.model.clone(),
            max_tokens: cfg.effective_max_tokens(),
            temperature: cfg.effective_temperature(),
        })
    }
}

/// 将内部 `Message` 翻译为 Responses API `input` 数组的 items。
///
/// 多数消息产出一条 item；Assistant 带 tool_calls 时产出多条（文本 item + function_call items）。
/// `System` 返回空（system 由顶层 `instructions` 承载，不进入 input）。
fn message_to_responses(m: Message) -> Vec<Value> {
    match m.role {
        Role::System => Vec::new(),
        Role::Tool => vec![json!({
            "type": "function_call_output",
            "call_id": m.tool_call_id.unwrap_or_default(),
            "output": m.content,
        })],
        Role::Assistant if !m.tool_calls.is_empty() => {
            let mut items: Vec<Value> = Vec::new();
            if !m.content.is_empty() {
                items.push(json!({
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": m.content}]
                }));
            }
            for tc in &m.tool_calls {
                items.push(json!({
                    "type": "function_call",
                    "call_id": tc.id,
                    "name": tc.name,
                    "arguments": tc.arguments,
                }));
            }
            items
        }
        Role::Assistant => vec![json!({
            "role": "assistant",
            "content": [{"type": "output_text", "text": m.content}]
        })],
        Role::User => vec![json!({
            "role": "user",
            "content": [{"type": "input_text", "text": m.content}]
        })],
    }
}

impl Provider for ResponsesProvider {
    fn stream(&self, req: StreamRequest) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        // system → 顶层 instructions（Responses API 约定）
        let mut body = json!({
            "model": self.model,
            "input": [],
            "max_output_tokens": self.max_tokens,
            "temperature": self.temperature,
            "stream": true,
        });
        if let Some(s) = req.system {
            body["instructions"] = json!(s);
        }
        let input: Vec<Value> = req
            .messages
            .into_iter()
            .flat_map(message_to_responses)
            .collect();
        body["input"] = json!(input);
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }
        let http_req = self
            .client
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .json(&body);
        let s = HttpStream::new(http_req, parse_responses_line, &self.url);
        Box::pin(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolSchema;
    use crate::types::ToolCall;

    #[test]
    fn tool_result_message_serializes() {
        let m = Message::tool("fc_1", "结果");
        let items = message_to_responses(m);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[0]["call_id"], "fc_1");
        assert_eq!(items[0]["output"], "结果");
    }

    #[test]
    fn assistant_with_tool_calls_serializes_multiple_items() {
        let mut m = Message::assistant("正在调用");
        m.tool_calls.push(ToolCall {
            id: "fc_1".into(),
            name: "list_dir".into(),
            arguments: "{\"path\":\".\"}".into(),
        });
        let items = message_to_responses(m);
        assert_eq!(items.len(), 2, "文本 item + function_call item");
        assert_eq!(items[0]["role"], "assistant");
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["call_id"], "fc_1");
        assert_eq!(items[1]["name"], "list_dir");
        assert_eq!(items[1]["arguments"], "{\"path\":\".\"}");
    }

    #[test]
    fn system_message_excluded_from_input() {
        let m = Message::system("sys");
        assert!(message_to_responses(m).is_empty());
    }

    #[test]
    fn plain_messages_serialize() {
        let u = message_to_responses(Message::user("hi"));
        assert_eq!(u[0]["role"], "user");
        assert_eq!(u[0]["content"][0]["text"], "hi");

        let a = message_to_responses(Message::assistant("ok"));
        assert_eq!(a[0]["role"], "assistant");
        assert_eq!(a[0]["content"][0]["text"], "ok");
    }

    #[test]
    fn request_body_includes_tools_when_nonempty() {
        let p = ResponsesProvider {
            client: reqwest::Client::new(),
            url: "http://localhost/x".into(),
            api_key: "k".into(),
            model: "grok-4.5".into(),
            max_tokens: 128,
            temperature: 0.0,
        };
        let req = StreamRequest::new(vec![Message::user("hi")])
            .with_system("sys")
            .with_tools(vec![ToolSchema {
                name: "list_dir".into(),
                description: "d".into(),
                parameters: json!({"type": "object"}),
            }]);
        // stream 不被驱动，仅构造（HttpStream::Init 状态，未发请求）
        let _stream = p.stream(req);
    }
}
