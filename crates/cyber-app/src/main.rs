//! Cyber Master 入口。
//!
//! 启动流程（对应 DESIGN §2.3 启动状态机）：
//! 1. clap 解析 CLI 参数
//! 2. 初始化 tracing 日志
//! 3. `load_app_context` 加载配置（首次初始化 `~/.cyber` + 三层合并 + `.cyber.md`）
//! 4. 按是否有项目上下文路由初始模式：有 → Chat；无 → Welcome
//! 5. 进入 ratatui TUI 主循环

use std::path::PathBuf;

use clap::Parser;
use cyber_core::load_app_context;
use cyber_tui::{App, Mode};

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
}

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    // tracing：RUST_LOG 优先，否则用 --log-level，最终回退 info
    let default_filter = cli.log_level.as_deref().unwrap_or("info");
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let cwd = match cli.cwd {
        Some(c) => c,
        None => std::env::current_dir()?,
    };

    let ctx = load_app_context(&cwd)?;

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

    tracing::info!(
        cwd = %cwd.display(),
        first_run = ctx.is_first_run,
        has_project = ctx.project.is_some(),
        has_project_config,
        initial_mode = ?initial_mode,
        "启动 TUI"
    );

    App::new(
        ctx.config,
        ctx.providers,
        ctx.project,
        initial_mode,
        ctx.is_first_run,
        ctx.paths.config_file.clone(),
        has_project_config,
    )
    .run()?;

    Ok(())
}
