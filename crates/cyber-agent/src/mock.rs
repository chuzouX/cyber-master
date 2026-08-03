//! Mock provider：离线回放，无需联网/API key。
//!
//! 逐字符（20ms 间隔）流式回放 `收到：{最后一条 user 消息}`，再发一次 `Done`。
//! 用于 `--mock` / `CYBER_MOCK_PROVIDER=1` 下的端到端冒烟测试。

use std::pin::Pin;
use std::time::Duration;

use futures::stream::{self, Stream};

use crate::provider::Provider;
use crate::types::{Message, StreamEvent};

pub struct MockProvider;

impl MockProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for MockProvider {
    fn stream(
        &self,
        messages: Vec<Message>,
        _system: Option<String>,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let last = messages
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default();
        let text = format!("收到：{last}");
        let chars: Vec<char> = text.chars().collect();

        // state: (chars, idx, done_sent)
        let s = stream::unfold((chars, 0usize, false), |(chars, mut idx, mut done_sent)| async move {
            if !done_sent {
                if idx < chars.len() {
                    let c = chars[idx];
                    idx += 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Some((StreamEvent::Delta(c.to_string()), (chars, idx, false)))
                } else {
                    done_sent = true;
                    Some((StreamEvent::Done, (chars, idx, true)))
                }
            } else {
                None
            }
        });
        Box::pin(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Role;
    use futures::StreamExt;

    #[tokio::test]
    async fn mock_streams_echo_then_done() {
        let p = MockProvider::new();
        let msgs = vec![Message { role: Role::User, content: "你好".into() }];
        let mut s = p.stream(msgs, None);
        let mut out = String::new();
        let mut got_done = false;
        while let Some(ev) = s.next().await {
            match ev {
                StreamEvent::Delta(t) => out.push_str(&t),
                StreamEvent::Done => got_done = true,
                StreamEvent::Error(_) => panic!("mock 不应产生错误"),
            }
        }
        assert_eq!(out, "收到：你好");
        assert!(got_done, "应以 Done 结束");
    }
}
