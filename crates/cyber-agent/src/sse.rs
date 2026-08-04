//! 流式响应行缓冲与解析。
//!
//! 三家协议：
//! - **OpenAI**：SSE，`data: {json}` 行，`choices[0].delta.content` 为 token；`data: [DONE]` 终止。
//! - **Anthropic**：SSE，`event:` + `data:` 行；靠 `data` 内 `type` 字段判断（`content_block_delta`→`delta.text`，`message_stop`→Done），无需跨行状态。
//! - **Ollama**：NDJSON（非 SSE），每行一个 JSON，`message.content` 为 token，`done==true` 终止。
//!
//! 用 `LineBuf` 在字节层按 `\n` 切行（ASCII 安全，不会切在多字节 UTF-8 中间），
//! 半行留到下次，避免分片边界丢数据。

use crate::types::{StreamEvent, ToolCallDelta, Usage};

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

/// OpenAI SSE 行解析。返回 0..N 个事件：
/// - `delta.content` → `Delta`
/// - `delta.reasoning_content` → `Reasoning`（DeepSeek 思考过程）
/// - `delta.tool_calls[]` → 每项一个 `ToolCallDelta`（首片带 id+name，后续只带 arguments 片段）
/// - `data: [DONE]` → `Done`
///
/// 注意：`finish_reason=="tool_calls"` **不**发 Done（避免 double-Done），Done 仅由 `[DONE]` 行触发。
pub fn parse_openai_line(line: &str) -> Vec<StreamEvent> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return Vec::new(); // 空行 / 注释心跳
    }
    let data = match line.strip_prefix("data:") {
        Some(d) => d.trim_start(),
        None => return Vec::new(),
    };
    if data == "[DONE]" {
        return vec![StreamEvent::Done];
    }
    let v: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    // usage chunk（stream_options.include_usage=true 时，流末尾的独立 chunk）：
    // `{"choices":[],"usage":{...}}`。choices 为空数组，delta 不存在。
    if let Some(usage) = v.get("usage") {
        if let Some(u) = parse_usage(usage) {
            return vec![StreamEvent::Usage(u)];
        }
    }
    let delta = match v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("delta"))
    {
        Some(d) => d,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
        out.push(StreamEvent::Delta(content.to_string()));
    }
    // DeepSeek reasoning_content（思考过程增量）
    if let Some(reasoning) = delta.get("reasoning_content").and_then(|r| r.as_str()) {
        if !reasoning.is_empty() {
            out.push(StreamEvent::Reasoning(reasoning.to_string()));
        }
    }
    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let index = tc
                .get("index")
                .and_then(|i| i.as_u64())
                .unwrap_or(0) as u32;
            let function = tc.get("function");
            let id = tc.get("id").and_then(|i| i.as_str()).map(str::to_owned);
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_owned);
            let arguments_fragment = function
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_owned();
            out.push(StreamEvent::ToolCallDelta(ToolCallDelta {
                index,
                id,
                name,
                arguments_fragment,
            }));
        }
    }
    out
}

/// 从 OpenAI/DeepSeek usage JSON 对象提取 token 用量。
///
/// DeepSeek 在标准 `prompt_tokens`/`completion_tokens` 基础上额外返回
/// `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`（prefix cache 分解）。
/// OpenAI 原生 API 无缓存分解字段，相应为 0。
fn parse_usage(usage: &serde_json::Value) -> Option<Usage> {
    let prompt_tokens = usage.get("prompt_tokens").and_then(|v| v.as_u64())?;
    let completion_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_hit_tokens = usage
        .get("prompt_cache_hit_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_miss_tokens = usage
        .get("prompt_cache_miss_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some(Usage {
        prompt_tokens,
        completion_tokens,
        cache_hit_tokens,
        cache_miss_tokens,
    })
}

