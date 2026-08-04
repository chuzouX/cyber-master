//! TUI 应用核心：模式状态机 + 异步主循环 + 渲染分发。
//!
//! P2 升级为 tokio 事件总线（DESIGN §10.2）：`main_loop` 用 `tokio::select!` 在
//! crossterm `EventStream` / agent 通道 / tick 三路分发。终端初始化与恢复交由
//! `ratatui::init/restore`，其内置 panic hook 会在 panic 时自动恢复终端。
//!
//! Settings 是"用 Mode 模拟的模态层"：全局 `s` 或 Welcome 第 4 项进入，`Esc` 返回
//! `prev_mode`；编辑即时改 `config` + live-apply（theme/mouse），保存由设置页内
//! 「保存设置」行 + `Enter` 触发（`save_config` 回写 `~/.cyber/config.toml`）。
//!
//! Chat 是文本输入态：字母（含 `s`/`q`）交 textarea；Enter 发送、Shift+Enter 换行、
//! Ctrl+, 设置、Ctrl+C/Ctrl+Q 退出、Esc 取消流式或返回。agent 任务由 `tokio::spawn`
//! 驱动，事件经 `UnboundedSender<AgentEvent>` 回传主循环；TUI 退出时 `send` 失败，
//! 任务静默终止，agent 永不 panic TUI（不在任务内 unwrap）。

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event, KeyEvent, KeyEventKind};
use crossterm::execute;
use futures::StreamExt;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::Paragraph,
    DefaultTerminal, Frame,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tracing::info;

use cyber_agent::{fetch_models, run_stream, AgentEvent, Message, ToolRegistry};
use cyber_core::{save_config, save_providers, Config, ProjectContext, ProvidersConfig};

use crate::chat::{ChatEntry, ChatState};
use crate::event::{chat_key_to_action, key_to_action, Action, ChatAction};
use crate::slash::{parse as parse_slash, SlashCommand, HELP_TEXT as SLASH_HELP};
use crate::theme::Theme;
use crate::views;
use crate::views::providers::{FormAction, ProviderFormState};
use crate::views::settings::{LiveApply, SettingsState};

/// 顶层模式 / 屏幕。Welcome 为启动入口屏，Settings 为模态设置层，ProviderForm 为
/// 服务商新增/编辑模态层（从 Settings 或 Chat 两路进入），其余三个对应 DESIGN §9。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Welcome,
    Chat,
    Workflow,
    Dashboard,
    Settings,
    ProviderForm,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Welcome => "Welcome",
            Mode::Chat => "Chat",
            Mode::Workflow => "Workflow",
            Mode::Dashboard => "Dashboard",
            Mode::Settings => "Settings",
            Mode::ProviderForm => "Provider Form",
        }
    }
}

/// 打包 4 个路径参数，避免 `App::new` 的 `too_many_arguments` 进一步恶化。
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub providers_file: PathBuf,
    pub history_dir: PathBuf,
    pub cwd: PathBuf,
}

/// 异步模型拉取结果（经 mpsc 通道回传主循环第 4 路 select! 分支）。
#[derive(Debug)]
pub struct FetchResult {
    pub fetch_id: u64,
    pub result: std::result::Result<Vec<String>, String>,
}

pub struct App {
    config: Config,
    providers: ProvidersConfig,
    project: Option<ProjectContext>,
    theme: Theme,
    mode: Mode,
    selected: usize,
    toast: Option<String>,
    first_run: bool,
    should_quit: bool,
    // Settings 模态层状态
    settings: SettingsState,
    prev_mode: Mode,
    /// 进入 Settings 时的配置快照，供 Esc 双击回退。
    config_at_entry: Config,
    /// 进入 Settings 时的 providers 快照，供 Esc 双击回退（与 config_at_entry 同步）。
    providers_at_entry: ProvidersConfig,
    paths: AppPaths,
    has_project_config: bool,
    // P2 Chat 状态
    chat: ChatState,
    /// agent 事件回传通道（clone 给每次 spawn 的任务），携带 (gen, AgentEvent)。
    agent_tx: UnboundedSender<(u64, AgentEvent)>,
    /// 当前 agent 任务句柄；cancel/新提交时 abort 旧任务，避免事件交错。
    agent_handle: Option<JoinHandle<()>>,
    /// generation 计数器：每次 spawn/cancel 递增，事件携带 gen，TUI 据此忽略 stale 事件。
    generation: u64,
    /// 模型拉取结果回传通道（clone 给每次 fetch 任务），第 4 路 select! 分支。
    fetch_tx: UnboundedSender<FetchResult>,
    /// Provider 表单状态（Mode::ProviderForm 时 Some）。
    provider_form: Option<ProviderFormState>,
    /// 是否强制使用 MockProvider（离线冒烟）。
    mock: bool,
}

