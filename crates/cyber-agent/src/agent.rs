//! Agent 任务：组装 prompt → 选 provider → 驱动 agent loop → 转发 `AgentEvent`。
//!
//! `run_stream` 是 TUI `tokio::spawn` 的入口。所有入参 owned/clone，任务不借用 TUI 状态。
//! 任何 `?` 失败被 `run_stream` 捕获转 `AgentEvent::Error`；`tx.send` 失败（TUI 已退出）静默返回。
//! agent 任务永不 panic TUI（不在任务内 unwrap/expect）。
//!
//! Agent loop（P2.2）：流式→累积 tool_calls→执行工具→结果回灌→再流式，循环至无工具调用或
//! `max_steps`。每个事件携带 `gen`（generation 计数器），TUI 据此忽略 cancel 后的 stale 事件。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use cyber_core::{Config, ProjectContext, ProviderConfig, ProvidersConfig};

use crate::compact::{
    auto_compact_threshold, compact_messages, estimate_messages_tokens,
};
use crate::error::{AgentError, Result};
use crate::prompt::{build_system_prompt, CTF_PROMPT};
use crate::provider::{provider_factory, Provider, StreamRequest};
use crate::tool::{ToolCtx, ToolOutput, ToolRegistry};
use crate::types::{AgentEvent, Message, StreamEvent, ToolCall, ToolCallDelta, Usage};

/// 连续相同工具调用检测器：记录每轮工具调用指纹，连续 `threshold` 轮相同则判定死循环。
///
/// 指纹 = 本轮所有 ToolCall 的 `name|arguments` 排序后拼接（排序消除顺序差异）。
/// 不同参数的同名工具不算重复（`read_file({"path":"a"})` vs `read_file({"path":"b"})` 指纹不同）。
/// 单轮多个工具调用时，整组指纹参与比较——只要任一个参数变了就不算重复。
struct LoopDetector {
    last_fingerprint: Option<String>,
    repeat_count: u32,
    threshold: u32,
}

impl LoopDetector {
    /// `threshold` = 连续多少轮相同触发（通常 3）。
    fn new(threshold: u32) -> Self {
        Self {
            last_fingerprint: None,
            repeat_count: 0,
            threshold: threshold.max(1),
        }
    }

    /// 记录本轮工具调用，返回 `true` 表示连续 `threshold` 轮指纹相同（死循环）。
    fn observe(&mut self, calls: &BTreeMap<u32, ToolCall>) -> bool {
        let fp = fingerprint(calls);
        if Some(&fp) == self.last_fingerprint.as_ref() {
            self.repeat_count += 1;
        } else {
            self.repeat_count = 1;
            self.last_fingerprint = Some(fp);
        }
        self.repeat_count >= self.threshold
    }
}

/// 计算一轮工具调用的指纹：所有 call 的 `name|arguments` 排序后拼接。
fn fingerprint(calls: &BTreeMap<u32, ToolCall>) -> String {
    let mut sigs: Vec<String> = calls
        .values()
        .map(|c| format!("{}|{}", c.name, c.arguments))
        .collect();
    sigs.sort();
    sigs.join("§")
}