/// Anthropic SSE 行解析（靠 `data` 内 `type` 字段，跳过 `event:` 行）。返回 0..N 个事件：
/// - `content_block_start` 且 block.type=tool_use → `ToolCallDelta{index,id,name,""}`（text 块 start 为 no-op）
/// - `content_block_delta` text_delta → `Delta`；input_json_delta → `ToolCallDelta{index,None,None,partial_json}`
/// - `message_stop` → `Done`
///
/// 不变量：text 块与 tool_use 块共享同一个 `index` 命名空间，但靠事件类型
/// （text_delta vs input_json_delta）路由隔离，累积器按事件类型分发即可。
pub fn parse_anthropic_line(line: &str) -> Vec<StreamEvent> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
        return Vec::new();
    }
    let data = match line.strip_prefix("data:") {
        Some(d) => d.trim_start(),
        None => return Vec::new(),
    };
    let v: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    match v.get("type").and_then(|t| t.as_str()) {
        Some("content_block_start") => {
            if let Some(block) = v.get("content_block") {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    let id = block.get("id").and_then(|i| i.as_str()).map(str::to_owned);
                    let name = block.get("name").and_then(|n| n.as_str()).map(str::to_owned);
                    out.push(StreamEvent::ToolCallDelta(ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments_fragment: String::new(),
                    }));
                }
                // text 块 start：无事件（文本在后续 text_delta 中到达）
            }
        }
        Some("content_block_delta") => {
            let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
            if let Some(delta) = v.get("delta") {
                match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                            out.push(StreamEvent::Delta(text.to_string()));
                        }
                    }
                    Some("input_json_delta") => {
                        let partial = delta
                            .get("partial_json")
                            .and_then(|p| p.as_str())
                            .unwrap_or("")
                            .to_owned();
                        out.push(StreamEvent::ToolCallDelta(ToolCallDelta {
                            index,
                            id: None,
                            name: None,
                            arguments_fragment: partial,
                        }));
                    }
                    _ => {}
                }
            }
        }
        Some("message_stop") => out.push(StreamEvent::Done),
        _ => {} // message_start / ping / content_block_stop 等
    }
    out
}

