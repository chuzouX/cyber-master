use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};

const TOOL_NAME: &str = "delegate_tasks";

pub struct DelegateTasksTool;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateTasksInput {
    tasks: Vec<DelegateTask>,
    /// `true`：任务后台执行（不阻塞工具调用），立即返回 job ids；
    /// 完成后由 CLI 注入当前会话。默认 false（同步等待全部结果）。
    #[serde(default)]
    background: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateTask {
    name: String,
    system_prompt: String,
    task: String,
    #[serde(default)]
    context: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    tools: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DelegateTasksOutput {
    results: Vec<DelegateTaskResult>,
}

#[derive(Debug, Serialize)]
struct DelegateTaskResult {
    name: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl DelegateTaskResult {
    fn completed(name: String, output: String) -> Self {
        Self {
            name,
            status: "completed",
            output: Some(output),
            error: None,
        }
    }

    fn error(name: String, error: impl Into<String>) -> Self {
        Self {
            name,
            status: "error",
            output: None,
            error: Some(error.into()),
        }
    }

    fn timed_out(name: String, timeout_secs: u64, partial: Option<String>) -> Self {
        let error = match partial {
            Some(tail) => {
                format!("Timed out after {timeout_secs} seconds. Partial transcript:\n{tail}")
            }
            None => format!("Timed out after {timeout_secs} seconds"),
        };
        Self {
            name,
            status: "timed_out",
            output: None,
            error: Some(error),
        }
    }
}

impl Tool for DelegateTasksTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: TOOL_NAME.into(),
            description: "Batch independent specialist tasks only. Use ordinary tools for simple operations. Each task has an isolated conversation, an explicit tool allowlist, and results are returned in input order.".into(),
            tags: vec!["agent".into(), "parallel".into(), "delegation".into()],
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["tasks"],
                "properties": {
                    "tasks": {
                        "type": "array",
                        "description": "Independent tasks with no dependencies on each other.",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["name", "system_prompt", "task", "tools"],
                            "properties": {
                                "name": {"type": "string"},
                                "system_prompt": {"type": "string"},
                                "task": {"type": "string"},
                                "context": {"type": "string", "default": ""},
                                "provider": {"type": "string"},
                                "model": {"type": "string"},
                                "tools": {
                                    "type": "array",
                                    "items": {"type": "string"},
                                    "description": "Exact tool allowlist. Empty means text-only; parent tools are not inherited."
                                }
                            }
                        }
                    },
                    "background": {
                        "type": "boolean",
                        "default": false,
                        "description": "Run tasks in the background and return job ids immediately; results are injected into the session when finished. Use only when you must keep working and do not need the results now."
                    }
                }
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        self.run_streaming(input, ctx, None)
    }

    fn run_streaming<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
        progress: Option<UnboundedSender<String>>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let input: DelegateTasksInput = serde_json::from_value(input)?;
            let runtime = ctx.subagent_runtime().cloned().ok_or_else(|| {
                AgentError::Provider("delegate_tasks is unavailable outside an agent turn".into())
            })?;
            if input.tasks.is_empty() {
                return Err(AgentError::Provider(
                    "delegate_tasks requires at least one task".into(),
                ));
            }

            let limits = &runtime.config().agent.subagents;
            let max_tasks = limits.effective_max_tasks();
            if input.tasks.len() > max_tasks {
                return Err(AgentError::Provider(format!(
                    "delegate_tasks received {} tasks; maximum is {max_tasks}",
                    input.tasks.len()
                )));
            }

            if input.background {
                return run_background(input.tasks, &runtime, ctx).await;
            }