/// 发起一次流式对话（含 agent loop + 工具调用）。
///
/// - `history`：已完成的会话历史（user/assistant，不含本次输入）
/// - `tx`：事件回传通道，携带 `(gen, AgentEvent)`；TUI 退出时 `send` 失败，任务静默终止
/// - `gen`：generation 计数器，TUI cancel/新提交时 bump，据此忽略 stale 事件
/// - `mock`：强制使用 MockProvider（离线）
/// - `cwd`：工作目录（工具执行的 ToolCtx.cwd 来源）
/// - `registry`：统一工具表（builtins + MCP + Skills），跨 agent turn 共享（`Arc` clone）
#[allow(clippy::too_many_arguments)]
pub async fn run_stream(
    config: Config,
    providers: ProvidersConfig,
    project: Option<ProjectContext>,
    user_input: String,
    history: Vec<Message>,
    tx: UnboundedSender<(u64, AgentEvent)>,
    gen: u64,
    mock: bool,
    cwd: PathBuf,
    registry: Arc<ToolRegistry>,
    ctf_enabled: bool,
) {
    let _ = tx.send((gen, AgentEvent::Started));
    let res = run_inner(
        &config,
        &providers,
        project.as_ref(),
        user_input,
        history,
        &tx,
        gen,
        mock,
        cwd,
        registry,
        ctf_enabled,
    )
    .await;
    if let Err(e) = res {
        warn!(error = %e, "agent run_stream 失败");
        let _ = tx.send((gen, AgentEvent::Error(e.to_string())));
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    config: &Config,
    providers: &ProvidersConfig,
    project: Option<&ProjectContext>,
    user_input: String,
    history: Vec<Message>,
    tx: &UnboundedSender<(u64, AgentEvent)>,
    gen: u64,
    mock: bool,
    cwd: PathBuf,
    registry: Arc<ToolRegistry>,
    ctf_enabled: bool,
) -> Result<()> {
    let name = &config.agent.default_provider;
    let cfg: &ProviderConfig = providers.providers.get(name).ok_or_else(|| {
        AgentError::Provider(format!("default_provider '{name}' 未在 providers.toml 配置"))
    })?;
    debug!(provider = %name, kind = %cfg.kind, mock, gen, "启动 agent loop");

    let provider = provider_factory(cfg, mock)?;
    let mut system = build_system_prompt(project);
    if ctf_enabled {
        system.push_str(CTF_PROMPT);
    }

    // 工具上下文 + 注册表
    let rules = project.map(|p| p.rules().to_vec()).unwrap_or_default();
    let scope = project.and_then(|p| p.frontmatter.scope.clone());
    let env = config
        .env
        .vars
        .iter()
        .map(|v| (v.key.clone(), v.value.clone()))
        .collect();
    let ctx = ToolCtx { cwd, rules, scope, env };

    // tools：auto_tool_call 开启且注册表非空时才暴露工具
    let tools = if config.agent.auto_tool_call && !registry.is_empty() {
        registry.schemas()
    } else {
        Vec::new()
    };

    let mut messages = history;
    messages.push(Message::user(user_input));

    // 有效上下文长度（用于自动压缩阈值 + TUI 剩余百分比显示）。
    // 未配置时为 None → 不触发自动压缩，TUI 不显示百分比。
    let effective_ctx_len = cfg.effective_context_length();

    // 首次发送上下文使用情况（TUI 据此显示初始剩余百分比）
    emit_context_update(tx, gen, &messages, effective_ctx_len);

    let max_steps = config.agent.max_steps.max(1);
    let mut detector = LoopDetector::new(3);
    let mut loop_detected = false;
    for step in 0..max_steps {
        debug!(step, gen, "agent loop 迭代");

        // 自动压缩检查：估算当前 messages token 数，超过阈值则先压缩再继续。
        // 仅在 effective_ctx_len 已知时触发；压缩失败仅记日志不中断（回退到原消息）。
        if let Some(threshold) = auto_compact_threshold(effective_ctx_len) {
            let used = estimate_messages_tokens(&messages);
            if used >= threshold as usize {
                debug!(step, used, threshold, gen, "触发自动上下文压缩");
                match do_compact(
                    provider.as_ref(),
                    &system,
                    &mut messages,
                    None,
                    tx,
                    gen,
                    true, // is_auto
                )
                .await
                {
                    Ok(()) => {}
                    Err(e) => warn!(error = %e, gen, "自动压缩失败，回退到原消息继续"),
                }
            }
        }

        let req = StreamRequest::new(messages.clone())
            .with_system(system.clone())
            .with_tools(tools.clone());
        let mut stream = provider.stream(req);

        // 累积本轮流式：Delta→Token 事件 + 文本；ToolCallDelta→按 index 合并；Done→break
        let (text, calls, usage) = accumulate_stream(&mut stream, tx, gen).await;

        // 发送本轮 usage（TUI 据此显示缓存命中率 + 成本）
        if let Some(ref u) = usage {
            let _ = tx.send((gen, AgentEvent::Usage(u.clone())));
        }

        if calls.is_empty() {
            // 无工具调用：push assistant 文本，发 Done，结束
            if !text.is_empty() {
                messages.push(Message::assistant(text));
            }
            emit_context_update(tx, gen, &messages, effective_ctx_len);
            let _ = tx.send((gen, AgentEvent::Done));
            return Ok(());
        }

        // 有工具调用：push assistant(text + tool_calls)
        let mut assistant_msg = Message::assistant(text);
        assistant_msg.tool_calls = calls.values().cloned().collect();
        messages.push(assistant_msg);

        // 逐个执行工具调用，结果回灌
        for call in calls.values() {
            let _ = tx.send((
                gen,
                AgentEvent::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                },
            ));
            let input: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
            let out = match registry.execute(&call.name, input, &ctx).await {
                Ok(o) => o,
                Err(e) => ToolOutput {
                    content: e.to_string(),
                    is_error: true,
                },
            };
            let _ = tx.send((
                gen,
                AgentEvent::ToolResult {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    output: out.content.clone(),
                    is_error: out.is_error,
                },
            ));
            messages.push(Message::tool(call.id.clone(), out.content));
        }

        // 工具结果回灌后发送上下文使用情况（工具结果可能显著增加 token 数）
        emit_context_update(tx, gen, &messages, effective_ctx_len);

        // 死循环检测：连续 3 轮相同工具调用指纹 → 提前中止（比空跑 max_steps 省钱省时）
        if detector.observe(&calls) {
            warn!(step, gen, repeat_count = detector.repeat_count, "检测到连续重复工具调用，提前中止 agent loop");
            loop_detected = true;
            break;
        }
        // 继续循环：带着工具结果再流式一次
    }

    // 收尾总结（max_steps 耗尽 或 死循环检测触发）：
    // 做一次无工具的收尾流式，让模型总结已收集的信息，而非直接报错中断。
    // 收尾流式 `tools=[]` → 模型无法再调工具，只能输出文本；其 Delta 经
    // `accumulate_stream` → `AgentEvent::Token` 流式回传 TUI。发 `Done` 而非 `Error`
    // → TUI 走正常定稿（总结文本成为 assistant 条目进入 history，使「继续」有上下文）。
    let wrap = if loop_detected {
        warn!(max_steps, gen, "agent loop 因连续重复工具调用提前中止，进入收尾总结");
        "（系统提示：检测到连续多次相同的工具调用，可能已陷入循环。请根据已收集的信息直接给出最终回答或阶段性结论，不要再调用工具。）".to_string()
    } else {
        warn!(max_steps, gen, "agent loop 超过最大步数，进入收尾总结");
        format!(
            "（系统提示：已达到工具调用步数上限 {max_steps}。请根据已收集的信息直接给出最终回答或阶段性结论，不要再调用工具。）"
        )
    };
    messages.push(Message::user(wrap));
    let req = StreamRequest::new(messages.clone())
        .with_system(system.clone())
        .with_tools(Vec::new()); // 不暴露工具 → 模型只能给文本
    let mut stream = provider.stream(req);
    let (text, _calls, usage) = accumulate_stream(&mut stream, tx, gen).await;
    if let Some(ref u) = usage {
        let _ = tx.send((gen, AgentEvent::Usage(u.clone())));
    }
    if !text.is_empty() {
        messages.push(Message::assistant(text));
    }
    emit_context_update(tx, gen, &messages, effective_ctx_len);
    let _ = tx.send((gen, AgentEvent::Done));
    Ok(())
}

