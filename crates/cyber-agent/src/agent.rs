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
use tracing::{debug, info, warn};

use cyber_core::{Config, ProjectContext, ProviderConfig, ProvidersConfig, ThinkingIntensity};

use crate::compact::{
    auto_compact_threshold, compact_messages_with_retry, estimate_messages_tokens,
};
use crate::error::{AgentError, Result};
use crate::prompt::{build_system_prompt, SkillSummary, CTF_PROMPT};
use crate::provider::{provider_factory, Provider, StreamRequest};
use crate::tool::{ToolCtx, ToolOutput, ToolRegistry};
use crate::types::{AgentEvent, Message, StreamEvent, ToolCall, ToolCallDelta, Usage};

/// 未配置 per-model `context_length` 时的默认回退值（token 数）。
///
/// `effective_context_length()` 仅在 providers.toml 的 `[providers.X.models.MODEL]`
/// 配置了 `context_length` 时才返回 Some；否则返回 None → 自动压缩永不触发，
/// 导致长对话在真实上下文耗尽时被 provider 突然截断（用户看到「突然停止思考」）。
/// 取 128_000 作为主流模型（GPT-4o / DeepSeek-V3 / Claude）的通用回退，
/// 用户可在 providers.toml 精确配置以获得更准确的阈值。
const DEFAULT_CONTEXT_LENGTH: u32 = 128_000;

/// 截断后连续「零新增正文」轮次容忍上限：达到即判定无法再产出正文，发 Notice 并结束回合。
const MAX_EMPTY_TRUNCATION_ROUNDS: u32 = 2;

/// 截断续写注入的内部指令（写入模型消息历史，不直接渲染到 UI）。
const TRUNCATION_CONTINUE_NUDGE: &str = "（系统提示：上一条输出因达到模型长度上限被截断。请立刻停止长篇推理，直接继续输出正文内容，不要重复已经输出过的部分。）";

/// 模型只产生思考、未输出正文时的提醒（整个回合最多注入一次）。
const EMPTY_ANSWER_NUDGE: &str =
    "（系统提示：你还没有输出任何面向用户的正文内容。请停止思考，直接给出最终答复。）";

