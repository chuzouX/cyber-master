//! 后台任务注册表：AI 启动的脚本（`bg_shell`）与后台子代理（`delegate_tasks`
//! `background=true`、`/bg run`）的会话级登记处。
//!
//! 任务在 detached tokio task 中运行，输出逐行写 `lines`（超 500 行丢最旧），
//! 结束定稿状态。CLI 后台面板（Ctrl+B）读 `snapshot()`；子代理完成后由 CLI
//! 事件循环 `take_unreported_completions()` 注入会话。进程退出即弃，不跨会话持久化。

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;

use cyber_core::{Config, ProjectContext, ProvidersConfig, ThinkingIntensity};

use crate::agent::SubagentRuntime;
use crate::subagent::{SubagentArchive, SubagentRun, SubagentStatus};
use crate::tool::{ToolCtx, ToolRegistry};
use crate::tools::guard::check_command;
use crate::types::AgentEvent;

/// 后台任务类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Shell,
    Subagent,
}

/// 后台任务生命周期状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Finished(i32),
    Killed,
    Failed(String),
}

impl JobStatus {
    /// 是否已结束（不可再 kill）。
    pub fn is_finished(&self) -> bool {
        !matches!(self, JobStatus::Running)
    }
}

/// 一条后台任务记录。
#[derive(Debug)]
pub struct BackgroundJob {
    pub id: u64,
    pub kind: JobKind,
    pub name: String,
    pub status: JobStatus,
    /// 输出日志行（最多保留最近 500 行）。
    pub lines: Vec<String>,
    pub created: Instant,
    /// 结束结果是否已注入会话（仅 `JobKind::Subagent` 会注入）。
    pub reported: bool,
    /// kill 信号发送端；`kill()` 取走并触发（Running 才有意义）。
    pub kill: Option<oneshot::Sender<()>>,
    /// 关联的 `SubagentArchive` run id（仅 `JobKind::Subagent`；面板据此展示转录）。
    pub archive_run_id: Option<u64>,
}

/// 快照 clone 不携带 kill 发送端（终止只能走 `BackgroundRegistry::kill` 原对象）。
impl Clone for BackgroundJob {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            kind: self.kind,
            name: self.name.clone(),
            status: self.status.clone(),
            lines: self.lines.clone(),
            created: self.created,
            reported: self.reported,
            kill: None,
            archive_run_id: self.archive_run_id,
        }
    }
}

/// 日志行数上限（超限丢最旧）。
const MAX_LINES: usize = 500;

/// 输出 drain 宽限期（毫秒）：进程退出后给管道缓冲的交割时间（同 shell 工具）。
const DRAIN_GRACE_MS: u64 = 500;

/// 后台任务注册表。所有方法对整个 `Mutex<Vec<_>>` 加锁（任务量为个位数级）。
#[derive(Default)]
pub struct BackgroundRegistry {
    jobs: Mutex<Vec<BackgroundJob>>,
    next_id: AtomicU64,
}

