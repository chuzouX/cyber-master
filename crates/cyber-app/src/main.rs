//! Cyber Master 入口。
//!
//! 运行模式：
//! 1. `cyber`（无子命令）：启动全屏 coding CLI
//! 2. `cyber run "<prompt>"`：headless 非交互执行一次 agent 任务，可被外部 agent/脚本接管
//! 3. `cyber tui`：启动全屏 TUI；`cyber setup`：全屏设置向导（服务商 / 工具库）
//!
//! TUI 启动流程（对应 DESIGN §2.3 启动状态机）：
//! 1. clap 解析 CLI 参数
//! 2. 初始化 tracing 日志
//! 3. `load_app_context` 加载配置（首次初始化 `~/.cyber` + 三层合并 + `.cyber.md`）
//! 4. 建 tokio 通道（agent 事件回传），按是否有项目上下文路由初始模式
//! 5. 进入 ratatui TUI 异步主循环（`tokio::select!` 事件总线）

mod setup;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use cyber_agent::AgentEvent;
use cyber_core::load_app_context;
use cyber_tui::{
    build_registries, run_cli, run_headless, App, AppPaths, FetchResult, HeadlessArgs,
    HeadlessOutcome, McpServersConfig, Mode,
};
use tokio::sync::mpsc;

/// Cyber Master CLI 参数。
#[derive(Parser, Debug)]
#[command(
    name = "cyber",
    version,
    about = "网络安全智能体终端：代码交互、任务编排与 CTF 协作",
    long_about = "Cyber Master 是基于 Rust 构建的高性能网络安全智能体终端。\n\n\
                 运行模式：\n  \
                 cyber                   默认交互模式：全屏简洁 coding CLI，支持流式对话、\n                          \
                 工具调用（Shell/文件/搜索等）、Todo 任务拆解与子 Agent 并行委派\n  \
                 cyber tui               全屏多面板模式：包含 CTF 题目协作、系统设置与多视图切换\n  \
                 cyber run \"<prompt>\"    Headless 非交互模式：单次执行任务（支持流式文本或结构化 JSON）\n  \
                 cyber setup             全屏设置向导：服务商 / API Key / 模型 / 工具库（Ctrl+S 保存，Esc 完成）\n  \
                 cyber update            检查并升级 Cyber Master 到最新版本",
    after_help = "常用示例:\n  \
                  cyber                         # 启动默认交互式 Coding CLI\n  \
                  cyber --mock                  # 以离线模拟模式启动（免配置 API 密钥快速体验）\n  \
                  cyber tui                     # 启动全屏 TUI 面板（CTF 题目/设置/多视图）\n  \
                  cyber setup                   # 打开全屏设置向导（服务商 / 模型 / 工具库）\n  \
                  cyber update                  # 检查是否有新版本及升级指南\n  \
                  cyber run \"总结当前目录结构\" # 单次执行任务并流式输出结果到终端\n  \
                  cyber run \"检查代码\" --format json --allow-tool list_dir,read_file\n  \
                  cyber run \"继续分析\" --session <id>  # 续接指定历史会话\n\n\
                  文档与命令参考:\n  \
                  https://github.com/chuzouX/cyber-master\n  \
                  docs/TUI_COMMANDS.md",
    subcommand_help_heading = "子命令",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Cli {
    /// 工作目录（默认当前目录，决定 `.cyber.md` / `.cyber/` 检测位置）
    #[arg(long, global = true, value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// 日志级别（trace | debug | info | warn | error，覆盖 RUST_LOG）
    #[arg(long, global = true, value_name = "LEVEL")]
    log_level: Option<String>,

    /// 离线 Mock 模式：强制使用 MockProvider，无需联网/API Key（用于冒烟测试与离线体验）
    #[arg(long, global = true)]
    mock: bool,

    /// 打印帮助信息
    #[arg(short = 'h', long = "help", action = clap::ArgAction::Help, global = true)]
    help: Option<bool>,

    /// 打印版本信息
    #[arg(short = 'V', long = "version", action = clap::ArgAction::Version)]
    version: Option<bool>,

    /// 子命令
    #[command(subcommand)]
    command: Option<Command>,
}

