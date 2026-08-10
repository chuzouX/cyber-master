//! shell 工具：执行 shell 命令（护栏先查 denylist；stdout/stderr 逐行流式输出）。

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::guard::check_command;

/// shell 工具执行超时（秒）。防止 `r.interactive()` 等永不退出的命令永久挂起 agent 任务。
const SHELL_TIMEOUT_SECS: u64 = 300;

/// 进程退出后 draining 剩余输出的宽限期（毫秒）。
///
/// Windows 上 `cmd /C` 常 spawn 子进程继承 stdout/stderr 管道句柄，
/// 主进程退出后管道仍被子进程持有 → `next_line()` 永远收不到 EOF → 挂起。
/// 进程退出后给 500ms 宽限期 drain 缓冲输出，超时则强制结束。
const DRAIN_GRACE_MS: u64 = 500;

/// 输出累积上限（字节）。超过后丢弃旧行保留尾部，并标注截断，避免 `yes`/大日志
/// 撑爆 TUI 缓冲与 LLM 上下文。流式推送与最终回灌共享此上限。
const MAX_OUTPUT_BYTES: usize = 65536;

pub struct ShellTool;

/// 处理一行子进程输出：先经 `progress` 流式推送（若调用方消费），再累积进 `buf`。
/// `buf` 超过 `MAX_OUTPUT_BYTES` 时截断头部保留尾部（在字符边界），并置 `truncated=true`；
/// 此后仅保留尾部、不再推送（避免无限增长 TUI 缓冲）。
fn push_output_line(
    buf: &mut String,
    line: &str,
    progress: &Option<UnboundedSender<String>>,
    truncated: &mut bool,
) {
    if *truncated {
        return;
    }
    if let Some(p) = progress {
        let _ = p.send(format!("{line}\n"));
    }
    buf.push_str(line);
    buf.push('\n');
    if buf.len() > MAX_OUTPUT_BYTES {
        // 保留尾部，在字符边界截断头部
        let target = buf.len() - MAX_OUTPUT_BYTES;
        let mut cut = target;
        while cut < buf.len() && !buf.is_char_boundary(cut) {
            cut += 1;
        }
        buf.drain(..cut);
        *truncated = true;
    }
}

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
#[cfg(target_os = "windows")]
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

/// 构建命令：Windows 用 `cmd /C` + raw_arg（避免 Rust arg() 转义引号），
/// Unix 用 `sh -c`。用 `#[cfg]` 守卫而非 `cfg!()` 运行时检查，
/// 因为 `std::os::windows::process::CommandExt` 和 `raw_arg` 是编译时 Windows 专有 API。
#[cfg(target_os = "windows")]
fn build_shell_command(command: &str) -> tokio::process::Command {
    use std::os::windows::process::CommandExt;
    let mut std_cmd = std::process::Command::new("cmd");
    std_cmd.arg("/C").raw_arg(command);
    let safe_pathext = sanitize_pathext(&std::env::var("PATHEXT").unwrap_or_default());
    std_cmd.env("PATHEXT", safe_pathext);
    tokio::process::Command::from(std_cmd)
}

