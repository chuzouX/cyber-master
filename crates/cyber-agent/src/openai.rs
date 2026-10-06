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
    thinking: Option<cyber_core::ThinkingConfig>,
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
        Ok(Self {
            client: reqwest::Client::new(),
            url: cfg.chat_endpoint(),
            api_key,
            model: cfg.model.clone(),
            max_tokens: cfg.effective_max_tokens(),
            temperature: cfg.effective_temperature(),
            thinking: cfg.thinking.clone(),
        })
    }

    /// 构造流式请求体（`thinking` / `reasoning_effort` 按配置条件下发；
    /// 两者独立，可只出现一个）。
    fn build_body(&self, msgs: Vec<Value>) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": msgs,
            "max_tokens": self.max_tokens,
            "temperature": self.temperature,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        let t = self.thinking.as_ref();
        match t.and_then(|t| t.r#type.as_deref()) {
            Some("enabled") => body["thinking"] = json!({"type": "enabled"}),
            Some("disabled") => body["thinking"] = json!({"type": "disabled"}),
            _ => {}
        }
        if let Some(effort) = t.and_then(|t| t.effort.as_deref()) {
            body["reasoning_effort"] = json!(effort);
        }
        body
    }
}

/// 将内部 `Message` 翻译为 OpenAI messages 数组条目：
/// - `Tool` → `{role:"tool", tool_call_id, content}`
/// - `Assistant` 带 tool_calls → `{role:"assistant", content, tool_calls:[{id,type:"function",function:{name,arguments}}]}`
/// - `User` 带 images → 多模态 `content` 数组：首个为 text 块，后续为 image_url 块（带 `detail`）
/// - 其余（包含带 images 的非 User 消息） → 严格降级为纯文本，拦截 image_url 块，防止 DeepSeek 400 校验错误
pub fn message_to_openai(m: Message) -> Value {
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
        Role::User if !m.images.is_empty() => {
            let mut parts = Vec::with_capacity(m.images.len() + 1);
            parts.push(json!({
                "type": "text",
                "text": m.content,
            }));
            for img in m.images {
                let detail = img.detail.as_deref().unwrap_or("auto");
                parts.push(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": img.url,
                        "detail": detail,
                    }
                }));
            }
            json!({
                "role": "user",
                "content": parts,
            })
        }
        _ => json!({"role": m.role.as_str(), "content": m.content}),
    }
}

impl Provider for OpenAiProvider {
    fn stream(
        &self,
        req: StreamRequest,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        // system 作为 messages 数组首条（OpenAI 约定）
        let mut msgs: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
        if let Some(s) = req.system {
            msgs.push(json!({ "role": "system", "content": s }));
        }
        for m in req.messages {
            msgs.push(message_to_openai(m));
        }
        let mut body = self.build_body(msgs);
        if !req.tools.is_empty() {
            let mut tools: Vec<Value> = req
                .tools
                .iter()
                .filter_map(|t| {
                    let sanitized = crate::tool::sanitize_tool_name(&t.name);
                    if sanitized.is_empty() {
                        return None;
                    }
                    Some(json!({
                        "type": "function",
                        "function": {"name": sanitized, "description": t.description, "parameters": t.parameters}
                    }))
                })
                .collect();
            if tools.len() > 128 {
                tracing::warn!(
                    total = tools.len(),
                    "工具总数超过 OpenAI 接口上限 128，已截断保留前 128 个"
                );
                tools.truncate(128);
            }
            body["tools"] = json!(tools);
        }
        let http_req = self
            .client
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .json(&body);
        let s = HttpStream::new(http_req, parse_openai_line, &self.url);
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
            ..Default::default()
        };
        let v = message_to_openai(m);
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"], "正在调用");
        assert_eq!(v["tool_calls"][0]["id"], "call_1");
        assert_eq!(v["tool_calls"][0]["type"], "function");
        assert_eq!(v["tool_calls"][0]["function"]["name"], "list_dir");
        assert_eq!(
            v["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\".\"}"
        );
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
        // stream 不被驱动，仅构造（HttpStream::Init 状态，未发请求）
        let _stream = p.stream(req);
    }