/// 子命令。
#[derive(Subcommand, Debug)]
enum Command {
    /// 启动全屏 TUI 多面板模式（包含 CTF 题目协作、系统设置、模型管理等）
    Tui,
    /// 打开全屏设置向导（服务商 / API Key / 模型 / 工具库；Ctrl+S 保存，Esc 完成）
    Setup,
    /// 单次非交互执行 agent 任务（headless 自动化模式，支持流式文本或 JSON 输出）
    #[command(
        about = "单次非交互执行 agent 任务（headless 模式，可被外部脚本接管）",
        long_about = "在命令行中单次执行 agent 任务，支持纯文本流式输出或结构化 JSON 输出。\n\n\
                     适用于自动化脚本、CI/CD 流程或作为外部工具集成。",
        after_help = "示例:\n  \
                      cyber run \"解释这个概念\"\n  \
                      cyber run \"列出当前目录\" --allow-tool list_dir\n  \
                      cyber run \"分析代码隐患\" --format json --allow-tool read_file,list_dir\n  \
                      cyber run \"继续分析\" --session <session-id>\n  \
                      cyber run \"离线测试\" --mock --allow-tool list_dir"
    )]
    Run(RunArgs),
    /// 检查并升级 Cyber Master 到最新版本
    #[command(
        about = "检查并升级 Cyber Master 到最新版本",
        long_about = "检查 GitHub 仓库的最新发布版本。如发现新版本，可显示更新日志并提供升级命令；\n\
                     支持传入 `--check` 仅检查新版本信息，或 `--apply` 自动在当前源码仓库下拉取编译。",
        after_help = "示例:\n  \
                      cyber update          # 检查更新并提示升级指南\n  \
                      cyber update --check  # 仅检查是否有新版本并输出版本号\n  \
                      cyber update --apply  # 检查并在当前 git 仓库下自动执行 git pull && cargo build --release"
    )]
    Update(UpdateArgs),
}

/// `cyber run` 参数。
#[derive(clap::Args, Debug)]
struct RunArgs {
    /// 任务描述（用户 prompt）
    #[arg(value_name = "PROMPT")]
    prompt: String,

    /// 输出格式：text（流式 Markdown）| json（结构化，含工具调用与 token 用量）
    #[arg(
        long,
        value_enum,
        default_value_t = OutputFormat::Text,
        value_name = "FORMAT"
    )]
    format: OutputFormat,

    /// 续接指定 session id（默认续接当前 session）
    #[arg(long, value_name = "ID")]
    session: Option<String>,

    /// 新建会话（忽略历史）
    #[arg(long)]
    new: bool,

    /// 最大工具调用步数（覆盖 config）
    #[arg(long, value_name = "STEPS")]
    max_steps: Option<u32>,

    /// 思考强度（覆盖 config：low / middle / high / max / auto）
    #[arg(long, value_name = "LEVEL")]
    think: Option<String>,

    /// 指定 provider（providers.toml 中的名称，覆盖 default_provider）
    #[arg(long, value_name = "NAME")]
    provider: Option<String>,

    /// 指定模型 id（覆盖所选 provider 的默认 model）
    #[arg(long, value_name = "MODEL")]
    model: Option<String>,

    /// 显式允许指定工具（可重复使用）；允许其任意参数，但不绕过内置护栏。
    #[arg(long = "allow-tool", value_name = "TOOL")]
    allow_tools: Vec<String>,
}
/// `cyber update` 参数。
#[derive(clap::Args, Debug, Clone)]
struct UpdateArgs {
    /// 仅检查更新，不执行升级操作
    #[arg(short = 'c', long)]
    check: bool,

    /// 免交互确认直接使用安装脚本执行升级 (同 -y / --yes)
    #[arg(short = 'a', short_alias = 'y', long, alias = "yes")]
    apply: bool,
}
/// 输出格式。
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq)]
enum OutputFormat {
    /// 流式 Markdown 文本（最终回答到 stdout，工具/思考到 stderr）
    Text,
    /// 结构化 JSON（含 session_id/answer/tool_calls/error）
    Json,
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    // 全局双击 Ctrl+C 紧急强杀看门狗：任何时刻连续收到两次系统级 Ctrl+C 均立即强退，防止任何死锁/挂起
    tokio::spawn(async {
        let mut count = 0;
        while let Ok(()) = tokio::signal::ctrl_c().await {
            count += 1;
            if count >= 2 {
                cyber_tui::restore_terminal();
                std::process::exit(130);
            }
        }
    });

    let cli = Cli::parse();

    let cwd = resolve_cwd(match cli.cwd {
        Some(c) => c,
        None => std::env::current_dir()?,
    })?;
    let mock = cli.mock || std::env::var("CYBER_MOCK_PROVIDER").is_ok_and(|v| v == "1");
    let interactive = std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal();