#[cfg(not(target_os = "windows"))]
fn build_shell_command(command: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("sh");
    c.arg("-c").arg(command);
    c
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
        // 非流式入口：progress=None → run_streaming 跳过推送（避免 channel 缓冲无限增长）
        self.run_streaming(input, ctx, None)
    }

    fn run_streaming<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
        progress: Option<UnboundedSender<String>>,
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
            let mut cmd = build_shell_command(command);
            // 强制子进程不缓冲 stdout/stderr：stdout 是 pipe（非 TTY）时，Python 等
            // 运行时默认块缓冲（4-8KB），导致 print() 输出攒满缓冲区或进程退出才到达
            // 我们的 BufReader::lines() → 流式读取看不到实时输出。
            // Claude Code 用 PTY 分配伪终端让子进程以为连着终端而行缓冲；本项目用 pipe，
            // 通过环境变量达到同等效果（PYTHONUNBUFFERED 对 Python 等价于 python -u）。
            // 放在用户 env 注入前，用户可在 Settings → Env 中覆盖。
            cmd.env("PYTHONUNBUFFERED", "1"); // Python: stdout/stderr 不缓冲
            // 注入用户在 Settings → Env 配置的环境变量（可覆盖上面的默认值）
            for (k, v) in &ctx.env {
                cmd.env(k, v);
            }
            cmd.current_dir(&ctx.cwd);
            // stdin=null：防止子进程（如 pwntools interactive()）继承 cyber 的终端 stdin，
            // 与 crossterm 事件循环抢控制台输入 → TUI 假死。
            // stdout/stderr=piped：逐行流式读取。
            // kill_on_drop：超时或 /cancel abort 任务时真正终止子进程，避免僵尸残留。
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);

            let mut child = cmd
                .spawn()
                .map_err(|e| AgentError::Provider(format!("执行命令失败: {e}")))?;
            let stdout = child.stdout.take().expect("stdout 已 piped");
            let stderr = child.stderr.take().expect("stderr 已 piped");
            let mut stdout_lines = BufReader::new(stdout).lines();
            let mut stderr_lines = BufReader::new(stderr).lines();

            let mut out_buf = String::new();
            let mut stdout_done = false;
            let mut stderr_done = false;
            let mut truncated = false;

            // 逐行读取 stdout/stderr，同时监听子进程退出。
            //
            // 关键：不能只等两路管道 EOF——Windows 上 `cmd /C` 常常 spawn 子进程
            // 继承管道句柄，主进程退出后管道仍被子进程持有 → `next_line()` 永远
            // 收不到 EOF → 挂起。解法：在 select! 中并发 `child.wait()`，进程退出
            // 后给 DRAIN_GRACE_MS 宽限期 drain 缓冲输出，超时则强制结束。
            let read_loop = async {
                let mut exit_status: Option<std::process::ExitStatus> = None;
                loop {
                    tokio::select! {
                        biased;
                        line = stdout_lines.next_line(), if !stdout_done => match line {
                            Ok(Some(l)) => push_output_line(&mut out_buf, &l, &progress, &mut truncated),
                            Ok(None) | Err(_) => stdout_done = true,
                        },
                        line = stderr_lines.next_line(), if !stderr_done => match line {
                            Ok(Some(l)) => push_output_line(&mut out_buf, &l, &progress, &mut truncated),
                            Ok(None) | Err(_) => stderr_done = true,
                        },
                        status = child.wait(), if exit_status.is_none() => {
                            match status {
                                Ok(s) => exit_status = Some(s),
                                Err(_) => break,
                            }
                        }
                    }
                    if stdout_done && stderr_done {
                        break;
                    }
                    if exit_status.is_some() {
                        // 进程已退出——drain 剩余输出（宽限期 DRAIN_GRACE_MS），然后强制结束。
                        let _ = tokio::time::timeout(
                            Duration::from_millis(DRAIN_GRACE_MS),
                            async {
                                loop {
                                    tokio::select! {
                                        biased;
                                        line = stdout_lines.next_line(), if !stdout_done => match line {
                                            Ok(Some(l)) => push_output_line(&mut out_buf, &l, &progress, &mut truncated),
                                            Ok(None) | Err(_) => stdout_done = true,
                                        },
                                        line = stderr_lines.next_line(), if !stderr_done => match line {
                                            Ok(Some(l)) => push_output_line(&mut out_buf, &l, &progress, &mut truncated),
                                            Ok(None) | Err(_) => stderr_done = true,
                                        },
                                    }
                                    if stdout_done && stderr_done { break; }
                                }
                            },
                        ).await;
                        break;
                    }
                }
                exit_status
            };

            // 超时兜底：interactive() 等永不退出的命令不能让工具调用永久挂起。
            match tokio::time::timeout(Duration::from_secs(SHELL_TIMEOUT_SECS), read_loop).await {
                Ok(exit_status_opt) => {
                    // 管道可能先于进程关闭（exit_status_opt=None），此时需 wait 兜底
                    let status = match exit_status_opt {
                        Some(s) => s,
                        None => child
                            .wait()
                            .await
                            .map_err(|e| AgentError::Provider(format!("等待命令失败: {e}")))?,
                    };
                    let code = status.code().unwrap_or(-1);
                    let mut content = std::mem::take(&mut out_buf);
                    if truncated {
                        content.push_str(&format!(
                            "\n[输出超过 {MAX_OUTPUT_BYTES} 字节，已截断保留尾部]"
                        ));
                    }
                    if content.is_empty() {
                        content.push_str(&format!("（命令退出码 {code}，无输出）"));
                    }
                    // 非零退出码视为错误（但内容仍回灌，让 LLM 看到错误信息）
                    let is_error = code != 0;
                    Ok(ToolOutput { content, is_error })
                }
                Err(_) => {
                    // 超时：read_loop 被 drop（释放对 out_buf 的借用）；child 在返回时
                    // drop → kill_on_drop 终止子进程。保留已累积的部分输出。
                    let mut content = std::mem::take(&mut out_buf);
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(&format!(
                        "[命令执行超时（{SHELL_TIMEOUT_SECS}s），已终止子进程]"
                    ));
                    Ok(ToolOutput { content, is_error: true })
                }
            }
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
