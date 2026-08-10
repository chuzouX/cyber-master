//! Provider trait + 工厂 + 通用 HTTP 流驱动。
//!
//! Provider 对象安全：`stream()` 返回 `Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>>`，
//! 不用 `async-trait`（避免开销 + 保持对象安全）。流由 `HttpStream` 手写 `Stream` impl 驱动：
//! 状态机 `Init → Sending → Streaming → Done`，三家共用，仅 `parser` 与请求构造不同。

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use tracing::warn;

use cyber_core::ProviderConfig;

use crate::error::Result;
use crate::tool::ToolSchema;
use crate::types::{Message, StreamEvent};

/// 一次流式请求的完整入参（前向兼容 + 自文档）。
///
/// - `messages`：历史 + 本次 user（agent loop 还会追加 assistant(tool_calls) 与 tool 结果）
/// - `system`：系统提示词（Anthropic 顶层字段；OpenAI/Ollama 由 provider 翻成 messages 首条）
/// - `tools`：工具 schema 列表；空 = 不发 `tools` 字段（等同无工具调用能力）
#[derive(Debug, Clone, Default)]
pub struct StreamRequest {
    pub messages: Vec<Message>,
    pub system: Option<String>,
    pub tools: Vec<ToolSchema>,
}

impl StreamRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            system: None,
            tools: Vec::new(),
        }
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn with_tools(mut self, tools: Vec<ToolSchema>) -> Self {
        self.tools = tools;
        self
    }

    pub fn tools_is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// LLM 提供方抽象。`Send + Sync` 以便 `&dyn Provider` 跨 `.await` 点（如压缩），
/// 且 `Box<dyn Provider>` 可跨 tokio task。
pub trait Provider: Send + Sync {
    /// 发起流式对话。返回 `'static` 流（impl 持有 owned reqwest::Client），
    /// 可直接 `tokio::spawn` 驱动。`req.tools` 空 = 不发 tools 字段。
    fn stream(&self, req: StreamRequest) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>>;
}

/// 按 `kind` 分发；`mock=true` 或 `kind=="mock"` 时返回 MockProvider。
pub fn provider_factory(cfg: &ProviderConfig, mock: bool) -> Result<Box<dyn Provider>> {
    if mock || cfg.kind == "mock" {
        return Ok(Box::new(crate::mock::MockProvider::new()));
    }
    match cfg.kind.as_str() {
        "openai" | "openai-compatible" => Ok(Box::new(crate::openai::OpenAiProvider::new(cfg)?)),
        "anthropic" => Ok(Box::new(crate::anthropic::AnthropicProvider::new(cfg)?)),
        "ollama" => Ok(Box::new(crate::ollama::OllamaProvider::new(cfg)?)),
        other => Err(crate::error::AgentError::Provider(format!("未知 provider kind: {other}"))),
    }
}

// ---- 通用 HTTP 流驱动（三家共用）----

type BytesStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;
type RespFuture = Pin<Box<dyn Future<Output = reqwest::Result<reqwest::Response>> + Send>>;

enum HttpState {
    /// 初始：尚未发请求。
    Init { req: reqwest::RequestBuilder },
    /// 请求已发出，等待响应。
    Sending { fut: RespFuture },
    /// 正在读取响应字节流（2xx 成功响应）。
    Streaming {
        body: BytesStream,
        lines: crate::sse::LineBuf,
        pending: VecDeque<StreamEvent>,
    },
    /// 正在读取错误响应体（非 2xx）：累积字节为文本，流结束后报错。
    ErrorBody { body: BytesStream, buf: String },
    /// 终态：排空 pending 后结束。
    Done { pending: VecDeque<StreamEvent> },
}

/// 通用 HTTP 流：`req` 发出 → 按 `parser` 逐行解析 → 产出 `StreamEvent`。
///
/// parser 无状态 `fn(&str) -> Vec<StreamEvent>`：一行可能产出多个事件
/// （如 OpenAI 一行可同时含 `delta.content` 与多个 `delta.tool_calls[]`）。
pub(crate) struct HttpStream {
    state: HttpState,
    parser: fn(&str) -> Vec<StreamEvent>,
}