/// 发送上下文使用情况更新事件（TUI 据此显示剩余百分比）。
/// `effective_ctx_len` 为 None 时不发送（TUI 不显示百分比）。
fn emit_context_update(
    tx: &UnboundedSender<(u64, AgentEvent)>,
    gen: u64,
    messages: &[Message],
    effective_ctx_len: Option<u32>,
) {
    if effective_ctx_len.is_none() {
        return; // 未知上下文长度 → 不发送
    }
    let used = estimate_messages_tokens(messages);
    let _ = tx.send((
        gen,
        AgentEvent::ContextUpdate {
            used_tokens: used,
            effective_context_length: effective_ctx_len,
        },
    ));
}

/// 执行上下文压缩：发 Compacting 事件 → 调用 compact_messages 替换 messages → 发 Compacted 事件。
/// `is_auto` 区分自动触发（达到阈值）与手动 `/compact`。
/// 压缩成功后 `messages` 将被替换为 `[摘要 user 消息]`。
async fn do_compact(
    provider: &dyn Provider,
    system: &str,
    messages: &mut Vec<Message>,
    custom_instructions: Option<&str>,
    tx: &UnboundedSender<(u64, AgentEvent)>,
    gen: u64,
    is_auto: bool,
) -> Result<()> {
    let before_tokens = estimate_messages_tokens(messages);
    let _ = tx.send((gen, AgentEvent::Compacting { is_auto }));
    let summary_msg = compact_messages(provider, system, messages, custom_instructions).await?;
    let after_tokens = estimate_messages_tokens(&[summary_msg.clone()]);
    *messages = vec![summary_msg.clone()];
    let _ = tx.send((
        gen,
        AgentEvent::Compacted {
            summary: summary_msg.content,
            before_tokens,
            after_tokens,
        },
    ));
    Ok(())
}

