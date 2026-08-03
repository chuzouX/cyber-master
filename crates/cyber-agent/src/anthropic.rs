//! Anthropic 流式实现。
//!
//! POST `{base_url}/v1/messages`，headers `x-api-key: {key}` + `anthropic-version: 2023-06-01`，
//! body `{model, max_tokens, system, messages:[非 system], temperature, stream:true}`。
//! 注意：Anthropic 的 `system` 是顶层字段，messages 数组不含 system 角色。
//! SSE：靠 `data` 内 `type` 字段判断（`content_block_delta`→`delta.text`，`message_stop`→Done）。

use std::pin::Pin;

use futures::Stream;
use serde_json::json;

use cyber_core::{resolve_api_key, ProviderConfig};

use crate::error::{AgentError, Result};
use crate::provider::{HttpStream, Provider};
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
            max_tokens: cfg.max_tokens,
            temperature: cfg.temperature,
        })
    }
}

impl Provider for AnthropicProvider {
    fn stream(
        &self,
        messages: Vec<Message>,
        system: Option<String>,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        // messages 数组过滤掉 System 角色（Anthropic 不允许 system 在数组中）
        let msgs: Vec<serde_json::Value> = messages
            .into_iter()
            .filter(|m| m.role != Role::System)
            .map(|m| json!({ "role": m.role.as_str(), "content": m.content }))
            .collect();
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
        let req = self
            .client
            .post(&self.url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        let s = HttpStream::new(req, parse_anthropic_line);
        Box::pin(s)
    }
}