/// Ollama NDJSON 行解析。返回 0..N 个事件：
/// - `message.content` 非空 → `Delta`
/// - `message.tool_calls[]` 非空（best-effort，非标准流式）→ 每项一个完整 `ToolCallDelta`
/// - `done==true` → `Done`
///
/// Ollama 工具调用流式支持非标准（多数模型一次性返回完整 tool_calls），
/// 此处 best-effort 解析；顺序：content/tool_calls 先于 Done，避免丢数据。
pub fn parse_ollama_line(line: &str) -> Vec<StreamEvent> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Some(msg) = v.get("message") {
        if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
            if !content.is_empty() {
                out.push(StreamEvent::Delta(content.to_string()));
            }
        }
        if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
            for (i, tc) in tool_calls.iter().enumerate() {
                let index = i as u32;
                let id = tc.get("id").and_then(|i| i.as_str()).map(str::to_owned);
                let function = tc.get("function");
                let name = function
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .map(str::to_owned);
                let arguments_fragment = function
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                    .unwrap_or("")
                    .to_owned();
                out.push(StreamEvent::ToolCallDelta(ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments_fragment,
                }));
            }
        }
    }
    if v.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
        out.push(StreamEvent::Done);
    }
    out
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
        let r = parse_openai_line(delta);
        assert_eq!(r.len(), 1);
        assert!(matches!(&r[0], StreamEvent::Delta(t) if t == "Hello"));

        let r = parse_openai_line("data: [DONE]");
        assert_eq!(r.len(), 1);
        assert!(matches!(r[0], StreamEvent::Done));

        assert!(parse_openai_line(": keep-alive").is_empty());
        assert!(parse_openai_line("").is_empty());
        // role-only delta（无 content，无 tool_calls）→ 空
        let role_only = r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#;
        assert!(parse_openai_line(role_only).is_empty());
    }

    #[test]
    fn openai_parses_tool_call_delta() {
        // 首片：带 index+id+name+空 arguments
        let first = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"list_dir","arguments":""}}]}}]}"#;
        let r = parse_openai_line(first);
        assert_eq!(r.len(), 1);
        match &r[0] {
            StreamEvent::ToolCallDelta(d) => {
                assert_eq!(d.index, 0);
                assert_eq!(d.id.as_deref(), Some("call_1"));
                assert_eq!(d.name.as_deref(), Some("list_dir"));
                assert!(d.arguments_fragment.is_empty());
            }
            other => panic!("应为 ToolCallDelta，实际 {other:?}"),
        }
        // 后续片：仅 arguments 片段
        let frag = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#;
        let r = parse_openai_line(frag);
        assert_eq!(r.len(), 1);
        match &r[0] {
            StreamEvent::ToolCallDelta(d) => {
                assert_eq!(d.index, 0);
                assert!(d.id.is_none());
                assert!(d.name.is_none());
                assert_eq!(d.arguments_fragment, "{\"pa");
            }
            other => panic!("应为 ToolCallDelta，实际 {other:?}"),
        }
    }

    #[test]
    fn openai_parses_content_and_tool_in_one_line() {
        // 一行可同时含 content + tool_calls（理论情况）
        let line = r#"data: {"choices":[{"delta":{"content":"hi","tool_calls":[{"index":0,"id":"c","type":"function","function":{"name":"f","arguments":"{}"}}]}}]}"#;
        let r = parse_openai_line(line);
        assert_eq!(r.len(), 2);
        assert!(matches!(&r[0], StreamEvent::Delta(t) if t == "hi"));
        assert!(matches!(&r[1], StreamEvent::ToolCallDelta(_)));
    }

    #[test]
    fn anthropic_parses_delta_and_stop() {
        let d = r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#;
        let r = parse_anthropic_line(d);
        assert_eq!(r.len(), 1);
        assert!(matches!(&r[0], StreamEvent::Delta(t) if t == "Hi"));

        let r = parse_anthropic_line(r#"data: {"type":"message_stop"}"#);
        assert_eq!(r.len(), 1);
        assert!(matches!(r[0], StreamEvent::Done));

        assert!(parse_anthropic_line("event: content_block_delta").is_empty());
        assert!(parse_anthropic_line(r#"data: {"type":"message_start"}"#).is_empty());
        assert!(parse_anthropic_line(r#"data: {"type":"ping"}"#).is_empty());
    }

    #[test]
    fn anthropic_parses_tool_use_block() {
        // content_block_start type=tool_use → 首片带 id+name+空 arguments
        let start = r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_abc","name":"read_file","input":{}}}"#;
        let r = parse_anthropic_line(start);
        assert_eq!(r.len(), 1);
        match &r[0] {
            StreamEvent::ToolCallDelta(d) => {
                assert_eq!(d.index, 1);
                assert_eq!(d.id.as_deref(), Some("toolu_abc"));
                assert_eq!(d.name.as_deref(), Some("read_file"));
                assert!(d.arguments_fragment.is_empty());
            }
            other => panic!("应为 ToolCallDelta，实际 {other:?}"),
        }
        // input_json_delta → 后续 arguments 片段
        let frag = r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a\"}"}}"#;
        let r = parse_anthropic_line(frag);
        assert_eq!(r.len(), 1);
        match &r[0] {
            StreamEvent::ToolCallDelta(d) => {
                assert_eq!(d.index, 1);
                assert!(d.id.is_none());
                assert!(d.name.is_none());
                assert_eq!(d.arguments_fragment, "{\"path\":\"a\"}");
            }
            other => panic!("应为 ToolCallDelta，实际 {other:?}"),
        }
        // text 块 start 为 no-op
        let text_start = r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        assert!(parse_anthropic_line(text_start).is_empty());
    }

    #[test]
    fn ollama_parses_delta_and_done() {
        let d = r#"{"message":{"role":"assistant","content":"Hi"},"done":false}"#;
        let r = parse_ollama_line(d);
        assert_eq!(r.len(), 1);
        assert!(matches!(&r[0], StreamEvent::Delta(t) if t == "Hi"));

        let r = parse_ollama_line(r#"{"done":true}"#);
        assert_eq!(r.len(), 1);
        assert!(matches!(r[0], StreamEvent::Done));

        // 空 content chunk → 空
        assert!(parse_ollama_line(r#"{"message":{"content":""},"done":false}"#).is_empty());
        assert!(parse_ollama_line("").is_empty());
    }

    #[test]
    fn ollama_parses_tool_calls_best_effort() {
        let d = r#"{"message":{"role":"assistant","content":"","tool_calls":[{"id":"c1","function":{"name":"list_dir","arguments":"{\"path\":\".\"}"}}]},"done":false}"#;
        let r = parse_ollama_line(d);
        assert_eq!(r.len(), 1);
        match &r[0] {
            StreamEvent::ToolCallDelta(d) => {
                assert_eq!(d.index, 0);
                assert_eq!(d.id.as_deref(), Some("c1"));
                assert_eq!(d.name.as_deref(), Some("list_dir"));
                assert_eq!(d.arguments_fragment, "{\"path\":\".\"}");
            }
            other => panic!("应为 ToolCallDelta，实际 {other:?}"),
        }
    }

    #[test]
    fn parse_openai_usage_chunk() {
        let d = r#"data: {"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":500,"total_tokens":1500,"prompt_cache_hit_tokens":900,"prompt_cache_miss_tokens":100}}"#;
        let r = parse_openai_line(d);
        assert_eq!(r.len(), 1);
        match &r[0] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.prompt_tokens, 1000);
                assert_eq!(u.completion_tokens, 500);
                assert_eq!(u.cache_hit_tokens, 900);
                assert_eq!(u.cache_miss_tokens, 100);
            }
            other => panic!("应为 Usage，实际 {other:?}"),
        }
    }

    #[test]
    fn parse_openai_usage_without_cache_fields() {
        // OpenAI 原生 API 无 cache 字段 → cache_hit/miss 为 0
        let d = r#"data: {"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":50}}"#;
        let r = parse_openai_line(d);
        assert_eq!(r.len(), 1);
        match &r[0] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.prompt_tokens, 100);
                assert_eq!(u.completion_tokens, 50);
                assert_eq!(u.cache_hit_tokens, 0);
                assert_eq!(u.cache_miss_tokens, 0);
            }
            other => panic!("应为 Usage，实际 {other:?}"),
        }
    }
}