/// 手动触发上下文压缩（`/compact` 命令入口）。
///
/// 与 `run_stream` 类似的入参签名，但不进入 agent loop——仅做一次压缩并返回。
/// 压缩后的 `messages`（含摘要）经 `AgentEvent::Compacted` 传回 TUI，TUI 据此
/// 替换本地 chat 历史。
///
/// - `history`：当前会话的全部已完成消息（user/assistant，不含工具调用中间态）
/// - `custom_instructions`：可选的自定义摘要指令（`/compact <instructions>` 参数）
#[allow(clippy::too_many_arguments)]
pub async fn run_compact_stream(
    config: Config,
    providers: ProvidersConfig,
    project: Option<ProjectContext>,
    history: Vec<Message>,
    custom_instructions: Option<String>,
    tx: UnboundedSender<(u64, AgentEvent)>,
    gen: u64,
    mock: bool,
) {
    let _ = tx.send((gen, AgentEvent::Started));
    let res = run_compact_inner(
        &config,
        &providers,
        project.as_ref(),
        history,
        custom_instructions,
        &tx,
        gen,
        mock,
    )
    .await;
    if let Err(e) = res {
        warn!(error = %e, "run_compact_stream 失败");
        let _ = tx.send((gen, AgentEvent::Error(e.to_string())));
    }
    let _ = tx.send((gen, AgentEvent::Done));
}

/// 生成 CTF 题目 writeup（`/ctf writeup` 入口）。
///
/// 与 `run_compact_stream` 类似的无工具文本生成流程，但：
/// - system prompt = ctf-writeup skill body（撰写指南）
/// - user message = 题目上下文（名称/分类/描述/靶机/flag/标签/用时/关键知识点）
/// - 不进入 agent loop，仅一次流式生成
///
/// 流式 token 经 `AgentEvent::Token` 转发，TUI 收集后拼成完整 writeup。
#[allow(clippy::too_many_arguments)]
pub async fn run_writeup_stream(
    config: Config,
    providers: ProvidersConfig,
    skill_body: String,
    challenge_context: String,
    tx: UnboundedSender<(u64, AgentEvent)>,
    gen: u64,
    mock: bool,
) {
    let _ = tx.send((gen, AgentEvent::Started));
    let res = run_writeup_inner(
        &config,
        &providers,
        &skill_body,
        &challenge_context,
        &tx,
        gen,
        mock,
    )
    .await;
    if let Err(e) = res {
        warn!(error = %e, "run_writeup_stream 失败");
        let _ = tx.send((gen, AgentEvent::Error(e.to_string())));
    }
    let _ = tx.send((gen, AgentEvent::Done));
}

