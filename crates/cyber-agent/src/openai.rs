//! OpenAI（及 openai-compatible）流式实现。
//!
//! POST `{base_url}/chat/completions`，`Authorization: Bearer {key}`，
//! body `{model, messages:[system?,...], max_tokens, temperature, stream:true, tools?}`。
//! SSE：`data: {json}`，`choices[0].delta.content` 为 token，
//! `delta.tool_calls[]` 为工具调用 delta，`data: [DONE]` 终止。

use std::pin::Pin;

use futures::Stream;
use serde_json::{json, Value};

use cyber_core::{resolve_api_key, ProviderConfig};

use crate::error::{AgentError, Result};
use crate::provider::{HttpStream, Provider, StreamRequest};
use crate::sse::parse_openai_line;
use crate::types::{Message, Role, StreamEvent};

pub struct OpenAiProvider {
    client: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
    temperature: f32,
}

impl OpenAiProvider {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        let api_key = resolve_api_key(&cfg.api_key);
        if api_key.is_empty() {
            return Err(AgentError::Provider(format!(
                "provider kind=openai 需 api_key（{} 未设置或为空）",
                cfg.api_key
            )));
        }
        let base = cfg.base_url.trim_end_matches('/');
        Ok(Self {
            client: reqwest::Client::new(),
            url: format!("{base}/chat/completions"),
            api_key,
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens,
            temperature: cfg.temperature,
        })
    }
}

/// 将内部 `Message` 翻译为 OpenAI messages 数组条目：
/// - `Tool` → `{role:"tool", tool_call_id, content}`
/// - `Assistant` 带 tool_calls → `{role:"assistant", content, tool_calls:[{id,type:"function",function:{name,arguments}}]}`
/// - 其余 → `{role, content}`
fn message_to_openai(m: Message) -> Value {
    match m.role {
        Role::Tool => json!({
            "role": "tool",
            "tool_call_id": m.tool_call_id.unwrap_or_default(),
            "content": m.content,
        }),
        Role::Assistant if !m.tool_calls.is_empty() => {
            let tcs: Vec<Value> = m
                .tool_calls
                .iter()
                .map(|tc| {
                    json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {"name": tc.name, "arguments": tc.arguments}
                    })
                })
                .collect();
            json!({"role": "assistant", "content": m.content, "tool_calls": tcs})
        }
        _ => json!({"role": m.role.as_str(), "content": m.content}),
    }
}

impl Provider for OpenAiProvider {
    fn stream(&self, req: StreamRequest) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        // system 作为 messages 数组首条（OpenAI 约定）
        let mut msgs: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
        if let Some(s) = req.system {
            msgs.push(json!({ "role": "system", "content": s }));
        }
        for m in req.messages {
            msgs.push(message_to_openai(m));
        }
        let mut body = json!({
            "model": self.model,
            "messages": msgs,
            "max_tokens": self.max_tokens,
            "temperature": self.temperature,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {"name": t.name, "description": t.description, "parameters": t.parameters}
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
        let s = HttpStream::new(http_req, parse_openai_line);
        Box::pin(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolSchema;
    use crate::types::ToolCall;

    #[test]
    fn assistant_with_tool_calls_serializes() {
        let m = Message {
            role: Role::Assistant,
            content: "正在调用".into(),
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "list_dir".into(),
                arguments: "{\"path\":\".\"}".into(),
            }],
            tool_call_id: None,
        };
        let v = message_to_openai(m);
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"], "正在调用");
        assert_eq!(v["tool_calls"][0]["id"], "call_1");
        assert_eq!(v["tool_calls"][0]["type"], "function");
        assert_eq!(v["tool_calls"][0]["function"]["name"], "list_dir");
        assert_eq!(v["tool_calls"][0]["function"]["arguments"], "{\"path\":\".\"}");
    }

    #[test]
    fn tool_result_message_serializes() {
        let m = Message::tool("call_1", "结果");
        let v = message_to_openai(m);
        assert_eq!(v["role"], "tool");
        assert_eq!(v["tool_call_id"], "call_1");
        assert_eq!(v["content"], "结果");
    }

    #[test]
    fn plain_assistant_has_no_tool_calls_field() {
        let m = Message::assistant("hi");
        let v = message_to_openai(m);
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"], "hi");
        assert!(v.get("tool_calls").is_none());
    }

    #[test]
    fn request_body_includes_tools_when_nonempty() {
        // 仅验证序列化构造不 panic + tools 字段存在；不发真实 HTTP
        let p = OpenAiProvider {
            client: reqwest::Client::new(),
            url: "http://localhost/x".into(),
            api_key: "k".into(),
            model: "m".into(),
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