            let total = input.tasks.len();
            let timeout_secs = limits.effective_timeout_secs();
            let archive = ctx.subagent_archive().cloned();
            let mut immediate = Vec::new();
            let mut pending = Vec::new();
            let mut run_ids = std::collections::BTreeMap::new();
            for (index, mut task) in input.tasks.into_iter().enumerate() {
                task.tools = deduplicate(task.tools);
                if let Some(error) = validate_task(&runtime, &task) {
                    immediate.push((index, DelegateTaskResult::error(task.name, error)));
                    continue;
                }
                let child_runtime = Arc::clone(&runtime);
                let child_progress = progress.clone();
                let display_name = task.name.clone();
                let task_num = index + 1;
                let run_id = archive.as_ref().map(|a| a.start(&display_name));
                let transcript =
                    run_id.and_then(|rid| archive.as_ref().and_then(|a| a.run_handle(rid)));
                run_ids.insert(index, run_id);
                let transcript_for_timeout = transcript.clone();
                let archive_for_finish = archive.clone();
                let run_id_for_finish = run_id;
                pending.push((index, display_name.clone(), transcript, async move {
                    if let Some(tx) = child_progress.as_ref() {
                        let _ = tx.send(format!("[{task_num}/{total}] {display_name} started\n"));
                    }
                    let (task_tx, mut task_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                    let sub_progress = child_progress.clone();
                    let forwarder_name = display_name.clone();
                    let forwarder_handle = tokio::spawn(async move {
                        while let Some(msg) = task_rx.recv().await {
                            if let Some(tx) = sub_progress.as_ref() {
                                let _ = tx
                                    .send(format!("[{task_num}/{total}] {forwarder_name}: {msg}"));
                            }
                        }
                    });
                    let res = child_runtime
                        .run_task(
                            task.system_prompt,
                            task.task,
                            task.context,
                            task.provider,
                            task.model,
                            task.tools,
                            Some(task_tx),
                            transcript_for_timeout,
                        )
                        .await;
                    let _ = forwarder_handle.await;
                    // 单任务结束后立即定稿自己的归档条目——不等整批结束，
                    // 面板才能在本任务完成的那一刻显示「✓ 完成」而非「运行中」。
                    if let (Some(archive), Some(run_id)) = (&archive_for_finish, run_id_for_finish)
                    {
                        let (status, output, error) = match &res {
                            Ok(output) => (
                                crate::subagent::SubagentStatus::Completed,
                                Some(output.clone()),
                                None,
                            ),
                            Err(error) => (
                                crate::subagent::SubagentStatus::Error,
                                None,
                                Some(error.to_string()),
                            ),
                        };
                        archive.finish(run_id, status, output, error);
                    }
                    res
                }));
            }

            let mut results = immediate;
            let completed = run_ordered_bounded(
                pending,
                limits.effective_max_parallel(),
                Duration::from_secs(timeout_secs),
            )
            .await;
            for (index, result) in completed {
                if let Some(tx) = progress.as_ref() {
                    let _ = tx.send(format!(
                        "[{}/{}] {} {}\n",
                        index + 1,
                        total,
                        result.name,
                        result.status
                    ));
                }
                // completed/error 已由各任务 future 内定稿；只有超时（future 被丢弃）
                // 需要在这里补定稿。
                if result.status == "timed_out" {
                    if let (Some(archive), Some(run_id)) =
                        (archive.as_ref(), run_ids.get(&index).copied().flatten())
                    {
                        archive.finish(
                            run_id,
                            crate::subagent::SubagentStatus::TimedOut,
                            None,
                            result.error.clone(),
                        );
                    }
                }
                results.push((index, result));
            }
            results.sort_by_key(|(index, _)| *index);
            let results: Vec<_> = results.into_iter().map(|(_, result)| result).collect();
            let all_failed = results.iter().all(|result| result.status != "completed");
            Ok(ToolOutput {
                content: serde_json::to_string(&DelegateTasksOutput { results })?,
                is_error: all_failed,
            })
        })
    }
}