impl BackgroundRegistry {
    /// 新建 Running 条目，返回 `(job_id, kill 信号接收端)`。
    pub fn start(&self, kind: JobKind, name: String) -> (u64, oneshot::Receiver<()>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (kill_tx, kill_rx) = oneshot::channel();
        let job = BackgroundJob {
            id,
            kind,
            name,
            status: JobStatus::Running,
            lines: Vec::new(),
            created: Instant::now(),
            reported: false,
            kill: Some(kill_tx),
            archive_run_id: None,
        };
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.push(job);
        }
        (id, kill_rx)
    }

    /// 追加一行日志（超 `MAX_LINES` 丢最旧）。
    pub fn append_line(&self, id: u64, line: String) {
        if let Ok(mut jobs) = self.jobs.lock() {
            if let Some(job) = jobs.iter_mut().find(|job| job.id == id) {
                job.lines.push(line);
                if job.lines.len() > MAX_LINES {
                    let drop = job.lines.len() - MAX_LINES;
                    job.lines.drain(..drop);
                }
            }
        }
    }

    /// 更新任务状态。
    pub fn set_status(&self, id: u64, status: JobStatus) {
        if let Ok(mut jobs) = self.jobs.lock() {
            if let Some(job) = jobs.iter_mut().find(|job| job.id == id) {
                job.status = status;
            }
        }
    }

    /// 关联子代理转录条目（面板详情据此展示 `SubagentArchive` 转录）。
    pub fn link_archive(&self, id: u64, archive_run_id: u64) {
        if let Ok(mut jobs) = self.jobs.lock() {
            if let Some(job) = jobs.iter_mut().find(|job| job.id == id) {
                job.archive_run_id = Some(archive_run_id);
            }
        }
    }

    /// 全部任务快照（clone，供渲染）。
    pub fn snapshot(&self) -> Vec<BackgroundJob> {
        self.jobs
            .lock()
            .map(|jobs| jobs.clone())
            .unwrap_or_default()
    }

    /// 取走「已结束且未注入」的子代理任务快照，并置 `reported=true`（不重复注入）。
    pub fn take_unreported_completions(&self) -> Vec<BackgroundJob> {
        let mut taken = Vec::new();
        if let Ok(mut jobs) = self.jobs.lock() {
            for job in jobs.iter_mut() {
                if job.status.is_finished() && !job.reported {
                    job.reported = true;
                    taken.push(job.clone());
                }
            }
        }
        taken
    }

    /// 终止运行中的任务：触发 kill oneshot。非 Running 或信号已发返回 false。
    pub fn kill(&self, id: u64) -> bool {
        if let Ok(mut jobs) = self.jobs.lock() {
            if let Some(job) = jobs.iter_mut().find(|job| job.id == id) {
                if matches!(job.status, JobStatus::Running) {
                    if let Some(sender) = job.kill.take() {
                        return sender.send(()).is_ok();
                    }
                }
            }
        }
        false
    }

    /// 移除已结束任务（Running 返回 false 不移除）。
    pub fn remove(&self, id: u64) -> bool {
        if let Ok(mut jobs) = self.jobs.lock() {
            if let Some(index) = jobs.iter().position(|job| job.id == id) {
                if jobs[index].status.is_finished() {
                    jobs.remove(index);
                    return true;
                }
            }
        }
        false
    }

    /// 终止全部运行中任务（进程退出清理；detached task 持 clone，Drop 不会触发）。
    pub fn kill_all(&self) {
        if let Ok(mut jobs) = self.jobs.lock() {
            for job in jobs.iter_mut() {
                if matches!(job.status, JobStatus::Running) {
                    if let Some(sender) = job.kill.take() {
                        let _ = sender.send(());
                    }
                }
            }
        }
    }
}

/// 按字符截断（字符边界安全）。超 `max` 补 `…`。
pub(crate) fn truncate_chars(value: &str, max: usize) -> String {
    let mut end = value.len().min(max);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = value[..end].to_string();
    if end < value.len() {
        result.push('…');
    }
    result
}

/// 读取一行（字节级，lossy UTF-8；流为 None 时永久 pending，供 select! 分支使用）。
///
/// 不用 `BufReader::lines()`：其 `read_line` 严格要求合法 UTF-8，中文 Windows 的
/// 程序（如 `ping`）输出 GBK 编码时会报 `InvalidData`，导致全部输出被丢弃。
async fn next_line<L>(lines: &mut Option<BufReader<L>>) -> std::io::Result<Option<String>>
where
    L: tokio::io::AsyncRead + Unpin,
{
    match lines.as_mut() {
        Some(lines) => {
            let mut buf = Vec::new();
            let read = lines.read_until(b'\n', &mut buf).await?;
            if read == 0 {
                return Ok(None);
            }
            while matches!(buf.last(), Some(b'\n' | b'\r')) {
                buf.pop();
            }
            Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
        }
        None => std::future::pending().await,
    }
}

