//! 流式响应行缓冲与解析。
//!
//! 三家协议：
//! - **OpenAI**：SSE，`data: {json}` 行，`choices[0].delta.content` 为 token；`data: [DONE]` 终止。
//! - **Anthropic**：SSE，`event:` + `data:` 行；靠 `data` 内 `type` 字段判断
//!  （`content_block_delta`→`delta.text`，`message_stop`→Done），无需跨行状态。
//! - **Ollama**：NDJSON（非 SSE），每行一个 JSON，`message.content` 为 token，`done==true` 终止。
//!
//! 用 `LineBuf` 在字节层按 `\n` 切行（ASCII 安全，不会切在多字节 UTF-8 中间），
//! 半行留到下次，避免分片边界丢数据。

use crate::types::StreamEvent;

/// 字节级行缓冲：喂任意分片，按 `\n` 切出完整行（去行尾 `\r\n`）。
///
/// 用 `Vec<u8>` 而非 `String` 缓冲：HTTP 分片可能切在多字节 UTF-8 序列中间，
/// 此时半字节无法构成合法 `String`。按字节找 `\n`（0x0A 不会出现在多字节序列内部），
/// 整行用 `from_utf8_lossy` 转 String。
#[derive(Debug, Default)]
pub struct LineBuf {
    buf: Vec<u8>,
}

impl LineBuf {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入字节分片，返回本次产出的完整行（不含行尾 `\n`/`\r`）。半行留 `buf`。
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut lines = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            // 取出 0..=pos（含 `\n`）
            let line_bytes: Vec<u8> = self.buf.drain(..=pos).collect();
            // 去行尾 `\n` 与可能的 `\r`
            let mut end = line_bytes.len();
            if end > 0 && line_bytes[end - 1] == b'\n' {
                end -= 1;
            }
            if end > 0 && line_bytes[end - 1] == b'\r' {
                end -= 1;
            }
            lines.push(String::from_utf8_lossy(&line_bytes[..end]).into_owned());
        }
        lines
    }

    /// 流结束时取出残留半行（无残留返回 `None`）。
    pub fn flush_remaining(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let line = std::mem::take(&mut self.buf);
        let mut s = String::from_utf8_lossy(&line).into_owned();
        if s.ends_with('\r') {
            s.pop();
        }
        Some(s)
    }
}

/// OpenAI SSE 行解析。
pub fn parse_openai_line(line: &str) -> Option<StreamEvent> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return None; // 空行 / 注释心跳
    }
    let data = line.strip_prefix("data:")?.trim_start();
    if data == "[DONE]" {
        return Some(StreamEvent::Done);
    }
    let v: serde_json::Value = serde_json::from_str(data).ok()?;
    // delta.content 缺失（role-only / tool-call delta）→ None 跳过
    let content = v
        .get("choices")?
        .get(0)?
        .get("delta")?
        .get("content")?
        .as_str()?;
    Some(StreamEvent::Delta(content.to_string()))
}

/// Anthropic SSE 行解析（靠 `data` 内 `type` 字段，跳过 `event:` 行）。
pub fn parse_anthropic_line(line: &str) -> Option<StreamEvent> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
        return None;
    }
    let data = line.strip_prefix("data:")?.trim_start();
    let v: serde_json::Value = serde_json::from_str(data).ok()?;
    match v.get("type")?.as_str()? {
        "content_block_delta" => {
            let text = v.get("delta")?.get("text")?.as_str()?;
            Some(StreamEvent::Delta(text.to_string()))
        }
        "message_stop" => Some(StreamEvent::Done),
        _ => None, // message_start / content_block_start / ping 等
    }
}

/// Ollama NDJSON 行解析。
pub fn parse_ollama_line(line: &str) -> Option<StreamEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
        return Some(StreamEvent::Done);
    }
    let content = v.get("message")?.get("content")?.as_str()?;
    if content.is_empty() {
        None
    } else {
        Some(StreamEvent::Delta(content.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linebuf_splits_on_newline() {
        let mut lb = LineBuf::new();
        let lines = lb.push_bytes(b"data: a\ndata: b\n");
        assert_eq!(lines, vec!["data: a", "data: b"]);
        assert!(lb.flush_remaining().is_none());
    }

    #[test]
    fn linebuf_holds_partial_line() {
        let mut lb = LineBuf::new();
        let lines = lb.push_bytes(b"data: hel");
        assert!(lines.is_empty(), "半行不应产出");
        let lines = lb.push_bytes(b"lo\ndata: world\n");
        assert_eq!(lines, vec!["data: hello", "data: world"]);
    }

    #[test]
    fn linebuf_strips_crlf() {
        let mut lb = LineBuf::new();
        let lines = lb.push_bytes(b"a\r\nb\r\n");
        assert_eq!(lines, vec!["a", "b"]);
    }

    #[test]
    fn linebuf_flush_remaining() {
        let mut lb = LineBuf::new();
        lb.push_bytes(b"partial no newline");
        assert_eq!(lb.flush_remaining().as_deref(), Some("partial no newline"));
        assert!(lb.flush_remaining().is_none());
    }

    #[test]
    fn linebuf_handles_multibyte_split() {
        // "你好" 的 UTF-8：E4 BD A0 E5 A5 BD，故意切在中间
        let bytes = "你好".as_bytes();
        let mut lb = LineBuf::new();
        lb.push_bytes(&bytes[..2]); // 半个字
        let lines = lb.push_bytes(&bytes[2..]);
        assert!(lines.is_empty()); // 仍无换行
        assert_eq!(lb.flush_remaining().unwrap(), "你好");
    }

    #[test]
    fn openai_parses_delta_and_done() {
        let delta = r#"data: {"choices":[{"delta":{"content":"Hello"}}]}"#;
        assert!(matches!(parse_openai_line(delta), Some(StreamEvent::Delta(t)) if t == "Hello"));
        assert!(matches!(parse_openai_line("data: [DONE]"), Some(StreamEvent::Done)));
        assert!(parse_openai_line(": keep-alive").is_none());
        assert!(parse_openai_line("").is_none());
        // role-only delta（无 content）→ None
        let role_only = r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#;
        assert!(parse_openai_line(role_only).is_none());
    }

    #[test]
    fn anthropic_parses_delta_and_stop() {
        let d = r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#;
        assert!(matches!(parse_anthropic_line(d), Some(StreamEvent::Delta(t)) if t == "Hi"));
        assert!(matches!(
            parse_anthropic_line(r#"data: {"type":"message_stop"}"#),
            Some(StreamEvent::Done)
        ));
        assert!(parse_anthropic_line("event: content_block_delta").is_none());
        assert!(parse_anthropic_line(r#"data: {"type":"message_start"}"#).is_none());
        assert!(parse_anthropic_line(r#"data: {"type":"ping"}"#).is_none());
    }

    #[test]
    fn ollama_parses_delta_and_done() {
        let d = r#"{"message":{"role":"assistant","content":"Hi"},"done":false}"#;
        assert!(matches!(parse_ollama_line(d), Some(StreamEvent::Delta(t)) if t == "Hi"));
        assert!(matches!(
            parse_ollama_line(r#"{"done":true}"#),
            Some(StreamEvent::Done)
        ));
        // 空 content chunk → None
        assert!(parse_ollama_line(r#"{"message":{"content":""},"done":false}"#).is_none());
        assert!(parse_ollama_line("").is_none());
    }
}
