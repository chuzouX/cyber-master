//! Cyber Master 入口。
//!
//! 启动流程（对应 DESIGN §2.3 启动状态机）：
//! 1. clap 解析 CLI 参数
//! 2. 初始化 tracing 日志
//! 3. `load_app_context` 加载配置（首次初始化 `~/.cyber` + 三层合并 + `.cyber.md`）
//! 4. 建 tokio 通道（agent 事件回传），按是否有项目上下文路由初始模式
//! 5. 进入 ratatui TUI 异步主循环（`tokio::select!` 事件总线）

use std::path::PathBuf;

use clap::Parser;
use cyber_agent::AgentEvent;
use cyber_core::load_app_context;
use cyber_tui::{build_registries, App, AppPaths, FetchResult, McpServersConfig, Mode};
use tokio::sync::mpsc;

/// Cyber Master CLI 参数。
#[derive(Parser, Debug)]
#[command(
    name = "cyber",
    version,
    about = "网络安全智能体终端（对话 + 工作流 DAG）"
)]
struct Cli {
    /// 工作目录（默认当前目录，决定 `.cyber.md` / `.cyber/` 检测位置）
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// 日志级别（覆盖 RUST_LOG，如 debug/info/warn）
    #[arg(long)]
    log_level: Option<String>,

    /// 离线 Mock 模式：强制使用 MockProvider，无需联网/API key（用于冒烟测试）
    #[arg(long)]
    mock: bool,
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    // tracing：RUST_LOG 优先，否则用 --log-level，最终回退 info
    let default_filter = cli.log_level.as_deref().unwrap_or("info");
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));

    let cwd = match cli.cwd {
        Some(c) => c,
        None => std::env::current_dir()?,
    };

    let ctx = load_app_context(&cwd)?;

    // 日志写文件（~/.cyber/logs/cyber.log），不输出到终端——避免干扰 TUI 渲染。
    // 启动早期（load_app_context 之前）的日志丢弃，无碍。
    let log_file = ctx.paths.logs_dir.join("cyber.log");
    let _ = std::fs::create_dir_all(&ctx.paths.logs_dir);
    match std::fs::OpenOptions::new().create(true).append(true).open(&log_file) {
        Ok(f) => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(f)
                .init();
        }
        Err(e) => {
            eprintln!("警告：无法打开日志文件 {}: {e}，回退 stderr（可能干扰 TUI）", log_file.display());
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .init();
        }
    }

    // mock：CLI flag 或环境变量 CYBER_MOCK_PROVIDER=1
    let mock = cli.mock || std::env::var("CYBER_MOCK_PROVIDER").is_ok_and(|v| v == "1");

    // 启动状态机：有项目上下文 → 按 config.ui.default_mode 选择初始模式；无 → Welcome
    let initial_mode = if ctx.project.is_some() {
        match ctx.config.ui.default_mode.as_str() {
            "workflow" => Mode::Workflow,
            "dashboard" => Mode::Dashboard,
            _ => Mode::Chat,
        }
    } else {
        Mode::Welcome
    };

    // 检测项目级 .cyber/config.toml 是否存在（Settings 页据此显示覆盖提示横幅）
    let has_project_config = cwd.join(".cyber").join("config.toml").exists();

    // agent 事件通道：tx 给 App（每次 spawn_agent clone），rx 给 main_loop select!
    // 携带 (gen, AgentEvent) 元组：generation 计数器隔离 cancel 后的 stale 事件。
    let (agent_tx, agent_rx) = mpsc::unbounded_channel::<(u64, AgentEvent)>();
    // 模型拉取通道：tx 给 App（每次 start_provider_fetch clone），rx 给 main_loop 第 4 路 select!
    let (fetch_tx, fetch_rx) = mpsc::unbounded_channel::<FetchResult>();

    tracing::info!(
        cwd = %cwd.display(),
        first_run = ctx.is_first_run,
        has_project = ctx.project.is_some(),
        has_project_config,
        initial_mode = ?initial_mode,
        mock,
        "启动 TUI"
    );

    // 加载 MCP servers 配置（~/.cyber/mcp/servers.toml）。文件不存在 → 空 config。
    // 注意在 `paths` move 前从 ctx.paths 借用加载，避免后续 borrow 冲突。
    let mcp_config = McpServersConfig::load(&ctx.paths.mcp_servers_file).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "MCP servers.toml 加载失败，使用空配置降级");
        McpServersConfig::default()
    });

    let paths = AppPaths {
        config_file: ctx.paths.config_file.clone(),
        providers_file: ctx.paths.providers_file.clone(),
        mcp_servers_file: ctx.paths.mcp_servers_file.clone(),
        log_file: log_file.clone(),
        history_dir: ctx.paths.history_dir.clone(),
        ctf_dir: ctx.paths.ctf_dir.clone(),
        ctf_writeup_dir: ctx.paths.ctf_writeup_dir.clone(),
        memory_file: ctx.paths.memory_file.clone(),
        cwd,
    };

    // 构建统一工具表（builtins + Skills + MCP）。注意在 `paths` move 前 borrow ctx.paths + cwd。
    // mock 模式跳过 MCP 连接。boot_errors 经 toast 展示（降级为仅可用部分，不阻断启动）。
    let (registries, boot_errors) = build_registries(&ctx.paths, &paths.cwd, mock).await;
    for e in &boot_errors {
        tracing::warn!(error = %e, "启动注册表构建警告");
    }

    let mut app = App::new(
        ctx.config,
        ctx.providers,
        mcp_config,
        ctx.project,
        initial_mode,
        ctx.is_first_run,
        paths,
        has_project_config,
        mock,
        agent_tx,
        fetch_tx,
        registries,
    );
    if !boot_errors.is_empty() {
        app.set_toast(format!("启动警告：{}", boot_errors.join("; ")));
    }
    app.run(agent_rx, fetch_rx).await?;

    Ok(())
}