/// 回合结束时的截断提示文案。
fn truncation_notice(answer: &str, truncated: bool) -> String {
    let hint = "可尝试用 /think 降低思考强度，或提高 providers.toml 中该模型的 max_tokens。";
    if truncated {
        if answer.trim().is_empty() {
            format!("⚠️ 模型输出因长度上限（max_tokens）被截断，自动续写后仍未产出正文内容。{hint}")
        } else {
            format!("⚠️ 模型回复因长度上限（max_tokens）被截断，已自动续写并保留全部已输出内容，但结尾可能不完整。{hint}")
        }
    } else {
        "⚠️ 模型未输出正文内容（仅产生思考）。可尝试用 /think 降低思考强度后重发。".to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentExitReason {
    Finished,
    MaxStepsReached,
    LoopDetected,
}

#[derive(Debug, Clone)]
pub(crate) struct SubagentTaskOutput {
    pub output: String,
    pub exit_reason: AgentExitReason,
}

pub(crate) struct SubagentRuntime {
    config: Config,
    providers: ProvidersConfig,
    project: Option<ProjectContext>,
    mock: bool,
    cwd: PathBuf,
    registry: Arc<ToolRegistry>,
    ctf_enabled: bool,
    intensity: ThinkingIntensity,
    memory: String,
    tx: UnboundedSender<(u64, AgentEvent)>,
    gen: u64,
    archive: Option<Arc<crate::subagent::SubagentArchive>>,
    background: Option<Arc<crate::background::BackgroundRegistry>>,
}

impl SubagentRuntime {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config: Config,
        providers: ProvidersConfig,
        project: Option<ProjectContext>,
        mock: bool,
        cwd: PathBuf,
        registry: Arc<ToolRegistry>,
        ctf_enabled: bool,
        intensity: ThinkingIntensity,
        memory: String,
        tx: UnboundedSender<(u64, AgentEvent)>,
        gen: u64,
        archive: Option<Arc<crate::subagent::SubagentArchive>>,
        background: Option<Arc<crate::background::BackgroundRegistry>>,
    ) -> Self {
        Self {
            config,
            providers,
            project,
            mock,
            cwd,
            registry,
            ctf_enabled,
            intensity,
            memory,
            tx,
            gen,
            archive,
            background,
        }
    }

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    pub(crate) fn providers(&self) -> &ProvidersConfig {
        &self.providers
    }

    pub(crate) fn registry(&self) -> &Arc<ToolRegistry> {
        &self.registry
    }

    pub(crate) fn archive(&self) -> Option<&Arc<crate::subagent::SubagentArchive>> {
        self.archive.as_ref()
    }

    pub(crate) fn background(&self) -> Option<&Arc<crate::background::BackgroundRegistry>> {
        self.background.as_ref()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_task(
        &self,
        specialist_prompt: String,
        task: String,
        context: String,
        provider_name: Option<String>,
        model: Option<String>,
        tool_names: Vec<String>,
        task_progress: Option<UnboundedSender<String>>,
        transcript: Option<Arc<std::sync::Mutex<crate::subagent::SubagentRun>>>,
        custom_max_steps: Option<u32>,
    ) -> Result<SubagentTaskOutput> {
        let provider_name =
            provider_name.unwrap_or_else(|| self.config.agent.default_provider.clone());
        let mut provider_config = self
            .providers
            .providers
            .get(&provider_name)
            .cloned()
            .ok_or_else(|| AgentError::Provider(format!("unknown provider '{provider_name}'")))?;
        if let Some(model) = model {
            provider_config.model = model;
        }
        let provider = provider_factory(&provider_config, self.mock)?;
        let allowed: std::collections::HashSet<String> = tool_names.into_iter().collect();
        if allowed.contains("delegate_tasks") {
            return Err(AgentError::Provider(
                "delegate_tasks cannot be delegated recursively".into(),
            ));
        }
        let schemas: Vec<_> = self
            .registry
            .all_schemas()
            .into_iter()
            .filter(|schema| allowed.contains(&schema.name))
            .filter(|schema| self.config.tools.web_search || schema.name != "web_fetch")
            .collect();
        if schemas.len() != allowed.len() {
            return Err(AgentError::Provider(
                "subagent tool allowlist contains an unknown tool".into(),
            ));
        }

        let mut system = build_system_prompt(
            self.project.as_ref(),
            self.intensity.resolve(self.ctf_enabled),
            &skill_summaries(&schemas),
            &self.memory,
        );
        if self.ctf_enabled {
            system.push_str(CTF_PROMPT);
        }
        system.push_str("\n\n# Specialist instructions\n");
        system.push_str(&specialist_prompt);
        system.push_str(
            "\n\nComplete only this task. Return an evidence-based result. You must not delegate or create subagents.",
        );
        system.push_str(
            "\n\n你是独立自主运行的专家子代理，全权负责完成所委派的具体任务。必须将任务执行到最终可验收状态（如需修改文件则必须完成写入，如需验证则必须运行验证命令）。不要中途停下询问，也不要在仅有计划时停止调用工具。在全部操作执行完毕前，保持通过工具推进。",
        );
        let user_message = if context.is_empty() {
            task
        } else {
            format!("{task}\n\nContext:\n{context}")
        };
        let rules = self
            .project
            .as_ref()
            .map(|project| project.rules().to_vec())
            .unwrap_or_default();
        let scope = self
            .project
            .as_ref()
            .and_then(|project| project.frontmatter.scope.clone());
        let env = self
            .config
            .env
            .vars
            .iter()
            .map(|value| (value.key.clone(), value.value.clone()))
            .collect();
        let ctx = ToolCtx::new(self.cwd.clone(), rules, scope, env)
            .with_subagent_archive(self.archive().cloned())
            .with_background(self.background().cloned())
            .with_provider_config(Some(provider_config.clone()))
            .with_mock(self.mock);
        let sink = EventSink::child_with_transcript(
            &self.tx,
            self.gen,
            task_progress.as_ref(),
            transcript.as_ref(),
        );
        let effective_steps = custom_max_steps
            .map(|s| s.clamp(1, 200))
            .unwrap_or_else(|| self.config.agent.subagents.effective_max_steps());
        let output = run_agent_loop(
            provider.as_ref(),
            system,
            vec![Message::user(user_message)],
            ctx,
            schemas,
            Some(&allowed),
            Arc::clone(&self.registry),
            effective_steps,
            provider_config
                .effective_context_length()
                .or(Some(DEFAULT_CONTEXT_LENGTH)),
            &sink,
            None,
            Some(SUBAGENT_TOOL_OUTPUT_BUDGET),
            self.config.agent.retry_attempts,
            self.config.agent.retry_delay_secs,
        )
        .await?;
        sink.flush_transcript();
        finalize_subagent_output(output, effective_steps)
    }
}

/// 子代理最终结果守卫：每个结束的子代理都必须带非空、有意义的结果内容。
///
/// - 非空 final → 原样返回（若达到步数上限或死循环则前置说明前缀）。
/// - 空 final 但有工具轨迹 → 合成摘要（截 1500 字符），完成的工作不丢失。
/// - 二者皆空 → 显式错误（内容非空）。
fn finalize_subagent_output(output: AgentRunOutput, max_steps: u32) -> Result<SubagentTaskOutput> {
    const MAX_SUMMARY_CHARS: usize = 1500;
    let mut text = if output.final_text.trim().is_empty() {
        if !output.tool_trail.is_empty() {
            let summary = format!(
                "subagent completed tool work without a final summary. Tool activity:\n{}",
                output.tool_trail.join("\n")
            );
            crate::background::truncate_chars(&summary, MAX_SUMMARY_CHARS)
        } else {
            return Err(AgentError::Provider(
                "subagent produced no output and executed no tools".into(),
            ));
        }
    } else {
        output.final_text
    };

    match output.exit_reason {
        AgentExitReason::Finished => {}
        AgentExitReason::MaxStepsReached => {
            text = format!("[阶段性总结：已达到步数上限 {max_steps}]\n\n{text}");
        }
        AgentExitReason::LoopDetected => {
            text = format!("[执行中断：检测到重复工具死循环]\n\n{text}");
        }
    }

    Ok(SubagentTaskOutput {
        output: text,
        exit_reason: output.exit_reason,
    })
}

/// 连续相同工具调用检测器：记录每轮工具调用指纹，连续 `threshold` 轮相同则判定死循环。
///
/// 指纹 = 本轮所有 ToolCall 的 `name|arguments` 排序后拼接（排序消除顺序差异）。
/// 不同参数的同名工具不算重复（`read_file({"path":"a"})` vs `read_file({"path":"b"})` 指纹不同）。
/// 单轮多个工具调用时，整组指纹参与比较——只要任一个参数变了就不算重复。
struct LoopDetector {
    last_fingerprint: Option<String>,
    repeat_count: u32,
    threshold: u32,
    /// 记录本轮 agent turn 中所有出现过的指纹（用于检测非连续重复）。
    seen_fingerprints: std::collections::HashSet<String>,
    /// 非连续重复是否已提醒过（避免每轮都提醒）。
    warned: bool,
}

impl LoopDetector {
    /// `threshold` = 连续多少轮相同触发（通常 3）。
    fn new(threshold: u32) -> Self {
        Self {
            last_fingerprint: None,
            repeat_count: 0,
            threshold: threshold.max(1),
            seen_fingerprints: std::collections::HashSet::new(),
            warned: false,
        }
    }

    /// 记录本轮工具调用，返回 `true` 表示连续 `threshold` 轮指纹相同（死循环）。
    /// `repeated` 为 true 表示本轮指纹之前出现过（非连续重复），调用方应注入提醒。
    fn observe(&mut self, calls: &BTreeMap<u32, ToolCall>) -> (bool, bool) {
        let fp = fingerprint(calls);
        let is_consecutive_dup = Some(&fp) == self.last_fingerprint.as_ref();
        let is_non_consecutive_dup = !is_consecutive_dup && self.seen_fingerprints.contains(&fp);

        if is_consecutive_dup {
            self.repeat_count += 1;
        } else {
            self.repeat_count = 1;
            self.last_fingerprint = Some(fp.clone());
        }
        self.seen_fingerprints.insert(fp);

        let loop_triggered = self.repeat_count >= self.threshold;
        let should_warn = is_non_consecutive_dup && !self.warned;
        if should_warn {
            self.warned = true;
        }
        (loop_triggered, should_warn)
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
pub type SteeringReceiver = tokio::sync::mpsc::UnboundedReceiver<String>;
pub type SteeringSender = tokio::sync::mpsc::UnboundedSender<String>;

pub fn steering_channel() -> (SteeringSender, SteeringReceiver) {
    tokio::sync::mpsc::unbounded_channel()
}

fn drain_steering(
    steering: &mut Option<SteeringReceiver>,
    messages: &mut Vec<Message>,
    sink: &EventSink<'_>,
) -> bool {
    let mut received_any = false;
    if let Some(rx) = steering.as_mut() {
        while let Ok(text) = rx.try_recv() {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                messages.push(Message::user(trimmed.to_string()));
                sink.send(AgentEvent::SteeringReceived(trimmed.to_string()));
                received_any = true;
            }
        }
    }
    received_any
}

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
    intensity: ThinkingIntensity,
    memory: String,
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
        intensity,
        &memory,
        None,
        None,
        None,
    )
    .await;
    if let Err(e) = res {
        warn!(error = %e, "agent run_stream 失败");
        let _ = tx.send((gen, AgentEvent::Error(e.to_string())));
    }
}

/// Same event sequence as `run_stream`, with mandatory pre-execution approval.
/// A closed approval channel or a missing response denies the tool call.
/// Optional `steering` channel allows appending instructions during thinking/execution.
#[allow(clippy::too_many_arguments)]
pub async fn run_stream_with_permissions(
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
    intensity: ThinkingIntensity,
    memory: String,
    permissions: Arc<crate::permission::PermissionBroker>,
    steering: Option<SteeringReceiver>,
    subagent_archive: Option<Arc<crate::subagent::SubagentArchive>>,
    background: Option<Arc<crate::background::BackgroundRegistry>>,
) {
    let _ = tx.send((gen, AgentEvent::Started));
    let registry = Arc::new(ToolRegistry::with_permissions(registry, permissions));
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
        intensity,
        &memory,
        steering,
        subagent_archive,
        background,
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
    intensity: ThinkingIntensity,
    memory: &str,
    steering: Option<SteeringReceiver>,
    subagent_archive: Option<Arc<crate::subagent::SubagentArchive>>,
    background: Option<Arc<crate::background::BackgroundRegistry>>,
) -> Result<()> {
    let name = &config.agent.default_provider;
    let cfg = providers.providers.get(name).ok_or_else(|| {
        AgentError::Provider(format!(
            "default_provider '{name}' 未在 providers.toml 配置"
        ))
    })?;
    debug!(provider = %name, kind = %cfg.kind, mock, gen, "启动 agent loop");
    let provider = provider_factory(cfg, mock)?;
    let mut system = build_system_prompt(
        project,
        intensity.resolve(ctf_enabled),
        &skill_summaries(&registry.all_schemas()),
        memory,
    );
    if ctf_enabled {
        system.push_str(CTF_PROMPT);
    }

    let rules = project.map(|p| p.rules().to_vec()).unwrap_or_default();
    let scope = project.and_then(|p| p.frontmatter.scope.clone());
    let env = config
        .env
        .vars
        .iter()
        .map(|v| (v.key.clone(), v.value.clone()))
        .collect();
    let runtime = Arc::new(SubagentRuntime::new(
        config.clone(),
        providers.clone(),
        project.cloned(),
        mock,
        cwd.clone(),
        Arc::clone(&registry),
        ctf_enabled,
        intensity,
        memory.to_string(),
        tx.clone(),
        gen,
        subagent_archive.clone(),
        background.clone(),
    ));
    let ctx = ToolCtx::new(cwd.clone(), rules, scope, env)
        .with_subagent_runtime(runtime.clone())
        .with_subagent_archive(runtime.archive().cloned())
        .with_background(runtime.background().cloned())
        .with_provider_config(Some(cfg.clone()))
        .with_mock(mock);
    let mut tools = if config.agent.auto_tool_call && !registry.is_empty() {
        registry.schemas()
    } else {
        Vec::new()
    };
    if !config.tools.web_search {
        tools.retain(|s| s.name != "web_fetch");
    }
    let mut messages = history;
    let (resolved_text, images) =
        match crate::vision::resolve_prompt_placeholders(&user_input, &[], &cwd) {
            Ok((text, imgs)) => (text, imgs),
            Err(err) => {
                warn!(error = %err, "解析图片占位符失败，回退为普通文本");
                (user_input.clone(), Vec::new())
            }
        };
    if !images.is_empty() {
        if mock {
            messages.push(Message::user_with_images(resolved_text, images));
        } else {
            let mut cap = cyber_core::get_model_vision_capability(cfg, name, &cfg.model, None);
            if cap.is_unknown() {
                let _ = tx.send((
                    gen,
                    AgentEvent::Reasoning(format!(
                        "🔍 [视觉探针] 正在动态探测模型 [{}] 的多模态识图能力...\n",
                        cfg.model
                    )),
                ));
                match crate::vision::probe_model_vision(cfg, &cfg.model).await {
                    Ok(probed) => {
                        cap = probed;
                        let _ = cyber_core::save_model_vision_capability(
                            name, &cfg.model, probed, None,
                        );
                        info!(
                            model = %cfg.model,
                            capability = ?probed,
                            "动态模型视觉能力探测成功并已缓存"
                        );
                    }
                    Err(e) => {
                        warn!(
                            model = %cfg.model,
                            error = %e,
                            "动态模型视觉能力探测失败，按未探测处理"
                        );
                    }
                }
            }

            if cap.is_supported() {
                messages.push(Message::user_with_images(resolved_text, images));
            } else {
                let _ = tx.send((
                    gen,
                    AgentEvent::Reasoning(format!(
                        "👁️ [识图引擎] 当前模型 [{}] 不支持原生图像输入，正在调用识图引擎分析 {} 张图片...\n",
                        cfg.model,
                        images.len()
                    )),
                ));
                let engine = crate::vision::VisionEngine::new(config.agent.vision.clone());
                match engine.describe_images(providers, &images).await {
                    Ok(descriptions) => {
                        let injected = crate::vision::format_injected_vision_description(
                            &resolved_text,
                            &descriptions,
                        );
                        messages.push(Message::user(injected));
                    }
                    Err(err) => {
                        warn!(error = %err, "识图引擎解析图片失败，回退文本注入错误提示");
                        let fallback = format!(
                            "{resolved_text}\n\n[提示: 当前模型不支持视觉多模态，且识图引擎解析失败: {err}]"
                        );
                        messages.push(Message::user(fallback));
                    }
                }
            }
        }
    } else {
        messages.push(Message::user(user_input));
    }
    let sink = EventSink::parent(tx, gen);
    run_agent_loop(
        provider.as_ref(),
        system,
        messages,
        ctx,
        tools,
        None,
        registry,
        config.agent.max_steps.max(1),
        cfg.effective_context_length()
            .or(Some(DEFAULT_CONTEXT_LENGTH)),
        &sink,
        steering,
        None,
        config.agent.retry_attempts,
        config.agent.retry_delay_secs,
    )
    .await?;
    Ok(())
}

struct AgentRunOutput {
    final_text: String,
    /// 每次工具执行的一行摘要（`工具名: 结果前 200 字符`），供空 final 时合成兜底结果。
    tool_trail: Vec<String>,
    exit_reason: AgentExitReason,
}

#[derive(Clone, Copy)]
enum EventMode {
    Parent,
    Child,
}

struct EventSink<'a> {
    tx: &'a UnboundedSender<(u64, AgentEvent)>,
    gen: u64,
    mode: EventMode,
    task_progress: Option<&'a UnboundedSender<String>>,
    /// Child 模式的转录写句柄（子代理面板数据源）。
    transcript: Option<crate::subagent::TranscriptWriter>,
}