/// 启动一个后台 shell 任务（`bg_shell` 工具与 `/bg shell` 共用的核心实现）。
///
/// 先过 `check_command` 护栏（denylist 子串），再 detached 运行：
/// 输出逐行写 registry，kill 信号 / 自然退出定稿状态。
pub fn spawn_shell_job(
    registry: &Arc<BackgroundRegistry>,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    command: String,
) -> Result<u64, String> {
    // 与 BgShellTool 一致的护栏（无 project rules/scope 的裸 ctx）。
    let ctx = ToolCtx::new(cwd.clone(), Vec::new(), None, env.clone());
    check_command(&command, &ctx)?;

    let name = truncate_chars(&command, 80);
    let (job_id, mut kill_rx) = registry.start(JobKind::Shell, name);

    let mut cmd = crate::tools::shell::build_shell_command(&command);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    cmd.env("PYTHONUNBUFFERED", "1");
    for (key, value) in &env {
        cmd.env(key, value);
    }
    cmd.current_dir(&cwd);
    let mut child = cmd.spawn().map_err(|error| {
        registry.set_status(job_id, JobStatus::Failed(error.to_string()));
        format!("执行命令失败: {error}")
    })?;

    let registry = Arc::clone(registry);
    tokio::spawn(async move {
        let mut stdout_lines = child.stdout.take().map(BufReader::new);
        let mut stderr_lines = child.stderr.take().map(BufReader::new);
        let mut stdout_done = stdout_lines.is_none();
        let mut stderr_done = stderr_lines.is_none();
        let mut killed = false;
        let mut code: Option<i32> = None;
        loop {
            tokio::select! {
                biased;
                _ = &mut kill_rx => {
                    let _ = child.kill().await;
                    killed = true;
                    break;
                }
                line = next_line(&mut stdout_lines), if !stdout_done => match line {
                    Ok(Some(l)) => registry.append_line(job_id, l),
                    Ok(None) | Err(_) => stdout_done = true,
                },
                line = next_line(&mut stderr_lines), if !stderr_done => match line {
                    Ok(Some(l)) => registry.append_line(job_id, format!("[stderr] {l}")),
                    Ok(None) | Err(_) => stderr_done = true,
                },
                status = child.wait(), if code.is_none() => {
                    if let Ok(status) = status {
                        code = status.code();
                    }
                }
            }
            if stdout_done && stderr_done {
                // 管道先于 `child.wait()` 到达 EOF：这里补一次 wait 拿真实退出码。
                if code.is_none() {
                    if let Ok(status) = child.wait().await {
                        code = status.code();
                    }
                }
                break;
            }
            if code.is_some() {
                // 进程已退出：drain 缓冲输出（宽限期），然后结束。
                let _ = tokio::time::timeout(Duration::from_millis(DRAIN_GRACE_MS), async {
                    while !(stdout_done && stderr_done) {
                        tokio::select! {
                            biased;
                            _ = &mut kill_rx => {
                                let _ = child.kill().await;
                                killed = true;
                                break;
                            }
                            line = next_line(&mut stdout_lines), if !stdout_done => match line {
                                Ok(Some(l)) => registry.append_line(job_id, l),
                                Ok(None) | Err(_) => stdout_done = true,
                            },
                            line = next_line(&mut stderr_lines), if !stderr_done => match line {
                                Ok(Some(l)) => registry.append_line(job_id, format!("[stderr] {l}")),
                                Ok(None) | Err(_) => stderr_done = true,
                            },
                        }
                    }
                })
                .await;
                break;
            }
        }
        if killed {
            registry.append_line(job_id, "[已终止]".into());
            registry.set_status(job_id, JobStatus::Killed);
        } else {
            registry.set_status(job_id, JobStatus::Finished(code.unwrap_or(-1)));
        }
    });
    Ok(job_id)
}