    #[test]
    fn openai_body_omits_thinking_when_unset() {
        let p = provider_with(None);
        let body = p.build_body(vec![json!({"role": "user", "content": "1"})]);
        assert!(body.get("thinking").is_none(), "{body}");
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }

    #[test]
    fn openai_body_carries_thinking_and_effort() {
        let p = provider_with(Some(cyber_core::ThinkingConfig {
            r#type: Some("enabled".into()),
            effort: Some("high".into()),
        }));
        let body = p.build_body(vec![json!({"role": "user", "content": "1"})]);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");

        // 只设 effort：thinking 键缺席
        let p = provider_with(Some(cyber_core::ThinkingConfig {
            r#type: None,
            effort: Some("low".into()),
        }));
        let body = p.build_body(vec![]);
        assert!(body.get("thinking").is_none(), "{body}");
        assert_eq!(body["reasoning_effort"], "low");

        // disabled 下发关闭值
        let p = provider_with(Some(cyber_core::ThinkingConfig {
            r#type: Some("disabled".into()),
            effort: None,
        }));
        let body = p.build_body(vec![]);
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    fn provider_with(thinking: Option<cyber_core::ThinkingConfig>) -> OpenAiProvider {
        OpenAiProvider::new(&ProviderConfig {
            kind: "openai".into(),
            base_url: "https://x".into(),
            api_key: "sk-test".into(),
            model: "m".into(),
            thinking,
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn user_with_images_serializes_as_multimodal_array() {
        use crate::types::ImageContent;
        let img1 = ImageContent::new("data:image/png;base64,iVBORw0KGgo...")
            .with_id(1)
            .with_detail("auto");
        let img2 = ImageContent::new("https://example.com/photo.jpg")
            .with_id(2)
            .with_detail("low");
        let m = Message::user_with_images(
            "请分析验证码 [image:1] 与参考图 [image:2]",
            vec![img1, img2],
        );

        let v = message_to_openai(m);
        assert_eq!(v["role"], "user");
        let content = v["content"].as_array().expect("content must be array");
        assert_eq!(content.len(), 3);

        // 首块为文本，保留占位符
        assert_eq!(content[0]["type"], "text");
        assert_eq!(
            content[0]["text"],
            "请分析验证码 [image:1] 与参考图 [image:2]"
        );

        // 第二块与第三块为 image_url
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/png;base64,iVBORw0KGgo..."
        );
        assert_eq!(content[1]["image_url"]["detail"], "auto");

        assert_eq!(content[2]["type"], "image_url");
        assert_eq!(
            content[2]["image_url"]["url"],
            "https://example.com/photo.jpg"
        );
        assert_eq!(content[2]["image_url"]["detail"], "low");
    }

    #[test]
    fn non_user_messages_filter_out_image_url_blocks() {
        use crate::types::ImageContent;
        let img = ImageContent::new("data:image/png;base64,iVBORw0KGgo...");

        // Assistant 消息即使附带 images，也坚决不生成 image_url 块（防 DeepSeek 400 校验错误）
        let m_asst = Message::assistant("这是助手的回复").with_image(img.clone());
        let v_asst = message_to_openai(m_asst);
        assert_eq!(v_asst["role"], "assistant");
        assert_eq!(v_asst["content"], "这是助手的回复");
        assert!(v_asst.get("image_url").is_none());

        // System 消息同理
        let m_sys = Message::system("系统提示词").with_image(img.clone());
        let v_sys = message_to_openai(m_sys);
        assert_eq!(v_sys["role"], "system");
        assert_eq!(v_sys["content"], "系统提示词");

        // Tool 消息同理
        let m_tool = Message::tool("call_id", "工具执行结果").with_image(img);
        let v_tool = message_to_openai(m_tool);
        assert_eq!(v_tool["role"], "tool");
        assert_eq!(v_tool["content"], "工具执行结果");
    }
}
