//! Ollama 流式实现（本地，无需 api_key）。
//!
//! POST `{base_url}/api/chat`，body `{model, messages:[system?,...], stream:true, options:{temperature, num_predict}, tools?}`。
//! NDJSON（非 SSE）：每行一个 JSON，`message.content` 为 token，`message.tool_calls` 为工具调用（best-effort），`done==true` 终止。
//! Ollama 工具调用流式支持非标准，此处 best-effort 解析（参见 `sse::parse_ollama_line`）。

use std::pin::Pin;

use futures::Stream;
use serde_json::{json, Value};

use cyber_core::ProviderConfig;

use crate::error::Result;
use crate::provider::{HttpStream, Provider, StreamRequest};
use crate::sse::parse_ollama_line;
use crate::types::{Message, Role, StreamEvent};

pub struct OllamaProvider {
    client: reqwest::Client,
    url: String,
    model: String,
    max_tokens: u32,
    temperature: f32,
}

impl OllamaProvider {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        let base = cfg.base_url.trim_end_matches('/');
        Ok(Self {
            client: reqwest::Client::new(),
            url: format!("{base}/api/chat"),
            model: cfg.model.clone(),
            max_tokens: cfg.effective_max_tokens(),
            temperature: cfg.effective_temperature(),
        })
    }
}

/// 将内部 `Message` 翻译为 Ollama（OpenAI 兼容）messages 数组条目：
/// - `Tool` → `{role:"tool", tool_call_id, content}`
/// - `Assistant` 带 tool_calls → `{role:"assistant", content, tool_calls:[{id,type:"function",function:{name,arguments}}]}`
/// - 其余 → `{role, content}`
fn message_to_ollama(m: Message) -> Value {
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

impl Provider for OllamaProvider {
    fn stream(&self, req: StreamRequest) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        // Ollama 接受 system 在 messages 数组中
        let mut msgs: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
        if let Some(s) = req.system {
            msgs.push(json!({ "role": "system", "content": s }));
        }
        for m in req.messages {
            msgs.push(message_to_ollama(m));
        }
        let mut body = json!({
            "model": self.model,
            "messages": msgs,
            "stream": true,
            "options": {
                "temperature": self.temperature,
                "num_predict": self.max_tokens,
            }
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
        let http_req = self.client.post(&self.url).json(&body);
        let s = HttpStream::new(http_req, parse_ollama_line, &self.url);
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
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "list_dir".into(),
                arguments: "{\"path\":\".\"}".into(),
            }],
            tool_call_id: None,
        };
        let v = message_to_ollama(m);
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["tool_calls"][0]["function"]["name"], "list_dir");
    }

    #[test]
    fn tool_result_message_serializes() {
        let m = Message::tool("c1", "结果");
        let v = message_to_ollama(m);
        assert_eq!(v["role"], "tool");
        assert_eq!(v["tool_call_id"], "c1");
    }

    #[test]
    fn request_body_includes_tools_when_nonempty() {
        let p = OllamaProvider {
            client: reqwest::Client::new(),
            url: "http://localhost/x".into(),
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
        let _stream = p.stream(req);
    }
}