/// `background=true` 路径：校验后逐个 detached 运行（无外层超时，靠 kill 信号终止），
/// 立即返回 job ids；结果由 CLI 注入会话、面板可查。
async fn run_background(
    tasks: Vec<DelegateTask>,
    runtime: &Arc<crate::agent::SubagentRuntime>,
    ctx: &ToolCtx,
) -> Result<ToolOutput> {
    let mut tasks = tasks;
    let mut validation_errors = Vec::new();
    for task in &mut tasks {
        task.tools = deduplicate(std::mem::take(&mut task.tools));
        if let Some(error) = validate_task(runtime, task) {
            validation_errors.push(format!("{}: {error}", task.name));
        }
    }
    if !validation_errors.is_empty() {
        return Err(AgentError::Provider(validation_errors.join("; ")));
    }
    let background = ctx.background().cloned().ok_or_else(|| {
        AgentError::Provider("background jobs are unavailable in this session".into())
    })?;
    let archive = ctx.subagent_archive().cloned();

    let mut job_ids = Vec::new();
    for task in tasks {
        let child_runtime = Arc::clone(runtime);
        let background = Arc::clone(&background);
        let archive = archive.clone();
        let (job_id, kill_rx) =
            background.start(crate::background::JobKind::Subagent, task.name.clone());
        let run_id = archive.as_ref().map(|a| a.start(&task.name));
        if let Some(run_id) = run_id {
            background.link_archive(job_id, run_id);
        }
        let transcript = run_id.and_then(|rid| archive.as_ref().and_then(|a| a.run_handle(rid)));
        job_ids.push(job_id);
        tokio::spawn(crate::background::run_detached_subagent(
            child_runtime,
            task.system_prompt,
            task.task,
            task.context,
            task.provider,
            task.model,
            task.tools,
            transcript,
            archive,
            background,
            job_id,
            run_id,
            kill_rx,
        ));
    }
    Ok(ToolOutput {
        content: serde_json::to_string(&json!({ "background_jobs": job_ids }))?,
        is_error: false,
    })
}

fn deduplicate(tools: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    tools
        .into_iter()
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

fn validate_task(runtime: &crate::agent::SubagentRuntime, task: &DelegateTask) -> Option<String> {
    if task.name.trim().is_empty() {
        return Some("name must not be blank".into());
    }
    if task.system_prompt.trim().is_empty() {
        return Some("system_prompt must not be blank".into());
    }
    if task.task.trim().is_empty() {
        return Some("task must not be blank".into());
    }
    if task
        .provider
        .as_ref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Some("provider must not be blank when supplied".into());
    }
    if task
        .model
        .as_ref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Some("model must not be blank when supplied".into());
    }
    if let Some(provider) = task.provider.as_ref() {
        if !runtime.providers().providers.contains_key(provider) {
            return Some(format!("unknown provider '{provider}'"));
        }
    }
    let available: HashSet<String> = runtime
        .registry()
        .all_schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect();
    for tool in &task.tools {
        if tool == TOOL_NAME {
            return Some("delegate_tasks cannot be delegated recursively".into());
        }
        if tool == "web_fetch" && !runtime.config().tools.web_search {
            return Some("web search and fetching are disabled in configuration".into());
        }
        if !available.contains(tool) {
            return Some(format!("unknown tool '{tool}'"));
        }
    }
    None
}

/// 子代理转录句柄（`SubagentArchive` 运行条目；超时分支据此取部分产出）。
type TranscriptHandle = Option<Arc<std::sync::Mutex<crate::subagent::SubagentRun>>>;

async fn run_ordered_bounded<F>(
    tasks: Vec<(usize, String, TranscriptHandle, F)>,
    max_parallel: usize,
    timeout: Duration,
) -> Vec<(usize, DelegateTaskResult)>
where
    F: Future<Output = Result<String>> + Send,
{
    let mut results: Vec<_> = stream::iter(tasks.into_iter().map(
        |(index, name, transcript, future)| async move {
            let result = match tokio::time::timeout(timeout, future).await {
                Ok(Ok(output)) => DelegateTaskResult::completed(name, output),
                Ok(Err(error)) => DelegateTaskResult::error(name, error.to_string()),
                Err(_) => DelegateTaskResult::timed_out(
                    name,
                    timeout.as_secs(),
                    transcript_tail(transcript),
                ),
            };
            (index, result)
        },
    ))
    .buffer_unordered(max_parallel.max(1))
    .collect()
    .await;
    results.sort_by_key(|(index, _)| *index);
    results
}

