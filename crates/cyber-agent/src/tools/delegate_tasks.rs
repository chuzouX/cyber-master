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

    fn timed_out(name: String, timeout_secs: u64) -> Self {
        Self {
            name,
            status: "timed_out",
            output: None,
            error: Some(format!("Timed out after {timeout_secs} seconds")),
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

            let total = input.tasks.len();
            let timeout_secs = limits.effective_timeout_secs();
            let mut immediate = Vec::new();
            let mut pending = Vec::new();
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
                pending.push((index, display_name.clone(), async move {
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
                        )
                        .await;
                    let _ = forwarder_handle.await;
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

async fn run_ordered_bounded<F>(
    tasks: Vec<(usize, String, F)>,
    max_parallel: usize,
    timeout: Duration,
) -> Vec<(usize, DelegateTaskResult)>
where
    F: Future<Output = Result<String>> + Send,
{
    let mut results: Vec<_> =
        stream::iter(tasks.into_iter().map(|(index, name, future)| async move {
            let result = match tokio::time::timeout(timeout, future).await {
                Ok(Ok(output)) => DelegateTaskResult::completed(name, output),
                Ok(Err(error)) => DelegateTaskResult::error(name, error.to_string()),
                Err(_) => DelegateTaskResult::timed_out(name, timeout.as_secs()),
            };
            (index, result)
        }))
        .buffer_unordered(max_parallel.max(1))
        .collect()
        .await;
    results.sort_by_key(|(index, _)| *index);
    results
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
                (index, format!("task-{index}"), async move {
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

    #[tokio::test(start_paused = true)]
    async fn delegate_tasks_times_out_one_task_without_blocking_another() {
        let tasks = (0..2)
            .map(|index| {
                (index, format!("task-{index}"), async move {
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
    }
}