    match cli.command {
        Some(Command::Run(args)) => {
            // headless：日志写 stderr（不污染 stdout 的结果输出）
            init_tracing(cli.log_level.as_deref());
            let res = run_headless_command(&cwd, args, mock).await;
            match res {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        Some(Command::Setup) => {
            let res = cyber_tui::run_setup(&cwd, mock).await;
            cyber_tui::restore_terminal();
            match res {
                Ok(true) => std::process::exit(0),
                Ok(false) => {
                    eprintln!("Provider configuration is missing or unusable. Run `cyber setup`; check the selected provider, endpoint, model and credential environment variable (including project overrides).");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        Some(Command::Tui) => {
            require_terminal(interactive)?;
            setup::ensure_configured(&cwd, mock, true)?;
            let res = run_tui(&cwd, mock, cli.log_level.as_deref()).await;
            cyber_tui::restore_terminal();
            std::process::exit(if res.is_ok() { 0 } else { 1 });
        }
        Some(Command::Update(args)) => {
            let res = run_update(args, &cwd).await;
            std::process::exit(if res.is_ok() { 0 } else { 1 });
        }
        None => {
            require_terminal(interactive)?;
            setup::ensure_configured(&cwd, mock, true)?;
            init_cli_tracing(cli.log_level.as_deref())?;
            let res = run_cli(&cwd, mock).await;
            cyber_tui::restore_terminal();
            std::process::exit(if res.is_ok() { 0 } else { 1 });
        }
    }
}

fn resolve_cwd(path: PathBuf) -> color_eyre::Result<PathBuf> {
    let path = std::fs::canonicalize(&path).map_err(|error| {
        color_eyre::eyre::eyre!(
            "Cannot resolve working directory {}: {error}",
            path.display()
        )
    })?;
    if !path.is_dir() {
        color_eyre::eyre::bail!("Working directory must be a directory: {}", path.display());
    }
    // Preserve the conventional Windows path spelling used by existing history
    // hashes rather than splitting every session on the verbatim prefix.
    #[cfg(windows)]
    let path = {
        use std::path::{Component, Prefix};
        let mut components = path.components();
        match components.next() {
            Some(Component::Prefix(prefix)) => {
                let base = match prefix.kind() {
                    Prefix::VerbatimDisk(disk) => {
                        Some(PathBuf::from(format!("{}:\\", disk as char)))
                    }
                    Prefix::VerbatimUNC(server, share) => {
                        let mut base = std::ffi::OsString::from("\\\\");
                        base.push(server);
                        base.push("\\");
                        base.push(share);
                        base.push("\\");
                        Some(PathBuf::from(base))
                    }
                    _ => None,
                };
                if let Some(mut base) = base {
                    for component in components {
                        if !matches!(component, Component::RootDir) {
                            base.push(component.as_os_str());
                        }
                    }
                    base
                } else {
                    path
                }
            }
            _ => path,
        }
    };
    Ok(path)
}

fn require_terminal(interactive: bool) -> color_eyre::Result<()> {
    if !interactive {
        color_eyre::eyre::bail!(
            "Interactive mode requires a terminal. Use `cyber run \"<prompt>\"` for scripts."
        );
    }
    Ok(())
}

/// headless 执行：`cyber run`。
async fn run_headless_command(cwd: &Path, args: RunArgs, mock: bool) -> color_eyre::Result<()> {
    let format = args.format;
    let outcome = match setup::ensure_run_configured(
        cwd,
        mock,
        args.provider.as_deref(),
        args.model.as_deref(),
    ) {
        Err(error) => HeadlessOutcome {
            session_id: String::new(),
            answer: String::new(),
            tool_calls: Vec::new(),
            error: Some(error.to_string()),
            permission_denied: false,
        },
        Ok(()) => {
            run_headless(
                cwd,
                HeadlessArgs {
                    prompt: args.prompt,
                    format: match args.format {
                        OutputFormat::Text => "text".into(),
                        OutputFormat::Json => "json".into(),
                    },
                    session: args.session,
                    new: args.new,
                    max_steps: args.max_steps,
                    think: args.think,
                    provider: args.provider,
                    model: args.model,
                    mock,
                    allow_tools: args.allow_tools,
                },
            )
            .await
        }
    };

    if format == OutputFormat::Json {
        // JSON：最终结果一次性输出（含失败），退出码由 error 决定
        println!("{}", cyber_tui::outcome_to_json(&outcome));
    } else if let Some(error) = &outcome.error {
        eprintln!("[error] {}", cyber_tui::headless::terminal_text(error));
    }
    if outcome.error.is_some() {
        std::process::exit(1);
    }
    Ok(())
}

/// TUI 启动。
async fn run_tui(cwd: &Path, mock_flag: bool, log_level: Option<&str>) -> color_eyre::Result<()> {
    let ctx = load_app_context(cwd)?;

    // 日志写文件（~/.cyber/logs/cyber.log），不输出到终端——避免干扰 TUI 渲染。
    // 启动早期（load_app_context 之前）的日志丢弃，无碍。
    let log_file = ctx.paths.logs_dir.join("cyber.log");
    let _ = std::fs::create_dir_all(&ctx.paths.logs_dir);
    let env_filter = log_filter(log_level);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
    {
        Ok(f) => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(f)
                .init();
        }
        Err(e) => {
            eprintln!(
                "警告：无法打开日志文件 {}: {e}，回退 stderr（可能干扰 TUI）",
                log_file.display()
            );
            tracing_subscriber::fmt().with_env_filter(env_filter).init();
        }
    }

    // mock：CLI flag 或环境变量 CYBER_MOCK_PROVIDER=1
    let mock = mock_flag || std::env::var("CYBER_MOCK_PROVIDER").is_ok_and(|v| v == "1");

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
        cwd: cwd.to_path_buf(),
    };

    // 构建统一工具表（builtins + Skills + MCP）。注意在 `paths` move 前 borrow ctx.paths + cwd。
    // mock 模式跳过 MCP 连接。boot_errors 经 toast 展示（降级为仅可用部分，不阻断启动）。
    let (registries, boot_errors) = build_registries(
        &ctx.paths,
        &paths.cwd,
        mock,
        ctx.config.agent.subagents.enabled,
    )
    .await;
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

/// 初始化 tracing：--log-level 优先，否则用 RUST_LOG，最终回退 info。
/// headless 模式下日志写 stderr 而非文件（避免与 stdout 输出竞争）。
fn init_tracing(log_level: Option<&str>) {
    let env_filter = log_filter(log_level);
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();
}

fn init_cli_tracing(log_level: Option<&str>) -> color_eyre::Result<()> {
    let paths = cyber_core::Paths::detect()?;
    std::fs::create_dir_all(&paths.logs_dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs_dir.join("cyber.log"))?;
    tracing_subscriber::fmt()
        .with_env_filter(log_filter(log_level))
        .with_ansi(false)
        .with_writer(file)
        .init();
    Ok(())
}

fn log_filter(log_level: Option<&str>) -> tracing_subscriber::EnvFilter {
    log_level.map_or_else(
        || {
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        },
        tracing_subscriber::EnvFilter::new,
    )
}

async fn run_update(args: UpdateArgs, _cwd: &Path) -> color_eyre::Result<()> {
    use std::io::Write;
    println!("Cyber Master 版本更新检查");
    println!("───────────────────────────────────────────────");
    println!("当前安装版本: v{}", cyber_core::update::CURRENT_VERSION);
    print!("正在连接版本源检查最新版本 (GitHub / CNB)... ");
    let _ = std::io::stdout().flush();

    let release = cyber_core::update::check_for_updates(true).await;
    match release {
        Some(info)
            if cyber_core::update::is_newer(cyber_core::update::CURRENT_VERSION, &info.version) =>
        {
            println!("✨ 发现新版本！");
            println!();
            println!("  最新版本: v{}", info.version);
            println!("  发布地址: {}", info.html_url);
            println!("  CNB 镜像: {}", cyber_core::update::CNB_RELEASES_URL);
            if let Some(notes) = &info.release_notes {
                let clean_notes = notes.trim();
                if !clean_notes.is_empty() {
                    println!();
                    println!("发布说明:");
                    let count = clean_notes.lines().count();
                    for line in clean_notes.lines().take(15) {
                        println!("  {line}");
                    }
                    if count > 15 {
                        println!("  ... (更多更新说明请查看发布页面)");
                    }
                }
            }
            println!();
            if args.check {
                return Ok(());
            }

            let should_update = if args.apply {
                true
            } else {
                print!("是否立即更新到最新版本 v{}？[Y/n]: ", info.version);
                let _ = std::io::stdout().flush();
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                let trimmed = input.trim();
                trimmed.is_empty() || trimmed.eq_ignore_ascii_case("y")
            };

            if should_update {
                println!(
                    "正在通过 CNB 国内极速源执行一键更新升级 (v{})...",
                    info.version
                );
                #[cfg(windows)]
                {
                    let script = format!(
                        "$env:CYBER_VERSION='v{}'; $env:CYBER_USE_CNB='1'; irm https://cnb.cool/{}/-/git/raw/main/install.ps1 | iex",
                        info.version,
                        cyber_core::update::CNB_REPO
                    );
                    let status = std::process::Command::new("powershell")
                        .args([
                            "-NoProfile",
                            "-ExecutionPolicy",
                            "Bypass",
                            "-Command",
                            &script,
                        ])
                        .status()?;
                    if status.success() {
                        println!("🎉 升级成功！请重新打开终端或直接运行 cyber 查看最新版本。");
                    } else {
                        eprintln!("一键升级执行失败，请尝试手动运行安装命令。");
                    }
                }
                #[cfg(not(windows))]
                {
                    let script = format!(
                        "curl -fsSL https://cnb.cool/{}/-/git/raw/main/install.sh | sh -s -- --cnb --version v{}",
                        cyber_core::update::CNB_REPO,
                        info.version
                    );
                    let status = std::process::Command::new("sh")
                        .args(["-c", &script])
                        .status()?;
                    if status.success() {
                        println!("🎉 升级成功！请重新打开终端或直接运行 cyber 查看最新版本。");
                    } else {
                        eprintln!("一键升级执行失败，请尝试手动运行安装命令。");
                    }
                }
            } else {
                println!("已取消更新。您也可以随时手动执行以下命令进行升级：");
                #[cfg(windows)]
                println!(
                    "  irm https://cnb.cool/{}/-/git/raw/main/install.ps1 | iex",
                    cyber_core::update::CNB_REPO
                );
                #[cfg(not(windows))]
                println!(
                    "  curl -fsSL https://cnb.cool/{}/-/git/raw/main/install.sh | sh",
                    cyber_core::update::CNB_REPO
                );
            }
        }
        Some(_info) => {
            println!("已是最新！");
            println!(
                "当前版本 (v{}) 已经是最新的发布版本。",
                cyber_core::update::CURRENT_VERSION
            );
        }
        None => {
            println!("检查失败。");
            println!("未能获取到最新版本信息，可能是网络不可达或超时。");
            println!("您可以手动访问以下镜像主页确认：");
            println!(
                "  CNB 镜像: https://cnb.cool/{}",
                cyber_core::update::CNB_REPO
            );
            println!(
                "  GitHub:   https://github.com/{}",
                cyber_core::update::GITHUB_REPO
            );
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_command_selects_cli() {
        assert!(Cli::try_parse_from(["cyber"]).unwrap().command.is_none());
    }

    #[test]
    fn explicit_tui_and_setup_commands_are_available() {
        assert!(matches!(
            Cli::try_parse_from(["cyber", "tui"]).unwrap().command,
            Some(Command::Tui)
        ));
        assert!(matches!(
            Cli::try_parse_from(["cyber", "setup"]).unwrap().command,
            Some(Command::Setup)
        ));
    }

    #[test]
    fn run_accepts_global_mock_flag() {
        let cli = Cli::try_parse_from(["cyber", "run", "hello", "--mock"]).unwrap();
        assert!(cli.mock);
        assert!(matches!(cli.command, Some(Command::Run(_))));
    }

    #[test]
    fn rejects_noninteractive_default_startup() {
        assert!(require_terminal(false)
            .unwrap_err()
            .to_string()
            .contains("cyber run"));
    }

    #[test]
    fn cwd_is_absolute_and_rejects_files() {
        assert!(resolve_cwd(PathBuf::from(".")).unwrap().is_absolute());
        #[cfg(windows)]
        assert!(!resolve_cwd(PathBuf::from("."))
            .unwrap()
            .to_string_lossy()
            .starts_with("\\\\?\\"));
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(resolve_cwd(file.path().to_owned()).is_err());
    }
    #[test]
    fn update_command_parses_flags() {
        let cli = Cli::try_parse_from(["cyber", "update"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Update(ref a)) if !a.check && !a.apply));

        let cli = Cli::try_parse_from(["cyber", "update", "--check"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Update(ref a)) if a.check && !a.apply));

        let cli = Cli::try_parse_from(["cyber", "update", "-a"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Update(ref a)) if !a.check && a.apply));

        let cli = Cli::try_parse_from(["cyber", "update", "-y"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Update(ref a)) if !a.check && a.apply));

        let cli = Cli::try_parse_from(["cyber", "update", "--yes"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Update(ref a)) if !a.check && a.apply));
    }
}