impl<'a> EventSink<'a> {
    fn parent(tx: &'a UnboundedSender<(u64, AgentEvent)>, gen: u64) -> Self {
        Self {
            tx,
            gen,
            mode: EventMode::Parent,
            task_progress: None,
            transcript: None,
        }
    }

    #[cfg(test)]
    fn child(tx: &'a UnboundedSender<(u64, AgentEvent)>, gen: u64) -> Self {
        Self {
            tx,
            gen,
            mode: EventMode::Child,
            task_progress: None,
            transcript: None,
        }
    }

    /// Child 模式 + 转录：除进度转发外把子代理事件写入 `SubagentRun.lines`。
    fn child_with_transcript(
        tx: &'a UnboundedSender<(u64, AgentEvent)>,
        gen: u64,
        task_progress: Option<&'a UnboundedSender<String>>,
        transcript: Option<&Arc<std::sync::Mutex<crate::subagent::SubagentRun>>>,
    ) -> Self {
        Self {
            tx,
            gen,
            mode: EventMode::Child,
            task_progress,
            transcript: transcript
                .map(|run| crate::subagent::TranscriptWriter::new(Arc::clone(run))),
        }
    }

    /// 定稿时 flush token 缓冲中剩余的最后一行（无换行结尾的 final 文本）。
    fn flush_transcript(&self) {
        if let Some(transcript) = &self.transcript {
            transcript.flush();
        }
    }

    fn send(&self, event: AgentEvent) -> bool {
        if matches!(self.mode, EventMode::Child) {
            if let Some(tp) = self.task_progress {
                match &event {
                    AgentEvent::ToolCall { name, .. } => {
                        let _ = tp.send(format!("running tool: {name}\n"));
                    }
                    AgentEvent::ToolResult { name, is_error, .. } => {
                        if *is_error {
                            let _ = tp.send(format!("tool {name} failed\n"));
                        } else {
                            let _ = tp.send(format!("tool {name} completed\n"));
                        }
                    }
                    AgentEvent::Retry {
                        attempt,
                        max_retries,
                        delay_secs,
                        error,
                    } => {
                        let _ = tp.send(format!(
                            "retry {attempt}/{max_retries} ({delay_secs}s): {error}\n"
                        ));
                    }
                    _ => {}
                }
            }
            if let Some(transcript) = &self.transcript {
                match &event {
                    // 推理流按缓冲聚合（事件边界可能切在词/数字中间，逐行会丢空格）
                    AgentEvent::Reasoning(text) => transcript.reasoning(text),
                    AgentEvent::Token(text) => transcript.token(text),
                    AgentEvent::ToolCall {
                        name, arguments, ..
                    } => {
                        transcript.line(format!(
                            "tool {name} args: {}",
                            crate::background::truncate_chars(arguments, 200)
                        ));
                    }
                    AgentEvent::ToolResult { name, output, .. } => {
                        transcript.line(format!(
                            "{name} => {}",
                            crate::background::truncate_chars(output, 300)
                        ));
                    }
                    AgentEvent::Retry {
                        attempt,
                        max_retries,
                        delay_secs,
                        error,
                    } => {
                        transcript.line(format!(
                            "retry {attempt}/{max_retries} ({delay_secs}s): {error}"
                        ));
                    }
                    _ => {}
                }
            }
            if !matches!(event, AgentEvent::Usage(_)) {
                return true;
            }
        }
        self.tx.send((self.gen, event)).is_ok()
    }
}

fn skill_summaries(schemas: &[crate::tool::ToolSchema]) -> Vec<SkillSummary> {
    schemas
        .iter()
        .filter(|schema| schema.name.starts_with("skill_"))
        .filter(|schema| !schema.description.contains("仅显式调用"))
        .map(|schema| SkillSummary {
            name: schema
                .name
                .strip_prefix("skill_")
                .unwrap_or(&schema.name)
                .to_string(),
            description: schema
                .description
                .strip_prefix("[Skill] ")
                .unwrap_or(&schema.description)
                .lines()
                .next()
                .unwrap_or("")
                .to_string(),
        })
        .collect()
}

/// 子代理工具结果进入消息历史的最大字符数：单条 shell 递归输出无法再撑爆
/// 上下文窗口（主 agent 不截断，传 None）。
const SUBAGENT_TOOL_OUTPUT_BUDGET: usize = 6_000;

