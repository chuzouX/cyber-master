//! shell 工具：执行 shell 命令（护栏先查 denylist；捕获 stdout+stderr）。

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::guard::check_command;

pub struct ShellTool;

impl Tool for ShellTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "shell".into(),
            description: "执行 shell 命令（受安全护栏约束；危险命令会被拒绝）。Windows 用 cmd /C，Unix 用 sh -c".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "要执行的命令" }
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
                .ok_or_else(|| AgentError::Provider("shell 缺少 command 参数".into()))?;
            // 护栏：denylist 命中 → 拒绝（结果回灌给 LLM，让其修正）
            if let Err(reason) = check_command(command, ctx) {
                return Ok(ToolOutput {
                    content: reason,
                    is_error: true,
                });
            }
            let mut cmd = if cfg!(windows) {
                let mut c = tokio::process::Command::new("cmd");
                c.arg("/C").arg(command);
                c
            } else {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg(command);
                c
            };
            cmd.current_dir(&ctx.cwd);
            let output = cmd.output().await.map_err(|e| {
                AgentError::Provider(format!("执行命令失败: {e}"))
            })?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let code = output.status.code().unwrap_or(-1);
            let mut content = String::new();
            if !stdout.is_empty() {
                content.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str("[stderr] ");
                content.push_str(&stderr);
            }
            if content.is_empty() {
                content.push_str(&format!("（命令退出码 {code}，无输出）"));
            }
            // 非零退出码视为错误（但内容仍回灌，让 LLM 看到错误信息）
            let is_error = code != 0;
            Ok(ToolOutput { content, is_error })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::env::temp_dir(),
            rules: vec![],
            scope: None,
        }
    }

    #[tokio::test]
    async fn runs_echo() {
        let out = ShellTool
            .run(json!({"command": "echo cyber_master"}), &ctx())
            .await
            .unwrap();
        assert!(out.content.contains("cyber_master"));
        assert!(!out.is_error, "echo 应退出码 0");
    }

    #[tokio::test]
    async fn rejects_dangerous() {
        let out = ShellTool
            .run(json!({"command": "rm -rf /"}), &ctx())
            .await
            .unwrap();
        assert!(out.is_error, "rm -rf / 应被护栏拒绝");
        assert!(out.content.contains("安全护栏"));
    }

    #[tokio::test]
    async fn nonzero_exit_is_error() {
        let cmd = if cfg!(windows) { "exit /B 1" } else { "false" };
        let out = ShellTool
            .run(json!({"command": cmd}), &ctx())
            .await
            .unwrap();
        assert!(out.is_error, "非零退出码应 is_error");
    }
}
