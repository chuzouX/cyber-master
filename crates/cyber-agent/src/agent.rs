//! Agent 任务：组装 prompt → 选 provider → 驱动流 → 转发 `AgentEvent`。
//!
//! `run_stream` 是 TUI `tokio::spawn` 的入口。所有入参 owned/clone，任务不借用 TUI 状态。
//! 任何 `?` 失败被 `run_stream` 捕获转 `AgentEvent::Error`；`tx.send` 失败（TUI 已退出）静默返回。
//! agent 任务永不 panic TUI（不在任务内 unwrap/expect）。

use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use cyber_core::{Config, ProjectContext, ProviderConfig, ProvidersConfig};

use crate::error::{AgentError, Result};
use crate::prompt::build_system_prompt;
use crate::provider::provider_factory;
use crate::types::{AgentEvent, Message, Role};

/// 发起一次流式对话。
///
/// - `history`：已完成的会话历史（user/assistant，不含本次输入）
/// - `tx`：事件回传通道；TUI 退出时 `send` 失败，任务静默终止
/// - `mock`：强制使用 MockProvider（离线）
pub async fn run_stream(
    config: Config,
    providers: ProvidersConfig,
    project: Option<ProjectContext>,
    user_input: String,
    history: Vec<Message>,
    tx: UnboundedSender<AgentEvent>,
    mock: bool,
) {
    let _ = tx.send(AgentEvent::Started);
    let res = run_inner(&config, &providers, project.as_ref(), user_input, history, &tx, mock).await;
    if let Err(e) = res {
        warn!(error = %e, "agent run_stream 失败");
        let _ = tx.send(AgentEvent::Error(e.to_string()));
    }
}

async fn run_inner(
    config: &Config,
    providers: &ProvidersConfig,
    project: Option<&ProjectContext>,
    user_input: String,
    history: Vec<Message>,
    tx: &UnboundedSender<AgentEvent>,
    mock: bool,
) -> Result<()> {
    let name = &config.agent.default_provider;
    let cfg: &ProviderConfig = providers.providers.get(name).ok_or_else(|| {
        AgentError::Provider(format!("default_provider '{name}' 未在 providers.toml 配置"))
    })?;
    debug!(provider = %name, kind = %cfg.kind, mock, "启动流式对话");

    let provider = provider_factory(cfg, mock)?;
    let system = build_system_prompt(project);

    let mut messages = history;
    messages.push(Message {
        role: Role::User,
        content: user_input,
    });

    let mut stream = provider.stream(messages, Some(system));
    while let Some(ev) = stream.next().await {
        // TUI 退出 → 通道关闭 → send 失败 → 静默结束（不视作错误）
        if tx.send(ev.into()).is_err() {
            debug!("TUI 通道已关闭，agent 任务终止");
            return Ok(());
        }
    }
    Ok(())
}