/// 超时子代理的部分产出：取转录最后 3 行（空转录返回 None，退化为现有文案）。
fn transcript_tail(transcript: TranscriptHandle) -> Option<String> {
    let lines = transcript.and_then(|run| run.lock().ok().map(|run| run.lines.clone()))?;
    let tail = lines
        .iter()
        .rev()
        .take(3)
        .rev()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if tail.is_empty() {
        None
    } else {
        Some(tail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Barrier;

    use cyber_core::{Config, ProvidersConfig, ThinkingIntensity};

    use crate::agent::SubagentRuntime;
    use crate::types::AgentEvent;
    use crate::ToolRegistry;

    fn context(config: Config) -> ToolCtx {
        let providers = ProvidersConfig::default_template();
        let registry = Arc::new(ToolRegistry::with_builtins());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<(u64, AgentEvent)>();
        let runtime = Arc::new(SubagentRuntime::new(
            config,
            providers,
            None,
            true,
            std::env::temp_dir(),
            registry,
            false,
            ThinkingIntensity::Middle,
            String::new(),
            tx,
            1,
            None,
            None,
        ));
        ToolCtx::new(std::env::temp_dir(), Vec::new(), None, Vec::new())
            .with_subagent_runtime(runtime)
    }

    fn task(name: &str, body: &str) -> Value {
        json!({
            "name": name,
            "system_prompt": "Be concise.",
            "task": body,
            "tools": []
        })
    }

    #[test]
    fn delegate_tasks_schema_requires_dynamic_task_fields() {
        let schema = DelegateTasksTool.schema();
        assert_eq!(schema.name, "delegate_tasks");
        assert_eq!(
            schema.parameters["properties"]["tasks"]["items"]["required"],
            json!(["name", "system_prompt", "task", "tools"])
        );
    }

    #[tokio::test]
    async fn delegate_tasks_rejects_empty_and_oversized_batches() {
        let tool = DelegateTasksTool;
        let ctx = context(Config::default());
        assert!(tool.run(json!({"tasks": []}), &ctx).await.is_err());

        let mut config = Config::default();
        config.agent.subagents.max_tasks = 1;
        let ctx = context(config);
        assert!(tool
            .run(
                json!({"tasks": [task("one", "first"), task("two", "second")]}),
                &ctx,
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn delegate_tasks_returns_partial_failures_in_input_order() {
        let tool = DelegateTasksTool;
        let ctx = context(Config::default());
        let output = tool
            .run(
                json!({
                    "tasks": [
                        task("duplicate", "first"),
                        {
                            "name": "bad",
                            "system_prompt": "specialist",
                            "task": "invalid",
                            "tools": ["delegate_tasks"]
                        },
                        task("duplicate", "third")
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        let value: Value = serde_json::from_str(&output.content).unwrap();
        let results = value["results"].as_array().unwrap();
        assert_eq!(results[0]["name"], "duplicate");
        assert_eq!(results[0]["status"], "completed");
        assert_eq!(results[1]["name"], "bad");
        assert_eq!(results[1]["status"], "error");
        assert_eq!(results[2]["name"], "duplicate");
        assert_eq!(results[2]["status"], "completed");
    }

    #[tokio::test]
    async fn delegate_tasks_validates_provider_model_and_tools_per_item() {
        let tool = DelegateTasksTool;
        let ctx = context(Config::default());
        let output = tool
            .run(
                json!({
                    "tasks": [
                        {
                            "name": "provider",
                            "system_prompt": "specialist",
                            "task": "check",
                            "provider": "missing",
                            "tools": []
                        },
                        {
                            "name": "model",
                            "system_prompt": "specialist",
                            "task": "check",
                            "model": " ",
                            "tools": []
                        },
                        {
                            "name": "tool",
                            "system_prompt": "specialist",
                            "task": "check",
                            "tools": ["missing_tool"]
                        }
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.is_error);
        let value: Value = serde_json::from_str(&output.content).unwrap();
        assert!(value["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|result| result["status"] == "error"));
    }

    #[tokio::test]
    async fn delegate_tasks_rejects_web_fetch_when_web_search_is_disabled() {
        let tool = DelegateTasksTool;
        let mut config = Config::default();
        config.tools.web_search = false;
        let ctx = context(config);
        let output = tool
            .run(
                json!({
                    "tasks": [
                        {
                            "name": "web-disabled",
                            "system_prompt": "specialist",
                            "task": "check",
                            "tools": ["web_fetch"]
                        }
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.is_error);
        let value: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(value["results"][0]["status"], "error");
        assert!(value["results"][0]["error"]
            .as_str()
            .unwrap()
            .contains("disabled"));
    }

    #[tokio::test]
    async fn delegate_tasks_limits_peak_concurrency_and_preserves_order() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(2));
        let tasks = (0..4)
            .map(|index| {
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                let barrier = Arc::clone(&barrier);
                (index, format!("task-{index}"), None, async move {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(current, Ordering::SeqCst);
                    barrier.wait().await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(format!("output-{index}"))
                })
            })
            .collect();
        let results = run_ordered_bounded(tasks, 2, Duration::from_secs(1)).await;
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(
            results.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[tokio::test]
    async fn delegate_tasks_background_returns_job_ids_and_completes_independently() {
        let tool = DelegateTasksTool;
        let archive = Arc::new(crate::subagent::SubagentArchive::default());
        let background = Arc::new(crate::background::BackgroundRegistry::default());
        let ctx = context(Config::default())
            .with_subagent_archive(Some(Arc::clone(&archive)))
            .with_background(Some(Arc::clone(&background)));
        let output = tool
            .run(
                json!({
                    "background": true,
                    "tasks": [task("bg-one", "do the background thing")]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        let value: Value = serde_json::from_str(&output.content).unwrap();
        let ids = value["background_jobs"].as_array().unwrap();
        assert_eq!(ids.len(), 1);
        let job_id = ids[0].as_u64().unwrap();
        // 任务独立完成（不阻塞工具返回；mock echo 模式逐字符流式）
        for _ in 0..500 {
            let status = background
                .snapshot()
                .into_iter()
                .find(|job| job.id == job_id)
                .map(|job| job.status)
                .unwrap_or(crate::background::JobStatus::Running);
            if status.is_finished() {
                assert!(matches!(status, crate::background::JobStatus::Finished(_)));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let job = background
            .snapshot()
            .into_iter()
            .find(|job| job.id == job_id)
            .unwrap();
        assert!(
            job.status.is_finished(),
            "后台子代理应独立完成，实际：{:?}",
            job.status
        );
        let runs = archive.snapshot();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, crate::subagent::SubagentStatus::Completed);
        assert!(runs[0].result.is_some());
        assert!(!runs[0].lines.is_empty(), "archive 应含转录行");
    }

    /// 把 wake 转成 `Notify::notify_one`，供测试内手动驱动 future。
    struct WakeNotify(Arc<tokio::sync::Notify>);
    impl std::task::Wake for WakeNotify {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.notify_one();
        }
        fn wake_by_ref(self: &std::sync::Arc<Self>) {
            self.0.notify_one();
        }
    }

    #[tokio::test]
    async fn delegate_tasks_marks_each_run_finished_as_its_task_ends() {
        use std::task::Poll;

        // 回归：任务 A 已完成而任务 B 仍在跑时，A 的归档条目应已「完成」，
        // 而不是等整批 delegate_tasks 结束后才定稿（面板显示错为运行中）。
        let tool = DelegateTasksTool;
        let archive = Arc::new(crate::subagent::SubagentArchive::default());
        let ctx = context(Config::default()).with_subagent_archive(Some(Arc::clone(&archive)));
        let slow_text = "x".repeat(120); // mock echo 逐字符 20ms ≈ 2.4s
        let fut = tool.run(
            json!({
                "tasks": [
                    {"name": "fast", "system_prompt": "s", "task": "hi", "tools": []},
                    {"name": "slow", "system_prompt": "s", "task": slow_text, "tools": []}
                ]
            }),
            &ctx,
        );
        tokio::pin!(fut);
        // 手动驱动：内部子任务由外层 future 的 poll 推进，wake 经 Notify 转回。
        let notify = Arc::new(tokio::sync::Notify::new());
        let waker = std::task::Waker::from(Arc::new(WakeNotify(Arc::clone(&notify))));
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(
            fut.as_mut().poll(&mut cx).is_pending(),
            "首个 poll 应进入等待"
        );
        // 在整批工具调用尚未返回时轮询：fast 应已定稿 Completed。
        let mut fast_completed = false;
        for _ in 0..200 {
            match fut.as_mut().poll(&mut cx) {
                Poll::Pending => {}
                Poll::Ready(_) => break, // 整批已结束（异常快）
            }
            let runs = archive.snapshot();
            if runs.iter().any(|run| {
                run.name == "fast" && run.status == crate::subagent::SubagentStatus::Completed
            }) {
                fast_completed = true;
                break;
            }
            tokio::select! {
                _ = notify.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(30)) => {}
            }
        }
        assert!(
            fast_completed,
            "fast 子代理结束后应立即定稿 Completed，实际：{:?}",
            archive.snapshot()
        );
        let output = fut.await.unwrap();
        assert!(!output.is_error);
        let runs = archive.snapshot();
        assert!(runs.len() == 2);
        assert!(runs
            .iter()
            .all(|run| run.status != crate::subagent::SubagentStatus::Running));
    }

    #[tokio::test]
    async fn delegate_tasks_background_rejects_invalid_task_before_spawning() {
        let tool = DelegateTasksTool;
        let archive = Arc::new(crate::subagent::SubagentArchive::default());
        let background = Arc::new(crate::background::BackgroundRegistry::default());
        let ctx = context(Config::default())
            .with_subagent_archive(Some(Arc::clone(&archive)))
            .with_background(Some(Arc::clone(&background)));
        let result = tool
            .run(
                json!({
                    "background": true,
                    "tasks": [{
                        "name": "bad",
                        "system_prompt": "",
                        "task": "x",
                        "tools": []
                    }]
                }),
                &ctx,
            )
            .await;
        assert!(result.is_err(), "校验失败应返回错误而非静默启动");
        assert!(background.snapshot().is_empty());
        assert!(archive.snapshot().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn delegate_tasks_times_out_one_task_without_blocking_another() {
        let tasks = (0..2)
            .map(|index| {
                (index, format!("task-{index}"), None, async move {
                    if index == 0 {
                        std::future::pending::<()>().await;
                    }
                    Ok(format!("output-{index}"))
                })
            })
            .collect();
        let results = run_ordered_bounded(tasks, 2, Duration::from_secs(5)).await;
        assert_eq!(results[0].1.status, "timed_out");
        assert_eq!(results[1].1.status, "completed");
        assert_eq!(
            results[0].1.error.as_deref(),
            Some("Timed out after 5 seconds")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_result_carries_partial_transcript_tail() {
        let archive = crate::subagent::SubagentArchive::default();
        let run_id = archive.start("slow");
        archive.append_line(run_id, "tool shell args: {\"command\":\"nmap\"}".into());
        archive.append_line(run_id, "shell => PORT 80 http".into());
        archive.append_line(run_id, "shell => PORT 443 https".into());
        let transcript = archive.run_handle(run_id);
        let tasks = vec![(0usize, "slow".to_string(), transcript, async {
            std::future::pending::<()>().await;
            Ok::<String, crate::error::AgentError>(String::new())
        })];
        let results = run_ordered_bounded(tasks, 1, Duration::from_secs(5)).await;
        assert_eq!(results[0].1.status, "timed_out");
        let error = results[0].1.error.as_deref().unwrap();
        assert!(
            error.starts_with("Timed out after 5 seconds. Partial transcript:"),
            "超时应携带转录尾部：{error}"
        );
        assert!(error.contains("shell => PORT 80 http"));
        assert!(error.contains("shell => PORT 443 https"));
    }
}
