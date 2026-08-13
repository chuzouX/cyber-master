//! 上下文压缩：基于 token 估算 + 模型生成摘要，将旧消息压缩为一条摘要消息。
//!
//! 参考 Claude Code 的实现：
//! - token 估算用 ~4 字符/token（与 `roughTokenCountEstimation` 一致）
//! - 自动压缩阈值 = 有效上下文长度 - 缓冲（预留摘要输出空间 + 安全余量）
//! - 压缩时把所有历史消息 + 一条「请生成摘要」的 user 消息发给 provider，
//!   生成摘要后用一条 user 消息替换全部历史
//! - 压缩不暴露工具 → 模型只能生成文本摘要

use futures::StreamExt;

use crate::error::{AgentError, Result};
use crate::provider::{Provider, StreamRequest};
use crate::types::{Message, StreamEvent};

/// 默认 token 估算比率（~4 字符/token，与 Claude Code 一致）。仅用于非 CJK 字符。
const CHARS_PER_TOKEN: usize = 4;

/// 判断是否为 CJK（中日韩）字符。CJK 字符在 tokenizer 中通常 1 字 ≈ 1 token，
/// 与 ASCII（~4 字符/token）差异巨大，需单独估算，否则中文 token 数被低估约 4 倍，
/// 导致自动压缩阈值形同虚设（实际早已超限但估算值迟迟不到阈值）。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F | // CJK 标点
        0x3040..=0x30FF | // 日文假名
        0x3400..=0x4DBF | // 扩展 A
        0x4E00..=0x9FFF | // 统一表意文字（常用汉字）
        0xF900..=0xFAFF | // 兼容表意文字
        0xFF00..=0xFFEF   // 全角形式
    )
}

/// 自动压缩触发阈值 = 有效上下文长度 - 缓冲（预留摘要输出空间 + 安全余量）。
/// 参考 Claude Code 的 `AUTOCOMPACT_BUFFER_TOKENS = 13_000`。
pub const AUTOCOMPACT_BUFFER_TOKENS: u32 = 13_000;

/// 压缩摘要输出的 token 上限（参考 Claude Code 的 `MAX_OUTPUT_TOKENS_FOR_SUMMARY`）。
pub const COMPACT_MAX_OUTPUT_TOKENS: u32 = 20_000;

/// 估算文本 token 数。CJK 字符按 1 字 ≈ 1 token，非 CJK 按 ~4 字符/token。
pub fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for c in text.chars() {
        if is_cjk(c) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other / CHARS_PER_TOKEN
}

/// 估算消息列表 token 数（含 role/tool_calls 等开销的粗略估计）。
pub fn estimate_messages_tokens(messages: &[Message]) -> usize {
    let mut total = 0;
    for m in messages {
        total += estimate_tokens(&m.content);
        // tool_calls 开销：name + arguments
        for tc in &m.tool_calls {
            total += estimate_tokens(&tc.name) + estimate_tokens(&tc.arguments);
        }
        // tool_call_id 开销
        if let Some(id) = &m.tool_call_id {
            total += estimate_tokens(id);
        }
    }
    total
}

/// 计算自动压缩阈值：有效上下文长度 - 缓冲。
/// 返回 None 表示有效上下文长度未知（无法触发自动压缩）。
pub fn auto_compact_threshold(effective_context_length: Option<u32>) -> Option<u32> {
    effective_context_length.map(|n| n.saturating_sub(AUTOCOMPACT_BUFFER_TOKENS))
}

/// 计算上下文剩余百分比（0-100）。
/// `used_tokens` 当前已用 token；`effective_context_length` 有效上下文长度。
/// 返回 None 表示有效上下文长度未知。
pub fn context_remaining_percent(used_tokens: usize, effective_context_length: Option<u32>) -> Option<u32> {
    let total = effective_context_length? as usize;
    if total == 0 {
        return None;
    }
    let used = used_tokens.min(total);
    let remaining = total.saturating_sub(used);
    Some((remaining * 100 / total) as u32)
}

