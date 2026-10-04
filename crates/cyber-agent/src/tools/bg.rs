//! 后台任务工具族（AI 工具级入口）：
//! - `bg_shell`：后台启动脚本（护栏先行），立即返回 job id，不阻塞 agent 回合。
//! - `bg_status`：查询全部/单个后台任务状态与日志（JSON）。
//! - `bg_kill`：请求终止运行中的后台任务。
//!
//! 后台任务在会话生命周期内有效；CLI 用 Ctrl+B 面板 / `/bg` 命令族查看与终止。

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::background::{spawn_shell_job, truncate_chars, BackgroundRegistry, JobKind, JobStatus};
use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::guard::check_command;

/// 后台 shell 任务。
pub struct BgShellTool;

impl Tool for BgShellTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "bg_shell".into(),
            description: "Run a shell command in the background without blocking the current turn. Returns a job id; poll with bg_status and stop with bg_kill.".into(),
            tags: vec!["background".into(), "shell".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Command to run in the background (same guard rules as shell)." }
                },
                "required": ["command"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let command = input
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("bg_shell 缺少 command 参数".into()))?;
            if let Err(reason) = check_command(command, ctx) {
                return Ok(ToolOutput {
                    content: reason,
                    is_error: true,
                });
            }
            let Some(registry) = ctx.background() else {
                return Ok(ToolOutput {
                    content: "后台任务不可用（当前会话未启用后台模式）".into(),
                    is_error: true,
                });
            };
            let id = spawn_shell_job(registry, ctx.cwd.clone(), ctx.env.clone(), command.into())
                .map_err(AgentError::Provider)?;
            Ok(ToolOutput {
                content: format!("后台任务 #{id} 已启动：{command}"),
                is_error: false,
            })
        })
    }
}

/// 查询后台任务状态。
pub struct BgStatusTool;

fn job_json(registry: &BackgroundRegistry, id: Option<u64>) -> Value {
    let snapshot = registry.snapshot();
    let jobs: Vec<Value> = snapshot
        .iter()
        .filter(|job| id.is_none_or(|wanted| job.id == wanted))
        .map(|job| {
            let status = match &job.status {
                JobStatus::Running => json!("running"),
                JobStatus::Finished(code) => json!({ "finished": code }),
                JobStatus::Killed => json!("killed"),
                JobStatus::Failed(error) => json!({ "failed": truncate_chars(error, 400) }),
            };
            json!({
                "id": job.id,
                "kind": match job.kind {
                    JobKind::Shell => "shell",
                    JobKind::Subagent => "subagent",
                },
                "name": job.name,
                "status": status,
                "lines": job.lines,
            })
        })
        .collect();
    json!({ "jobs": jobs })
}

impl Tool for BgStatusTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "bg_status".into(),
            description: "Query background job status. Without an id returns all jobs as JSON; with an id returns that job (last output lines included).".into(),
            tags: vec!["background".into(), "status".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "Optional job id." }
                },
                "required": []
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let Some(registry) = ctx.background() else {
                return Ok(ToolOutput {
                    content: "后台任务不可用（当前会话未启用后台模式）".into(),
                    is_error: true,
                });
            };
            let id = input.get("id").and_then(|v| v.as_u64());
            let jobs = job_json(registry, id);
            let content = if jobs["jobs"].as_array().is_some_and(|arr| arr.is_empty()) {
                "无后台任务".to_string()
            } else {
                serde_json::to_string(&jobs)?
            };
            Ok(ToolOutput {
                content,
                is_error: false,
            })
        })
    }
}

/// 终止后台任务。
pub struct BgKillTool;

impl Tool for BgKillTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "bg_kill".into(),
            description: "Request termination of a running background job. Returns whether the kill signal was delivered.".into(),
            tags: vec!["background".into(), "kill".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "Job id to terminate." }
                },
                "required": ["id"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let Some(registry) = ctx.background() else {
                return Ok(ToolOutput {
                    content: "后台任务不可用（当前会话未启用后台模式）".into(),
                    is_error: true,
                });
            };
            let id = input
                .get("id")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| AgentError::Provider("bg_kill 缺少 id 参数".into()))?;
            let content = if registry.kill(id) {
                format!("已请求终止 #{id}")
            } else {
                "任务不存在或已结束".to_string()
            };
            Ok(ToolOutput {
                content,
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn ctx_with_background() -> ToolCtx {
        ToolCtx::new(std::env::temp_dir(), Vec::new(), None, Vec::new())
            .with_background(Some(Arc::new(BackgroundRegistry::default())))
    }

    #[tokio::test]
    async fn bg_shell_runs_and_bg_status_reports_finished() {
        let ctx = ctx_with_background();
        let out = BgShellTool
            .run(json!({"command": "echo cyber_bg_marker"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("#"), "应返回 job id：{}", out.content);
        let id = out
            .content
            .split('#')
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|id| id.parse::<u64>().ok())
            .unwrap();
        // 轮询直到 Finished
        let registry = ctx.background().cloned().unwrap();
        for _ in 0..200 {
            let status = registry
                .snapshot()
                .into_iter()
                .find(|job| job.id == id)
                .map(|job| job.status)
                .unwrap_or(JobStatus::Running);
            if status.is_finished() {
                assert!(matches!(status, JobStatus::Finished(0)));
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let status_out = BgStatusTool.run(json!({}), &ctx).await.unwrap();
        assert!(status_out.content.contains("cyber_bg_marker"));
        assert!(status_out.content.contains("\"finished\":0"));
    }

    #[tokio::test]
    async fn bg_status_empty_registry_is_nonempty_text() {
        let ctx = ctx_with_background();
        let out = BgStatusTool.run(json!({}), &ctx).await.unwrap();
        assert_eq!(out.content, "无后台任务");
    }

    #[tokio::test]
    async fn bg_shell_guard_denied_returns_error_output() {
        let ctx = ctx_with_background();
        let out = BgShellTool
            .run(json!({"command": "rm -rf /"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("安全护栏"));
    }

    #[tokio::test]
    async fn bg_shell_without_registry_reports_unavailable() {
        let ctx = ToolCtx::new(std::env::temp_dir(), Vec::new(), None, Vec::new());
        let out = BgShellTool
            .run(json!({"command": "echo hi"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("未启用后台模式"));
    }

    #[tokio::test]
    async fn bg_kill_terminates_running_job() {
        let ctx = ctx_with_background();
        let cmd = if cfg!(windows) {
            "ping 127.0.0.1 -n 60"
        } else {
            "sleep 60"
        };
        let out = BgShellTool
            .run(json!({"command": cmd}), &ctx)
            .await
            .unwrap();
        let id = out
            .content
            .split('#')
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|id| id.parse::<u64>().ok())
            .unwrap();
        let kill_out = BgKillTool.run(json!({"id": id}), &ctx).await.unwrap();
        assert!(kill_out.content.contains("已请求终止"));
        let registry = ctx.background().cloned().unwrap();
        for _ in 0..200 {
            let status = registry
                .snapshot()
                .into_iter()
                .find(|job| job.id == id)
                .map(|job| job.status)
                .unwrap_or(JobStatus::Running);
            if status.is_finished() {
                assert!(matches!(status, JobStatus::Killed));
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("kill 后任务未在预期时间内结束");
    }

    #[tokio::test]
    async fn bg_kill_unknown_id_says_finished_or_missing() {
        let ctx = ctx_with_background();
        let out = BgKillTool.run(json!({"id": 999}), &ctx).await.unwrap();
        assert_eq!(out.content, "任务不存在或已结束");
    }
}
