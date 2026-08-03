//! OpenAI（及 openai-compatible）流式实现。
//!
//! POST `{base_url}/chat/completions`，`Authorization: Bearer {key}`，
//! body `{model, messages:[system?,...], max_tokens, temperature, stream:true}`。
//! SSE：`data: {json}`，`choices[0].delta.content` 为 token，`data: [DONE]` 终止。

use std::pin::Pin;

use futures::Stream;
use serde_json::json;

use cyber_core::{resolve_api_key, ProviderConfig};

use crate::error::{AgentError, Result};
use crate::provider::{HttpStream, Provider};
use crate::sse::parse_openai_line;
use crate::types::{Message, StreamEvent};

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

impl Provider for OpenAiProvider {
    fn stream(
        &self,
        messages: Vec<Message>,
        system: Option<String>,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        // system 作为 messages 数组首条（OpenAI 约定）
        let mut msgs: Vec<serde_json::Value> = Vec::with_capacity(messages.len() + 1);
        if let Some(s) = system {
            msgs.push(json!({ "role": "system", "content": s }));
        }
        for m in messages {
            msgs.push(json!({ "role": m.role.as_str(), "content": m.content }));
        }
        let body = json!({
            "model": self.model,
            "messages": msgs,
            "max_tokens": self.max_tokens,
            "temperature": self.temperature,
            "stream": true,
        });
        let req = self
            .client
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .json(&body);
        let s = HttpStream::new(req, parse_openai_line);
        Box::pin(s)
    }
}