/// 构造压缩提示词（含可选自定义指令）。
///
/// 参考 Claude Code 的 `getCompactPrompt`，要求模型生成一份结构化摘要，覆盖：
/// 主要请求、关键技术概念、文件与代码段、错误与修复、问题解决、所有用户消息、
/// 待办任务、当前工作、可选的下一步。
pub fn compact_prompt(custom_instructions: Option<&str>) -> String {
    let mut p = String::from(
        "你的任务是为之前的对话创建一份详尽的摘要，以便在上下文空间有限时继续对话。\
这份摘要应全面捕获技术细节、代码模式、架构决策以及用户提出的所有显式请求，\
确保后续工作能在不丢失上下文的前提下继续。\n\n\
摘要应包含以下章节：\n\
1. 主要请求与意图：详细描述用户的所有显式请求和意图\n\
2. 关键技术概念：列出所有重要的技术概念、技术和框架\n\
3. 文件与代码段：枚举已检查、修改或创建的具体文件和代码段，包含完整的代码片段（如适用），并说明该文件的重要性\n\
4. 错误与修复：列出遇到的错误及其修复方式，特别关注用户的反馈\n\
5. 问题解决：记录已解决的问题和正在进行的故障排除\n\
6. 所有用户消息：列出所有非工具结果的用户消息（这对理解用户反馈和意图变化至关重要）\n\
7. 待办任务：概述任何待处理任务\n\
8. 当前工作：精确描述摘要请求前正在进行的最新工作，包含文件名和代码片段（如适用）\n\
9. 可选的下一步：列出与当前工作直接相关的下一步。确保该步骤与用户最近的显式请求直接相关。\n\n\
请确保摘要精确、详尽，保持技术准确性。"
    );
    if let Some(extra) = custom_instructions {
        let t = extra.trim();
        if !t.is_empty() {
            p.push_str(&format!("\n\n额外指令：\n{t}"));
        }
    }
    p
}