impl HttpStream {
    pub(crate) fn new(req: reqwest::RequestBuilder, parser: fn(&str) -> Vec<StreamEvent>) -> Self {
        Self {
            state: HttpState::Init { req },
            parser,
        }
    }
}

impl Stream for HttpStream {
    type Item = StreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            // 取出 state by value，避免 `&mut self.state` 与内部 `&mut` 借用冲突
            let state = std::mem::replace(
                &mut this.state,
                HttpState::Done {
                    pending: VecDeque::new(),
                },
            );
            match state {
                HttpState::Init { req } => {
                    this.state = HttpState::Sending {
                        fut: Box::pin(req.send()),
                    };
                    continue;
                }
                HttpState::Sending { mut fut } => match fut.as_mut().poll(cx) {
                    Poll::Ready(Ok(resp)) => {
                        let status = resp.status();
                        if !status.is_success() {
                            // 非 2xx：响应体是 JSON 错误对象而非 SSE 流。
                            // 累积 body 文本后报错，避免解析器静默丢弃。
                            this.state = HttpState::ErrorBody {
                                body: Box::pin(resp.bytes_stream()),
                                buf: format!("HTTP {} {}", status.as_u16(), status.canonical_reason().unwrap_or("")),
                            };
                            continue;
                        }
                        this.state = HttpState::Streaming {
                            body: Box::pin(resp.bytes_stream()),
                            lines: crate::sse::LineBuf::new(),
                            pending: VecDeque::new(),
                        };
                        continue;
                    }
                    Poll::Ready(Err(e)) => {
                        warn!(error = %e, "provider HTTP 请求失败");
                        let mut p = VecDeque::new();
                        p.push_back(StreamEvent::Error(format!("http: {e}")));
                        this.state = HttpState::Done { pending: p };
                        continue;
                    }
                    Poll::Pending => {
                        this.state = HttpState::Sending { fut };
                        return Poll::Pending;
                    }
                },
                HttpState::Streaming {
                    mut body,
                    mut lines,
                    mut pending,
                } => {
                    // 先吐已解析事件
                    if let Some(ev) = pending.pop_front() {
                        this.state = HttpState::Streaming { body, lines, pending };
                        return Poll::Ready(Some(ev));
                    }
                    match body.as_mut().poll_next(cx) {
                        Poll::Ready(Some(Ok(chunk))) => {
                            for line in lines.push_bytes(&chunk) {
                                pending.extend((this.parser)(&line));
                            }
                            this.state = HttpState::Streaming { body, lines, pending };
                            continue; // 可能立刻有 pending，或继续读下一块
                        }
                        Poll::Ready(Some(Err(e))) => {
                            warn!(error = %e, "provider 字节流读取失败");
                            let mut p = VecDeque::new();
                            p.push_back(StreamEvent::Error(format!("stream: {e}")));
                            this.state = HttpState::Done { pending: p };
                            continue;
                        }
                        Poll::Ready(None) => {
                            // 流结束：flush 残留半行，再补一个 Done
                            if let Some(last) = lines.flush_remaining() {
                                pending.extend((this.parser)(&last));
                            }
                            pending.push_back(StreamEvent::Done);
                            this.state = HttpState::Done { pending };
                            continue;
                        }
                        Poll::Pending => {
                            this.state = HttpState::Streaming { body, lines, pending };
                            return Poll::Pending;
                        }
                    }
                }
                HttpState::ErrorBody { mut body, mut buf } => {
                    match body.as_mut().poll_next(cx) {
                        Poll::Ready(Some(Ok(chunk))) => {
                            buf.push_str(&String::from_utf8_lossy(&chunk));
                            this.state = HttpState::ErrorBody { body, buf };
                            continue;
                        }
                        Poll::Ready(Some(Err(e))) => {
                            warn!(error = %e, "读取错误响应体失败");
                            let mut p = VecDeque::new();
                            p.push_back(StreamEvent::Error(format!("{buf} (读取中断: {e})")));
                            this.state = HttpState::Done { pending: p };
                            continue;
                        }
                        Poll::Ready(None) => {
                            // 错误响应体读取完毕：尝试提取 JSON error.message
                            let msg = extract_error_message(&buf).unwrap_or(buf);
                            warn!(error = %msg, "provider 返回非 2xx 错误");
                            let mut p = VecDeque::new();
                            p.push_back(StreamEvent::Error(msg));
                            this.state = HttpState::Done { pending: p };
                            continue;
                        }
                        Poll::Pending => {
                            this.state = HttpState::ErrorBody { body, buf };
                            return Poll::Pending;
                        }
                    }
                }
                HttpState::Done { mut pending } => {
                    if let Some(ev) = pending.pop_front() {
                        this.state = HttpState::Done { pending };
                        return Poll::Ready(Some(ev));
                    }
                    this.state = HttpState::Done { pending };
                    return Poll::Ready(None);
                }
            }
        }
    }
}