#[allow(clippy::too_many_arguments)]
async fn run_agent_loop(
    provider: &dyn Provider,
    system: String,
    mut messages: Vec<Message>,
    ctx: ToolCtx,
    tools: Vec<crate::tool::ToolSchema>,
    allowed_tools: Option<&std::collections::HashSet<String>>,
    registry: Arc<ToolRegistry>,
    max_steps: u32,
    effective_ctx_len: Option<u32>,
    sink: &EventSink<'_>,
    mut steering: Option<SteeringReceiver>,
    tool_output_budget: Option<usize>,
    retry_attempts: u32,
    retry_delay_secs: u64,
) -> Result<AgentRunOutput> {
    emit_context_update(sink, &messages, effective_ctx_len);
    let mut detector = LoopDetector::new(3);
    let mut loop_detected = false;
    let mut nudged = false;
    let mut answer_text = String::new();
    let mut empty_truncation_rounds: u32 = 0;
    let mut empty_answer_nudged = false;
    let is_subagent = allowed_tools.is_some();
    let mut tool_trail: Vec<String> = Vec::new();
    for step in 0..max_steps {
        debug!(step, gen = sink.gen, "agent loop 迭代");
        drain_steering(&mut steering, &mut messages, sink);
        if let Some(threshold) = auto_compact_threshold(effective_ctx_len) {
            let used = estimate_messages_tokens(&messages);
            if used >= threshold as usize {
                if let Err(error) = do_compact(
                    provider,
                    &system,
                    &mut messages,
                    None,
                    sink,
                    true,
                    retry_attempts,
                    retry_delay_secs,
                )
                .await
                {
                    warn!(error = %error, gen = sink.gen, "自动压缩失败，回退到原消息继续");
                }
            }
        }

        let accumulation = accumulate_stream_with_retry(
            provider,
            &StreamRequest::new(messages.clone())
                .with_system(system.clone())
                .with_tools(tools.clone()),
            sink,
            retry_attempts,
            retry_delay_secs,
        )
        .await?;
        if let Some(usage) = accumulation.usage.as_ref() {
            sink.send(AgentEvent::Usage(usage.clone()));
        }
        if accumulation.calls.is_empty() {
            let truncated = accumulation.truncated.is_some();
            let round_text = accumulation.text;
            answer_text.push_str(&round_text);
            if !round_text.is_empty() {
                messages.push(Message::assistant(round_text.clone()));
            }
            let has_steer = drain_steering(&mut steering, &mut messages, sink);
            if has_steer && step + 1 < max_steps {
                // 用户追加了新指示 → 新一轮答复取代本轮正文（维持「final_text = 最后一轮」语义）。
                answer_text.clear();
                continue;
            }
            if is_subagent
                && !tools.is_empty()
                && tool_trail.is_empty()
                && !nudged
                && step + 1 < max_steps
            {
                nudged = true;
                messages.push(Message::user(
                    "（系统提醒：你是一个自主运行的子代理，输出纯文本将永久结束你的执行，没有交互用户为你补充反馈。如果你还有未执行的步骤、文件修改或命令验证，请立即在当前轮次调用相应工具执行；若目标已切实完成或客观无法继续，请直接总结说明。）".to_string(),
                ));
                answer_text.clear();
                continue;
            }
            // 截断 / 空正文自动续写：继续请求，直到产出正文或判定无法再取得进展。
            if step + 1 < max_steps {
                if truncated {
                    empty_truncation_rounds = if round_text.is_empty() {
                        empty_truncation_rounds + 1
                    } else {
                        0
                    };
                    if empty_truncation_rounds < MAX_EMPTY_TRUNCATION_ROUNDS {
                        messages.push(Message::user(TRUNCATION_CONTINUE_NUDGE.to_string()));
                        continue;
                    }
                } else if answer_text.trim().is_empty() && !empty_answer_nudged {
                    empty_answer_nudged = true;
                    messages.push(Message::user(EMPTY_ANSWER_NUDGE.to_string()));
                    continue;
                }
            }
            emit_context_update(sink, &messages, effective_ctx_len);
            if truncated || answer_text.trim().is_empty() {
                sink.send(AgentEvent::Notice(truncation_notice(
                    &answer_text,
                    truncated,
                )));
            }
            sink.send(AgentEvent::Done);
            return Ok(AgentRunOutput {
                final_text: answer_text,
                tool_trail,
                exit_reason: AgentExitReason::Finished,
            });
        }
        answer_text.clear();

        let calls = accumulation.calls;
        let history_calls = sanitized_tool_calls(&calls);
        let mut assistant_msg = Message::assistant(accumulation.text);
        assistant_msg.tool_calls = history_calls.values().cloned().collect();
        messages.push(assistant_msg);

        for (index, call) in &calls {
            let raw_args = call.arguments.trim();
            let parsed_input = if raw_args.is_empty() {
                Some(Value::Object(serde_json::Map::new()))
            } else {
                match serde_json::from_str::<Value>(raw_args) {
                    Ok(Value::Object(input)) => Some(Value::Object(input)),
                    Ok(other) => {
                        warn!(
                            gen = sink.gen,
                            tool = %call.name,
                            call_id = %call.id,
                            arguments = %truncate_for_log(raw_args),
                            json_type = %json_type_name(&other),
                            "工具调用参数不是 JSON 对象，拒绝执行"
                        );
                        None
                    }
                    Err(error) => {
                        warn!(
                            gen = sink.gen,
                            tool = %call.name,
                            call_id = %call.id,
                            arguments = %truncate_for_log(raw_args),
                            error = %error,
                            "工具调用参数不是合法 JSON，拒绝执行"
                        );
                        None
                    }
                }
            };
            sink.send(AgentEvent::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: history_calls
                    .get(index)
                    .map(|item| item.arguments.clone())
                    .unwrap_or_else(|| "{}".into()),
            });

            let out = if allowed_tools.is_some_and(|allowed| {
                call.name == "delegate_tasks" || !allowed.contains(&call.name)
            }) {
                ToolOutput {
                    content: format!("Tool '{}' is not allowed for this subagent", call.name),
                    is_error: true,
                }
            } else if let Some(input) = parsed_input {
                let (progress_tx, mut progress_rx) =
                    tokio::sync::mpsc::unbounded_channel::<String>();
                let mut exec = Box::pin(registry.execute_streaming(
                    &call.name,
                    input,
                    &ctx,
                    Some(progress_tx),
                ));
                loop {
                    tokio::select! {
                        biased;
                        result = exec.as_mut() => {
                            break match result {
                                Ok(output) => output,
                                Err(error) => ToolOutput {
                                    content: error.to_string(),
                                    is_error: true,
                                },
                            };
                        }
                        chunk = progress_rx.recv() => {
                            if let Some(chunk) = chunk {
                                sink.send(AgentEvent::ToolProgress {
                                    id: call.id.clone(),
                                    name: call.name.clone(),
                                    chunk,
                                });
                            }
                        }
                    }
                }
            } else {
                ToolOutput {
                    content:
                        "工具调用参数不是合法 JSON 对象，未执行工具；请重新发送完整 JSON 参数。"
                            .into(),
                    is_error: true,
                }
            };
            tool_trail.push(format!(
                "{}: {}",
                call.name,
                crate::background::truncate_chars(&out.content, 200)
            ));
            sink.send(AgentEvent::ToolResult {
                id: call.id.clone(),
                name: call.name.clone(),
                output: out.content.clone(),
                is_error: out.is_error,
            });
            // 子代理（budget=Some）工具结果进入消息历史前截断：控制上下文增长。
            let history_content = match tool_output_budget {
                Some(budget) if out.content.chars().count() > budget => {
                    let total = out.content.chars().count();
                    let mut content = crate::background::truncate_chars(&out.content, budget);
                    content.push_str(&format!(
                        "\n\n（工具输出共 {total} 字符，已截断至前 {budget} 字符；如需完整内容请用更聚焦的命令重查。）"
                    ));
                    content
                }
                _ => out.content,
            };
            messages.push(Message::tool(call.id.clone(), history_content));
        }
        drain_steering(&mut steering, &mut messages, sink);

        emit_context_update(sink, &messages, effective_ctx_len);
        let (loop_triggered, should_warn) = detector.observe(&calls);
        if should_warn {
            messages.push(Message::user(
                "（系统提醒：你之前已经执行过相同的工具调用，请回顾上方对话历史中的结果，不要重复操作。如果之前的尝试没有成功，请换一个不同的策略。）".to_string(),
            ));
        }
        if loop_triggered {
            loop_detected = true;
            break;
        }
    }

    let (wrap, exit_reason) = if loop_detected {
        (
            "（系统提示：检测到连续多次相同的工具调用，可能已陷入循环。请根据已收集的信息直接给出最终回答或阶段性结论，不要再调用工具。）".to_string(),
            AgentExitReason::LoopDetected,
        )
    } else {
        (
            format!(
                "（系统提示：已达到本次任务的步数上限（最大步数限制 {max_steps}）。请立即停止工具操作，并对当前任务进行完整的收敛总结：1. 已完成的具体工作与关键发现；2. 尚未完成的部分与阻碍原因；3. 当前系统/文件的最终状态；4. 后续建议执行的步骤。如实说明现状，严禁谎称任务已全部完成。）"
            ),
            AgentExitReason::MaxStepsReached,
        )
    };
    messages.push(Message::user(wrap));
    drain_steering(&mut steering, &mut messages, sink);
    let accumulation = accumulate_stream_with_retry(
        provider,
        &StreamRequest::new(messages.clone())
            .with_system(system)
            .with_tools(Vec::new()),
        sink,
        retry_attempts,
        retry_delay_secs,
    )
    .await?;
    if let Some(usage) = accumulation.usage.as_ref() {
        sink.send(AgentEvent::Usage(usage.clone()));
    }
    let final_text = accumulation.text;
    if !final_text.is_empty() {
        messages.push(Message::assistant(final_text.clone()));
    }
    emit_context_update(sink, &messages, effective_ctx_len);
    sink.send(AgentEvent::Done);
    Ok(AgentRunOutput {
        final_text,
        tool_trail,
        exit_reason,
    })
}

/// 发送上下文使用情况更新事件（TUI 据此显示剩余百分比）。
/// `effective_ctx_len` 为 None 时不发送（TUI 不显示百分比）。
fn emit_context_update(sink: &EventSink<'_>, messages: &[Message], effective_ctx_len: Option<u32>) {
    if effective_ctx_len.is_none() {
        return;
    }
    sink.send(AgentEvent::ContextUpdate {
        used_tokens: estimate_messages_tokens(messages),
        effective_context_length: effective_ctx_len,
    });
}

/// 执行上下文压缩：发 Compacting 事件 → 调用 compact_messages 替换 messages → 发 Compacted 事件。
/// `is_auto` 区分自动触发（达到阈值）与手动 `/compact`。
/// 压缩成功后 `messages` 将被替换为 `[摘要 user 消息]`。
#[allow(clippy::too_many_arguments)]
async fn do_compact(
    provider: &dyn Provider,
    system: &str,
    messages: &mut Vec<Message>,
    custom_instructions: Option<&str>,
    sink: &EventSink<'_>,
    is_auto: bool,
    retry_attempts: u32,
    retry_delay_secs: u64,
) -> Result<()> {
    let before_tokens = estimate_messages_tokens(messages);
    sink.send(AgentEvent::Compacting { is_auto });
    let summary_msg = compact_messages_with_retry(
        provider,
        system,
        messages,
        custom_instructions,
        retry_attempts,
        retry_delay_secs,
    )
    .await?;
    let after_tokens = estimate_messages_tokens(std::slice::from_ref(&summary_msg));
    *messages = vec![summary_msg.clone()];
    sink.send(AgentEvent::Compacted {
        summary: summary_msg.content,
        before_tokens,
        after_tokens,
    });
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
        AgentError::Provider(format!(
            "default_provider '{name}' 未在 providers.toml 配置"
        ))
    })?;
    let provider = provider_factory(cfg, mock)?;

    // writeup 是纯文本生成任务（不暴露工具），但 DeepSeek 等模型在 system 提示
    // 中出现「查看 exp 文件」等描述时，可能自发输出 `<function_calls>` 文本而非
    // 正文。追加一段硬约束，明确禁止工具调用并直接输出 Markdown。
    let system = format!(
        "{}\n\n# 输出约束（必须遵守）\n\
- 这是纯文本撰写任务，你没有可调用的工具，**禁止**输出任何工具调用格式（如 <function_calls>、<invoke>、<parameter>、tool_use 等）。\n\
- 直接输出完整的 Markdown writeup 正文，从标题开始，不要输出任何与正文无关的说明。",
        skill_body
    );
    let req = StreamRequest::new(vec![Message::user(challenge_context)]).with_system(system);
    let sink = EventSink::parent(tx, gen);
    let accumulation = accumulate_stream_with_retry(
        provider.as_ref(),
        &req,
        &sink,
        config.agent.retry_attempts,
        config.agent.retry_delay_secs,
    )
    .await?;
    if let Some(u) = accumulation.usage {
        let _ = tx.send((gen, AgentEvent::Usage(u)));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
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
        AgentError::Provider(format!(
            "default_provider '{name}' 未在 providers.toml 配置"
        ))
    })?;
    let provider = provider_factory(cfg, mock)?;
    let system = build_system_prompt(project, ThinkingIntensity::Middle, &[], "");

    if history.is_empty() {
        return Err(AgentError::Provider("无消息可压缩".into()));
    }

    let mut messages = history;
    let sink = EventSink::parent(tx, gen);
    do_compact(
        provider.as_ref(),
        &system,
        &mut messages,
        custom_instructions.as_deref(),
        &sink,
        false,
        config.agent.retry_attempts,
        config.agent.retry_delay_secs,
    )
    .await
}