/// 执行压缩：将 `messages` 压缩为单条 user 摘要消息。
///
/// 调用 provider 流式生成摘要；失败时返回 Err（调用方可回退到原消息）。
/// 不暴露工具 → 模型只能生成文本摘要。
pub async fn compact_messages(
    provider: &dyn Provider,
    system: &str,
    messages: &[Message],
    custom_instructions: Option<&str>,
) -> Result<Message> {
    if messages.is_empty() {
        return Err(AgentError::Provider("压缩无消息可处理".into()));
    }
    let prompt = compact_prompt(custom_instructions);
    // 构造请求：原始消息 + 末尾追加一条 user 请求生成摘要
    let mut req_messages = messages.to_vec();
    req_messages.push(Message::user(prompt));
    let req = StreamRequest::new(req_messages).with_system(system.to_string());
    // 压缩不暴露工具 → 模型只能生成文本摘要
    let mut stream = provider.stream(req);
    let mut summary = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::Delta(t) => summary.push_str(&t),
            StreamEvent::Done => break,
            StreamEvent::Error(m) => {
                return Err(AgentError::Provider(format!("压缩流式失败: {m}")));
            }
            _ => {}
        }
    }
    if summary.trim().is_empty() {
        return Err(AgentError::Provider("压缩未生成有效摘要".into()));
    }
    // 摘要包装为 user 消息（与 Claude Code 的 isCompactSummary 一致）
    let content = format!(
        "（上下文已压缩：以下是之前对话的摘要）\n\n{summary}"
    );
    Ok(Message::user(content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_ascii() {
        // 4 字符 ≈ 1 token
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
    }

    #[test]
    fn estimate_tokens_cjk() {
        // CJK 字符 1 字 ≈ 1 token（不再 4 字 1 token 低估）
        assert_eq!(estimate_tokens("你好世界"), 4);
        assert_eq!(estimate_tokens("你好世界你好世界"), 8);
    }

    #[test]
    fn estimate_tokens_mixed_cjk_ascii() {
        // 混合：2 个 CJK + 8 个 ASCII（8/4=2） = 4
        assert_eq!(estimate_tokens("你好abcdefgh"), 4);
    }

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_messages_tokens_includes_content() {
        let msgs = vec![
            Message::user("abcd"),    // 1
            Message::assistant("efgh"), // 1
        ];
        assert_eq!(estimate_messages_tokens(&msgs), 2);
    }

    #[test]
    fn estimate_messages_tokens_includes_tool_calls() {
        let mut m = Message::assistant("ab"); // 0 (整数除法)
        m.tool_calls.push(crate::types::ToolCall {
            id: "call_1".into(),     // 不计入 id（仅 name + arguments）
            name: "list_dir".into(), // 2
            arguments: "{\"path\":\".\"}".into(), // 3
        });
        // 0 + 2 + 3 = 5（id 不参与估算）
        assert_eq!(estimate_messages_tokens(&[m]), 5);
    }

    #[test]
    fn auto_compact_threshold_subtracts_buffer() {
        assert_eq!(
            auto_compact_threshold(Some(128_000)),
            Some(128_000 - AUTOCOMPACT_BUFFER_TOKENS)
        );
    }

    #[test]
    fn auto_compact_threshold_none_when_unknown() {
        assert_eq!(auto_compact_threshold(None), None);
    }

    #[test]
    fn auto_compact_threshold_saturates_at_zero() {
        // 上下文长度小于缓冲时阈值饱和为 0（立即触发）
        assert_eq!(auto_compact_threshold(Some(5_000)), Some(0));
    }

    #[test]
    fn context_remaining_percent_basic() {
        // 100 tokens used out of 1000 → 90%
        assert_eq!(
            context_remaining_percent(100, Some(1000)),
            Some(90)
        );
    }

    #[test]
    fn context_remaining_percent_full() {
        assert_eq!(
            context_remaining_percent(0, Some(1000)),
            Some(100)
        );
    }

    #[test]
    fn context_remaining_percent_zero_remaining() {
        assert_eq!(
            context_remaining_percent(1000, Some(1000)),
            Some(0)
        );
    }

    #[test]
    fn context_remaining_percent_clamps_overflow() {
        // used > total → 0%
        assert_eq!(
            context_remaining_percent(2000, Some(1000)),
            Some(0)
        );
    }

    #[test]
    fn context_remaining_percent_none_when_unknown() {
        assert_eq!(context_remaining_percent(100, None), None);
    }

    #[test]
    fn compact_prompt_includes_structure() {
        let p = compact_prompt(None);
        assert!(p.contains("主要请求与意图"));
        assert!(p.contains("当前工作"));
        assert!(p.contains("可选的下一步"));
    }

    #[test]
    fn compact_prompt_includes_custom_instructions() {
        let p = compact_prompt(Some("  关注安全相关内容  "));
        assert!(p.contains("额外指令"));
        assert!(p.contains("关注安全相关内容"));
    }

    #[test]
    fn compact_prompt_ignores_blank_instructions() {
        let p = compact_prompt(Some("   "));
        assert!(!p.contains("额外指令"));
    }

    #[test]
    fn compact_prompt_ignores_empty_instructions() {
        let p = compact_prompt(Some(""));
        assert!(!p.contains("额外指令"));
    }

    #[test]
    fn compact_messages_empty_returns_error() {
        // 空消息列表应返回错误（无法压缩）
        use crate::mock::MockProvider;
        let p = MockProvider::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(compact_messages(&p, "sys", &[], None));
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn compact_messages_with_mock_generates_summary() {
        use crate::mock::MockProvider;
        // MockProvider echo 模式（tools 空）：回放「收到：{最后一条 user 消息}」
        // 最后一条 user 是压缩提示词 → 摘要 = 「收到：{提示词开头...}」
        let p = MockProvider::new();
        let msgs = vec![
            Message::user("你好"),
            Message::assistant("你好！有什么可以帮你的？"),
        ];
        let result = compact_messages(&p, "sys", &msgs, None).await;
        assert!(result.is_ok(), "压缩应成功: {:?}", result.err());
        let m = result.unwrap();
        assert_eq!(m.role, crate::types::Role::User);
        assert!(m.content.contains("上下文已压缩"));
        assert!(m.content.contains("收到："));
    }

    #[tokio::test]
    async fn compact_messages_with_custom_instructions() {
        use crate::mock::MockProvider;
        let p = MockProvider::new();
        let msgs = vec![Message::user("测试")];
        // 自定义指令不影响 mock 回放（仅追加到提示词末尾）
        let result = compact_messages(&p, "sys", &msgs, Some("聚焦于代码改动")).await;
        assert!(result.is_ok());
    }
}