const WELCOME_OPTIONS: usize = 4;

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        providers: ProvidersConfig,
        project: Option<ProjectContext>,
        initial: Mode,
        first_run: bool,
        paths: AppPaths,
        has_project_config: bool,
        mock: bool,
        agent_tx: UnboundedSender<(u64, AgentEvent)>,
        fetch_tx: UnboundedSender<FetchResult>,
    ) -> Self {
        let theme = Theme::resolve(&config.ui.theme);
        Self {
            config_at_entry: config.clone(),
            providers_at_entry: providers.clone(),
            config,
            providers,
            project,
            theme,
            mode: initial,
            selected: 0,
            toast: None,
            first_run,
            should_quit: false,
            settings: SettingsState::new(),
            prev_mode: initial,
            paths,
            has_project_config,
            chat: ChatState::new(),
            agent_tx,
            agent_handle: None,
            generation: 0,
            fetch_tx,
            provider_form: None,
            mock,
        }
    }

    /// 启动 TUI 异步主循环。终端初始化与恢复由 `ratatui::init/restore` 负责。
    ///
    /// `agent_rx` / `fetch_rx` 是 `main_loop` 局部 `&mut`（不存于 `self`），避免
    /// `select!` 内 `&mut self`（handle_*）与 `recv()` 的借用冲突。
    pub async fn run(
        mut self,
        mut agent_rx: UnboundedReceiver<(u64, AgentEvent)>,
        mut fetch_rx: UnboundedReceiver<FetchResult>,
    ) -> color_eyre::Result<()> {
        // 启动时加载该 cwd 的历史对话（按 cwd hash 索引，互不干扰）。
        let saved = crate::history::load(&self.paths.history_dir, &self.paths.cwd);
        if !saved.is_empty() {
            info!(count = saved.len(), "恢复历史对话");
            self.chat.entries.extend(saved);
            // prepare_render 会在首帧通过 len 变化自动重建缓存，无需手动 invalidate。
        }
        let mut terminal: DefaultTerminal = ratatui::init();
        if self.config.ui.mouse {
            execute!(io::stdout(), EnableMouseCapture)?;
        }
        // 即使 main_loop 出错也先恢复终端，避免终端卡在 alternate screen。
        let result = self
            .main_loop(&mut terminal, &mut agent_rx, &mut fetch_rx)
            .await;
        // 退出前持久化当前对话（catch-all，覆盖所有退出路径）。
        self.save_history();
        // 取消任何仍在运行的 agent 任务，避免后台 HTTP 流泄漏
        if let Some(h) = self.agent_handle.take() {
            h.abort();
        }
        // 无条件禁用鼠标捕获（幂等），避免中途开启鼠标后退出泄漏。
        let _ = execute!(io::stdout(), DisableMouseCapture);
        ratatui::restore();
        result?;
        info!(mode = ?self.mode, "TUI 退出");
        Ok(())
    }

    /// 持久化当前对话历史到 `~/.cyber/history/{cwd_hash}.json`。
    /// 失败仅记日志（不影响会话）。在 Done/Error/cancel/clear/quit 及退出时调用。
    fn save_history(&self) {
        crate::history::save(&self.paths.history_dir, &self.paths.cwd, &self.chat.entries);
    }

    async fn main_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
        agent_rx: &mut UnboundedReceiver<(u64, AgentEvent)>,
        fetch_rx: &mut UnboundedReceiver<FetchResult>,
    ) -> io::Result<()> {
        // crossterm EventStream 必须fuse；Windows console handle 可用。
        let mut events = crossterm::event::EventStream::new().fuse();
        // tick：周期重绘兜底（流式期由 agent 事件驱动重绘，idle 期低频刷新）
        let mut tick = tokio::time::interval(Duration::from_millis(120));
        loop {
            // textarea 的 set_block/set_style 需 &mut，而 render 是 &self；
            // 故在 draw 前以 &mut self 应用样式（含 streaming 态边框/标题切换、
            // ProviderForm 的 textarea 样式）。
            self.style_chat_input();
            terminal.draw(|f| self.render(f))?;
            tokio::select! {
                biased;
                maybe_ev = events.next() => {
                    if let Some(Ok(ev)) = maybe_ev {
                        self.handle_event(ev);
                    }
                }
                maybe_ae = agent_rx.recv() => {
                    if let Some((gen, ae)) = maybe_ae {
                        self.handle_agent_event(gen, ae);
                    }
                }
                maybe_fetch = fetch_rx.recv() => {
                    if let Some(fr) = maybe_fetch {
                        self.handle_fetch_result(fr);
                    }
                }
                _ = tick.tick() => {}
            }
            if self.should_quit {
                break;
            }
        }
        Ok(())
    }

    /// 处理一个 crossterm 事件：仅 Press；按模式分发到 Chat / ProviderForm / 非 Chat 路径。
    fn handle_event(&mut self, ev: Event) {
        let Event::Key(k) = ev else {
            return;
        };
        if k.kind != KeyEventKind::Press {
            return;
        }
        match self.mode {
            Mode::Chat => self.handle_chat_key(k),
            Mode::ProviderForm => self.handle_provider_form_key(k),
            Mode::Welcome | Mode::Workflow | Mode::Dashboard | Mode::Settings => {
                self.handle_action(key_to_action(k));
            }
        }
    }

    /// Chat 模式按键分发（文本输入态）。
    fn handle_chat_key(&mut self, k: KeyEvent) {
        if self.toast.is_some() {
            self.toast = None;
        }
        match chat_key_to_action(k) {
            ChatAction::Submit => {
                // 斜杠命令拦截：输入以 `/` 开头时不发 agent，转命令处理
                let peek: String = self.chat.input.lines().join("\n");
                if peek.trim_start().starts_with('/') {
                    let text = self.chat.take_input();
                    self.handle_slash_command(&text);
                } else if let Some((text, history)) = self.chat.submit() {
                    self.spawn_agent(text, history);
                }
            }
            ChatAction::Newline => {
                // Shift/Alt+Enter 或 Ctrl+J：透传给 textarea 插入换行
                if !self.chat.streaming {
                    self.chat.input.input(k);
                }
            }
            ChatAction::Back => {
                if self.chat.streaming {
                    // 流式期 Esc：取消（abort 任务 + bump gen 隔离 stale 事件 + 丢弃 buffer）
                    if let Some(h) = self.agent_handle.take() {
                        h.abort();
                    }
                    self.generation = self.generation.wrapping_add(1);
                    self.chat.cancel_stream();
                    self.save_history();
                    self.toast = Some("已取消生成".into());
                } else if self.project.is_none() {
                    // 非流式 + 无项目：返回 Welcome
                    self.mode = Mode::Welcome;
                }
                // 有项目时 Esc 停留（与 P1 一致）
            }
            ChatAction::SwitchMode => {
                // Chat → Workflow（其余切换由各模式 Tab 处理，形成 Chat→Workflow→Dashboard→Chat 循环）
                self.mode = Mode::Workflow;
            }
            ChatAction::OpenSettings => {
                if self.mode != Mode::Settings {
                    self.prev_mode = self.mode;
                    self.config_at_entry = self.config.clone();
                    self.providers_at_entry = self.providers.clone();
                    self.settings.pending_discard = false;
                    self.mode = Mode::Settings;
                }
            }
            ChatAction::Quit => {
                self.save_history();
                self.should_quit = true;
            }
            ChatAction::Input => {
                if !self.chat.streaming {
                    self.chat.input.input(k);
                }
            }
        }
    }

    /// 处理 agent 任务回传事件，更新 chat 流式状态。
    /// generation 守卫：cancel/新提交 bump generation 后，旧任务的 stale 事件（gen 不匹配）被忽略。
    fn handle_agent_event(&mut self, gen: u64, ae: AgentEvent) {
        if gen != self.generation {
            return; // stale 事件（来自已 cancel/被取代的旧任务）
        }
        match ae {
            AgentEvent::Started => {
                // streaming 已由 submit 置 true；Started 仅作信号
            }
            AgentEvent::Token(t) => {
                if self.chat.streaming {
                    self.chat.streaming_buffer.push_str(&t);
                }
            }
            AgentEvent::ToolCall { id, name, arguments } => {
                if self.chat.streaming {
                    self.chat.push_tool_call(id, name, arguments);
                }
            }
            AgentEvent::ToolResult {
                id,
                name,
                output,
                is_error,
            } => {
                if self.chat.streaming {
                    self.chat.push_tool_result(id, name, output, is_error);
                }
            }
            AgentEvent::Done => {
                if self.chat.streaming {
                    self.chat.finalize_stream();
                    // 任务自然结束，清理句柄（abort 对已完成任务是 no-op，置 None 更整洁）
                    self.agent_handle = None;
                    self.save_history();
                }
            }
            AgentEvent::Error(m) => {
                if self.chat.streaming {
                    self.chat.finalize_stream();
                    self.save_history();
                }
                self.agent_handle = None;
                self.toast = Some(format!("生成失败: {m}"));
            }
        }
    }

    /// ProviderForm 模式按键分发：委托 `form.handle_key`，按 `FormAction` 执行副作用。
    fn handle_provider_form_key(&mut self, k: KeyEvent) {
        let Some(form) = self.provider_form.as_mut() else {
            return;
        };
        let action = form.handle_key(k, &self.providers);
        match action {
            FormAction::None => {}
            FormAction::Cancel => {
                self.provider_form = None;
                self.mode = self.prev_mode;
            }
            FormAction::Save => self.save_provider_form(),
            FormAction::Fetch => self.start_provider_fetch(),
            FormAction::Toast(msg) => self.toast = Some(msg),
        }
    }

    /// 接收异步模型拉取结果：非 ProviderForm 模式丢弃；否则 deliver_fetch。
    fn handle_fetch_result(&mut self, fr: FetchResult) {
        if self.mode != Mode::ProviderForm {
            return;
        }
        if let Some(form) = self.provider_form.as_mut() {
            form.deliver_fetch(fr.fetch_id, fr.result);
        }
    }

    /// 发起异步模型拉取：bump fetch_id + spawn `fetch_models` 任务发 `fetch_tx`。
    fn start_provider_fetch(&mut self) {
        let (fetch_id, cfg_snapshot) = {
            let Some(form) = self.provider_form.as_mut() else {
                return;
            };
            let id = form.start_fetch();
            (id, form.to_provider_config_snapshot())
        };
        let tx = self.fetch_tx.clone();
        tokio::spawn(async move {
            let result = fetch_models(&cfg_snapshot)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(FetchResult { fetch_id, result });
        });
    }

    /// 保存 Provider 表单：校验 → upsert → 持久化（Settings 延迟 / Chat 立即）→ 返回 prev_mode。
    /// 校验失败时保留表单 + toast，不退出。
    fn save_provider_form(&mut self) {
        // 先校验（不消费 form），失败则保留表单 + toast
        let (result, original_name) = {
            let Some(form) = self.provider_form.as_ref() else {
                return;
            };
            (form.into_provider(&self.providers), form.original_name.clone())
        };
        match result {
            Err(msg) => {
                self.toast = Some(msg);
            }
            Ok((name, cfg)) => {
                // 处理重命名：先删旧名再 upsert 新名
                if let Some(orig) = &original_name {
                    if orig != &name {
                        self.providers.remove(orig);
                        // 同步 default_provider 改名
                        if self.config.agent.default_provider == *orig {
                            self.config.agent.default_provider = name.clone();
                            self.settings.dirty = true;
                        }
                    }
                }
                self.providers.upsert(&name, cfg);
                let from_settings = self.prev_mode == Mode::Settings;
                if from_settings {
                    self.settings.dirty_providers = true;
                    self.toast = Some(format!("Provider '{name}' 已暂存（保存设置后写入）"));
                } else {
                    // Chat 入口：立即写盘
                    match save_providers(&self.providers, &self.paths.providers_file) {
                        Ok(()) => {
                            self.toast = Some(format!("Provider '{name}' 已保存"));
                        }
                        Err(e) => {
                            self.toast = Some(format!("保存失败: {e}"));
                        }
                    }
                }
                self.provider_form = None;
                self.mode = self.prev_mode;
            }
        }
    }

    /// 拉起一次 agent 流式任务。先 abort 任何旧任务 + bump generation（隔离 stale 事件）。
    fn spawn_agent(&mut self, text: String, history: Vec<Message>) {
        if let Some(h) = self.agent_handle.take() {
            h.abort();
        }
        self.generation = self.generation.wrapping_add(1);
        let gen = self.generation;
        let config = self.config.clone();
        let providers = self.providers.clone();
        let project = self.project.clone();
        let tx = self.agent_tx.clone();
        let mock = self.mock;
        let cwd = self.paths.cwd.clone();
        let handle = tokio::spawn(async move {
            run_stream(config, providers, project, text, history, tx, gen, mock, cwd).await;
        });
        self.agent_handle = Some(handle);
    }

    /// 处理斜杠命令（输入以 `/` 开头时由 Submit 分支拦截）。
    ///
    /// 命令本身先作为 User 条目记录（便于追溯），再按命令执行副作用。
    /// 流式期：`/clear` `/mode` `/model` 被阻止（须先 `/cancel`）；`/cancel` 仅流式期有效；
    /// `/help` `/tools` `/quit` 任意时刻可用。
    fn handle_slash_command(&mut self, raw: &str) {
        let cmd = parse_slash(raw);
        // 记录命令本身（/clear 会随后清空，无妨）
        self.chat.entries.push(ChatEntry::User(raw.trim().to_string()));
        match cmd {
            SlashCommand::Help => {
                self.chat
                    .entries
                    .push(ChatEntry::System(SLASH_HELP.to_string()));
            }
            SlashCommand::Clear => {
                if self.chat.streaming {
                    self.chat.entries.push(ChatEntry::System(
                        "生成中，无法清空（先 /cancel 取消生成）".into(),
                    ));
                } else {
                    self.chat.clear();
                    self.chat
                        .entries
                        .push(ChatEntry::System("已清空对话历史".into()));
                    // 清空后持久化空历史，覆盖旧文件（磁盘同步清空）。
                    self.save_history();
                }
            }
            SlashCommand::Mode(name) => {
                if self.chat.streaming {
                    self.chat
                        .entries
                        .push(ChatEntry::System("生成中，无法切换模式".into()));
                } else {
                    match name.as_str() {
                        "chat" => {
                            self.mode = Mode::Chat;
                            self.chat
                                .entries
                                .push(ChatEntry::System("已切换到 Chat 模式".into()));
                        }
                        "workflow" => {
                            self.mode = Mode::Workflow;
                            self.chat
                                .entries
                                .push(ChatEntry::System("已切换到 Workflow 模式".into()));
                        }
                        "dashboard" => {
                            self.mode = Mode::Dashboard;
                            self.chat
                                .entries
                                .push(ChatEntry::System("已切换到 Dashboard 模式".into()));
                        }
                        "" => self.chat.entries.push(ChatEntry::System(
                            "用法：/mode <chat|workflow|dashboard>".into(),
                        )),
                        other => self.chat.entries.push(ChatEntry::System(format!(
                            "未知模式：{other}（可选：chat / workflow / dashboard）"
                        ))),
                    }
                }
            }
            SlashCommand::Model(name) => {
                if self.chat.streaming {
                    self.chat
                        .entries
                        .push(ChatEntry::System("生成中，无法切换 provider".into()));
                } else if name.is_empty() {
                    self.chat.entries.push(ChatEntry::System(format!(
                        "当前 provider：{}（用法：/model <provider>）",
                        self.config.agent.default_provider
                    )));
                } else if self.providers.providers.contains_key(&name) {
                    self.config.agent.default_provider = name.clone();
                    self.chat
                        .entries
                        .push(ChatEntry::System(format!("已切换 provider 到 {name}")));
                } else {
                    let available: Vec<&str> = self
                        .providers
                        .providers
                        .keys()
                        .map(String::as_str)
                        .collect();
                    self.chat.entries.push(ChatEntry::System(format!(
                        "未知 provider：{name}（可用：{}）",
                        available.join(", ")
                    )));
                }
            }
            SlashCommand::Provider(args) => {
                self.handle_provider_slash(&args);
            }
            SlashCommand::Tools => {
                let reg = ToolRegistry::with_builtins();
                let mut lines = String::from("可用工具：");
                for s in reg.schemas() {
                    lines.push_str(&format!("\n  {} — {}", s.name, s.description));
                }
                self.chat.entries.push(ChatEntry::System(lines));
            }
            SlashCommand::Cancel => {
                if self.chat.streaming {
                    if let Some(h) = self.agent_handle.take() {
                        h.abort();
                    }
                    self.generation = self.generation.wrapping_add(1);
                    self.chat.cancel_stream();
                    self.chat
                        .entries
                        .push(ChatEntry::System("已取消生成".into()));
                    self.save_history();
                } else {
                    self.chat
                        .entries
                        .push(ChatEntry::System("当前无生成任务".into()));
                }
            }
            SlashCommand::Quit => {
                self.save_history();
                self.should_quit = true;
            }
            SlashCommand::Unknown(name) => {
                self.chat.entries.push(ChatEntry::System(format!(
                    "未知命令：{name}（输入 /help 查看可用命令）"
                )));
            }
        }
    }

    /// 处理 `/provider <subcommand>`：list / add / edit <name> / use <name> / remove <name>。
    /// 流式期阻止（与 /model 一致）。add/edit 进入 ProviderForm（prev_mode=Chat，立即持久化）。
    fn handle_provider_slash(&mut self, args: &str) {
        if self.chat.streaming {
            self.chat
                .entries
                .push(ChatEntry::System("生成中，无法管理 provider（先 /cancel）".into()));
            return;
        }
        let mut parts = args.splitn(2, char::is_whitespace);
        let sub = parts.next().unwrap_or("").to_lowercase();
        let rest = parts.next().unwrap_or("").trim();
        match sub.as_str() {
            "" | "list" => {
                let names = self.providers.sorted_names();
                if names.is_empty() {
                    self.chat
                        .entries
                        .push(ChatEntry::System("（无 provider，用 /provider add 新增）".into()));
                } else {
                    let mut lines = String::from("Providers：");
                    for name in &names {
                        let star = if name == &self.config.agent.default_provider {
                            " ★默认"
                        } else {
                            ""
                        };
                        let p = &self.providers.providers[name];
                        lines.push_str(&format!(
                            "\n  {name}{star}  [{}] {} · {}",
                            p.kind, p.base_url, p.model
                        ));
                    }
                    self.chat.entries.push(ChatEntry::System(lines));
                }
            }
            "add" => {
                self.prev_mode = Mode::Chat;
                self.provider_form = Some(ProviderFormState::empty());
                self.mode = Mode::ProviderForm;
            }
            "edit" => {
                if rest.is_empty() {
                    self.chat.entries.push(ChatEntry::System(
                        "用法：/provider edit <name>".into(),
                    ));
                } else if let Some(cfg) = self.providers.providers.get(rest).cloned() {
                    self.prev_mode = Mode::Chat;
                    self.provider_form = Some(ProviderFormState::from_provider(rest, &cfg));
                    self.mode = Mode::ProviderForm;
                } else {
                    self.chat.entries.push(ChatEntry::System(format!(
                        "未知 provider：{rest}"
                    )));
                }
            }
            "use" => {
                if rest.is_empty() {
                    self.chat.entries.push(ChatEntry::System(format!(
                        "当前 provider：{}（用法：/provider use <name>）",
                        self.config.agent.default_provider
                    )));
                } else if self.providers.providers.contains_key(rest) {
                    self.config.agent.default_provider = rest.to_string();
                    match save_config(&self.config, &self.paths.config_file) {
                        Ok(()) => self.chat.entries.push(ChatEntry::System(format!(
                            "已切换 provider 到 {rest}"
                        ))),
                        Err(e) => self.chat.entries.push(ChatEntry::System(format!(
                            "切换成功但保存失败: {e}"
                        ))),
                    }
                } else {
                    self.chat.entries.push(ChatEntry::System(format!(
                        "未知 provider：{rest}（可用：{}）",
                        self.providers.sorted_names().join(", ")
                    )));
                }
            }
            "remove" => {
                if rest.is_empty() {
                    self.chat.entries.push(ChatEntry::System(
                        "用法：/provider remove <name>".into(),
                    ));
                } else if self.providers.remove(rest).is_some() {
                    // default_provider 防悬空
                    if self.config.agent.default_provider == rest {
                        let fallback = self.providers.sorted_names().into_iter().next();
                        self.config.agent.default_provider =
                            fallback.unwrap_or_default();
                    }
                    match save_providers(&self.providers, &self.paths.providers_file) {
                        Ok(()) => self.chat.entries.push(ChatEntry::System(format!(
                            "已删除 provider：{rest}"
                        ))),
                        Err(e) => self.chat.entries.push(ChatEntry::System(format!(
                            "删除成功但保存失败: {e}"
                        ))),
                    }
                } else {
                    self.chat.entries.push(ChatEntry::System(format!(
                        "未知 provider：{rest}"
                    )));
                }
            }
            other => {
                self.chat.entries.push(ChatEntry::System(format!(
                    "未知子命令：{other}（可用：list / add / edit / use / remove）"
                )));
            }
        }
    }

    /// 从 Settings Providers 段打开新增表单。
    fn open_provider_form_add(&mut self) {
        self.prev_mode = Mode::Settings;
        self.provider_form = Some(ProviderFormState::empty());
        self.mode = Mode::ProviderForm;
    }

    /// 从 Settings Providers 段打开编辑表单（按 provider_selected 选中项）。
    fn open_provider_form_edit(&mut self) {
        let names = self.providers.sorted_names();
        let Some(name) = names.get(self.settings.provider_selected).cloned() else {
            return;
        };
        let Some(cfg) = self.providers.providers.get(&name).cloned() else {
            return;
        };
        self.prev_mode = Mode::Settings;
        self.provider_form = Some(ProviderFormState::from_provider(&name, &cfg));
        self.mode = Mode::ProviderForm;
    }

    /// 删除当前选中的 provider（双击 d 确认）。处理 default_provider 防悬空。
    fn delete_selected_provider(&mut self) {
        let names = self.providers.sorted_names();
        let Some(idx) = self.settings.pending_delete_idx else {
            return;
        };
        let Some(name) = names.get(idx).cloned() else {
            self.settings.pending_delete_idx = None;
            return;
        };
        self.providers.remove(&name);
        // default_provider 防悬空
        if self.config.agent.default_provider == name {
            let fallback = self.providers.sorted_names().into_iter().next();
            let new = fallback.unwrap_or_default();
            self.config.agent.default_provider = new.clone();
            self.toast = Some(format!(
                "已删除 '{name}'，默认 provider 回退到 '{new}'"
            ));
        } else {
            self.toast = Some(format!("已删除 provider：{name}"));
        }
        self.settings.dirty_providers = true;
        self.settings.pending_delete_idx = None;
        // clamp cursor
        let len = self.providers.providers.len();
        if self.settings.provider_selected >= len && len > 0 {
            self.settings.provider_selected = len - 1;
        }
    }

    /// draw 前的 `&mut self` 准备：① 刷新 ChatState 行缓存（entries/theme 变化时重建），
    /// ② 按 current theme + streaming 态配置 textarea 边框/样式。
    /// 两者都需 `&mut self`（`prepare_render` 写缓存、`set_block`/`set_style` 写 textarea），
    /// 而 `render` 是 `&self`，故在此统一 apply，render 仅渲染已配置好的状态。
    fn style_chat_input(&mut self) {
        // ProviderForm 模式：apply 表单 textarea 样式（与 chat 同理：render 是 &self）
        if self.mode == Mode::ProviderForm {
            if let Some(form) = self.provider_form.as_mut() {
                form.prepare_render(&self.theme);
            }
            return;
        }
        self.chat.prepare_render(&self.theme);
        let border_fg = if self.chat.streaming {
            self.theme.muted
        } else {
            self.theme.border
        };
        let title = if self.chat.streaming {
            " 生成中… "
        } else {
            " 输入 "
        };
        self.chat.input.set_block(
            ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_style(Style::default().fg(border_fg))
                .title(Line::from(title).style(Style::default().fg(self.theme.title))),
        );
        self.chat
            .input
            .set_style(Style::default().fg(self.theme.fg).bg(self.theme.bg));
        self.chat
            .input
            .set_placeholder_style(Style::default().fg(self.theme.muted));
    }

    fn handle_action(&mut self, a: Action) {
        // 任意有效输入清除一次性 toast（Other 除外，避免无谓清除）
        if a != Action::Other && self.toast.is_some() {
            self.toast = None;
        }
        // Settings 下：除 Esc/Quit/Other 外的任何动作取消"待丢弃"状态
        if self.mode == Mode::Settings && !matches!(a, Action::Esc | Action::Quit | Action::Other) {
            self.settings.pending_discard = false;
        }
        // Providers 段：非 DeleteProvider 的动作清除待删除确认（"任一其他键取消"）
        if self.mode == Mode::Settings
            && self.settings.on_providers_section()
            && !matches!(a, Action::DeleteProvider)
        {
            self.settings.pending_delete_idx = None;
        }
        match a {
            Action::Quit => self.should_quit = true,
            Action::OpenSettings => {
                // 已在 Settings 时 no-op；否则记 prev_mode + 快照后进入
                if self.mode != Mode::Settings {
                    self.prev_mode = self.mode;
                    self.config_at_entry = self.config.clone();
                    self.providers_at_entry = self.providers.clone();
                    self.settings.pending_discard = false;
                    self.mode = Mode::Settings;
                }
            }
            Action::Tab => {
                if self.mode == Mode::Settings {
                    self.settings.next_section();
                } else {
                    // Welcome 下 Tab 无效；其余三模式循环
                    self.mode = match self.mode {
                        Mode::Chat => Mode::Workflow,
                        Mode::Workflow => Mode::Dashboard,
                        Mode::Dashboard => Mode::Chat,
                        Mode::Welcome | Mode::Settings | Mode::ProviderForm => self.mode,
                    };
                }
            }
            Action::Esc => {
                if self.mode == Mode::Settings {
                    self.exit_settings();
                } else if self.project.is_none() && self.mode != Mode::Welcome {
                    // 仅在无项目上下文时允许从占位模式返回 Welcome
                    self.mode = Mode::Welcome;
                }
            }
            Action::Up => {
                if self.mode == Mode::Settings {
                    if self.settings.on_providers_section() {
                        self.settings
                            .prev_provider(self.providers.providers.len());
                    } else {
                        self.settings.prev_field();
                    }
                } else if self.mode == Mode::Welcome {
                    self.selected = (self.selected + WELCOME_OPTIONS - 1) % WELCOME_OPTIONS;
                }
            }
            Action::Down => {
                if self.mode == Mode::Settings {
                    if self.settings.on_providers_section() {
                        self.settings
                            .next_provider(self.providers.providers.len());
                    } else {
                        self.settings.next_field();
                    }
                } else if self.mode == Mode::Welcome {
                    self.selected = (self.selected + 1) % WELCOME_OPTIONS;
                }
            }
            Action::Left => {
                if self.mode == Mode::Settings && !self.settings.on_providers_section() {
                    let live = self.settings.apply_edit(&mut self.config, &self.providers, false);
                    self.apply_live(live);
                }
            }
            Action::Right => {
                if self.mode == Mode::Settings && !self.settings.on_providers_section() {
                    let live = self.settings.apply_edit(&mut self.config, &self.providers, true);
                    self.apply_live(live);
                }
            }
            Action::Enter => {
                if self.mode == Mode::Settings {
                    if self.settings.on_providers_section() {
                        // Providers 段 Enter：设当前选中为默认 provider
                        let names = self.providers.sorted_names();
                        if let Some(name) = names.get(self.settings.provider_selected).cloned() {
                            if self.config.agent.default_provider != name {
                                self.config.agent.default_provider = name.clone();
                                self.settings.dirty = true;
                                self.toast = Some(format!("默认 provider 设为 {name}"));
                            }
                        }
                    } else if self.settings.on_save_row() {
                        self.save_settings();
                    } else {
                        let live =
                            self.settings.apply_edit(&mut self.config, &self.providers, true);
                        self.apply_live(live);
                    }
                } else if self.mode == Mode::Welcome {
                    match self.selected {
                        2 => self.mode = Mode::Chat, // 进入聊天
                        3 => {
                            // 进入设置
                            self.prev_mode = Mode::Welcome;
                            self.config_at_entry = self.config.clone();
                            self.providers_at_entry = self.providers.clone();
                            self.settings.pending_discard = false;
                            self.mode = Mode::Settings;
                        }
                        0 => self.toast =
                            Some("（P1 占位：新建项目向导将在后续阶段实现）".into()),
                        1 => self.toast = Some("（P1 占位：工作流编辑器将在 P4 实现）".into()),
                        _ => {}
                    }
                }
                // Chat 走 handle_chat_key；Workflow/Dashboard 占位态：Enter 无操作
            }
            Action::AddProvider => {
                if self.mode == Mode::Settings && self.settings.on_providers_section() {
                    self.open_provider_form_add();
                }
            }
            Action::EditProvider => {
                if self.mode == Mode::Settings && self.settings.on_providers_section() {
                    self.open_provider_form_edit();
                }
            }
            Action::DeleteProvider => {
                if self.mode == Mode::Settings && self.settings.on_providers_section() {
                    let cur = self.settings.provider_selected;
                    if self.settings.pending_delete_idx == Some(cur) {
                        // 二次 d：执行删除
                        self.delete_selected_provider();
                    } else {
                        // 首次 d：标记待删除
                        self.settings.pending_delete_idx = Some(cur);
                    }
                }
            }
            Action::Other => {}
        }
    }

    /// 编辑副作用即时应用（theme 重解析 / mouse 捕获切换）。
    fn apply_live(&mut self, live: LiveApply) {
        match live {
            LiveApply::None => {}
            LiveApply::Theme => {
                self.theme = Theme::resolve(&self.config.ui.theme);
                // 缓存行内嵌旧 theme 颜色，须标脏让 prepare_render 重建。
                self.chat.invalidate_cache();
            }
            LiveApply::Mouse => {
                let res = if self.config.ui.mouse {
                    execute!(io::stdout(), EnableMouseCapture)
                } else {
                    execute!(io::stdout(), DisableMouseCapture)
                };
                if let Err(e) = res {
                    tracing::warn!(error = %e, "切换鼠标捕获失败");
                }
            }
        }
    }

    /// 保存配置到 `~/.cyber/config.toml` + providers 到 `~/.cyber/providers.toml`。
    fn save_settings(&mut self) {
        let config_res = save_config(&self.config, &self.paths.config_file);
        let providers_res = if self.settings.dirty_providers {
            save_providers(&self.providers, &self.paths.providers_file)
        } else {
            Ok(())
        };
        match (config_res, providers_res) {
            (Ok(()), Ok(())) => {
                self.settings.dirty = false;
                self.settings.dirty_providers = false;
                self.settings.pending_discard = false;
                self.config_at_entry = self.config.clone();
                self.providers_at_entry = self.providers.clone();
                self.toast = Some("配置已保存".into());
            }
            (Err(e), _) => {
                self.toast = Some(format!("配置保存失败: {e}"));
            }
            (_, Err(e)) => {
                self.toast = Some(format!("providers 保存失败: {e}"));
            }
        }
    }

    /// 退出设置：dirty（config 或 providers）时首次 Esc 提示，二次 Esc 回退到快照后返回 prev_mode。
    fn exit_settings(&mut self) {
        let dirty = self.settings.dirty || self.settings.dirty_providers;
        if dirty {
            if !self.settings.pending_discard {
                self.settings.pending_discard = true;
                self.toast = Some("再按 Esc 丢弃改动，或选择「保存设置」".into());
                return;
            }
            // 二次 Esc：回退到进入时的快照（config + providers）
            self.config = self.config_at_entry.clone();
            self.providers = self.providers_at_entry.clone();
            self.apply_live(LiveApply::Theme);
            self.apply_live(LiveApply::Mouse);
            self.settings.dirty = false;
            self.settings.dirty_providers = false;
            self.settings.pending_discard = false;
            self.toast = Some("已丢弃改动".into());
        }
        self.mode = self.prev_mode;
    }

    fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let chunks = Layout::vertical([
            Constraint::Length(1), // 标题栏
            Constraint::Min(0),    // 主区
            Constraint::Length(1), // 状态栏
        ])
        .split(area);
        self.render_title_bar(frame, chunks[0]);
        self.render_main(frame, chunks[1]);
        self.render_status_bar(frame, chunks[2]);
    }

    fn render_title_bar(&self, frame: &mut Frame, area: Rect) {
        let project_name = self
            .project
            .as_ref()
            .and_then(|p| p.frontmatter.project.clone())
            .unwrap_or_else(|| "<无项目>".into());
        let first_run = if self.first_run { " · first-run" } else { "" };
        let title = Line::from(format!(
            " {} │ {} │ provider={} │ theme={}{} ",
            self.mode.label(),
            project_name,
            self.config.agent.default_provider,
            self.config.ui.theme,
            first_run,
        ));
        frame.render_widget(
            Paragraph::new(title).style(
                Style::default()
                    .bg(self.theme.accent)
                    .fg(self.theme.bg)
                    .add_modifier(Modifier::BOLD),
            ),
            area,
        );
    }

    fn render_main(&self, frame: &mut Frame, area: Rect) {
        match self.mode {
            Mode::Welcome => views::welcome::render(
                frame,
                area,
                &self.theme,
                self.selected,
                self.toast.as_deref(),
            ),
            Mode::Chat => views::chat::render(
                frame,
                area,
                &self.theme,
                &self.chat,
                self.project.as_ref(),
                &self.config.agent.default_provider,
            ),
            Mode::Workflow => views::chat::render_placeholder(
                frame,
                area,
                &self.theme,
                "Workflow Mode",
                "P4 阶段实现（DAG 画布 / 节点编辑 / 并行执行）",
                self.project.as_ref(),
            ),
            Mode::Dashboard => views::chat::render_placeholder(
                frame,
                area,
                &self.theme,
                "Dashboard Mode",
                "P5 阶段实现（工作流监控 / 节点日志 / 资产统计）",
                self.project.as_ref(),
            ),
            Mode::Settings => views::settings::render(
                frame,
                area,
                &self.theme,
                &self.config,
                &self.providers,
                &self.settings,
                self.has_project_config,
            ),
            Mode::ProviderForm => {
                if let Some(form) = &self.provider_form {
                    views::providers::render_form(frame, area, &self.theme, form);
                }
            }
        }
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let hint = match self.mode {
            Mode::Welcome => " ↑/↓ 导航   Enter 确认   s 设置   q 退出",
            Mode::Settings => " ↑↓ 行  Tab 段  Enter 编辑/保存  ←→ 调整  Esc 返回  q 退出",
            Mode::Chat if self.chat.streaming => " ● 流式生成中… Esc 取消 · Ctrl+C 退出",
            Mode::Chat => " ○ 就绪 · 输入消息 Enter 发送",
            Mode::ProviderForm => " ↑↓ 选字段  Enter 编辑/确认  ←→ 切 kind  Esc 取消",
            _ => " Tab 切换模式   s 设置   Esc 返回 Welcome   q 退出",
        };
        frame.render_widget(
            Paragraph::new(Line::from(hint))
                .style(Style::default().bg(self.theme.muted).fg(self.theme.bg)),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyber_core::{Config, ProvidersConfig};

    fn temp_config_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cyber_app_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    fn make_app(initial: Mode, config_file: PathBuf) -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<(u64, AgentEvent)>();
        let (ftx, _frx) = tokio::sync::mpsc::unbounded_channel::<FetchResult>();
        App::new(
            Config::default(),
            ProvidersConfig::default(),
            None,
            initial,
            false,
            AppPaths {
                config_file,
                providers_file: std::env::temp_dir().join("cyber_test_providers.toml"),
                history_dir: std::env::temp_dir(),
                cwd: std::env::temp_dir(),
            },
            false,
            false,
            tx,
            ftx,
        )
    }

    #[test]
    fn open_settings_records_prev_mode() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_action(Action::OpenSettings);
        assert_eq!(app.mode, Mode::Settings);
        assert_eq!(app.prev_mode, Mode::Chat);
        // 已在 Settings 时 OpenSettings 为 no-op（不改 prev_mode）
        app.prev_mode = Mode::Workflow;
        app.handle_action(Action::OpenSettings);
        assert_eq!(app.mode, Mode::Settings);
        assert_eq!(app.prev_mode, Mode::Workflow, "重复 OpenSettings 不应改 prev_mode");
    }

    #[test]
    fn theme_edit_live_applies_and_marks_dirty() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_action(Action::OpenSettings);
        // selected=0 → theme；Right 正向循环：cyberpunk → dracula
        app.handle_action(Action::Right);
        assert_eq!(app.config.ui.theme, "dracula");
        assert_eq!(
            app.theme.bg,
            Theme::resolve("dracula").bg,
            "theme 应即时重新解析"
        );
        assert!(app.settings.dirty);
    }

    #[test]
    fn esc_double_press_rolls_back() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_action(Action::OpenSettings);
        app.handle_action(Action::Right); // 改 theme → dirty
        assert!(app.settings.dirty);
        // 首次 Esc：提示，不退出
        app.handle_action(Action::Esc);
        assert_eq!(app.mode, Mode::Settings, "首次 Esc 不应退出");
        assert!(app.settings.pending_discard);
        // 二次 Esc：回退 + 退出
        app.handle_action(Action::Esc);
        assert_eq!(app.mode, Mode::Chat, "二次 Esc 应返回 prev_mode");
        assert_eq!(app.config.ui.theme, "cyberpunk", "回退应恢复原值");
        assert!(!app.settings.dirty);
        assert_eq!(app.theme.bg, Theme::resolve("cyberpunk").bg, "theme 应回滚");
    }

    #[test]
    fn esc_not_dirty_exits_immediately() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_action(Action::OpenSettings);
        app.handle_action(Action::Esc);
        assert_eq!(app.mode, Mode::Chat, "无改动时 Esc 应直接返回");
    }

    #[test]
    fn save_persists_and_clears_dirty() {
        let path = temp_config_path();
        let mut app = make_app(Mode::Chat, path.clone());
        app.handle_action(Action::OpenSettings);
        app.handle_action(Action::Right); // theme → dracula, dirty
        assert!(app.settings.dirty);
        // 导航到保存行（UI 段 5 字段，Down 5 次到 selected=5）
        for _ in 0..5 {
            app.handle_action(Action::Down);
        }
        assert!(app.settings.on_save_row());
        app.handle_action(Action::Enter); // 保存
        assert!(!app.settings.dirty, "保存后 dirty 应清");
        assert!(path.exists(), "配置文件应已写入");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("theme = \"dracula\""),
            "磁盘应写入新主题值，实际: {raw}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn welcome_item_3_enters_settings() {
        let mut app = make_app(Mode::Welcome, temp_config_path());
        // selected 0→3
        for _ in 0..3 {
            app.handle_action(Action::Down);
        }
        assert_eq!(app.selected, 3);
        app.handle_action(Action::Enter);
        assert_eq!(app.mode, Mode::Settings);
        assert_eq!(app.prev_mode, Mode::Welcome);
    }

    #[test]
    fn tab_in_settings_switches_section() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_action(Action::OpenSettings);
        assert_eq!(app.settings.section, 0);
        app.handle_action(Action::Tab);
        assert_eq!(app.settings.section, 1, "Tab 应切到下一段");
        assert_eq!(app.settings.selected, 0, "切段应重置 selected");
    }

    #[test]
    fn settings_and_providers_render_without_panic() {
        use ratatui::{backend::TestBackend, Terminal};
        // UI 段（默认）渲染
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.mode = Mode::Settings;
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();

        // Providers 段 + 项目级覆盖横幅渲染（has_project_config=true）
        let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel::<(u64, AgentEvent)>();
        let (ftx2, _frx2) = tokio::sync::mpsc::unbounded_channel::<FetchResult>();
        let mut app2 = App::new(
            Config::default(),
            ProvidersConfig::default_template(),
            None,
            Mode::Chat,
            false,
            AppPaths {
                config_file: temp_config_path(),
                providers_file: std::env::temp_dir().join("cyber_test_providers2.toml"),
                history_dir: std::env::temp_dir(),
                cwd: std::env::temp_dir(),
            },
            true,
            false,
            tx2,
            ftx2,
        );
        app2.mode = Mode::Settings;
        app2.settings.section = 5; // Providers
        let mut t2 = Terminal::new(TestBackend::new(90, 24)).unwrap();
        t2.draw(|f| app2.render(f)).unwrap();
    }

    #[test]
    fn welcome_with_four_options_renders() {
        use ratatui::{backend::TestBackend, Terminal};
        let app = make_app(Mode::Welcome, temp_config_path());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
    }

    #[test]
    fn chat_view_renders_with_state() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.chat
            .entries
            .push(crate::chat::ChatEntry::User("hello".into()));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
    }

    #[tokio::test]
    async fn chat_submit_spawns_agent_and_sets_streaming() {
        // mock=true 避免 spawn 的 agent 任务尝试真实 provider（需 tokio runtime + 无 key）
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<(u64, AgentEvent)>();
        let (ftx, _frx) = tokio::sync::mpsc::unbounded_channel::<FetchResult>();
        let mut app = App::new(
            Config::default(),
            ProvidersConfig::default_template(),
            None,
            Mode::Chat,
            false,
            AppPaths {
                config_file: temp_config_path(),
                providers_file: std::env::temp_dir().join("cyber_test_providers3.toml"),
                history_dir: std::env::temp_dir(),
                cwd: std::env::temp_dir(),
            },
            false,
            true, // mock
            tx,
            ftx,
        );
        app.chat.input.insert_str("你好");
        // 模拟 Submit（无修饰 Enter）
        let k = crossterm::event::KeyEvent::new_with_kind_and_state(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Press,
            crossterm::event::KeyEventState::NONE,
        );
        app.handle_chat_key(k);
        assert!(app.chat.streaming, "submit 后应 streaming");
        assert_eq!(app.chat.entries.len(), 1);
        assert!(matches!(&app.chat.entries[0], crate::chat::ChatEntry::User(c) if c == "你好"));
        assert!(app.agent_handle.is_some(), "应已 spawn agent 任务");
        assert_eq!(app.generation, 1, "spawn 应 bump generation");
    }

    #[test]
    fn chat_done_event_finalizes_stream() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.chat.streaming = true;
        app.chat.streaming_buffer.push_str("收到：hi");
        app.handle_agent_event(0, AgentEvent::Token("（续）".into()));
        assert_eq!(app.chat.streaming_buffer, "收到：hi（续）");
        app.handle_agent_event(0, AgentEvent::Done);
        assert!(!app.chat.streaming, "Done 后应退出 streaming");
        assert_eq!(app.chat.entries.len(), 1);
        assert!(matches!(&app.chat.entries[0], crate::chat::ChatEntry::Assistant(c) if c == "收到：hi（续）"));
    }

    #[test]
    fn chat_error_event_finalizes_and_toasts() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.chat.streaming = true;
        app.chat.streaming_buffer.push_str("部分");
        app.handle_agent_event(0, AgentEvent::Error("boom".into()));
        assert!(!app.chat.streaming);
        assert_eq!(app.chat.entries.len(), 1, "应定稿部分 buffer 为 assistant 条目");
        assert!(app.toast.as_deref().unwrap_or("").contains("boom"));
    }

    #[test]
    fn chat_cancel_aborts_and_drops_buffer() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.chat.streaming = true;
        app.chat.streaming_buffer.push_str("部分");
        let k = crossterm::event::KeyEvent::new_with_kind_and_state(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Press,
            crossterm::event::KeyEventState::NONE,
        );
        app.handle_chat_key(k);
        assert!(!app.chat.streaming);
        assert!(app.chat.entries.is_empty(), "取消不应追加 assistant 条目");
        assert!(app.chat.streaming_buffer.is_empty());
    }

    #[test]
    fn stale_token_after_cancel_ignored() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.chat.streaming = true;
        app.generation = 1; // 模拟已 spawn（gen=1）
        // cancel：bump generation 到 2（与 Back 路径一致）
        app.generation = app.generation.wrapping_add(1);
        app.chat.cancel_stream();
        // 旧任务（gen=1）的 stale token 应被 generation 守卫忽略
        app.handle_agent_event(1, AgentEvent::Token("late".into()));
        assert!(app.chat.streaming_buffer.is_empty(), "stale gen 的 token 应被忽略");
    }

    #[test]
    fn tool_call_and_result_events_render_entries() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.chat.streaming = true;
        app.generation = 1;
        // 先收到一段 token，再 tool call（应 flush 为 assistant），再 result
        app.handle_agent_event(1, AgentEvent::Token("让我看看".into()));
        app.handle_agent_event(
            1,
            AgentEvent::ToolCall {
                id: "c1".into(),
                name: "list_dir".into(),
                arguments: "{\"path\":\".\"}".into(),
            },
        );
        app.handle_agent_event(
            1,
            AgentEvent::ToolResult {
                id: "c1".into(),
                name: "list_dir".into(),
                output: "a.txt".into(),
                is_error: false,
            },
        );
        assert!(app.chat.streaming, "工具调用后仍应 streaming");
        assert_eq!(app.chat.entries.len(), 3, "应含 assistant + toolcall + toolresult");
        assert!(matches!(&app.chat.entries[0], crate::chat::ChatEntry::Assistant(c) if c == "让我看看"));
        assert!(matches!(&app.chat.entries[1], crate::chat::ChatEntry::ToolCall { name, .. } if name == "list_dir"));
        assert!(matches!(&app.chat.entries[2], crate::chat::ChatEntry::ToolResult { output, .. } if output == "a.txt"));
    }

    #[test]
    fn chat_ctrl_comma_opens_settings() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        let k = crossterm::event::KeyEvent::new_with_kind_and_state(
            crossterm::event::KeyCode::Char(','),
            crossterm::event::KeyModifiers::CONTROL,
            crossterm::event::KeyEventKind::Press,
            crossterm::event::KeyEventState::NONE,
        );
        app.handle_chat_key(k);
        assert_eq!(app.mode, Mode::Settings);
        assert_eq!(app.prev_mode, Mode::Chat);
    }

    #[test]
    fn chat_plain_q_is_input_not_quit() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        let k = crossterm::event::KeyEvent::new_with_kind_and_state(
            crossterm::event::KeyCode::Char('q'),
            crossterm::event::KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Press,
            crossterm::event::KeyEventState::NONE,
        );
        app.handle_chat_key(k);
        assert!(!app.should_quit, "Chat 内 plain q 不应退出");
        // q 应进入 textarea
        assert_eq!(app.chat.input.lines().join(""), "q");
    }

    #[test]
    fn save_history_persists_to_cwd_hash_file() {
        use crate::history;
        let hist_dir = std::env::temp_dir().join(format!(
            "cyber_app_hist_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&hist_dir);
        std::fs::create_dir_all(&hist_dir).unwrap();
        let cwd = std::env::temp_dir().join("cyber_test_proj_unique");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<(u64, AgentEvent)>();
        let (ftx, _frx) = tokio::sync::mpsc::unbounded_channel::<FetchResult>();
        let mut app = App::new(
            Config::default(),
            ProvidersConfig::default(),
            None,
            Mode::Chat,
            false,
            AppPaths {
                config_file: temp_config_path(),
                providers_file: std::env::temp_dir().join("cyber_test_providers4.toml"),
                history_dir: hist_dir.clone(),
                cwd: cwd.clone(),
            },
            false,
            false,
            tx,
            ftx,
        );
        app.chat.entries.push(ChatEntry::User("你好".into()));
        app.chat.entries.push(ChatEntry::Assistant("收到".into()));
        app.save_history();

        let file = history::history_file(&hist_dir, &cwd);
        assert!(file.exists(), "save_history 应写入 cwd_hash.json 文件");
        let loaded = history::load(&hist_dir, &cwd);
        assert_eq!(loaded.len(), 2, "重新加载应得到相同条目数");
        assert!(matches!(&loaded[0], ChatEntry::User(c) if c == "你好"));
        assert!(matches!(&loaded[1], ChatEntry::Assistant(c) if c == "收到"));
        let _ = std::fs::remove_dir_all(&hist_dir);
    }

    #[test]
    fn done_event_persists_history() {
        // Done 事件应触发 save_history，历史写入磁盘
        use crate::history;
        let hist_dir = std::env::temp_dir().join(format!(
            "cyber_app_done_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&hist_dir);
        std::fs::create_dir_all(&hist_dir).unwrap();
        let cwd = std::env::temp_dir().join("cyber_test_done_proj");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<(u64, AgentEvent)>();
        let (ftx, _frx) = tokio::sync::mpsc::unbounded_channel::<FetchResult>();
        let mut app = App::new(
            Config::default(),
            ProvidersConfig::default(),
            None,
            Mode::Chat,
            false,
            AppPaths {
                config_file: temp_config_path(),
                providers_file: std::env::temp_dir().join("cyber_test_providers5.toml"),
                history_dir: hist_dir.clone(),
                cwd: cwd.clone(),
            },
            false,
            false,
            tx,
            ftx,
        );
        app.chat.streaming = true;
        app.generation = 0;
        app.chat.streaming_buffer.push_str("回复内容");
        app.handle_agent_event(0, AgentEvent::Done);
        assert!(!app.chat.streaming, "Done 应退出 streaming");
        let loaded = history::load(&hist_dir, &cwd);
        assert_eq!(loaded.len(), 1, "Done 应已持久化 assistant 条目");
        assert!(
            matches!(&loaded[0], ChatEntry::Assistant(c) if c == "回复内容"),
            "持久化的应为 finalize 后的 assistant 条目"
        );
        let _ = std::fs::remove_dir_all(&hist_dir);
    }
}
