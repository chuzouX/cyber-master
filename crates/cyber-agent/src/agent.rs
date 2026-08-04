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

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use cyber_core::{Config, ProjectContext, ProviderConfig, ProvidersConfig};

use crate::error::{AgentError, Result};
use crate::prompt::build_system_prompt;
use crate::provider::{provider_factory, StreamRequest};
use crate::tool::{ToolCtx, ToolOutput, ToolRegistry};
use crate::types::{AgentEvent, Message, StreamEvent, ToolCall, ToolCallDelta};

/// 发起一次流式对话（含 agent loop + 工具调用）。
///
/// - `history`：已完成的会话历史（user/assistant，不含本次输入）
/// - `tx`：事件回传通道，携带 `(gen, AgentEvent)`；TUI 退出时 `send` 失败，任务静默终止
/// - `gen`：generation 计数器，TUI cancel/新提交时 bump，据此忽略 stale 事件
/// - `mock`：强制使用 MockProvider（离线）
/// - `cwd`：工作目录（工具执行的 ToolCtx.cwd 来源）
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
) -> Result<()> {
    let name = &config.agent.default_provider;
    let cfg: &ProviderConfig = providers.providers.get(name).ok_or_else(|| {
        AgentError::Provider(format!("default_provider '{name}' 未在 providers.toml 配置"))
    })?;
    debug!(provider = %name, kind = %cfg.kind, mock, gen, "启动 agent loop");

    let provider = provider_factory(cfg, mock)?;
    let system = build_system_prompt(project);

    // 工具上下文 + 注册表
    let rules = project.map(|p| p.rules().to_vec()).unwrap_or_default();
    let scope = project.and_then(|p| p.frontmatter.scope.clone());
    let ctx = ToolCtx { cwd, rules, scope };
    let registry = ToolRegistry::with_builtins();

    // tools：auto_tool_call 开启且注册表非空时才暴露工具
    let tools = if config.agent.auto_tool_call && !registry.is_empty() {
        registry.schemas()
    } else {
        Vec::new()
    };

    let mut messages = history;
    messages.push(Message::user(user_input));

    let max_steps = config.agent.max_steps.max(1);
    for step in 0..max_steps {
        debug!(step, gen, "agent loop 迭代");
        let req = StreamRequest::new(messages.clone())
            .with_system(system.clone())
            .with_tools(tools.clone());
        let mut stream = provider.stream(req);

        // 累积本轮流式：Delta→Token 事件 + 文本；ToolCallDelta→按 index 合并；Done→break
        let (text, calls) = accumulate_stream(&mut stream, tx, gen).await;

        if calls.is_empty() {
            // 无工具调用：push assistant 文本，发 Done，结束
            if !text.is_empty() {
                messages.push(Message::assistant(text));
            }
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
        // 继续循环：带着工具结果再流式一次
    }

    // 超过 max_steps
    warn!(max_steps, gen, "agent loop 超过最大步数");
    let _ = tx.send((
        gen,
        AgentEvent::Error(format!("超过 max_steps({max_steps}) 限制，已停止")),
    ));
    Ok(())
}

/// 驱动一个流到结束，累积 Delta 文本与 ToolCallDelta（按 index 合并为完整 ToolCall）。
/// Delta→发 `(gen, AgentEvent::Token(t))` 并 append 到文本；Done→break；Error→发 Error 并 break。
/// 返回 `(累积文本, 按 index 排序的工具调用)`。
async fn accumulate_stream(
    stream: &mut (impl futures::Stream<Item = StreamEvent> + Unpin),
    tx: &UnboundedSender<(u64, AgentEvent)>,
    gen: u64,
) -> (String, BTreeMap<u32, ToolCall>) {
    let mut text = String::new();
    let mut calls: BTreeMap<u32, ToolCall> = BTreeMap::new();

    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::Delta(t) => {
                text.push_str(&t);
                if tx.send((gen, AgentEvent::Token(t))).is_err() {
                    debug!("TUI 通道已关闭，agent 累积终止");
                    return (text, calls);
                }
            }
            StreamEvent::ToolCallDelta(d) => {
                accumulate_tool_delta(&mut calls, d);
            }
            StreamEvent::Done => break,
            StreamEvent::Error(m) => {
                let _ = tx.send((gen, AgentEvent::Error(m)));
                break;
            }
        }
    }
    (text, calls)
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
        let (text, calls) = accumulate_stream(&mut s, &tx, 0).await;
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
        let (text, calls) = accumulate_stream(&mut s, &tx, 0).await;
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[&0].name, "list_dir");
        assert_eq!(calls[&0].arguments, "{\"path\":\".\"}");
    }
}
