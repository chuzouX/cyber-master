//! Server-Sent Events 解析（HTTP event-stream 响应 + legacy SSE 传输共用）。
//!
//! SSE 帧格式（[W3C EventSource](https://html.spec.whatwg.org/multipage/server-sent-events.html)）：
//! - 每行形如 `field: value`，字段有 `event` / `data` / `id` / `retry`。
//! - `data:` 可多行，以 `\n` 拼接成事件 data（末尾补一个 `\n`，最后去掉）。
//! - 空行（仅 `\n` 或 `\r\n`）触发派发当前事件，并重置 event 类型为默认 `message`。
//! - `:` 开头的行是注释，忽略。
//!
//! 两种使用方式：
//! - [`SseParser`]：增量状态机，feed 字节块（`bytes_stream()`）逐步产出事件。
//! - [`parse_sse_text`]：对完整 body 一次性解析（HTTP event-stream 响应整读完后用）。

use crate::proto::JsonRpcResponse;

/// 一个已派发的 SSE 事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// `event:` 字段值；缺省为 `message`。
    pub event: String,
    /// 所有 `data:` 行拼接后的内容（行间以 `\n` 连接）。
    pub data: String,
}

/// 增量 SSE 解析器。feed 字节块，按空行边界派发事件。
///
/// 内部缓冲未以 `\n` 结尾的半行，跨 feed 累积。不处理 BOM（调用方应已剥离）。
#[derive(Default)]
pub struct SseParser {
    buf: String,
    /// 当前正在累积的事件的 event 类型（None=未设，派发时回退 "message"）。
    event: Option<String>,
    /// 当前事件的 data 行。
    data_lines: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一块字节，返回本次新派发的事件。
    /// 非 UTF-8 字节用 lossy 降级（SSE 协议要求 UTF-8，正常 server 不会触发）。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        self.drain_completed()
    }

    /// 处理缓冲中所有完整行（以 `\n` 结尾），返回派发的事件。
    /// 末尾未以 `\n` 结尾的半行留在 `buf` 等下次 feed。
    fn drain_completed(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        while let Some(nl) = self.buf.find('\n') {
            // 取出 `[0, nl)` 作为完整行，`[nl+1..]` 留作余下缓冲
            let mut line = self.buf[..nl].to_string();
            self.buf = self.buf[nl + 1..].to_string();
            // 去掉行尾 `\r`（CRLF）
            if line.ends_with('\r') {
                line.pop();
            }
            self.process_line(&line, &mut events);
        }
        events
    }

    fn process_line(&mut self, line: &str, events: &mut Vec<SseEvent>) {
        // 空行 → 派发当前事件（仅当有 data 行时；spec: data 空则不派发）
        if line.is_empty() {
            if !self.data_lines.is_empty() {
                events.push(SseEvent {
                    event: self.event.take().unwrap_or_else(|| "message".into()),
                    data: self.data_lines.join("\n"),
                });
                self.data_lines.clear();
            }
            return;
        }
        if line.starts_with(':') {
            return; // 注释
        }
        let (field, value) = match line.find(':') {
            Some(i) => {
                let f = &line[..i];
                let mut v = &line[i + 1..];
                if v.starts_with(' ') {
                    v = &v[1..]; // spec: 去掉一个前导空格
                }
                (f, v)
            }
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data_lines.push(value.to_string()),
            _ => {} // id / retry / 未知：忽略
        }
    }

    /// 流结束时调用：若缓冲有未派发的半行（无空行结尾），补派发最后一个事件。
    pub fn finish(&mut self) -> Vec<SseEvent> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        let mut line = std::mem::take(&mut self.buf);
        if line.ends_with('\r') {
            line.pop();
        }
        let mut events = Vec::new();
        self.process_line(&line, &mut events);
        self.process_line("", &mut events); // 补空行触发派发
        events
    }
}

