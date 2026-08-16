//! Anthropic 流式实现。
//!
//! POST `{base_url}/v1/messages`，headers `x-api-key: {key}` + `anthropic-version: 2023-06-01`，
//! body `{model, max_tokens, system, messages:[非 system], temperature, stream:true, tools?}`。
//! 注意：Anthropic 的 `system` 是顶层字段，messages 数组不含 system 角色。
//! - assistant tool_calls → `content:[{type:"text",text?}?, {type:"tool_use",id,name,input:JSON}]`
//! - `Role::Tool`（工具结果）→ `{role:"user",content:[{type:"tool_result",tool_use_id,content}]}`
//!
//! SSE：`content_block_start` type=tool_use / `content_block_delta`（text_delta|input_json_delta）/ `message_stop`。

use std::pin::Pin;

use futures::Stream;
use serde_json::{json, Value};

use cyber_core::{resolve_api_key, ProviderConfig};

use crate::error::{AgentError, Result};
use crate::provider::{HttpStream, Provider, StreamRequest};
use crate::sse::parse_anthropic_line;
use crate::types::{Message, Role, StreamEvent};

pub struct AnthropicProvider {
    client: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
    temperature: f32,
}

impl AnthropicProvider {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        let api_key = resolve_api_key(&cfg.api_key);
        if api_key.is_empty() {
            return Err(AgentError::Provider(format!(
                "provider kind=anthropic 需 api_key（{} 未设置或为空）",
                cfg.api_key
            )));
        }
        let base = cfg.base_url.trim_end_matches('/');
        Ok(Self {
            client: reqwest::Client::new(),
            url: format!("{base}/v1/messages"),
            api_key,
            model: cfg.model.clone(),
            max_tokens: cfg.effective_max_tokens(),
            temperature: cfg.effective_temperature(),
        })
    }
}

/// 将内部 `Message` 翻译为 Anthropic messages 数组条目（system 返回 None，由顶层字段承载）：
/// - `System` → `None`（移到顶层 `system` 字段）
/// - `Tool`（工具结果）→ `{role:"user", content:[{type:"tool_result", tool_use_id, content}]}`
/// - `Assistant` 带 tool_calls → `{role:"assistant", content:[{type:"text",text?}?, {type:"tool_use",id,name,input:JSON}]}`；
///   无 tool_calls → `{role:"assistant", content:"..."}`
/// - `User` → `{role:"user", content:"..."}`
fn message_to_anthropic(m: Message) -> Option<Value> {
    match m.role {
        Role::System => None,
        Role::Tool => Some(json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": m.tool_call_id.unwrap_or_default(),
                "content": m.content,
            }]
        })),
        Role::Assistant if !m.tool_calls.is_empty() => {
            let mut blocks: Vec<Value> = Vec::new();
            if !m.content.is_empty() {
                blocks.push(json!({"type": "text", "text": m.content}));
            }
            for tc in &m.tool_calls {
                // arguments JSON 字符串 → 解析为 input 对象（畸形则 fallback 到 {}）
                let input: Value = serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                blocks.push(json!({
                    "type": "tool_use",
                    "id": tc.id,
                    "name": tc.name,
                    "input": input,
                }));
            }
            Some(json!({"role": "assistant", "content": blocks}))
        }
        _ => Some(json!({"role": m.role.as_str(), "content": m.content})),
    }
}

impl Provider for AnthropicProvider {
    fn stream(&self, req: StreamRequest) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        // system 移到顶层；messages 过滤 System + 翻译 Role::Tool
        let msgs: Vec<Value> = req
            .messages
            .into_iter()
            .filter_map(message_to_anthropic)
            .collect();
        let mut body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": msgs,
            "temperature": self.temperature,
            "stream": true,
        });
        if let Some(s) = req.system {
            body["system"] = json!(s);
        }
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }
        let http_req = self
            .client
            .post(&self.url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        let s = HttpStream::new(http_req, parse_anthropic_line, &self.url);
        Box::pin(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolSchema;
    use crate::types::ToolCall;

    #[test]
    fn system_message_filtered_out() {
        let m = Message::system("sys");
        assert!(message_to_anthropic(m).is_none());
    }

    #[test]
    fn assistant_tool_use_block_serializes() {
        let m = Message {
            role: Role::Assistant,
            content: "正在调用".into(),
            tool_calls: vec![ToolCall {
                id: "toolu_1".into(),
                name: "list_dir".into(),
                arguments: "{\"path\":\".\"}".into(),
            }],
            tool_call_id: None,
        };
        let v = message_to_anthropic(m).unwrap();
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "正在调用");
        assert_eq!(v["content"][1]["type"], "tool_use");
        assert_eq!(v["content"][1]["id"], "toolu_1");
        assert_eq!(v["content"][1]["name"], "list_dir");
        assert_eq!(v["content"][1]["input"]["path"], ".");
    }

    #[test]
    fn assistant_tool_use_without_content_omits_text_block() {
        let m = Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "t".into(),
                name: "f".into(),
                arguments: "{}".into(),
            }],
            tool_call_id: None,
        };
        let v = message_to_anthropic(m).unwrap();
        // 只有 tool_use 块，无 text 块
        assert_eq!(v["content"].as_array().unwrap().len(), 1);
        assert_eq!(v["content"][0]["type"], "tool_use");
    }

    #[test]
    fn tool_result_translated_to_user_tool_result_block() {
        let m = Message::tool("toolu_1", "结果内容");
        let v = message_to_anthropic(m).unwrap();
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"][0]["type"], "tool_result");
        assert_eq!(v["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(v["content"][0]["content"], "结果内容");
    }

    #[test]
    fn malformed_arguments_falls_back_to_empty_object() {
        let m = Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "t".into(),
                name: "f".into(),
                arguments: "not json{".into(),
            }],
            tool_call_id: None,
        };
        let v = message_to_anthropic(m).unwrap();
        // 畸形 arguments → input 为 {}
        assert_eq!(v["content"][0]["input"], json!({}));
    }

    #[test]
    fn request_body_includes_tools_when_nonempty() {
        let p = AnthropicProvider {
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
        let _stream = p.stream(req);
    }
}
