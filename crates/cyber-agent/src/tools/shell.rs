//! shell 工具：执行 shell 命令（护栏先查 denylist；捕获 stdout+stderr）。

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::guard::check_command;

pub struct ShellTool;

/// Windows：从 `PATHEXT` 中剥除 `.PY` / `.PYW`。
///
/// 背景：用户装了 PyCharm / VSCode 等 IDE 后，`.py` 文件关联常被抢占为「用 IDE 打开」
/// 而非「用 python 执行」。当 `PATHEXT` 含 `.PY` 时，cmd 裸名调用会优先解析到 PATH 中
/// 靠前的 `xxx.py` 脚本并走文件关联 → 在 IDE 里打开脚本源码（无 stdout、弹编辑器窗口），
/// 而真正可用的 `xxx.exe`（如 mingw64 的 `readelf.exe`）被遮蔽永远跑不到。
///
/// 剥除 `.PY`/`.PYW` 让 cmd 仅解析原生可执行格式（`.COM/.EXE/.BAT/.CMD/...`）：
/// 既杜绝「命令在 IDE 里打开」的怪行为，又让被 `.py` 遮蔽的同名 `.exe` 正常生效。
/// 需跑 `.py` 脚本时显式 `python xxx.py`（python 自带 `.exe` shim 不受影响）。
#[cfg(windows)]
fn sanitize_pathext(pathext: &str) -> String {
    let filtered: Vec<&str> = pathext
        .split(';')
        .map(|e| e.trim())
        .filter(|ext| {
            let up = ext.to_uppercase();
            !up.is_empty() && up != ".PY" && up != ".PYW"
        })
        .collect();
    if filtered.is_empty() {
        ".COM;.EXE;.BAT;.CMD".to_string()
    } else {
        filtered.join(";")
    }
}

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
                // 剥除 PATHEXT 中的 .PY/.PYW，避免裸名命令解析到 .py 脚本后走 IDE 文件
                // 关联（在 PyCharm/VSCode 里打开源码而非执行），并让被遮蔽的同名 .exe 生效。
                let safe_pathext =
                    sanitize_pathext(&std::env::var("PATHEXT").unwrap_or_default());
                c.env("PATHEXT", safe_pathext);
                c
            } else {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg(command);
                c
            };
            // 注入用户在 Settings → Env 配置的环境变量
            for (k, v) in &ctx.env {
                cmd.env(k, v);
            }
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
            env: Vec::new(),
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

    #[cfg(windows)]
    #[test]
    fn sanitize_pathext_strips_py_and_pyw() {
        let out = sanitize_pathext(".COM;.EXE;.BAT;.CMD;.VBS;.PY;.PYW;.CPL");
        let up = out.to_uppercase();
        assert!(!up.contains(".PY"), "应剥除 .PY/.PYW：{out}");
        assert!(up.contains(".EXE"), "应保留 .EXE：{out}");
        assert!(up.contains(".CPL"), "应保留 .CPL：{out}");
        assert!(up.contains(".BAT"), "应保留 .BAT：{out}");
    }

    #[cfg(windows)]
    #[test]
    fn sanitize_pathext_case_insensitive() {
        let out = sanitize_pathext(".exe;.py;.PY;.pyw");
        let up = out.to_uppercase();
        assert!(!up.contains(".PY"), "大小写不敏感剥除：{out}");
        assert!(up.contains(".EXE"));
    }

    #[cfg(windows)]
    #[test]
    fn sanitize_pathext_empty_falls_back_to_default() {
        let out = sanitize_pathext("");
        assert_eq!(out, ".COM;.EXE;.BAT;.CMD", "空 PATHEXT 应用默认值");
    }

    #[cfg(windows)]
    #[test]
    fn sanitize_pathext_only_py_falls_back_to_default() {
        // 全部都是 .py/.pyw → 过滤后为空 → 回退默认
        let out = sanitize_pathext(".PY;.PYW");
        assert_eq!(out, ".COM;.EXE;.BAT;.CMD");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_bare_command_finds_exe_not_py() {
        // 回归：readelf 这类同名 .py（IDE 关联）+ .exe 共存时，剥除 .PY 后应跑到 .exe。
        // 用 where readelf 验证 cmd 在剥除 .PY 的 PATHEXT 下解析到的第一个是 .exe 而非 .py。
        // 注：ShellTool 内部已注入 sanitized PATHEXT，这里直接复刻该行为断言。
        let safe = sanitize_pathext(&std::env::var("PATHEXT").unwrap_or_default());
        let out = tokio::process::Command::new("where")
            .arg("readelf")
            .env("PATHEXT", safe)
            .output()
            .await
            .expect("where readelf 失败");
        let stdout = String::from_utf8_lossy(&out.stdout);
        // where 在 sanitized PATHEXT 下不应再列出 .py（应直接命中 .exe）
        assert!(
            !stdout.to_lowercase().contains("readelf.py"),
            "剥除 .PY 后 where 不应再解析到 readelf.py，实际：{stdout}"
        );
        assert!(
            stdout.to_lowercase().contains("readelf.exe"),
            "应解析到原生 readelf.exe，实际：{stdout}"
        );
    }
}