async fn run_writeup_inner(
    config: &Config,
    providers: &ProvidersConfig,
    skill_body: &str,
    challenge_context: &str,
    tx: &UnboundedSender<(u64, AgentEvent)>,
    gen: u64,
    mock: bool,
) -> Result<()> {
    let name = &config.agent.default_provider;
    let cfg: &ProviderConfig = providers.providers.get(name).ok_or_else(|| {
        AgentError::Provider(format!("default_provider '{name}' 未在 providers.toml 配置"))
    })?;
    let provider = provider_factory(cfg, mock)?;

    let req = StreamRequest::new(vec![Message::user(challenge_context)])
        .with_system(skill_body.to_string());
    let mut stream = provider.stream(req);
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::Delta(t) => {
                if tx.send((gen, AgentEvent::Token(t))).is_err() {
                    debug!("TUI 通道已关闭，writeup 生成终止");
                    return Ok(());
                }
            }
            StreamEvent::Usage(u) => {
                let _ = tx.send((gen, AgentEvent::Usage(u)));
            }
            StreamEvent::Done => break,
            StreamEvent::Error(m) => {
                return Err(AgentError::Provider(format!("writeup 生成失败: {m}")));
            }
            _ => {}
        }
    }
    Ok(())
}

async fn run_compact_inner(
    config: &Config,
    providers: &ProvidersConfig,
    project: Option<&ProjectContext>,
    history: Vec<Message>,
    custom_instructions: Option<String>,
    tx: &UnboundedSender<(u64, AgentEvent)>,
    gen: u64,
    mock: bool,
) -> Result<()> {
    let name = &config.agent.default_provider;
    let cfg: &ProviderConfig = providers.providers.get(name).ok_or_else(|| {
        AgentError::Provider(format!("default_provider '{name}' 未在 providers.toml 配置"))
    })?;
    let provider = provider_factory(cfg, mock)?;
    let system = build_system_prompt(project);

    if history.is_empty() {
        return Err(AgentError::Provider("无消息可压缩".into()));
    }

    let mut messages = history;
    do_compact(
        provider.as_ref(),
        &system,
        &mut messages,
        custom_instructions.as_deref(),
        tx,
        gen,
        false, // is_auto = false（手动触发）
    )
    .await
}

/// 驱动一个流到结束，累积 Delta 文本与 ToolCallDelta（按 index 合并为完整 ToolCall）。
/// Delta→发 `(gen, AgentEvent::Token(t))` 并 append 到文本；Done→break；Error→发 Error 并 break。
/// 返回 `(累积文本, 按 index 排序的工具调用, usage 用量)`。
async fn accumulate_stream(
    stream: &mut (impl futures::Stream<Item = StreamEvent> + Unpin),
    tx: &UnboundedSender<(u64, AgentEvent)>,
    gen: u64,
) -> (String, BTreeMap<u32, ToolCall>, Option<Usage>) {
    let mut text = String::new();
    let mut calls: BTreeMap<u32, ToolCall> = BTreeMap::new();
    let mut usage: Option<Usage> = None;

    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::Delta(t) => {
                text.push_str(&t);
                if tx.send((gen, AgentEvent::Token(t))).is_err() {
                    debug!("TUI 通道已关闭，agent 累积终止");
                    return (text, calls, usage);
                }
            }
            StreamEvent::Reasoning(t) => {
                if tx.send((gen, AgentEvent::Reasoning(t))).is_err() {
                    debug!("TUI 通道已关闭，agent 累积终止");
                    return (text, calls, usage);
                }
            }
            StreamEvent::ToolCallDelta(d) => {
                accumulate_tool_delta(&mut calls, d);
            }
            StreamEvent::Usage(u) => {
                usage = Some(u);
            }
            StreamEvent::Done => break,
            StreamEvent::Error(m) => {
                let _ = tx.send((gen, AgentEvent::Error(m)));
                break;
            }
        }
    }
    (text, calls, usage)
}