/// 启动一个后台子代理（`/bg run` 入口，`delegate_tasks background=true` 走工具内路径）。
///
/// 立即返回 job id；detached task 内 `run_task` 全权执行（无外层超时，
/// 靠 `bg_kill` / 面板 `k` 终止）。结束写 `SubagentArchive` 定稿 +
/// registry 状态，结果由 CLI 注入当前会话。
#[allow(clippy::too_many_arguments)]
pub fn spawn_background_subagent(
    config: Config,
    providers: ProvidersConfig,
    project: Option<ProjectContext>,
    mock: bool,
    cwd: PathBuf,
    registry: Arc<ToolRegistry>,
    ctf_enabled: bool,
    intensity: ThinkingIntensity,
    memory: String,
    prompt: String,
    archive: Option<Arc<SubagentArchive>>,
    background: Arc<BackgroundRegistry>,
) -> u64 {
    let name = truncate_chars(prompt.trim(), 80);
    let (job_id, kill_rx) = background.start(JobKind::Subagent, name.clone());
    let run_id = archive.as_ref().map(|a| a.start(&name));
    if let Some(run_id) = run_id {
        background.link_archive(job_id, run_id);
    }
    let transcript = run_id.and_then(|rid| archive.as_ref().and_then(|a| a.run_handle(rid)));

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<(u64, AgentEvent)>();
    let runtime = Arc::new(SubagentRuntime::new(
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
        0,
        archive.clone(),
        Some(Arc::clone(&background)),
    ));
    // 除 delegate_tasks（禁止递归委派）与禁用联网时的 web_fetch 外的全部工具。
    // 必须与 `run_task` 内部的 schema 过滤一致（web_search 关闭时剔除 web_fetch），
    // 否则白名单长度不匹配 → "subagent tool allowlist contains an unknown tool"。
    let web_search = runtime.config().tools.web_search;
    let tool_names: Vec<String> = runtime
        .registry()
        .all_schemas()
        .into_iter()
        .map(|schema| schema.name)
        .filter(|name| name != "delegate_tasks")
        .filter(|name| web_search || name != "web_fetch")
        .collect();
    let specialist_prompt =
        "You are a specialist subagent running in the background. The user cannot interact with \
         you; work autonomously with the available tools and finish with a concise, \
         evidence-based result."
            .to_string();

    tokio::spawn(run_detached_subagent(
        runtime,
        specialist_prompt,
        prompt,
        String::new(),
        None,
        None,
        tool_names,
        transcript,
        archive,
        background,
        job_id,
        run_id,
        kill_rx,
        None,
    ));
    job_id
}

