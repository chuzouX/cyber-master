//! Anthropic 流式实现。
//!
//! POST `{base_url}/messages`（`base_url` 未含版本段时自动补 `/v1`；已含 `/v1`、`/v1beta` 等
//! 则原样使用，见 `cyber_core::with_api_version`），
//! headers `x-api-key: {key}` + `anthropic-version: 2023-06-01`，
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
    thinking: Option<cyber_core::ThinkingConfig>,
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
        let base = cyber_core::with_api_version(&cfg.base_url);
        Ok(Self {
            client: reqwest::Client::new(),
            url: format!("{base}/messages"),
            api_key,
            model: cfg.model.clone(),
            max_tokens: cfg.effective_max_tokens(),
            temperature: cfg.effective_temperature(),
            thinking: cfg.thinking.clone(),
        })
    }

    /// 构造流式请求体（`system` 为顶层字段；tools 由调用方追加）。
    ///
    /// extended thinking：`thinking.type=enabled` 时按 `max_tokens` 分派
    /// `budget_tokens`（必须 `>= 1024` 且 `< max_tokens`），并把 `temperature` 固定为 1
    /// （Anthropic 在思考模式下拒绝其它温度）。`max_tokens <= 1024` 无法满足约束 → 不下发。
    /// `type=disabled` 与 `effort` 对 Messages API 无对应字段 → 一律不下发。
    fn build_body(&self, msgs: Vec<Value>, system: Option<String>) -> Value {
        let mut body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": msgs,
            "temperature": self.temperature,
            "stream": true,
        });
        if let Some(s) = system {
            body["system"] = json!(s);
        }
        if self
            .thinking
            .as_ref()
            .and_then(|t| t.r#type.as_deref())
            .is_some_and(|t| t == "enabled")
            && self.max_tokens > 1024
        {
            let budget = (self.max_tokens / 2).clamp(1024, 32000);
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
            body["temperature"] = json!(1.0);
        }
        body
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
    fn stream(
        &self,
        req: StreamRequest,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        // system 移到顶层；messages 过滤 System + 翻译 Role::Tool
        let msgs: Vec<Value> = req
            .messages
            .into_iter()
            .filter_map(message_to_anthropic)
            .collect();
        let mut body = self.build_body(msgs, req.system);
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .filter_map(|t| {
                    let sanitized = crate::tool::sanitize_tool_name(&t.name);
                    if sanitized.is_empty() {
                        return None;
                    }
                    Some(json!({
                        "name": sanitized,
                        "description": t.description,
                        "input_schema": t.parameters,
                    }))
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            thinking: None,
        };
        let req = StreamRequest::new(vec![Message::user("hi")])
            .with_system("sys")
            .with_tools(vec![ToolSchema {
                name: "list_dir".into(),
                description: "d".into(),
                tags: vec![],
                parameters: json!({"type": "object"}),
            }]);
        let _stream = p.stream(req);
    }

    #[test]
    fn anthropic_url_appends_version_only_when_missing() {
        let mk = |base: &str| {
            AnthropicProvider::new(&ProviderConfig {
                kind: "anthropic".into(),
                base_url: base.into(),
                api_key: "k".into(),
                model: "m".into(),
                ..Default::default()
            })
            .unwrap()
            .url
        };
        assert_eq!(
            mk("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        // 用户把 base_url 配成 /v1（CLI `/provider add-with-kind anthropic` 的默认值）时不得重复
        assert_eq!(
            mk("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            mk("https://api.anthropic.com/v1/"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            mk("https://gw.test/v1beta"),
            "https://gw.test/v1beta/messages"
        );
    }

    #[test]
    fn anthropic_body_sets_budget_and_temperature_one() {
        let provider = |max_tokens: u32, ty: Option<&str>| {
            AnthropicProvider::new(&ProviderConfig {
                kind: "anthropic".into(),
                base_url: "https://a".into(),
                api_key: "k".into(),
                model: "m".into(),
                max_tokens,
                thinking: Some(cyber_core::ThinkingConfig {
                    r#type: ty.map(|s| s.to_string()),
                    effort: Some("high".into()), // anthropic 忽略 effort
                }),
                ..Default::default()
            })
            .unwrap()
        };

        let body = provider(8192, Some("enabled")).build_body(vec![], None);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 4096);
        assert_eq!(body["temperature"], 1.0);
        assert!(body.get("reasoning_effort").is_none());

        // max_tokens 过小：无法同时满足 budget_tokens>=1024 且 < max_tokens → 不下发
        let small = provider(512, Some("enabled")).build_body(vec![], None);
        assert!(small.get("thinking").is_none(), "{small}");
        assert_ne!(small["temperature"], 1.0);

        // disabled / 未设置 → 都不下发（Messages API 无显式关闭字段）
        assert!(provider(8192, Some("disabled"))
            .build_body(vec![], None)
            .get("thinking")
            .is_none());
        assert!(provider(8192, None)
            .build_body(vec![], None)
            .get("thinking")
            .is_none());
    }
}