/// 把一个 `ToolCallDelta` 片段合并进 `calls` 累积器（按 index）。
/// 首片带 id+name 时初始化；后续片段只 append arguments_fragment。
fn accumulate_tool_delta(calls: &mut BTreeMap<u32, ToolCall>, d: ToolCallDelta) {
    let entry = calls.entry(d.index).or_insert_with(|| ToolCall {
        id: String::new(),
        name: String::new(),
        arguments: String::new(),
    });
    if let Some(id) = d.id {
        entry.id = id;
    }
    if let Some(name) = d.name {
        entry.name = name;
    }
    entry.arguments.push_str(&d.arguments_fragment);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulate_tool_delta_merges_by_index() {
        let mut calls = BTreeMap::new();
        // 首片：id+name+空 arguments
        accumulate_tool_delta(
            &mut calls,
            ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("list_dir".into()),
                arguments_fragment: String::new(),
            },
        );
        // 后续片 1
        accumulate_tool_delta(
            &mut calls,
            ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_fragment: "{\"pa".into(),
            },
        );
        // 后续片 2
        accumulate_tool_delta(
            &mut calls,
            ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_fragment: "th\":\".\"}".into(),
            },
        );
        assert_eq!(calls.len(), 1);
        let tc = &calls[&0];
        assert_eq!(tc.id, "call_1");
        assert_eq!(tc.name, "list_dir");
        assert_eq!(tc.arguments, "{\"path\":\".\"}");
    }

    #[test]
    fn accumulate_tool_delta_handles_multiple_indices() {
        let mut calls = BTreeMap::new();
        accumulate_tool_delta(
            &mut calls,
            ToolCallDelta {
                index: 0,
                id: Some("a".into()),
                name: Some("read_file".into()),
                arguments_fragment: "{}".into(),
            },
        );
        accumulate_tool_delta(
            &mut calls,
            ToolCallDelta {
                index: 1,
                id: Some("b".into()),
                name: Some("list_dir".into()),
                arguments_fragment: "{}".into(),
            },
        );
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[&0].name, "read_file");
        assert_eq!(calls[&1].name, "list_dir");
    }

    #[tokio::test]
    async fn accumulate_stream_collects_text_and_breaks_on_done() {
        use futures::stream;
        let events = vec![
            StreamEvent::Delta("Hello".into()),
            StreamEvent::Delta(" world".into()),
            StreamEvent::Done,
        ];
        let mut s = stream::iter(events);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<(u64, AgentEvent)>();
        let (text, calls, _usage) = accumulate_stream(&mut s, &tx, 0).await;
        assert_eq!(text, "Hello world");
        assert!(calls.is_empty());
    }

    #[tokio::test]
    async fn accumulate_stream_collects_tool_call_deltas() {
        use futures::stream;
        let events = vec![
            StreamEvent::ToolCallDelta(ToolCallDelta {
                index: 0,
                id: Some("c1".into()),
                name: Some("list_dir".into()),
                arguments_fragment: String::new(),
            }),
            StreamEvent::ToolCallDelta(ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_fragment: "{\"path\":\".\"}".into(),
            }),
            StreamEvent::Done,
        ];
        let mut s = stream::iter(events);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<(u64, AgentEvent)>();
        let (text, calls, _usage) = accumulate_stream(&mut s, &tx, 0).await;
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[&0].name, "list_dir");
        assert_eq!(calls[&0].arguments, "{\"path\":\".\"}");
    }

    // ── LoopDetector / fingerprint ──────────────────────────────────────────

    fn tc(name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: String::new(),
            name: name.into(),
            arguments: args.into(),
        }
    }

    fn calls_map(calls: &[ToolCall]) -> BTreeMap<u32, ToolCall> {
        calls
            .iter()
            .enumerate()
            .map(|(i, c)| (i as u32, c.clone()))
            .collect()
    }

    #[test]
    fn fingerprint_same_calls_same_result() {
        let a = calls_map(&[tc("list_dir", "{\"path\":\".\"}")]);
        let b = calls_map(&[tc("list_dir", "{\"path\":\".\"}")]);
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn fingerprint_different_args_different_result() {
        let a = calls_map(&[tc("read_file", "{\"path\":\"a.txt\"}")]);
        let b = calls_map(&[tc("read_file", "{\"path\":\"b.txt\"}")]);
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn fingerprint_order_independent() {
        let a = calls_map(&[
            tc("read_file", "{\"path\":\"a\"}"),
            tc("list_dir", "{\"path\":\".\"}"),
        ]);
        // 不同顺序
        let b = calls_map(&[
            tc("list_dir", "{\"path\":\".\"}"),
            tc("read_file", "{\"path\":\"a\"}"),
        ]);
        assert_eq!(fingerprint(&a), fingerprint(&b), "排序后应相同");
    }

    #[test]
    fn loop_detector_no_trigger_on_diverse_calls() {
        let mut d = LoopDetector::new(3);
        // 每轮不同参数 → 不触发
        assert!(!d.observe(&calls_map(&[tc("read_file", "{\"p\":\"a\"}")])));
        assert!(!d.observe(&calls_map(&[tc("read_file", "{\"p\":\"b\"}")])));
        assert!(!d.observe(&calls_map(&[tc("read_file", "{\"p\":\"c\"}")])));
    }

    #[test]
    fn loop_detector_triggers_on_3_consecutive_same() {
        let mut d = LoopDetector::new(3);
        let c = calls_map(&[tc("shell", "{\"command\":\"ls\"}")]);
        assert!(!d.observe(&c), "第 1 轮：不触发");
        assert!(!d.observe(&c), "第 2 轮：不触发");
        assert!(d.observe(&c), "第 3 轮连续相同：应触发");
    }

    #[test]
    fn loop_detector_resets_on_different_call() {
        let mut d = LoopDetector::new(3);
        let c1 = calls_map(&[tc("shell", "{\"command\":\"ls\"}")]);
        let c2 = calls_map(&[tc("shell", "{\"command\":\"pwd\"}")]);
        assert!(!d.observe(&c1));
        assert!(!d.observe(&c1)); // 2 次相同
        assert!(!d.observe(&c2)); // 不同 → 重置
        assert!(!d.observe(&c2)); // 重新 2 次
        assert!(d.observe(&c2)); // 3 次 → 触发
    }

    #[test]
    fn loop_detector_threshold_1_triggers_immediately() {
        let mut d = LoopDetector::new(1);
        let c = calls_map(&[tc("list_dir", "{}")]);
        assert!(d.observe(&c), "threshold=1 第 1 轮就应触发");
    }

    #[test]
    fn loop_detector_multiple_tools_same_set_triggers() {
        let mut d = LoopDetector::new(3);
        // 每轮调两个工具，组合相同
        let c = calls_map(&[
            tc("read_file", "{\"path\":\"a\"}"),
            tc("list_dir", "{\"path\":\".\"}"),
        ]);
        assert!(!d.observe(&c));
        assert!(!d.observe(&c));
        assert!(d.observe(&c), "连续 3 轮相同组合应触发");
    }

    #[test]
    fn loop_detector_different_extra_tool_resets() {
        let mut d = LoopDetector::new(3);
        let base = calls_map(&[tc("shell", "{\"command\":\"ls\"}")]);
        let extra = calls_map(&[
            tc("shell", "{\"command\":\"ls\"}"),
            tc("read_file", "{\"path\":\"x\"}"),
        ]);
        assert!(!d.observe(&base));
        assert!(!d.observe(&base));
        assert!(!d.observe(&extra)); // 组合变了 → 重置
        assert!(!d.observe(&base));
        assert!(!d.observe(&base));
        // 仅 2 次 base，不触发
    }
}