/// 从非 2xx 响应体中提取可读错误信息。
///
/// 尝试常见 JSON 错误格式：
/// - OpenAI/DeepSeek：`{"error":{"message":"..."}}`
/// - Anthropic：`{"type":"error","error":{"type":"...","message":"..."}}`
/// - 通用：`{"message":"..."}` / `{"error":"..."}`
/// 解析失败返回 None（调用方使用原始 body 文本）。
fn extract_error_message(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    // OpenAI / DeepSeek: {"error":{"message":"..."}}
    if let Some(msg) = v
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        return Some(msg.to_string());
    }
    // Anthropic: {"type":"error","error":{"message":"..."}}
    // 已被上面覆盖（同样在 error.message 路径）
    // 通用: {"message":"..."}
    if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
        return Some(msg.to_string());
    }
    // 通用: {"error":"..."}
    if let Some(msg) = v.get("error").and_then(|e| e.as_str()) {
        return Some(msg.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyber_core::ProviderConfig;

    #[test]
    fn factory_mock_flag_bypasses_kind() {
        // kind=openai 但 mock=true → MockProvider（不查 api_key，必 Ok）
        let cfg = ProviderConfig::default();
        assert!(provider_factory(&cfg, true).is_ok());
    }

    #[test]
    fn factory_openai_without_key_errors() {
        let cfg = ProviderConfig::default(); // api_key 空
        assert!(provider_factory(&cfg, false).is_err());
    }

    #[test]
    fn factory_anthropic_without_key_errors() {
        let cfg = ProviderConfig {
            kind: "anthropic".into(),
            ..Default::default()
        };
        assert!(provider_factory(&cfg, false).is_err());
    }

    #[test]
    fn factory_ollama_no_key_ok() {
        let cfg = ProviderConfig {
            kind: "ollama".into(),
            base_url: "http://localhost:11434".into(),
            ..Default::default()
        };
        assert!(provider_factory(&cfg, false).is_ok());
    }

    #[test]
    fn factory_unknown_kind_errors() {
        let cfg = ProviderConfig {
            kind: "unknown".into(),
            ..Default::default()
        };
        assert!(provider_factory(&cfg, false).is_err());
    }

    #[test]
    fn extract_error_message_openai_format() {
        let body = r#"{"error":{"message":"Invalid API key","type":"authentication_error","code":"invalid_api_key"}}"#;
        let msg = extract_error_message(body).unwrap();
        assert!(msg.contains("Invalid API key"));
    }

    #[test]
    fn extract_error_message_anthropic_format() {
        let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let msg = extract_error_message(body).unwrap();
        assert!(msg.contains("Overloaded"));
    }

    #[test]
    fn extract_error_message_generic_format() {
        let body = r#"{"message":"something went wrong"}"#;
        assert_eq!(extract_error_message(body).unwrap(), "something went wrong");
    }

    #[test]
    fn extract_error_message_non_json_returns_none() {
        assert!(extract_error_message("not json").is_none());
    }
}