#[derive(Debug)]
struct StreamAccumulation {
    text: String,
    calls: BTreeMap<u32, ToolCall>,
    usage: Option<Usage>,
    truncated: Option<String>,
}

/// 驱动一个流到结束，累积文本、工具调用和 usage。
async fn accumulate_stream(
    stream: &mut (impl futures::Stream<Item = StreamEvent> + Unpin),
    sink: &EventSink<'_>,
) -> Result<StreamAccumulation> {
    let mut text = String::new();
    let mut calls: BTreeMap<u32, ToolCall> = BTreeMap::new();
    let mut usage: Option<Usage> = None;
    let mut truncated: Option<String> = None;

    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::Delta(token) => {
                text.push_str(&token);
                if !sink.send(AgentEvent::Token(token)) {
                    debug!("TUI 通道已关闭，agent 累积终止");
                    break;
                }
            }
            StreamEvent::Reasoning(reasoning) => {
                if !sink.send(AgentEvent::Reasoning(reasoning)) {
                    debug!("TUI 通道已关闭，agent 累积终止");
                    break;
                }
            }
            StreamEvent::ToolCallDelta(delta) => {
                accumulate_tool_delta(&mut calls, delta);
            }
            StreamEvent::Usage(value) => {
                usage = Some(value);
            }
            StreamEvent::Truncated(reason) => {
                truncated = Some(reason);
            }
            StreamEvent::Done => break,
            StreamEvent::Error(message) => return Err(AgentError::Provider(message)),
        }
    }
    Ok(StreamAccumulation {
        text,
        calls,
        usage,
        truncated,
    })
}

/// 判断错误信息是否属于临时性异常（可安全重试）。
/// 致命客户端权限/密钥配置错误立即失败不空耗重试；
/// 网络故障、超时、流中断、5xx 网关错误或 429 窗口限频均可重试。
pub fn is_retryable_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("401 unauthorized")
        || lower.contains("401")
        || lower.contains("invalid api key")
        || lower.contains("invalid_api_key")
        || lower.contains("authentication")
        || lower.contains("403 forbidden")
        || lower.contains("403")
        || lower.contains("404 not found")
        || lower.contains("404")
        || (lower.contains("400 bad request") && !lower.contains("timeout"))
    {
        return false;
    }
    true
}

/// 带自动重试与回滚通知的流式累积执行体。
async fn accumulate_stream_with_retry(
    provider: &dyn Provider,
    req: &StreamRequest,
    sink: &EventSink<'_>,
    max_retries: u32,
    delay_secs: u64,
) -> Result<StreamAccumulation> {
    let mut attempt = 0;
    loop {
        let mut stream = provider.stream(req.clone());
        match accumulate_stream(&mut stream, sink).await {
            Ok(accumulation) => return Ok(accumulation),
            Err(err) => {
                let err_msg = err.to_string();
                if attempt >= max_retries || !is_retryable_error(&err_msg) {
                    return Err(err);
                }
                attempt += 1;
                sink.send(AgentEvent::Retry {
                    attempt,
                    max_retries,
                    delay_secs,
                    error: err_msg,
                });
                if sink.tx.is_closed() {
                    return Err(AgentError::Provider("任务已取消".into()));
                }
                tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
            }
        }
    }
}