/// 对一段完整 SSE 文本一次性解析为事件列表。
pub fn parse_sse_text(text: &str) -> Vec<SseEvent> {
    let mut parser = SseParser::new();
    let mut events = parser.feed(text.as_bytes());
    events.extend(parser.finish());
    events
}

/// 从事件列表中抽取所有 JSON-RPC 响应（解析失败的 data 行忽略）。
/// 同时包含 `event: message` 与默认事件（无 `event:` 字段）。
pub fn extract_jsonrpc_responses(events: &[SseEvent]) -> Vec<JsonRpcResponse> {
    let mut out = Vec::new();
    for ev in events {
        // 只处理 message 类事件（endpoint 等非 JSON 事件跳过）
        if ev.event != "message" {
            continue;
        }
        if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(ev.data.trim()) {
            out.push(resp);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_message_event() {
        let text = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{}}\n\n";
        let events = parse_sse_text(text);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "message");
        assert!(events[0].data.contains("\"id\":0"));
    }

    #[test]
    fn parse_default_event_is_message() {
        // 无 event: 行 → 默认 message
        let text = "data: hello\n\n";
        let events = parse_sse_text(text);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "message");
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn parse_multiline_data_joined_with_newline() {
        let text = "data: line1\ndata: line2\ndata: line3\n\n";
        let events = parse_sse_text(text);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2\nline3");
    }

    #[test]
    fn parse_crlf_line_endings() {
        let text = "event: message\r\ndata: {\"id\":1}\r\n\r\n";
        let events = parse_sse_text(text);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"id\":1}");
    }

    #[test]
    fn parse_endpoint_event() {
        let text = "event: endpoint\ndata: /messages?session=xyz\n\n";
        let events = parse_sse_text(text);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "endpoint");
        assert_eq!(events[0].data, "/messages?session=xyz");
    }

    #[test]
    fn comment_lines_ignored() {
        let text = ": this is a comment\ndata: ok\n\n";
        let events = parse_sse_text(text);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "ok");
    }

    #[test]
    fn value_without_space_after_colon() {
        // spec: 仅去掉一个前导空格；无空格时原样
        let text = "data:nospace\n\n";
        let events = parse_sse_text(text);
        assert_eq!(events[0].data, "nospace");
    }

    #[test]
    fn empty_data_no_event_dispatched() {
        // data 行全空 + 空行 → spec 不派发
        let text = "event: message\n\n";
        let events = parse_sse_text(text);
        assert!(events.is_empty(), "无 data 行不应派发事件");
    }

    #[test]
    fn multiple_events_in_one_stream() {
        let text = "event: endpoint\ndata: /msg\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"x\":1}}\n\n";
        let events = parse_sse_text(text);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "endpoint");
        assert_eq!(events[1].event, "message");
    }

    #[test]
    fn incremental_feed_across_boundaries() {
        let mut parser = SseParser::new();
        // 分三次喂入，跨行边界
        let e1 = parser.feed(b"event: message\nda");
        assert!(e1.is_empty(), "未遇空行不应派发");
        let e2 = parser.feed(b"ta: {\"id\":5}\n\n");
        assert_eq!(e2.len(), 1);
        assert_eq!(e2[0].data, "{\"id\":5}");
    }

    #[test]
    fn extract_jsonrpc_skips_non_message_events() {
        let events = vec![
            SseEvent {
                event: "endpoint".into(),
                data: "/msg".into(),
            },
            SseEvent {
                event: "message".into(),
                data: "{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{}}".into(),
            },
            SseEvent {
                event: "message".into(),
                data: "not json".into(),
            },
        ];
        let resp = extract_jsonrpc_responses(&events);
        assert_eq!(resp.len(), 1, "仅 1 条合法 JSON-RPC message");
        assert_eq!(resp[0].id, Some(0));
    }

    #[test]
    fn finish_dispatches_trailing_event_without_newline() {
        let mut parser = SseParser::new();
        let _ = parser.feed(b"data: tail");
        let events = parser.finish();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "tail");
    }
}