/// detached 子代理执行体（`/bg run` 与 `delegate_tasks background=true` 共用）：
/// `run_task` 与 kill 信号 select!，结束按状态定稿 archive + registry。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_detached_subagent(
    runtime: Arc<SubagentRuntime>,
    specialist_prompt: String,
    task: String,
    context: String,
    provider: Option<String>,
    model: Option<String>,
    tools: Vec<String>,
    transcript: Option<Arc<Mutex<SubagentRun>>>,
    archive: Option<Arc<SubagentArchive>>,
    background: Arc<BackgroundRegistry>,
    job_id: u64,
    run_id: Option<u64>,
    mut kill_rx: oneshot::Receiver<()>,
    custom_max_steps: Option<u32>,
) {
    let run_fut = runtime.run_task(
        specialist_prompt,
        task,
        context,
        provider,
        model,
        tools,
        None,
        transcript,
        custom_max_steps,
    );
    tokio::pin!(run_fut);
    tokio::select! {
        _ = &mut kill_rx => {
            if let Some(archive) = &archive {
                if let Some(run_id) = run_id {
                    archive.finish(run_id, SubagentStatus::Killed, None, Some("killed by user".into()));
                }
            }
            background.append_line(job_id, "[已终止]".into());
            background.set_status(job_id, JobStatus::Killed);
        }
        result = &mut run_fut => {
            match result {
                Ok(output) => {
                    let (status, error_msg) = match output.exit_reason {
                        crate::agent::AgentExitReason::Finished => (SubagentStatus::Completed, None),
                        crate::agent::AgentExitReason::MaxStepsReached => (
                            SubagentStatus::StepLimitReached,
                            Some("step limit reached".to_string()),
                        ),
                        crate::agent::AgentExitReason::LoopDetected => (
                            SubagentStatus::LoopDetected,
                            Some("loop detected".to_string()),
                        ),
                    };
                    if let Some(archive) = &archive {
                        if let Some(run_id) = run_id {
                            archive.finish(run_id, status, Some(output.output.clone()), error_msg);
                        }
                    }
                    background.append_line(job_id, truncate_chars(&output.output, 400));
                    background.set_status(job_id, JobStatus::Finished(0));
                }
                Err(error) => {
                    if let Some(archive) = &archive {
                        if let Some(run_id) = run_id {
                            archive.finish(run_id, SubagentStatus::Error, None, Some(error.to_string()));
                        }
                    }
                    background.append_line(job_id, format!("[error] {error}"));
                    background.set_status(job_id, JobStatus::Failed(error.to_string()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_assigns_unique_ids_and_kill_channel() {
        let registry = BackgroundRegistry::default();
        let (a, _rx_a) = registry.start(JobKind::Shell, "a".into());
        let (b, _rx_b) = registry.start(JobKind::Subagent, "b".into());
        assert_ne!(a, b);
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot
            .iter()
            .all(|job| matches!(job.status, JobStatus::Running)));
        // kill 信号只存在于 registry 原对象（快照 clone 不带发送端）
        assert!(registry.kill(a));
        assert!(!registry.kill(a), "kill 发送端已被取走");
    }

    #[test]
    fn kill_only_works_on_running_and_remove_only_on_finished() {
        let registry = BackgroundRegistry::default();
        let (id, _kill_rx) = registry.start(JobKind::Shell, "s".into());
        assert!(registry.kill(id));
        // kill 已发（sender 已取走）→ 再次 kill 返回 false
        assert!(!registry.kill(id));
        // Running 不可移除
        assert!(!registry.remove(id));
        registry.set_status(id, JobStatus::Finished(0));
        assert!(registry.remove(id));
        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn take_unreported_completions_marks_reported() {
        let registry = BackgroundRegistry::default();
        let (id, _kill_rx) = registry.start(JobKind::Subagent, "bg".into());
        assert!(registry.take_unreported_completions().is_empty());
        registry.set_status(id, JobStatus::Finished(0));
        let taken = registry.take_unreported_completions();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].id, id);
        // 第二次不再重复返回
        assert!(registry.take_unreported_completions().is_empty());
    }

    #[test]
    fn append_line_drops_oldest_beyond_cap() {
        let registry = BackgroundRegistry::default();
        let (id, _kill_rx) = registry.start(JobKind::Shell, "cap".into());
        for i in 0..(MAX_LINES + 5) {
            registry.append_line(id, format!("line-{i}"));
        }
        let job = &registry.snapshot()[0];
        assert_eq!(job.lines.len(), MAX_LINES);
        assert_eq!(job.lines.first().unwrap(), "line-5");
    }

    #[tokio::test]
    async fn spawn_shell_job_runs_to_completion() {
        let registry = Arc::new(BackgroundRegistry::default());
        let cmd = "echo cyber_bg_marker";
        let id = spawn_shell_job(&registry, std::env::temp_dir(), Vec::new(), cmd.into()).unwrap();
        // 轮询直到结束（echo 秒退）
        for _ in 0..200 {
            let status = registry
                .snapshot()
                .into_iter()
                .find(|job| job.id == id)
                .map(|job| job.status)
                .unwrap_or(JobStatus::Running);
            if status.is_finished() {
                assert!(matches!(status, JobStatus::Finished(0)));
                let job = registry
                    .snapshot()
                    .into_iter()
                    .find(|job| job.id == id)
                    .unwrap();
                assert!(job
                    .lines
                    .iter()
                    .any(|line| line.contains("cyber_bg_marker")));
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("shell 任务未在预期时间内结束");
    }

    #[tokio::test]
    async fn spawn_shell_job_kill_terminates() {
        let registry = Arc::new(BackgroundRegistry::default());
        let cmd = if cfg!(windows) {
            "ping 127.0.0.1 -n 60"
        } else {
            "sleep 60"
        };
        let id = spawn_shell_job(&registry, std::env::temp_dir(), Vec::new(), cmd.into()).unwrap();
        assert!(registry.kill(id));
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
    async fn spawn_shell_job_rejects_guard_denied() {
        let registry = Arc::new(BackgroundRegistry::default());
        let error = spawn_shell_job(
            &registry,
            std::env::temp_dir(),
            Vec::new(),
            "rm -rf /".into(),
        )
        .unwrap_err();
        assert!(error.contains("安全护栏"), "护栏应拒绝：{error}");
        assert!(registry.snapshot().is_empty());
    }

    #[tokio::test]
    async fn spawn_shell_job_captures_ping_output() {
        // 回归：用户报告 `ping 127.0.0.1` 完成(0) 但日志为空。
        let registry = Arc::new(BackgroundRegistry::default());
        let cmd = if cfg!(windows) {
            "ping 127.0.0.1"
        } else {
            "ping -c 2 127.0.0.1"
        };
        let id = spawn_shell_job(&registry, std::env::temp_dir(), Vec::new(), cmd.into()).unwrap();
        for _ in 0..1000 {
            let job = registry
                .snapshot()
                .into_iter()
                .find(|job| job.id == id)
                .expect("job 应存在");
            if job.status.is_finished() {
                assert!(
                    matches!(job.status, JobStatus::Finished(0)),
                    "ping 应退出码 0：{:?}",
                    job.status
                );
                assert!(!job.lines.is_empty(), "ping 输出应被捕获，实际 lines 为空");
                let joined = job.lines.join("\n").to_lowercase();
                // GBK 输出经 lossy 转码为乱码，断言用与编码无关的稳定片段。
                assert!(
                    joined.contains("127.0.0.1")
                        || joined.contains("ttl=")
                        || joined.contains("<1ms"),
                    "应含 ping 输出：{:?}",
                    job.lines
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("ping 任务未在预期时间内结束");
    }

    #[tokio::test]
    async fn spawn_background_subagent_respects_disabled_web_search() {
        // 回归：web_search=false 时 web_fetch 仍在 registry，但 run_task 会剔除它；
        // 白名单必须同步剔除，否则报 "allowlist contains an unknown tool"。
        let mut config = Config::default();
        config.tools.web_search = false;
        let registry = Arc::new(ToolRegistry::with_builtins());
        assert!(
            registry.all_schemas().iter().any(|s| s.name == "web_fetch"),
            "前置：registry 应含 web_fetch"
        );
        let archive = Arc::new(SubagentArchive::default());
        let background = Arc::new(BackgroundRegistry::default());
        let id = spawn_background_subagent(
            config,
            ProvidersConfig::default_template(),
            None,
            true,
            std::env::temp_dir(),
            registry,
            false,
            ThinkingIntensity::Middle,
            String::new(),
            "probe".into(),
            Some(archive),
            background.clone(),
        );
        for _ in 0..500 {
            let status = background
                .snapshot()
                .into_iter()
                .find(|job| job.id == id)
                .map(|job| job.status)
                .unwrap_or(JobStatus::Running);
            if status.is_finished() {
                assert!(
                    matches!(&status, JobStatus::Finished(_)),
                    "web_search=false 时后台子代理应正常完成，实际：{status:?}"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("后台子代理未在预期时间内结束");
    }
}