/// 将工具调用参数规范化后写入 assistant 历史，避免非法 JSON 被 provider 拒绝。
fn sanitized_tool_calls(calls: &BTreeMap<u32, ToolCall>) -> BTreeMap<u32, ToolCall> {
    calls
        .iter()
        .map(|(index, call)| {
            let arguments = match serde_json::from_str::<Value>(call.arguments.trim()) {
                Ok(Value::Object(_)) => call.arguments.clone(),
                _ => "{}".to_string(),
            };
            (
                *index,
                ToolCall {
                    arguments,
                    ..call.clone()
                },
            )
        })
        .collect()
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn truncate_for_log(value: &str) -> String {
    const MAX: usize = 512;
    let mut end = value.len().min(MAX);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = value[..end].to_string();
    if end < value.len() {
        result.push_str("...");
    }
    result
}

/// 把一个 `ToolCallDelta` 片段合并进 `calls` 累积器（按 index）。
/// 首片带 id+name 时初始化；后续片段只 append arguments_fragment。
fn accumulate_tool_delta(calls: &mut BTreeMap<u32, ToolCall>, d: ToolCallDelta) {
    let entry = calls.entry(d.index).or_insert_with(|| ToolCall {
        id: String::new(),
        name: String::new(),
        arguments: String::new(),
    });
    if entry.id.is_empty() {
        if let Some(id) = d.id {
            if !id.is_empty() {
                entry.id = id;
            }
        }
    }
    if entry.name.is_empty() {
        if let Some(name) = d.name {
            if !name.is_empty() {
                entry.name = name;
            }
        }
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
    fn sanitized_tool_calls_replaces_invalid_arguments() {
        let mut calls = BTreeMap::new();
        calls.insert(
            0,
            ToolCall {
                id: "ok".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"a.txt"}"#.into(),
            },
        );
        calls.insert(
            1,
            ToolCall {
                id: "bad".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"a.txt""#.into(),
            },
        );
        calls.insert(
            2,
            ToolCall {
                id: "array".into(),
                name: "read_file".into(),
                arguments: "[]".into(),
            },
        );
        let sanitized = sanitized_tool_calls(&calls);
        assert_eq!(sanitized[&0].arguments, r#"{"path":"a.txt"}"#);
        assert_eq!(sanitized[&1].arguments, "{}");
        assert_eq!(sanitized[&2].arguments, "{}");
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
        let sink = EventSink::parent(&tx, 0);
        let accumulation = accumulate_stream(&mut s, &sink).await.unwrap();
        assert_eq!(accumulation.text, "Hello world");
        assert!(accumulation.calls.is_empty());
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
        let sink = EventSink::parent(&tx, 0);
        let accumulation = accumulate_stream(&mut s, &sink).await.unwrap();
        assert!(accumulation.text.is_empty());
        assert_eq!(accumulation.calls.len(), 1);
        assert_eq!(accumulation.calls[&0].name, "list_dir");
        assert_eq!(accumulation.calls[&0].arguments, "{\"path\":\".\"}");
    }

    #[tokio::test]
    async fn provider_stream_error_does_not_emit_done() {
        use futures::stream;
        let mut stream = stream::iter(vec![StreamEvent::Error("boom".into())]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u64, AgentEvent)>();
        let sink = EventSink::parent(&tx, 7);
        let error = accumulate_stream(&mut stream, &sink).await.unwrap_err();
        assert!(matches!(error, AgentError::Provider(message) if message == "boom"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn delegate_tasks_disallowed_tool_is_never_executed() {
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use futures::{stream, Stream};

        struct ScriptProvider {
            calls: AtomicUsize,
        }

        impl Provider for ScriptProvider {
            fn stream(
                &self,
                _req: StreamRequest,
            ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Box::pin(stream::iter(vec![
                        StreamEvent::ToolCallDelta(ToolCallDelta {
                            index: 0,
                            id: Some("blocked-call".into()),
                            name: Some("blocked".into()),
                            arguments_fragment: "{}".into(),
                        }),
                        StreamEvent::Done,
                    ]))
                } else {
                    Box::pin(stream::iter(vec![
                        StreamEvent::Delta("done".into()),
                        StreamEvent::Done,
                    ]))
                }
            }
        }

        struct CountingTool(Arc<AtomicUsize>);

        impl crate::Tool for CountingTool {
            fn schema(&self) -> crate::ToolSchema {
                crate::ToolSchema {
                    name: "blocked".into(),
                    description: "must not execute".into(),
                    parameters: serde_json::json!({"type": "object"}),
                    tags: Vec::new(),
                }
            }

            fn run<'a>(
                &'a self,
                _input: Value,
                _ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
                Box::pin(async move {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    Ok(ToolOutput {
                        content: "executed".into(),
                        is_error: false,
                    })
                })
            }
        }

        let executions = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(CountingTool(Arc::clone(&executions))));
        let registry = Arc::new(registry);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = EventSink::child(&tx, 1);
        let allowed = std::collections::HashSet::new();
        let output = run_agent_loop(
            &ScriptProvider {
                calls: AtomicUsize::new(0),
            },
            String::new(),
            vec![Message::user("test")],
            ToolCtx::new(std::env::temp_dir(), Vec::new(), None, Vec::new()),
            registry.schemas(),
            Some(&allowed),
            registry,
            2,
            Some(DEFAULT_CONTEXT_LENGTH),
            &sink,
            None,
            None,
            0,
            0,
        )
        .await
        .unwrap();
        assert_eq!(output.final_text, "done");
        assert_eq!(executions.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn finalize_subagent_output_passes_through_nonempty_final() {
        let output = AgentRunOutput {
            final_text: "  结论：发现 2 个漏洞  ".into(),
            tool_trail: vec!["shell: nmap 输出".into()],
            exit_reason: AgentExitReason::Finished,
        };
        let result = finalize_subagent_output(output, 25).unwrap();
        assert_eq!(result.output, "  结论：发现 2 个漏洞  ");
        assert_eq!(result.exit_reason, AgentExitReason::Finished);
    }

    #[test]
    fn finalize_subagent_output_synthesizes_summary_from_tool_trail() {
        let output = AgentRunOutput {
            final_text: String::new(),
            tool_trail: vec!["counting: TOOL-OUT-42".into()],
            exit_reason: AgentExitReason::Finished,
        };
        let result = finalize_subagent_output(output, 25).unwrap();
        assert!(
            result.output.contains("Tool activity"),
            "应合成摘要：{}",
            result.output
        );
        assert!(
            result.output.contains("TOOL-OUT-42"),
            "应含工具轨迹：{}",
            result.output
        );
        assert!(result.output.contains("counting"));
        assert_eq!(result.exit_reason, AgentExitReason::Finished);
    }

    #[test]
    fn finalize_subagent_output_errors_when_no_text_and_no_tools() {
        let output = AgentRunOutput {
            final_text: "   \n".into(),
            tool_trail: Vec::new(),
            exit_reason: AgentExitReason::Finished,
        };
        let error = finalize_subagent_output(output, 25).unwrap_err();
        assert!(
            matches!(&error, AgentError::Provider(m) if m == "subagent produced no output and executed no tools"),
            "应为显式错误：{error}"
        );
    }

    #[test]
    fn finalize_subagent_output_prepends_step_limit_prefix() {
        let output = AgentRunOutput {
            final_text: "已完成部分扫描".into(),
            tool_trail: vec!["nmap: done".into()],
            exit_reason: AgentExitReason::MaxStepsReached,
        };
        let result = finalize_subagent_output(output, 10).unwrap();
        assert!(result
            .output
            .starts_with("[阶段性总结：已达到步数上限 10]\n\n"));
        assert!(result.output.contains("已完成部分扫描"));
        assert_eq!(result.exit_reason, AgentExitReason::MaxStepsReached);
    }

    #[test]
    fn finalize_subagent_output_prepends_loop_detected_prefix() {
        let output = AgentRunOutput {
            final_text: "死循环中断".into(),
            tool_trail: vec!["ls: a".into()],
            exit_reason: AgentExitReason::LoopDetected,
        };
        let result = finalize_subagent_output(output, 25).unwrap();
        assert!(result
            .output
            .starts_with("[执行中断：检测到重复工具死循环]\n\n"));
        assert_eq!(result.exit_reason, AgentExitReason::LoopDetected);
    }

    #[tokio::test]
    async fn run_agent_loop_collects_tool_trail_when_final_is_empty() {
        use futures::{stream, Stream};
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct ScriptProvider {
            calls: AtomicUsize,
        }

        impl Provider for ScriptProvider {
            fn stream(
                &self,
                _req: StreamRequest,
            ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Box::pin(stream::iter(vec![
                        StreamEvent::ToolCallDelta(ToolCallDelta {
                            index: 0,
                            id: Some("trail-call".into()),
                            name: Some("counting".into()),
                            arguments_fragment: "{}".into(),
                        }),
                        StreamEvent::Done,
                    ]))
                } else {
                    // 第二轮：空 final（模拟模型未产出最终回答）
                    Box::pin(stream::iter(vec![StreamEvent::Done]))
                }
            }
        }

        struct CountingTool(Arc<AtomicUsize>);

        impl crate::Tool for CountingTool {
            fn schema(&self) -> crate::ToolSchema {
                crate::ToolSchema {
                    name: "counting".into(),
                    description: "counting tool".into(),
                    parameters: serde_json::json!({"type": "object"}),
                    tags: Vec::new(),
                }
            }

            fn run<'a>(
                &'a self,
                _input: Value,
                _ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
                Box::pin(async move {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    Ok(ToolOutput {
                        content: "TOOL-OUT-42".into(),
                        is_error: false,
                    })
                })
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(CountingTool(Arc::new(AtomicUsize::new(0)))));
        let registry = Arc::new(registry);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = EventSink::child(&tx, 1);
        let allowed = std::collections::HashSet::from(["counting".to_string()]);
        let output = run_agent_loop(
            &ScriptProvider {
                calls: AtomicUsize::new(0),
            },
            String::new(),
            vec![Message::user("run the tool only")],
            ToolCtx::new(std::env::temp_dir(), Vec::new(), None, Vec::new()),
            registry.schemas(),
            Some(&allowed),
            registry,
            2,
            Some(DEFAULT_CONTEXT_LENGTH),
            &sink,
            None,
            None,
            0,
            0,
        )
        .await
        .unwrap();
        assert!(output.final_text.trim().is_empty());
        assert_eq!(output.tool_trail.len(), 1);
        assert_eq!(output.tool_trail[0], "counting: TOOL-OUT-42");
        // 合成兜底结果非空且含工具产出
        let fallback = finalize_subagent_output(output, 20).unwrap();
        assert!(fallback.output.contains("Tool activity"));
        assert!(fallback.output.contains("TOOL-OUT-42"));
    }

    #[tokio::test]
    async fn run_agent_loop_nudges_on_empty_tool_calls() {
        use futures::{stream, Stream};
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct NudgeProvider {
            calls: AtomicUsize,
        }

        impl Provider for NudgeProvider {
            fn stream(
                &self,
                req: StreamRequest,
            ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
                let call_num = self.calls.fetch_add(1, Ordering::SeqCst);
                if call_num == 0 {
                    // 第一轮：纯文本输出，无工具调用。应触发 nudge。
                    Box::pin(stream::iter(vec![
                        StreamEvent::Delta("我想先分析一下".into()),
                        StreamEvent::Done,
                    ]))
                } else {
                    // 第二轮：必须收到了包含引导提示的消息
                    let last_msg = req
                        .messages
                        .last()
                        .map(|m| m.content.clone())
                        .unwrap_or_default();
                    assert!(
                        last_msg.contains("你是一个自主运行的子代理"),
                        "第二轮应收到系统级引导提示，实际最后消息：{last_msg}"
                    );
                    // 再次输出纯文本，模型确认收敛，本次应正常退出
                    Box::pin(stream::iter(vec![
                        StreamEvent::Delta("确认目标已完成，这是最终总结。".into()),
                        StreamEvent::Done,
                    ]))
                }
            }
        }

        struct DummyTool;
        impl crate::Tool for DummyTool {
            fn schema(&self) -> crate::ToolSchema {
                crate::ToolSchema {
                    name: "dummy".into(),
                    description: "dummy tool".into(),
                    parameters: serde_json::json!({"type": "object"}),
                    tags: Vec::new(),
                }
            }
            fn run<'a>(
                &'a self,
                _input: Value,
                _ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
                Box::pin(async move {
                    Ok(ToolOutput {
                        content: "ok".into(),
                        is_error: false,
                    })
                })
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DummyTool));
        let registry = Arc::new(registry);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = EventSink::child(&tx, 1);
        let allowed = std::collections::HashSet::from(["dummy".to_string()]);
        let provider = NudgeProvider {
            calls: AtomicUsize::new(0),
        };

        let output = run_agent_loop(
            &provider,
            String::new(),
            vec![Message::user("do something")],
            ToolCtx::new(std::env::temp_dir(), Vec::new(), None, Vec::new()),
            registry.schemas(),
            Some(&allowed),
            registry,
            5,
            Some(DEFAULT_CONTEXT_LENGTH),
            &sink,
            None,
            None,
            0,
            0,
        )
        .await
        .unwrap();

        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            2,
            "应执行 2 轮调用（第 1 轮 nudge，第 2 轮退出）"
        );
        assert_eq!(output.final_text, "确认目标已完成，这是最终总结。");
        assert_eq!(output.exit_reason, AgentExitReason::Finished);
        assert!(output.tool_trail.is_empty());
    }

    /// 截断后应自动续写：第 1 轮只产出思考并被截断，第 2 轮必须带上续写指令并产出正文。
    #[tokio::test]
    async fn truncated_stream_triggers_continuation_until_answer() {
        use futures::{stream, Stream};
        use std::pin::Pin;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct TruncatingProvider {
            calls: AtomicUsize,
            last_messages: parking_lot::Mutex<Vec<Message>>,
        }

        impl Provider for TruncatingProvider {
            fn stream(
                &self,
                req: StreamRequest,
            ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
                *self.last_messages.lock() = req.messages.clone();
                let call_num = self.calls.fetch_add(1, Ordering::SeqCst);
                if call_num == 0 {
                    Box::pin(stream::iter(vec![
                        StreamEvent::Reasoning("长篇推理……".into()),
                        StreamEvent::Truncated("max_tokens".into()),
                        StreamEvent::Done,
                    ]))
                } else {
                    Box::pin(stream::iter(vec![
                        StreamEvent::Delta("最终答复".into()),
                        StreamEvent::Done,
                    ]))
                }
            }
        }

        let provider = TruncatingProvider::default();
        let registry = Arc::new(ToolRegistry::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // parent sink：child 模式只转发进度/转录，不向通道发 AgentEvent。
        let sink = EventSink::parent(&tx, 1);
        let output = run_agent_loop(
            &provider,
            String::new(),
            vec![Message::user("解题")],
            ToolCtx::new(std::env::temp_dir(), Vec::new(), None, Vec::new()),
            Vec::new(),
            None,
            registry,
            5,
            Some(DEFAULT_CONTEXT_LENGTH),
            &sink,
            None,
            None,
            0,
            0,
        )
        .await
        .unwrap();

        assert!(output.final_text.contains("最终答复"));
        assert_eq!(output.exit_reason, AgentExitReason::Finished);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        let last = provider.last_messages.lock().last().cloned();
        let last = last.expect("第 2 轮请求应有消息");
        assert_eq!(last.role, crate::types::Role::User);
        assert_eq!(last.content, TRUNCATION_CONTINUE_NUDGE);
        drop(tx);
        let mut events = Vec::new();
        while let Ok((_, ev)) = rx.try_recv() {
            events.push(ev);
        }
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Notice(_))),
            "已成功续写出正文时不应发 Notice"
        );
    }

    /// 持续「被截断且零新增正文」时必须终止并给出 Notice，不得无限续写。
    #[tokio::test]
    async fn persistent_empty_truncation_emits_notice_and_finishes() {
        use futures::{stream, Stream};
        use std::pin::Pin;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct AlwaysTruncatedProvider {
            calls: AtomicUsize,
        }

        impl Provider for AlwaysTruncatedProvider {
            fn stream(
                &self,
                _req: StreamRequest,
            ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(stream::iter(vec![
                    StreamEvent::Truncated("max_tokens".into()),
                    StreamEvent::Done,
                ]))
            }
        }

        let provider = AlwaysTruncatedProvider {
            calls: AtomicUsize::new(0),
        };
        let registry = Arc::new(ToolRegistry::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = EventSink::parent(&tx, 1);
        let output = run_agent_loop(
            &provider,
            String::new(),
            vec![Message::user("解题")],
            ToolCtx::new(std::env::temp_dir(), Vec::new(), None, Vec::new()),
            Vec::new(),
            None,
            registry,
            50,
            Some(DEFAULT_CONTEXT_LENGTH),
            &sink,
            None,
            None,
            0,
            0,
        )
        .await
        .unwrap();

        assert_eq!(output.exit_reason, AgentExitReason::Finished);
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            MAX_EMPTY_TRUNCATION_ROUNDS as usize,
            "应在 MAX_EMPTY_TRUNCATION_ROUNDS 轮后停止续写"
        );
        drop(tx);
        let mut events = Vec::new();
        while let Ok((_, ev)) = rx.try_recv() {
            events.push(ev);
        }
        let notice_pos = events
            .iter()
            .position(|e| matches!(e, AgentEvent::Notice(t) if t.contains("截断")));
        let notice_pos = notice_pos.expect("应发出含「截断」的 Notice");
        let done_pos = events
            .iter()
            .position(|e| matches!(e, AgentEvent::Done))
            .expect("应以 Done 结束");
        assert!(notice_pos < done_pos, "Notice 应先于 Done 到达");
    }

    #[tokio::test]
    async fn run_agent_loop_reports_step_limit_reached() {
        use futures::{stream, Stream};
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct StepLimitProvider {
            calls: AtomicUsize,
        }

        impl Provider for StepLimitProvider {
            fn stream(
                &self,
                _req: StreamRequest,
            ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
                let call_num = self.calls.fetch_add(1, Ordering::SeqCst);
                if call_num == 0 {
                    // 第 1 步：调用工具
                    Box::pin(stream::iter(vec![
                        StreamEvent::ToolCallDelta(ToolCallDelta {
                            index: 0,
                            id: Some("call-1".into()),
                            name: Some("test_tool".into()),
                            arguments_fragment: "{}".into(),
                        }),
                        StreamEvent::Done,
                    ]))
                } else {
                    // 步数耗尽收尾调用
                    Box::pin(stream::iter(vec![
                        StreamEvent::Delta("这是达到步数限制后的阶段性总结。".into()),
                        StreamEvent::Done,
                    ]))
                }
            }
        }

        struct SimpleTool;
        impl crate::Tool for SimpleTool {
            fn schema(&self) -> crate::ToolSchema {
                crate::ToolSchema {
                    name: "test_tool".into(),
                    description: "test tool".into(),
                    parameters: serde_json::json!({"type": "object"}),
                    tags: Vec::new(),
                }
            }
            fn run<'a>(
                &'a self,
                _input: Value,
                _ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
                Box::pin(async move {
                    Ok(ToolOutput {
                        content: "tool output ok".into(),
                        is_error: false,
                    })
                })
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(SimpleTool));
        let registry = Arc::new(registry);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = EventSink::child(&tx, 1);
        let allowed = std::collections::HashSet::from(["test_tool".to_string()]);
        let provider = StepLimitProvider {
            calls: AtomicUsize::new(0),
        };

        // max_steps = 1：第 1 轮调用工具后即达到步数限制
        let output = run_agent_loop(
            &provider,
            String::new(),
            vec![Message::user("run task")],
            ToolCtx::new(std::env::temp_dir(), Vec::new(), None, Vec::new()),
            registry.schemas(),
            Some(&allowed),
            registry,
            1,
            Some(DEFAULT_CONTEXT_LENGTH),
            &sink,
            None,
            None,
            0,
            0,
        )
        .await
        .unwrap();

        assert_eq!(output.exit_reason, AgentExitReason::MaxStepsReached);
        assert_eq!(output.final_text, "这是达到步数限制后的阶段性总结。");
        assert_eq!(output.tool_trail.len(), 1);

        let finalized = finalize_subagent_output(output, 1).unwrap();
        assert!(finalized
            .output
            .starts_with("[阶段性总结：已达到步数上限 1]\n\n"));
        assert_eq!(finalized.exit_reason, AgentExitReason::MaxStepsReached);
    }

    #[tokio::test]
    async fn run_agent_loop_truncates_tool_output_for_subagent_budget() {
        use crate::Role;
        use futures::{stream, Stream};
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CaptureProvider {
            calls: AtomicUsize,
            seen: parking_lot::Mutex<Vec<Vec<Message>>>,
        }

        impl Provider for CaptureProvider {
            fn stream(
                &self,
                req: StreamRequest,
            ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                self.seen.lock().push(req.messages);
                if call == 0 {
                    Box::pin(stream::iter(vec![
                        StreamEvent::ToolCallDelta(ToolCallDelta {
                            index: 0,
                            id: Some("dump-call".into()),
                            name: Some("counting".into()),
                            arguments_fragment: "{}".into(),
                        }),
                        StreamEvent::Done,
                    ]))
                } else {
                    Box::pin(stream::iter(vec![StreamEvent::Done]))
                }
            }
        }

        struct BigOutputTool;

        impl crate::Tool for BigOutputTool {
            fn schema(&self) -> crate::ToolSchema {
                crate::ToolSchema {
                    name: "counting".into(),
                    description: "dumps a lot of lines".into(),
                    parameters: serde_json::json!({"type": "object"}),
                    tags: Vec::new(),
                }
            }

            fn run<'a>(
                &'a self,
                _input: Value,
                _ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
                Box::pin(async move {
                    Ok(ToolOutput {
                        content: "BIG-".repeat(2000),
                        is_error: false,
                    })
                })
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(BigOutputTool));
        let registry = Arc::new(registry);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = EventSink::child(&tx, 1);
        let allowed = std::collections::HashSet::from(["counting".to_string()]);
        let provider = CaptureProvider {
            calls: AtomicUsize::new(0),
            seen: parking_lot::Mutex::new(Vec::new()),
        };
        let output = run_agent_loop(
            &provider,
            String::new(),
            vec![Message::user("run it")],
            ToolCtx::new(std::env::temp_dir(), Vec::new(), None, Vec::new()),
            registry.schemas(),
            Some(&allowed),
            registry,
            2,
            Some(DEFAULT_CONTEXT_LENGTH),
            &sink,
            None,
            Some(100),
            0,
            0,
        )
        .await
        .unwrap();
        assert!(output.final_text.trim().is_empty());
        // 第二轮请求里的工具结果必须被截断（100 + 提示）
        let second = provider.seen.lock()[1].clone();
        let tool_content = second
            .iter()
            .find_map(|m| match m.role {
                Role::Tool => Some(m.content.as_str()),
                _ => None,
            })
            .expect("第二轮应含工具结果消息");
        assert!(tool_content.chars().count() <= 200, "{tool_content}");
        assert!(
            tool_content.contains("已截断至前 100 字符"),
            "{tool_content}"
        );
    }

    #[tokio::test]
    async fn run_agent_loop_steering_appends_and_continues() {
        use futures::{stream, Stream};
        use std::pin::Pin;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct SteerScriptProvider {
            calls: AtomicUsize,
            steer_tx: SteeringSender,
        }

        impl Provider for SteerScriptProvider {
            fn stream(
                &self,
                req: StreamRequest,
            ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
                let call_idx = self.calls.fetch_add(1, Ordering::SeqCst);
                if call_idx == 0 {
                    // 在第一轮生成期间，通过 steering 管道追加新指示
                    let _ = self.steer_tx.send("追加的新问题".into());
                    Box::pin(stream::iter(vec![
                        StreamEvent::Delta("第一轮回答".into()),
                        StreamEvent::Done,
                    ]))
                } else {
                    // 第二轮应当接收到前序回答以及追加的指示
                    assert!(req.messages.iter().any(|m| m.content == "第一轮回答"));
                    assert!(req.messages.iter().any(|m| m.content == "追加的新问题"));
                    Box::pin(stream::iter(vec![
                        StreamEvent::Delta("第二轮回答".into()),
                        StreamEvent::Done,
                    ]))
                }
            }
        }

        let (steer_tx, steer_rx) = steering_channel();
        let provider = SteerScriptProvider {
            calls: AtomicUsize::new(0),
            steer_tx,
        };
        let registry = Arc::new(ToolRegistry::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = EventSink::parent(&tx, 1);
        let output = run_agent_loop(
            &provider,
            String::new(),
            vec![Message::user("初始问题")],
            ToolCtx::new(std::env::temp_dir(), Vec::new(), None, Vec::new()),
            Vec::new(),
            None,
            registry,
            3,
            Some(DEFAULT_CONTEXT_LENGTH),
            &sink,
            Some(steer_rx),
            None,
            0,
            0,
        )
        .await
        .unwrap();

        assert_eq!(output.final_text, "第二轮回答");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);

        // 验证前端 sink 成功收到了 SteeringReceived 事件
        let mut got_steering_event = false;
        while let Ok((_, ev)) = rx.try_recv() {
            if let AgentEvent::SteeringReceived(text) = ev {
                if text == "追加的新问题" {
                    got_steering_event = true;
                }
            }
        }
        assert!(got_steering_event, "sink 应当收到 SteeringReceived 事件");
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
        assert!(!d.observe(&calls_map(&[tc("read_file", "{\"p\":\"a\"}")])).0);
        assert!(!d.observe(&calls_map(&[tc("read_file", "{\"p\":\"b\"}")])).0);
        assert!(!d.observe(&calls_map(&[tc("read_file", "{\"p\":\"c\"}")])).0);
    }

    #[test]
    fn loop_detector_triggers_on_3_consecutive_same() {
        let mut d = LoopDetector::new(3);
        let c = calls_map(&[tc("shell", "{\"command\":\"ls\"}")]);
        assert!(!d.observe(&c).0, "第 1 轮：不触发");
        assert!(!d.observe(&c).0, "第 2 轮：不触发");
        assert!(d.observe(&c).0, "第 3 轮连续相同：应触发");
    }

    #[test]
    fn loop_detector_resets_on_different_call() {
        let mut d = LoopDetector::new(3);
        let c1 = calls_map(&[tc("shell", "{\"command\":\"ls\"}")]);
        let c2 = calls_map(&[tc("shell", "{\"command\":\"pwd\"}")]);
        assert!(!d.observe(&c1).0);
        assert!(!d.observe(&c1).0); // 2 次相同
        assert!(!d.observe(&c2).0); // 不同 → 重置
        assert!(!d.observe(&c2).0); // 重新 2 次
        assert!(d.observe(&c2).0); // 3 次 → 触发
    }

    #[test]
    fn loop_detector_threshold_1_triggers_immediately() {
        let mut d = LoopDetector::new(1);
        let c = calls_map(&[tc("list_dir", "{}")]);
        assert!(d.observe(&c).0, "threshold=1 第 1 轮就应触发");
    }

    #[test]
    fn loop_detector_multiple_tools_same_set_triggers() {
        let mut d = LoopDetector::new(3);
        // 每轮调两个工具，组合相同
        let c = calls_map(&[
            tc("read_file", "{\"path\":\"a\"}"),
            tc("list_dir", "{\"path\":\".\"}"),
        ]);
        assert!(!d.observe(&c).0);
        assert!(!d.observe(&c).0);
        assert!(d.observe(&c).0, "连续 3 轮相同组合应触发");
    }

    #[test]
    fn loop_detector_different_extra_tool_resets() {
        let mut d = LoopDetector::new(3);
        let base = calls_map(&[tc("shell", "{\"command\":\"ls\"}")]);
        let extra = calls_map(&[
            tc("shell", "{\"command\":\"ls\"}"),
            tc("read_file", "{\"path\":\"x\"}"),
        ]);
        assert!(!d.observe(&base).0);
        assert!(!d.observe(&base).0);
        assert!(!d.observe(&extra).0); // 组合变了 → 重置
        assert!(!d.observe(&base).0);
        assert!(!d.observe(&base).0);
        // 仅 2 次 base，不触发
    }

    #[test]
    fn loop_detector_non_consecutive_dup_warns() {
        let mut d = LoopDetector::new(3);
        let c1 = calls_map(&[tc("shell", "{\"command\":\"ls\"}")]);
        let c2 = calls_map(&[tc("shell", "{\"command\":\"pwd\"}")]);
        // c1 → c2 → c1：c1 非连续重复
        let (looped, warn1) = d.observe(&c1);
        assert!(!looped && !warn1, "第 1 轮：无重复");
        let (looped, warn2) = d.observe(&c2);
        assert!(!looped && !warn2, "第 2 轮：不同指纹，无重复");
        let (looped, warn3) = d.observe(&c1);
        assert!(!looped && warn3, "第 3 轮：c1 非连续重复，应提醒");
        // 再次非连续重复不应重复提醒
        let (looped, warn4) = d.observe(&c2);
        assert!(!looped && !warn4, "第 4 轮：已提醒过，不再重复提醒");
    }

    #[test]
    fn test_is_retryable_error() {
        assert!(!is_retryable_error(
            "HTTP 401 Unauthorized: invalid api key"
        ));
        assert!(!is_retryable_error("403 Forbidden: access denied"));
        assert!(!is_retryable_error(
            "404 Not Found: model deepseek-v9 does not exist"
        ));
        assert!(!is_retryable_error("400 Bad Request: unknown field"));
        assert!(is_retryable_error("500 Internal Server Error"));
        assert!(is_retryable_error("502 Bad Gateway"));
        assert!(is_retryable_error("503 Service Unavailable"));
        assert!(is_retryable_error("504 Gateway Timeout"));
        assert!(is_retryable_error(
            "HTTP 429 Too Many Requests: rate limit exceeded"
        ));
        assert!(is_retryable_error("stream: connection reset by peer"));
        assert!(is_retryable_error("request timed out after 30s"));
    }

    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockRetryProvider {
        calls: AtomicUsize,
        responses: Vec<Vec<StreamEvent>>,
    }

    impl Provider for MockRetryProvider {
        fn stream(
            &self,
            _req: StreamRequest,
        ) -> Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            let events = if idx < self.responses.len() {
                self.responses[idx].clone()
            } else {
                vec![StreamEvent::Error("out of responses".into())]
            };
            Box::pin(futures::stream::iter(events))
        }
    }

    #[tokio::test]
    async fn test_accumulate_stream_with_retry_success_recovery() {
        let provider = MockRetryProvider {
            calls: AtomicUsize::new(0),
            responses: vec![
                vec![
                    StreamEvent::Delta("partial tokens before crash".into()),
                    StreamEvent::Error("stream: connection reset".into()),
                ],
                vec![
                    StreamEvent::Delta("full answer recovered".into()),
                    StreamEvent::Done,
                ],
            ],
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = EventSink::parent(&tx, 42);
        let req = StreamRequest::new(vec![Message::user("hello")]);
        let accumulation = accumulate_stream_with_retry(&provider, &req, &sink, 5, 0)
            .await
            .expect("should recover after retry");
        assert_eq!(accumulation.text, "full answer recovered");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);

        let mut retries = Vec::new();
        while let Ok((_, ev)) = rx.try_recv() {
            if let AgentEvent::Retry {
                attempt,
                max_retries,
                delay_secs,
                error,
            } = ev
            {
                retries.push((attempt, max_retries, delay_secs, error));
            }
        }
        assert_eq!(retries.len(), 1);
        assert_eq!(retries[0].0, 1);
        assert_eq!(retries[0].1, 5);
        assert_eq!(retries[0].2, 0);
        assert!(retries[0].3.contains("connection reset"));
    }

    #[tokio::test]
    async fn test_accumulate_stream_fatal_401_no_retry() {
        let provider = MockRetryProvider {
            calls: AtomicUsize::new(0),
            responses: vec![vec![StreamEvent::Error(
                "HTTP 401 Unauthorized: Invalid API key".into(),
            )]],
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = EventSink::parent(&tx, 1);
        let req = StreamRequest::new(vec![Message::user("hello")]);
        let err = accumulate_stream_with_retry(&provider, &req, &sink, 5, 0)
            .await
            .expect_err("fatal error must not succeed");
        assert!(err.to_string().contains("401"));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        while let Ok((_, ev)) = rx.try_recv() {
            if let AgentEvent::Retry { .. } = ev {
                panic!("should not emit Retry event for 401");
            }
        }
    }

    #[tokio::test]
    async fn test_accumulate_stream_max_retries_exceeded() {
        let provider = MockRetryProvider {
            calls: AtomicUsize::new(0),
            responses: vec![
                vec![StreamEvent::Error("timeout 1".into())],
                vec![StreamEvent::Error("timeout 2".into())],
                vec![StreamEvent::Error("timeout 3".into())],
                vec![StreamEvent::Error("timeout 4".into())],
                vec![StreamEvent::Error("timeout 5".into())],
                vec![StreamEvent::Error("timeout 6".into())],
            ],
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = EventSink::parent(&tx, 1);
        let req = StreamRequest::new(vec![Message::user("hello")]);
        let err = accumulate_stream_with_retry(&provider, &req, &sink, 5, 0)
            .await
            .expect_err("must fail after max retries");
        assert!(err.to_string().contains("timeout"));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 6);

        let mut retry_count = 0;
        while let Ok((_, ev)) = rx.try_recv() {
            if let AgentEvent::Retry { .. } = ev {
                retry_count += 1;
            }
        }
        assert_eq!(retry_count, 5);
    }
}
