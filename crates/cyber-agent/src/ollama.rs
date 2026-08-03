//! Ollama 流式实现（本地，无需 api_key）。
//!
//! POST `{base_url}/api/chat`，body `{model, messages:[system?,...], stream:true, options:{temperature, num_predict}}`。
//! NDJSON（非 SSE）：每行一个 JSON，`message.content` 为 token，`done==true` 终止。

use std::pin::Pin;

use futures::Stream;
use serde_json::json;

use cyber_core::ProviderConfig;

use crate::error::Result;
use crate::provider::{HttpStream, Provider};
use crate::sse::parse_ollama_line;
use crate::types::{Message, StreamEvent};

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
            max_tokens: cfg.max_tokens,
            temperature: cfg.temperature,
        })
    }
}

impl Provider for OllamaProvider {
    fn stream(
        &self,
        messages: Vec<Message>,
        system: Option<String>,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        // Ollama 接受 system 在 messages 数组中
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
            "stream": true,
            "options": {
                "temperature": self.temperature,
                "num_predict": self.max_tokens,
            }
        });
        let req = self.client.post(&self.url).json(&body);
        let s = HttpStream::new(req, parse_ollama_line);
        Box::pin(s)
    }
}
