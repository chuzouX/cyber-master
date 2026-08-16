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

use std::cell::Cell;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyEvent, KeyEventKind, MouseEventKind,
};
use crossterm::execute;
use futures::StreamExt;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    DefaultTerminal, Frame,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tracing::info;

use cyber_agent::{
    context_remaining_percent, fetch_models, run_compact_stream, run_stream, run_writeup_stream,
    AgentEvent, Message, ToolRegistry, Usage,
};
use cyber_core::{
    current_time_str, save_config, save_providers, Config, CtfCategory, CtfChallenge, CtfStatus,
    MemoryScope, MemoryStore, ProjectContext, ProvidersConfig,
};
use cyber_mcp::{McpRegistry, McpServersConfig};
use cyber_skills::SkillRegistry;

use crate::chat::{ChatEntry, ChatState};
use crate::event::{chat_key_to_action, key_to_action, Action, ChatAction};
use crate::history::{SessionIndex, SessionMeta};
use crate::slash::{parse as parse_slash, SlashCommand, HELP_TEXT as SLASH_HELP};
use crate::theme::Theme;
use crate::ctf_store;
use crate::views;
use crate::views::ctf_edit_form::{CtfEditFormAction, CtfEditFormState};
use crate::views::ctf_panel;
use crate::views::mcp_form::{McpFormAction, McpFormState};
use crate::views::env_form::{EnvFormAction, EnvFormState};
use crate::views::providers::{FormAction, ProviderFormState};
use crate::views::settings::{LiveApply, SettingsState};

/// 顶层模式 / 屏幕。Welcome 为启动入口屏，Settings 为模态设置层，ProviderForm 为
/// 服务商新增/编辑模态层（从 Settings 或 Chat 两路进入），ModelPicker 为 `/model` 面板
/// （双栏选 provider + model），Sessions 为会话管理面板（`/sessions` 或 `/new` 触发），
/// 其余三个对应 DESIGN §9。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Welcome,
    Chat,
    Workflow,
    Dashboard,
    Settings,
    ProviderForm,
    McpForm,
    EnvForm,
    ModelPicker,
    Sessions,
    LogViewer,
    CtfEditForm,
    About,
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
            Mode::McpForm => "MCP Form",
            Mode::EnvForm => "Env Form",
            Mode::CtfEditForm => "CTF Edit",
            Mode::ModelPicker => "Model Picker",
            Mode::Sessions => "Sessions",
            Mode::LogViewer => "Logs",
            Mode::About => "About",
        }
    }
}

/// 打包路径参数，避免 `App::new` 的 `too_many_arguments` 进一步恶化。
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub providers_file: PathBuf,
    pub mcp_servers_file: PathBuf,
    pub log_file: PathBuf,
    pub history_dir: PathBuf,
    pub cwd: PathBuf,
    pub ctf_dir: PathBuf,
    pub ctf_writeup_dir: PathBuf,
    /// 全局用户记忆文件（`~/.cyber/memory.md`）。
    pub memory_file: PathBuf,
}

/// 异步模型拉取结果（经 mpsc 通道回传主循环第 4 路 select! 分支）。
#[derive(Debug)]
pub struct FetchResult {
    pub fetch_id: u64,
    pub result: std::result::Result<Vec<String>, String>,
}

/// 统一工具表 + Skill / MCP 注册表。
///
/// `tools` 是 `Arc<ToolRegistry>`，跨 agent turn 共享（MCP 连接长存，不每轮重连）。
/// `skills` 供 `/skill` 命令查找；`mcp` 供 `/mcp` 命令展示状态与退出时 `shutdown_all`。
/// 由 `bootstrap::build_registries` 启动时构建一次，`App::new` 接收后不再变更。
pub struct AppRegistries {
    /// 统一工具表（builtins + Skills + MCP tools），agent 经此调工具。
    pub tools: Arc<ToolRegistry>,
    /// Skill 注册表（`/skill` 查找 + 展示用）。
    pub skills: Arc<SkillRegistry>,
    /// MCP 连接注册表（`/mcp` 展示 + 退出 shutdown）。mock 模式或无 server 时为 None。
    pub mcp: Option<Arc<McpRegistry>>,
    /// CTF 题目共享状态（工具与 App 共享）。
    pub ctf_challenges: Option<Arc<Mutex<Vec<CtfChallenge>>>>,
}

impl std::fmt::Debug for AppRegistries {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppRegistries")
            .field("tools", &self.tools.schemas().len())
            .field("skills", &self.skills.len())
            .field("mcp", &self.mcp.as_ref().map(|m| m.len()))
            .field(
                "ctf_challenges",
                &self.ctf_challenges.as_ref().map(|c| c.lock().map(|g| g.len()).unwrap_or(0)),
            )
            .finish()
    }
}

impl AppRegistries {
    /// 测试用：仅内置工具 + 空 skills + 无 MCP。
    pub fn with_builtins() -> Self {
        Self {
            tools: Arc::new(ToolRegistry::with_builtins()),
            skills: Arc::new(SkillRegistry::new()),
            mcp: None,
            ctf_challenges: None,
        }
    }
}

/// session 内累计 token 用量统计（TUI 底部状态栏显示缓存命中率 + 成本）。
#[derive(Debug, Clone, Default)]
pub struct UsageStats {
    /// 累计缓存命中 token。
    pub cache_hit: u64,
    /// 累计缓存未命中 token。
    pub cache_miss: u64,
    /// 累计输出 token。
    pub completion: u64,
}

impl UsageStats {
    /// 累加单轮 usage。
    fn add(&mut self, u: &Usage) {
        self.cache_hit += u.cache_hit_tokens;
        self.cache_miss += u.cache_miss_tokens;
        self.completion += u.completion_tokens;
    }

    /// 缓存命中率（0.0-1.0）。无输入时返回 0。
    pub(crate) fn hit_rate(&self) -> f64 {
        let total = self.cache_hit + self.cache_miss;
        if total == 0 {
            0.0
        } else {
            self.cache_hit as f64 / total as f64
        }
    }

    /// 计算成本（美元）。需提供价格配置。
    pub(crate) fn cost(&self, price: &cyber_core::PriceConfig) -> f64 {
        let miss_cost = price.input_per_m.unwrap_or(0.0) * self.cache_miss as f64 / 1_000_000.0;
        let hit_cost = price
            .cache_hit_per_m
            .or(price.input_per_m)
            .unwrap_or(0.0)
            * self.cache_hit as f64
            / 1_000_000.0;
        let out_cost = price.output_per_m.unwrap_or(0.0) * self.completion as f64 / 1_000_000.0;
        miss_cost + hit_cost + out_cost
    }
}

/// 上下文使用情况（TUI 状态栏显示剩余百分比）。
///
/// 由 agent 的 `ContextUpdate` 事件更新。`effective_context_length` 为 None 时
/// 表示模型未配置上下文长度 → TUI 不显示百分比段。
#[derive(Debug, Clone, Default)]
pub struct ContextUsage {
    /// 估算的当前消息列表 token 数。
    pub used_tokens: usize,
    /// 模型有效上下文长度（token）。None = 未知。
    pub effective_context_length: Option<u32>,
}

impl ContextUsage {
    /// 计算剩余百分比（0-100）。None 表示上下文长度未知。
    pub(crate) fn remaining_percent(&self) -> Option<u32> {
        context_remaining_percent(self.used_tokens, self.effective_context_length)
    }
}

/// `/sessions` 面板状态：选中索引 + 待删除确认 + 进入时刷新的列表快照。
///
/// `list` 是进入面板时从 `SessionIndex.sessions` 克隆的快照，面板内导航/删除均
/// 操作此快照；切换/新建/删除时同步更新 `App.sessions` 并刷新 `list`。
#[derive(Debug, Default, Clone)]
pub struct SessionsPanelState {
    /// 当前选中项索引（在 `list` 内）。
    pub selected: usize,
    /// 待删除确认：`d` 首次按下记录索引，二次确认执行删除，其它键清除。
    pub pending_delete: Option<usize>,
    /// 进入面板时快照的 session 列表（标题/计数/id/当前标记）。
    pub list: Vec<SessionMeta>,
}

impl SessionsPanelState {
    /// 从 SessionIndex 刷新快照：克隆 sessions + selected 指向 current。
    pub fn refresh(&mut self, idx: &SessionIndex) {
        self.list = idx.sessions.clone();
        self.selected = idx
            .sessions
            .iter()
            .position(|s| s.id == idx.current)
            .unwrap_or(0);
        self.pending_delete = None;
    }
}

/// `/model` 面板状态：双栏选择 provider + model。
///
/// 左栏（providers）选中项变化时自动拉取该 provider 的模型列表；右栏（models）
/// 列出拉取结果，Enter 确认 → 保存 `default_provider` + 更新 `providers[name].model`。
#[derive(Debug, Default, Clone)]
pub struct ModelPickerState {
    /// 当前选中的 provider 索引（在 `sorted_names` 内）。
    pub provider_selected: usize,
    /// 当前选中的 model 索引（在 `models` 内）。
    pub model_selected: usize,
    /// 已拉取的当前 provider 模型列表。
    pub models: Vec<String>,
    /// 是否正在拉取模型列表。
    pub fetching: bool,
    /// fetch_id（防 stale 结果，bump 后旧结果被忽略）。
    pub fetch_id: u64,
    /// 拉取错误信息。
    pub fetch_error: Option<String>,
    /// 焦点：false=provider 列表，true=model 列表。
    pub focus_models: bool,
}

impl ModelPickerState {
    /// bump fetch_id + 置 fetching + 清空旧结果。返回新 fetch_id 供 App spawn 任务。
    pub fn start_fetch(&mut self) -> u64 {
        self.fetch_id = self.fetch_id.wrapping_add(1);
        self.fetching = true;
        self.fetch_error = None;
        self.models.clear();
        self.model_selected = 0;
        self.fetch_id
    }

    /// 接收拉取结果。fetch_id 不匹配（已发起新一轮或面板已重开）则丢弃。
    pub fn deliver_fetch(&mut self, fetch_id: u64, result: Result<Vec<String>, String>) {
        if fetch_id != self.fetch_id {
            return;
        }
        self.fetching = false;
        match result {
            Ok(models) => {
                if models.is_empty() {
                    self.fetch_error = Some("未返回任何模型".into());
                } else {
                    self.models = models;
                    self.model_selected = 0;
                }
            }
            Err(e) => {
                self.fetch_error = Some(e);
            }
        }
    }
}

/// 日志查看器状态（Ctrl+L 打开，Esc 关闭）。
///
/// 进入时从 `log_file` 读取尾部 N 行缓存到 `lines`，之后滚动纯操作 `scroll`。
/// 日志文件可能很大，只保留最后 1000 行避免内存膨胀。
#[derive(Debug, Default, Clone)]
pub struct LogViewerState {
    /// 已缓存的日志行（从文件尾部读取，最多 1000 行）。
    pub lines: Vec<String>,
    /// 当前滚动偏移（0 = 尾部最末，增大向上翻）。
    pub scroll: usize,
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
    /// 表单/面板（ProviderForm/McpForm/LogViewer/ModelPicker）的返回模式。
    /// 与 prev_mode 分离：避免从 Settings 打开表单时覆盖 Settings 自身的 prev_mode。
    form_prev_mode: Mode,
    /// 进入 Settings 时的配置快照，供 Esc 双击回退。
    config_at_entry: Config,
    /// 进入 Settings 时的 providers 快照，供 Esc 双击回退（与 config_at_entry 同步）。
    providers_at_entry: ProvidersConfig,
    /// MCP servers 配置（~/.cyber/mcp/servers.toml），设置页可编辑。
    mcp_config: McpServersConfig,
    /// 进入 Settings 时的 MCP 配置快照，供 Esc 双击回退。
    mcp_config_at_entry: McpServersConfig,
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
    /// MCP server 表单状态（Mode::McpForm 时 Some）。
    mcp_form: Option<McpFormState>,
    /// Env var 编辑表单状态（Mode::EnvForm 时 Some）。
    env_form: Option<EnvFormState>,
    /// CTF 题目编辑表单状态（Mode::CtfEditForm 时 Some）。
    ctf_edit_form: Option<CtfEditFormState>,
    /// 是否强制使用 MockProvider（离线冒烟）。
    mock: bool,
    /// 统一工具表 + Skill / MCP 注册表（P3：跨 agent turn 共享）。
    registries: AppRegistries,
    /// 多 session 索引（current + sessions 元数据），同 cwd 内多会话。
    sessions: SessionIndex,
    /// `/sessions` 面板状态（selected + pending_delete + list 快照）。
    sessions_panel: SessionsPanelState,
    /// session 内累计 token 用量（TUI 状态栏显示命中率 + 成本）。
    usage: UsageStats,
    /// 上下文使用情况（TUI 状态栏显示剩余百分比）。
    /// 由 agent 的 `ContextUpdate` 事件更新；None 表示有效上下文长度未知。
    context_usage: ContextUsage,
    /// 是否正在执行上下文压缩（手动 `/compact` 或自动触发）。
    /// 压缩期间阻止提交/切换等操作（与 streaming 期一致）。
    compacting: bool,
    /// CTF 模式是否开启（`/ctf enable` 开启）。
    ctf_enabled: bool,
    /// 题目面板是否可见（Ctrl+T 切换）。
    ctf_panel_visible: bool,
    /// 题目面板是否聚焦（聚焦时按键由面板消费）。
    ctf_panel_focused: bool,
    /// 题目面板是否全屏展开（Ctrl+Left 展开，Ctrl+Right/Esc 恢复）。
    ctf_panel_fullscreen: bool,
    /// CTF 题目列表（与 CtfChallengeTool 共享，Arc<Mutex>）。
    ctf_challenges: Arc<Mutex<Vec<CtfChallenge>>>,
    /// 面板内选中索引。
    ctf_selected: usize,
    /// 面板内视图：false=列表, true=详情。
    ctf_detail_view: bool,
    /// 面板内详情视图滚动偏移。
    ctf_detail_scroll: usize,
    /// 面板内列表视图滚动偏移（render 时按选中项自动调整，用 Cell 供 &self render 写回）。
    ctf_list_scroll: Cell<usize>,
    /// Writeup 生成中：待写入的题目名称（None = 未在生成 writeup）。
    ctf_writeup_pending: Option<String>,
    /// Writeup 生成中：累积的流式 token（Done 时整体写入文件）。
    ctf_writeup_buffer: String,
    /// 日志查看器状态（Ctrl+L 打开，Esc 关闭）。
    log_viewer: LogViewerState,
    /// `/model` 面板状态（双栏选 provider + model）。
    model_picker: ModelPickerState,
    /// 鼠标捕获开关（F9 切换）。
    /// true：滚轮翻页（终端原生选区被禁用）；
    /// false：可拖拽选区复制（滚轮事件会被终端翻译为 ↑/↓，不再路由到 scroll_history）。
    mouse_capture: bool,
}

const WELCOME_OPTIONS: usize = 5;

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        providers: ProvidersConfig,
        mcp_config: McpServersConfig,
        project: Option<ProjectContext>,
        initial: Mode,
        first_run: bool,
        paths: AppPaths,
        has_project_config: bool,
        mock: bool,
        agent_tx: UnboundedSender<(u64, AgentEvent)>,
        fetch_tx: UnboundedSender<FetchResult>,
        registries: AppRegistries,
    ) -> Self {
        let theme = Theme::resolve(&config.ui.theme);
        // 取出 bootstrap 构建的共享 ctf_challenges（与 CtfChallengeTool 共享同一 Arc）。
        // ctf_challenges 初始化为空，run() 中按当前 session 加载。
        let ctf_challenges = registries
            .ctf_challenges
            .clone()
            .unwrap_or_else(|| Arc::new(Mutex::new(Vec::new())));
        Self {
            config_at_entry: config.clone(),
            providers_at_entry: providers.clone(),
            mcp_config_at_entry: mcp_config.clone(),
            config,
            providers,
            mcp_config,
            project,
            theme,
            mode: initial,
            selected: 0,
            toast: None,
            first_run,
            should_quit: false,
            settings: SettingsState::new(),
            prev_mode: initial,
            form_prev_mode: initial,
            paths,
            has_project_config,
            chat: ChatState::new(),
            agent_tx,
            agent_handle: None,
            generation: 0,
            fetch_tx,
            provider_form: None,
            mcp_form: None,
            env_form: None,
            ctf_edit_form: None,
            mock,
            registries,
            sessions: SessionIndex::default(),
            sessions_panel: SessionsPanelState::default(),
            usage: UsageStats::default(),
            context_usage: ContextUsage::default(),
            compacting: false,
            ctf_enabled: false,
            ctf_panel_visible: false,
            ctf_panel_focused: false,
            ctf_panel_fullscreen: false,
            ctf_challenges,
            ctf_selected: 0,
            ctf_detail_view: false,
            ctf_detail_scroll: 0,
            ctf_list_scroll: Cell::new(0),
            ctf_writeup_pending: None,
            ctf_writeup_buffer: String::new(),
            log_viewer: LogViewerState::default(),
            model_picker: ModelPickerState::default(),
            mouse_capture: true,
        }
    }

    /// 设置 toast 消息（启动时展示 boot_errors 用）。
    pub fn set_toast(&mut self, msg: String) {
        self.toast = Some(msg);
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
        // 启动时加载该 cwd 的多 session 索引 + 当前 session 的对话条目。
        // 无历史时 load_index 自动建默认空 session 并写盘；旧单文件历史自动迁移。
        let (index, saved) = crate::history::load_current(&self.paths.history_dir, &self.paths.cwd);
        self.sessions = index;
        if !saved.is_empty() {
            info!(count = saved.len(), "恢复历史对话");
            self.chat.entries.extend(saved);
            // prepare_render 会在首帧通过 len 变化自动重建缓存，无需手动 invalidate。
        }
        // 从已加载的 User 条目派生输入历史，使 ↑/↓ 可跨会话呼出历史指令。
        self.chat.seed_input_history();
        // 加载 CTF 题目：全局 + 当前 session（按名称去重，session 版本优先）
        let global_challenges = ctf_store::load_challenges(&self.paths.ctf_dir);
        let session_challenges =
            ctf_store::load_session_challenges(&self.paths.ctf_dir, &self.sessions.current);
        if let Ok(mut list) = self.ctf_challenges.lock() {
            *list = merge_challenges(global_challenges, session_challenges);
        }
        let mut terminal: DefaultTerminal = ratatui::init();
        // 启用 bracketed paste：终端在粘贴内容前后发送 \e[200~ / \e[201~ 标记，
        // crossterm 解析为 Event::Paste(String) → 整块插入 textarea，而非逐行
        // 拆成 KeyEvent(Enter) 触发误提交。与 Claude Code 的粘贴行为一致。
        execute!(io::stdout(), EnableBracketedPaste)?;
        // 鼠标捕获开关：true 时启用滚轮翻页（默认），false 时不启用（可拖拽选区复制）。
        // F9 在 Chat 模式下随时切换。
        if self.mouse_capture {
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
        // 关闭所有 MCP 连接（发 Shutdown + await actor 退出，回收子进程）
        if let Some(mcp) = self.registries.mcp.as_ref() {
            mcp.shutdown_all().await;
        }
        // 无条件禁用鼠标捕获与 bracketed paste（幂等），避免退出后终端状态泄漏。
        let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        ratatui::restore();
        result?;
        info!(mode = ?self.mode, "TUI 退出");
        Ok(())
    }

    /// 持久化当前 session：写 entries 文件 + 刷新 meta（message_count/updated_at/title 派生）+ 写 index。
    /// 失败仅记日志（不影响会话）。在 Done/Error/cancel/clear/quit 及退出时调用。
    fn save_history(&mut self) {
        crate::history::save_current(
            &self.paths.history_dir,
            &self.paths.cwd,
            &mut self.sessions,
            &self.chat.entries,
        );
        // CTF 题目：全局题目存 challenges.json，session 题目存 sessions/{id}.json
        let challenges: Vec<_> = self
            .ctf_challenges
            .lock()
            .map(|l| l.clone())
            .unwrap_or_default();
        let global: Vec<_> = challenges.iter().filter(|c| c.is_global).cloned().collect();
        let session: Vec<_> = challenges
            .iter()
            .filter(|c| !c.is_global)
            .cloned()
            .collect();
        ctf_store::save_challenges(&self.paths.ctf_dir, &global);
        ctf_store::save_session_challenges(
            &self.paths.ctf_dir,
            &self.sessions.current,
            &session,
        );
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
                _ = tick.tick() => {
                    // 粘贴缓冲兜底：buffer 非空且距上次按键 > 50ms 时 flush
                    if let Some(text) = self.chat.paste_detector.flush_if_stale() {
                        self.chat.paste(&text);
                    }
                }
            }
            // 排空所有待处理的 agent 事件，合并为一次重绘（避免逐 token 触发 draw 卡顿）。
            while let Ok((gen, ae)) = agent_rx.try_recv() {
                self.handle_agent_event(gen, ae);
            }
            // 同理排空 fetch 事件。
            while let Ok(fr) = fetch_rx.try_recv() {
                self.handle_fetch_result(fr);
            }
            if self.should_quit {
                break;
            }
        }
        Ok(())
    }

    /// 处理一个 crossterm 事件：按键仅 Press；Chat 模式额外处理鼠标滚轮滚动历史。
    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) => {
                if k.kind != KeyEventKind::Press {
                    return;
                }
                match self.mode {
                    Mode::Chat => self.handle_chat_key(k),
                    Mode::ProviderForm => self.handle_provider_form_key(k),
                    Mode::McpForm => self.handle_mcp_form_key(k),
                    Mode::EnvForm => self.handle_env_form_key(k),
                    Mode::CtfEditForm => self.handle_ctf_edit_form_key(k),
                    Mode::ModelPicker => self.handle_model_picker_key(k),
                    Mode::Sessions => self.handle_sessions_key(k),
                    Mode::LogViewer => self.handle_log_viewer_key(k),
                    Mode::Welcome | Mode::Workflow | Mode::Dashboard | Mode::Settings | Mode::About => {
                        self.handle_action(key_to_action(k));
                    }
                }
            }
            // Chat 模式：鼠标滚轮滚动历史区（鼠标捕获在 config.ui.mouse 时启用）
            Event::Mouse(m) if self.mode == Mode::Chat => match m.kind {
                MouseEventKind::ScrollUp => self.chat.scroll_history(-3),
                MouseEventKind::ScrollDown => self.chat.scroll_history(3),
                _ => {}
            },
            // 粘贴（bracketed paste）：整块插入当前活跃的 textarea，不触发提交。
            // 未启用 bracketed paste 的终端不会产生此事件，回退为逐字符 KeyEvent。
            Event::Paste(text) => self.handle_paste(text),
            _ => {}
        }
    }

    /// 处理粘贴事件：把文本整块插入当前活跃的 textarea（不触发提交）。
    /// Chat 模式插入输入框；表单模式仅在字段编辑态时插入。
    fn handle_paste(&mut self, text: String) {
        match self.mode {
            Mode::Chat => self.chat.paste(&text),
            Mode::ProviderForm => {
                if let Some(f) = self.provider_form.as_mut() {
                    if f.editing {
                        f.textarea.insert_str(&text);
                    }
                }
            }
            Mode::McpForm => {
                if let Some(f) = self.mcp_form.as_mut() {
                    if f.editing {
                        f.textarea.insert_str(&text);
                    }
                }
            }
            Mode::EnvForm => {
                if let Some(f) = self.env_form.as_mut() {
                    if f.editing {
                        f.textarea.insert_str(&text);
                    }
                }
            }
            Mode::CtfEditForm => {
                if let Some(f) = self.ctf_edit_form.as_mut() {
                    if f.editing {
                        f.textarea.insert_str(&text);
                    }
                }
            }
            _ => {}
        }
    }

    /// Chat 模式按键分发（文本输入态）。
    fn handle_chat_key(&mut self, k: KeyEvent) {
        if self.toast.is_some() {
            self.toast = None;
        }
        // 粘贴检测：快速连续的 Char/Enter 缓冲为整块粘贴，防止 Enter 误触发 Submit。
        // 只对非斜杠菜单态生效（菜单打开时走菜单逻辑，不缓冲）。
        match self.chat.paste_detector.observe(k) {
            crate::chat::KeyDisposition::Buffer => return,
            crate::chat::KeyDisposition::FlushThenProcess => {
                if let Some(text) = self.chat.paste_detector.flush() {
                    self.chat.paste(&text);
                }
                // 继续处理当前键
            }
            crate::chat::KeyDisposition::Process => {}
        }
        // 斜杠补全菜单打开时：Up/Down/Enter/Tab/Esc 由菜单消费，不触发其他动作
        if self.chat.slash_menu_key(k) {
            return;
        }
        // CTF 面板聚焦时：拦截导航键交面板处理
        if self.ctf_panel_focused {
            if let Some(consumed) = self.handle_ctf_panel_key(k) {
                if consumed {
                    return;
                }
            }
        }
        match chat_key_to_action(k) {
            ChatAction::Submit => {
                // 斜杠命令拦截：输入以 `/` 开头时不发 agent，转命令处理
                let peek: String = self.chat.input.lines().join("\n");
                if peek.trim_start().starts_with('/') {
                    let text = self.chat.take_input();
                    self.chat.update_slash_menu(); // 输入已清空 → 关闭菜单
                    self.handle_slash_command(&text);
                } else if let Some((text, history)) = self.chat.submit() {
                    self.spawn_agent(text, history);
                }
            }
            ChatAction::Newline => {
                // Shift/Alt+Enter 或 Ctrl+J：透传给 textarea 插入换行
                if !self.chat.streaming {
                    self.chat.input.input(k);
                    self.chat.update_slash_menu();
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
                    self.mcp_config_at_entry = self.mcp_config.clone();
                    self.settings.pending_discard = false;
                    self.mode = Mode::Settings;
                }
            }
            ChatAction::ToggleLogs => self.toggle_log_viewer(),
            ChatAction::ToggleMouse => self.toggle_mouse_capture(),
            ChatAction::ToggleToolResult => self.chat.toggle_last_tool_result_expansion(),
            ChatAction::ToggleCtfPanel => self.toggle_ctf_panel(),
            ChatAction::Quit => {
                self.save_history();
                self.should_quit = true;
            }
            ChatAction::ScrollPageUp => {
                let page = self.chat.last_visible_height_get() as i32;
                self.chat.scroll_history(-page.max(1));
            }
            ChatAction::ScrollPageDown => {
                let page = self.chat.last_visible_height_get() as i32;
                self.chat.scroll_history(page.max(1));
            }
            ChatAction::ScrollLineUp => self.chat.scroll_history(-1),
            ChatAction::ScrollLineDown => self.chat.scroll_history(1),
            ChatAction::HistoryPrev => {
                // 空输入框时呼出更早的已发送消息；未呼出（非空/无历史）→ 交 textarea 移光标
                if !self.chat.streaming {
                    if self.chat.history_prev() {
                        // 历史浏览态：关闭斜杠菜单，避免菜单拦截后续 Up/Down
                        self.chat.slash_menu.close();
                    } else {
                        self.chat.input.input(k);
                        self.chat.update_slash_menu();
                    }
                }
            }
            ChatAction::HistoryNext => {
                // 浏览态呼出更新；非浏览态 → 交 textarea 移光标
                if !self.chat.streaming {
                    if self.chat.history_next() {
                        self.chat.slash_menu.close();
                    } else {
                        self.chat.input.input(k);
                        self.chat.update_slash_menu();
                    }
                }
            }
            ChatAction::Input => {
                if !self.chat.streaming {
                    self.chat.input.input(k);
                    self.chat.update_slash_menu();
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
                if self.ctf_writeup_pending.is_some() {
                    // writeup 模式：token 累积到 writeup buffer 而非 chat
                    self.ctf_writeup_buffer.push_str(&t);
                    self.chat.streaming_buffer.push_str(&t);
                } else if self.chat.streaming {
                    self.chat.streaming_buffer.push_str(&t);
                }
            }
            AgentEvent::Reasoning(t) => {
                if self.chat.streaming {
                    self.chat.thinking_buffer.push_str(&t);
                }
            }
            AgentEvent::ToolCall { id, name, arguments } => {
                if self.chat.streaming {
                    self.chat.push_tool_call(id, name, arguments);
                }
            }
            AgentEvent::ToolProgress { id: _, name: _, chunk } => {
                if self.chat.streaming {
                    self.chat.push_tool_progress(&chunk);
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
                // writeup 生成完成：保存文件 + 更新题目状态
                if let Some(name) = self.ctf_writeup_pending.take() {
                    let raw = std::mem::take(&mut self.ctf_writeup_buffer);
                    // 兜底：剥离模型可能误输出的工具调用标签，仅保留正文
                    let writeup_text = strip_tool_call_tags(&raw);
                    self.chat.streaming = false;
                    self.chat.streaming_buffer.clear();
                    self.chat.thinking_buffer.clear();
                    self.chat.streaming_tool_output.clear();
                    self.agent_handle = None;
                    if writeup_text.trim().is_empty() {
                        self.chat.entries.push(ChatEntry::System(format!(
                            "writeup 生成失败：未生成有效内容（题目 {name}）"
                        )));
                        return;
                    }
                    // 保存 writeup 到项目级 .cyber/ctf/{category}/{name}/
                    let proj_ctf_dir = self.paths.cwd.join(".cyber").join("ctf");
                    let saved_path = if let Ok(mut list) = self.ctf_challenges.lock() {
                        if let Some(c) = list.iter_mut().find(|c| c.name == name) {
                            c.writeup = Some(writeup_text.clone());
                            let path = ctf_store::save_writeup(
                                &proj_ctf_dir,
                                c,
                                &writeup_text,
                            );
                            path
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    // 持久化题目（锁已释放）：全局→challenges.json，session→sessions/{id}.json
                    self.save_history();
                    self.chat.entries.push(ChatEntry::System(format!(
                        "题目 {} 的 writeup 已生成{}",
                        name,
                        saved_path
                            .map(|p| format!("：{}", p.display()))
                            .unwrap_or_default()
                    )));
                    return;
                }
                if self.chat.streaming {
                    self.chat.finalize_stream();
                    // 任务自然结束，清理句柄（abort 对已完成任务是 no-op，置 None 更整洁）
                    self.agent_handle = None;
                    self.save_history();
                }
                if self.compacting {
                    self.compacting = false;
                    self.save_history();
                }
            }
            AgentEvent::Usage(u) => {
                self.usage.add(&u);
            }
            AgentEvent::ContextUpdate {
                used_tokens,
                effective_context_length,
            } => {
                self.context_usage.used_tokens = used_tokens;
                self.context_usage.effective_context_length = effective_context_length;
            }
            AgentEvent::Compacting { is_auto } => {
                // 自动压缩在 streaming 期间发生，仅追加 System 提示；
                // 手动压缩 compacting 已在 spawn_compact 置 true。
                if is_auto {
                    self.chat.entries.push(ChatEntry::System(
                        "上下文已超过阈值，正在自动压缩…".into(),
                    ));
                } else {
                    self.chat
                        .entries
                        .push(ChatEntry::System("正在压缩上下文…".into()));
                }
            }
            AgentEvent::Compacted {
                summary,
                before_tokens,
                after_tokens,
            } => {
                // 压缩完成：用摘要替换全部对话历史。
                // 摘要作为 User 条目（chat.history() 会转为 user Message），
                // 使下一次请求以摘要为上下文起点。
                self.chat.clear();
                self.chat.entries.push(ChatEntry::User(summary));
                self.usage = UsageStats::default();
                self.context_usage.used_tokens = after_tokens;
                self.chat.entries.push(ChatEntry::System(format!(
                    "上下文已压缩：{} → {} tokens",
                    fmt_compact_tokens(before_tokens),
                    fmt_compact_tokens(after_tokens)
                )));
            }
            AgentEvent::Error(m) => {
                // writeup 生成失败：清理状态
                if self.ctf_writeup_pending.take().is_some() {
                    self.ctf_writeup_buffer.clear();
                    self.chat.streaming = false;
                    self.chat.streaming_buffer.clear();
                    self.chat.thinking_buffer.clear();
                    self.chat.streaming_tool_output.clear();
                    self.agent_handle = None;
                    self.chat
                        .entries
                        .push(ChatEntry::System(format!("writeup 生成失败: {m}")));
                    return;
                }
                if self.chat.streaming {
                    self.chat.finalize_stream();
                    self.save_history();
                }
                if self.compacting {
                    self.compacting = false;
                    self.chat
                        .entries
                        .push(ChatEntry::System(format!("压缩失败: {m}")));
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
                self.mode = self.form_prev_mode;
            }
            FormAction::Save => self.save_provider_form(),
            FormAction::Fetch => self.start_provider_fetch(),
            FormAction::Toast(msg) => self.toast = Some(msg),
        }
    }

    /// 接收异步模型拉取结果：按当前模式分发（ProviderForm / ModelPicker），其余丢弃。
    fn handle_fetch_result(&mut self, fr: FetchResult) {
        match self.mode {
            Mode::ProviderForm => {
                if let Some(form) = self.provider_form.as_mut() {
                    form.deliver_fetch(fr.fetch_id, fr.result);
                }
            }
            Mode::ModelPicker => {
                self.model_picker.deliver_fetch(fr.fetch_id, fr.result);
            }
            _ => {}
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

    /// `/model` 面板：为当前选中 provider 拉取模型列表。
    /// 取 sorted_names[provider_selected] 的配置快照 spawn 异步任务。
    fn start_model_fetch(&mut self) {
        let names = self.providers.sorted_names();
        let Some(name) = names.get(self.model_picker.provider_selected).cloned() else {
            return;
        };
        let Some(cfg_snapshot) = self.providers.providers.get(&name).cloned() else {
            return;
        };
        let fetch_id = self.model_picker.start_fetch();
        let tx = self.fetch_tx.clone();
        tokio::spawn(async move {
            let result = fetch_models(&cfg_snapshot)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(FetchResult { fetch_id, result });
        });
    }

    /// `/model` 面板：确认选择 → 设 default_provider + 更新 provider.model + 持久化 + 返回。
    fn confirm_model_pick(&mut self) {
        let names = self.providers.sorted_names();
        let Some(name) = names.get(self.model_picker.provider_selected).cloned() else {
            return;
        };
        let Some(model) = self.model_picker.models.get(self.model_picker.model_selected).cloned() else {
            self.toast = Some("无模型可选".into());
            return;
        };
        // 更新 provider 的 model 字段
        if let Some(cfg) = self.providers.providers.get_mut(&name) {
            cfg.model = model.clone();
        }
        self.config.agent.default_provider = name.clone();
        // 持久化 config + providers
        let cfg_res = save_config(&self.config, &self.paths.config_file);
        let prov_res = save_providers(&self.providers, &self.paths.providers_file);
        match (cfg_res, prov_res) {
            (Ok(()), Ok(())) => {
                self.toast = Some(format!("已切换到 {name} · {model}"));
            }
            (Err(e), _) => {
                self.toast = Some(format!("config 保存失败: {e}"));
            }
            (_, Err(e)) => {
                self.toast = Some(format!("providers 保存失败: {e}"));
            }
        }
        self.mode = self.form_prev_mode;
    }

    /// `/model` 面板按键分发：双栏导航 + 拉取 + 确认。
    ///
    /// - Provider 栏（focus_models=false）：↑/↓ 切 provider 并自动拉取模型；Tab/Enter 切到 Models 栏。
    /// - Model 栏（focus_models=true）：↑/↓ 选模型；Enter 确认保存；Tab 切回 Providers 栏。
    /// - Esc：返回 prev_mode。
    fn handle_model_picker_key(&mut self, k: KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        // q / Ctrl+C 退出
        if matches!(k.code, KeyCode::Char('q'))
            || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.save_history();
            self.should_quit = true;
            return;
        }
        let provider_count = self.providers.providers.len();
        match k.code {
            KeyCode::Esc => {
                self.mode = self.form_prev_mode;
            }
            KeyCode::Tab => {
                self.model_picker.focus_models = !self.model_picker.focus_models;
            }
            KeyCode::Up => {
                if self.model_picker.focus_models {
                    if self.model_picker.model_selected > 0 {
                        self.model_picker.model_selected -= 1;
                    }
                } else if provider_count > 0 {
                    self.model_picker.provider_selected =
                        (self.model_picker.provider_selected + provider_count - 1) % provider_count;
                    self.start_model_fetch();
                }
            }
            KeyCode::Down => {
                if self.model_picker.focus_models {
                    if self.model_picker.model_selected + 1 < self.model_picker.models.len() {
                        self.model_picker.model_selected += 1;
                    }
                } else if provider_count > 0 {
                    self.model_picker.provider_selected =
                        (self.model_picker.provider_selected + 1) % provider_count;
                    self.start_model_fetch();
                }
            }
            KeyCode::Enter => {
                if self.model_picker.focus_models {
                    if self.model_picker.models.is_empty() {
                        self.toast = Some("无模型可选，请先等待拉取或切换 provider".into());
                    } else {
                        self.confirm_model_pick();
                    }
                } else {
                    // Provider 栏 Enter → 切到 Model 栏（若未拉取则触发拉取）
                    self.model_picker.focus_models = true;
                    if self.model_picker.models.is_empty() && !self.model_picker.fetching {
                        self.start_model_fetch();
                    }
                }
            }
            _ => {}
        }
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
                let from_settings = self.form_prev_mode == Mode::Settings;
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
                self.mode = self.form_prev_mode;
            }
        }
    }

    /// McpForm 模式按键分发：委托 `form.handle_key`，按 `McpFormAction` 执行副作用。
    fn handle_mcp_form_key(&mut self, k: KeyEvent) {
        let Some(form) = self.mcp_form.as_mut() else {
            return;
        };
        let action = form.handle_key(k);
        match action {
            McpFormAction::None => {}
            McpFormAction::Cancel => {
                self.mcp_form = None;
                self.mode = self.form_prev_mode;
            }
            McpFormAction::Save => self.save_mcp_form(),
        }
    }

    /// 保存 MCP 表单：校验 → upsert → 标脏（Settings 延迟保存）→ 返回 prev_mode。
    /// 校验失败时保留表单 + toast，不退出。
    fn save_mcp_form(&mut self) {
        // 先校验（不消费 form），失败则保留表单 + toast
        let result = {
            let Some(form) = self.mcp_form.as_ref() else {
                return;
            };
            let names: Vec<&str> = self.mcp_config.servers.iter().map(|s| s.name.as_str()).collect();
            form.into_spec(&names)
        };
        match result {
            Err(msg) => {
                self.toast = Some(msg);
            }
            Ok(spec) => {
                let name = spec.name.clone();
                let original = self
                    .mcp_form
                    .as_ref()
                    .and_then(|f| f.original_name.clone());
                // 处理重命名：先删旧名再 upsert 新名
                if let Some(orig) = &original {
                    if orig != &name {
                        self.mcp_config.remove(orig);
                    }
                }
                self.mcp_config.upsert(spec);
                self.settings.dirty_mcp = true;
                self.toast = Some(format!("MCP server '{name}' 已暂存（保存设置后写入，重启生效）"));
                self.mcp_form = None;
                self.mode = self.form_prev_mode;
            }
        }
    }

    /// CTF 编辑表单按键分发：委托 `form.handle_key`，按 `CtfEditFormAction` 执行副作用。
    fn handle_ctf_edit_form_key(&mut self, k: KeyEvent) {
        let Some(form) = self.ctf_edit_form.as_mut() else {
            return;
        };
        let action = form.handle_key(k);
        match action {
            CtfEditFormAction::None => {}
            CtfEditFormAction::Cancel => {
                self.ctf_edit_form = None;
                self.mode = self.form_prev_mode;
            }
            CtfEditFormAction::Save => self.save_ctf_edit_form(),
        }
    }

    /// 保存 CTF 编辑表单：将表单值回写到选中题目 → 持久化 → 返回 prev_mode。
    fn save_ctf_edit_form(&mut self) {
        let form = match self.ctf_edit_form.as_ref() {
            Some(f) => f,
            None => return,
        };
        let challenge_id = form.challenge_id.clone();
        let applied = {
            let Ok(list) = self.ctf_challenges.lock() else {
                return;
            };
            let Some(c) = list.iter().find(|c| c.id == challenge_id).cloned() else {
                return;
            };
            form.apply_to(c)
        };
        let name = applied.name.clone();
        let updated = self
            .ctf_challenges
            .lock()
            .map(|mut list| {
                let mut done = false;
                for c in list.iter_mut() {
                    if c.id == challenge_id {
                        *c = applied.clone();
                        done = true;
                        break;
                    }
                }
                done
            })
            .unwrap_or(false);
        if updated {
            self.toast = Some(format!("「{name}」已保存"));
            self.save_history();
        }
        self.ctf_edit_form = None;
        self.mode = self.form_prev_mode;
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
        let registry = self.registries.tools.clone();
        let ctf_enabled = self.ctf_enabled;
        let intensity = self.config.agent.thinking_intensity;
        // 读取两层用户记忆（全局 + 项目级），注入系统提示词。每次 spawn 重读，
        // 使 save_memory 工具 / /memory 命令写入的记忆在下次请求即生效。
        let memory = MemoryStore::new(
            self.paths.memory_file.clone(),
            self.paths.cwd.join(".cyber").join("memory.md"),
        )
        .load_all();
        let handle = tokio::spawn(async move {
            run_stream(
                config, providers, project, text, history, tx, gen, mock, cwd, registry,
                ctf_enabled, intensity, memory,
            )
            .await;
        });
        self.agent_handle = Some(handle);
    }

    /// 拉起一次手动上下文压缩任务（`/compact`）。
    ///
    /// 与 `spawn_agent` 类似：abort 旧任务 + bump generation（隔离 stale 事件）。
    /// 置 `compacting = true` 阻止期间提交/切换；任务结束（Done/Error）由
    /// `handle_agent_event` 复位。
    fn spawn_compact(&mut self, history: Vec<Message>, custom_instructions: Option<String>) {
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
        self.compacting = true;
        let handle = tokio::spawn(async move {
            run_compact_stream(config, providers, project, history, custom_instructions, tx, gen, mock).await;
        });
        self.agent_handle = Some(handle);
    }

    /// 拉起一次 writeup 生成任务（`/ctf writeup <name>` 或面板 'w' 键）。
    ///
    /// 与 `spawn_compact` 类似：abort 旧任务 + bump generation。
    /// 置 `ctf_writeup_pending = Some(name)` 标记 writeup 模式，
    /// `handle_agent_event` 据此将 Token 事件导入 buffer 而非 chat。
    fn spawn_writeup(&mut self, challenge: &CtfChallenge) {
        // 查找 ctf-writeup skill body（撰写指南作为 system prompt）
        let skill_body = self
            .registries
            .skills
            .find("ctf-writeup")
            .map(|s| s.body.clone())
            .unwrap_or_else(|| {
                "# Writeup 撰写\n\n请为以下 CTF 题目撰写一篇结构清晰的 writeup，包含题目信息、解题过程、关键知识点。".into()
            });

        // 构造题目上下文
        let mut ctx = format!(
            "请为以下 CTF 题目撰写 writeup：\n\n\
             题目名称：{}\n\
             分类：{}\n\
             描述：{}\n",
            challenge.name,
            challenge.category.label(),
            if challenge.description.is_empty() {
                "（无描述）"
            } else {
                &challenge.description
            },
        );
        if let Some(target) = &challenge.target {
            ctx.push_str(&format!("靶机：{target}\n"));
        }
        if let Some(flag) = &challenge.flag {
            ctx.push_str(&format!("Flag：{flag}\n"));
        }
        if !challenge.tags.is_empty() {
            ctx.push_str(&format!("标签：{}\n", challenge.tags.join("、")));
        }
        if let Some(kp) = &challenge.key_points {
            ctx.push_str(&format!("关键知识点/卡点：{kp}\n"));
        }
        ctx.push_str(&format!(
            "开始时间：{}  结束时间：{}\n",
            challenge.start_time,
            challenge.end_time.as_deref().unwrap_or("--"),
        ));
        if let Some(dur) = challenge.duration_str() {
            ctx.push_str(&format!("用时：{dur}\n"));
        }
        ctx.push_str(&format!(
            "\n请根据以上信息撰写一篇完整的 writeup（Markdown 格式）。\n\
             注意：writeup 中的代码应简洁可复现。如有 exp 脚本，可在 writeup 中引用，\n\
             exp 文件路径为 .cyber/ctf/{}/{}/exp.py（或其他扩展名，放在项目级 .cyber 目录）。",
            challenge.category.as_str(),
            challenge.name,
        ));

        // abort 旧任务 + bump generation
        if let Some(h) = self.agent_handle.take() {
            h.abort();
        }
        self.generation = self.generation.wrapping_add(1);
        let gen = self.generation;
        let config = self.config.clone();
        let providers = self.providers.clone();
        let tx = self.agent_tx.clone();
        let mock = self.mock;

        self.ctf_writeup_pending = Some(challenge.name.clone());
        self.ctf_writeup_buffer.clear();
        self.chat.streaming = true;
        self.chat.streaming_buffer.clear();
        self.chat.thinking_buffer.clear();

        let handle = tokio::spawn(async move {
            run_writeup_stream(config, providers, skill_body, ctx, tx, gen, mock).await;
        });
        self.agent_handle = Some(handle);
    }

    // ── Session 管理（/new + /sessions + 面板） ───────────────────────────

    /// 切换到指定 session：保存当前 → 加载目标 entries → 重置 chat → 更新 current。
    /// id 不存在或已是 current → toast 提示，不切换。
    fn switch_session(&mut self, id: &str) {
        if id == self.sessions.current {
            self.toast = Some("已在当前会话".into());
            return;
        }
        if self.sessions.get(id).is_none() {
            self.toast = Some(format!("未知会话：{id}"));
            return;
        }
        // 保存当前 session（含 CTF 题目）
        self.save_history();
        // 加载目标 session
        let entries =
            crate::history::load_entries(&self.paths.history_dir, &self.paths.cwd, id);
        self.chat = ChatState::new();
        self.chat.entries.extend(entries);
        self.chat.seed_input_history();
        self.usage = UsageStats::default();
        self.sessions.current = id.to_string();
        // 加载目标 session 的 CTF 题目（全局题目保留不动）
        let global_challenges = ctf_store::load_challenges(&self.paths.ctf_dir);
        let session_challenges =
            ctf_store::load_session_challenges(&self.paths.ctf_dir, id);
        if let Ok(mut list) = self.ctf_challenges.lock() {
            *list = merge_challenges(global_challenges, session_challenges);
        }
        // 重置面板选中状态
        self.ctf_selected = 0;
        self.ctf_detail_view = false;
        self.ctf_detail_scroll = 0;
        self.ctf_list_scroll.set(0);
        crate::history::save_index(&self.paths.history_dir, &self.paths.cwd, &self.sessions);
        let title = self
            .sessions
            .current_meta()
            .map(|m| m.title.clone())
            .unwrap_or_default();
        self.toast = Some(format!("已切换到「{title}」"));
    }

    /// 新建空 session：保存当前 → 建 meta + 切到新 session → 重置 chat → 持久化。
    fn create_session(&mut self) {
        self.save_history();
        let meta = crate::history::create_session_meta();
        let new_id = meta.id.clone();
        self.sessions.sessions.push(meta);
        self.sessions.current = new_id.clone();
        self.chat = ChatState::new();
        self.usage = UsageStats::default();
        // 新 session 无题目
        if let Ok(mut list) = self.ctf_challenges.lock() {
            list.clear();
        }
        self.ctf_selected = 0;
        self.ctf_detail_view = false;
        self.ctf_detail_scroll = 0;
        self.ctf_list_scroll.set(0);
        crate::history::save_index(&self.paths.history_dir, &self.paths.cwd, &self.sessions);
        crate::history::save_entries(
            &self.paths.history_dir,
            &self.paths.cwd,
            &new_id,
            &[],
        );
        self.toast = Some("新会话已创建".into());
    }

    /// 删除指定 session：拒绝删除最后一个；删 current 则切到剩余首个并重载 chat。
    /// 返回 true 表示已删除（供面板决定是否刷新）。
    fn delete_session(&mut self, id: &str) -> bool {
        if self.sessions.sessions.len() <= 1 {
            self.toast = Some("至少保留 1 个会话（无法删除最后一个）".into());
            return false;
        }
        let was_current = id == self.sessions.current;
        // 删除 session 的 CTF 题目文件
        ctf_store::delete_session_challenges(&self.paths.ctf_dir, id);
        let _remaining = crate::history::delete_session(
            &self.paths.history_dir,
            &self.paths.cwd,
            id,
        );
        // 重新加载索引（delete_session 已处理 current 重指 + 写盘）
        self.sessions = crate::history::load_index(&self.paths.history_dir, &self.paths.cwd);
        if was_current {
            // 切到新的 current（已由 history::delete_session 切到剩余首个）
            let entries = crate::history::load_entries(
                &self.paths.history_dir,
                &self.paths.cwd,
                &self.sessions.current,
            );
            self.chat = ChatState::new();
            self.chat.entries.extend(entries);
            self.chat.seed_input_history();
            // 加载新 current session 的 CTF 题目
            let new_challenges =
                ctf_store::load_session_challenges(&self.paths.ctf_dir, &self.sessions.current);
            if let Ok(mut list) = self.ctf_challenges.lock() {
                *list = new_challenges;
            }
            self.ctf_selected = 0;
            self.ctf_detail_view = false;
            self.ctf_detail_scroll = 0;
            self.ctf_list_scroll.set(0);
        }
        self.toast = Some("会话已删除".into());
        true
    }

    /// 处理 `/sessions <list|read <id|关键词>|new>` 斜杠命令。
    /// 流式期阻止（与 /mode 一致）。list → 打开面板；read → 跨读注入 System；new → 新建。
    fn handle_sessions_slash(&mut self, args: &str) {
        if self.chat.streaming {
            self.chat.entries.push(ChatEntry::System(
                "生成中，无法管理会话（先 /cancel）".into(),
            ));
            return;
        }
        let mut parts = args.splitn(2, char::is_whitespace);
        let sub = parts.next().unwrap_or("").to_lowercase();
        let rest = parts.next().unwrap_or("").trim();
        match sub.as_str() {
            "" | "list" => {
                self.sessions_panel.refresh(&self.sessions);
                self.form_prev_mode = Mode::Chat;
                self.mode = Mode::Sessions;
            }
            "read" => {
                if rest.is_empty() {
                    // 列出所有 session
                    let mut lines = String::from("会话列表：");
                    for s in &self.sessions.sessions {
                        let star = if s.id == self.sessions.current {
                            " ★当前"
                        } else {
                            ""
                        };
                        lines.push_str(&format!(
                            "\n  {} [{}] · {} 条消息{}",
                            s.title, s.id, s.message_count, star
                        ));
                    }
                    self.chat.entries.push(ChatEntry::System(lines));
                } else {
                    // 精确匹配 id 或部分匹配 title
                    let matches: Vec<&SessionMeta> = self
                        .sessions
                        .sessions
                        .iter()
                        .filter(|s| s.id == rest || s.title.contains(rest))
                        .collect();
                    if matches.is_empty() {
                        self.chat.entries.push(ChatEntry::System(format!(
                            "未找到匹配「{rest}」的会话"
                        )));
                    } else if matches.len() > 1 {
                        let mut lines =
                            format!("多个会话匹配「{rest}」，请用 id 指定：");
                        for s in matches {
                            lines.push_str(&format!(
                                "\n  {} [{}] · {} 条消息",
                                s.title, s.id, s.message_count
                            ));
                        }
                        self.chat.entries.push(ChatEntry::System(lines));
                    } else {
                        let s = matches[0];
                        match crate::history::read_session_text(
                            &self.paths.history_dir,
                            &self.paths.cwd,
                            &s.id,
                        ) {
                            Some(text) => {
                                self.chat.entries.push(ChatEntry::System(format!(
                                    "📖 会话「{}」内容：\n{}",
                                    s.title, text
                                )));
                            }
                            None => {
                                self.chat.entries.push(ChatEntry::System(format!(
                                    "会话「{}」为空",
                                    s.title
                                )));
                            }
                        }
                    }
                }
            }
            "new" => self.create_session(),
            other => {
                self.chat.entries.push(ChatEntry::System(format!(
                    "未知子命令：{other}（用法：/sessions <list|read <id|关键词>|new>）"
                )));
            }
        }
    }

    /// `/sessions` 面板按键处理：Up/Down 选、Enter 切换、n 新建、d 删除（双击确认）、Esc 返回。
    /// 直接处理原始 KeyEvent，不污染 Action 枚举（仿 handle_provider_form_key）。
    fn handle_sessions_key(&mut self, k: KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        // q / Ctrl+C 退出
        if matches!(k.code, KeyCode::Char('q'))
            || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.save_history();
            self.should_quit = true;
            return;
        }

        let len = self.sessions_panel.list.len();

        // 'd' 单独处理（双击确认删除）
        if matches!(k.code, KeyCode::Char('d')) {
            if self.sessions_panel.pending_delete == Some(self.sessions_panel.selected) {
                // 二次确认：执行删除
                if let Some(meta) =
                    self.sessions_panel.list.get(self.sessions_panel.selected).cloned()
                {
                    let id = meta.id.clone();
                    if self.delete_session(&id) {
                        self.sessions_panel.refresh(&self.sessions);
                    }
                }
            } else {
                // 首次 d：标记待删除
                self.sessions_panel.pending_delete = Some(self.sessions_panel.selected);
            }
            return;
        }

        // 其它键均清除 pending_delete（"任一其他键取消"）
        self.sessions_panel.pending_delete = None;

        match k.code {
            KeyCode::Up => {
                if len > 0 {
                    self.sessions_panel.selected =
                        (self.sessions_panel.selected + len - 1) % len;
                }
            }
            KeyCode::Down => {
                if len > 0 {
                    self.sessions_panel.selected = (self.sessions_panel.selected + 1) % len;
                }
            }
            KeyCode::Enter => {
                if let Some(meta) =
                    self.sessions_panel.list.get(self.sessions_panel.selected).cloned()
                {
                    let id = meta.id.clone();
                    self.switch_session(&id);
                    self.mode = Mode::Chat;
                }
            }
            KeyCode::Char('n') => {
                self.create_session();
                self.mode = Mode::Chat;
            }
            KeyCode::Esc => {
                self.mode = Mode::Chat;
            }
            _ => {}
        }
    }

    /// 切换日志查看器：已开则关（回 form_prev_mode），未开则读日志文件进入。
    fn toggle_log_viewer(&mut self) {
        if self.mode == Mode::LogViewer {
            self.mode = self.form_prev_mode;
            return;
        }
        // 读取日志文件尾部（最多 1000 行）
        self.log_viewer.lines = read_log_tail(&self.paths.log_file, 1000);
        self.log_viewer.scroll = 0; // 0 = 定位到最末
        self.form_prev_mode = self.mode;
        self.mode = Mode::LogViewer;
    }

    /// 切换鼠标捕获（F9）。
    /// - 开启时：滚轮翻页（终端原生选区被禁用）。
    /// - 关闭时：可拖拽选区复制（滚轮会被终端翻译为 ↑/↓，不再路由到 scroll_history）。
    fn toggle_mouse_capture(&mut self) {
        self.mouse_capture = !self.mouse_capture;
        let res = if self.mouse_capture {
            execute!(io::stdout(), EnableMouseCapture)
        } else {
            execute!(io::stdout(), DisableMouseCapture)
        };
        if let Err(e) = res {
            tracing::warn!(error = %e, "切换鼠标捕获失败");
        }
        self.toast = if self.mouse_capture {
            Some("鼠标已启用：滚轮可翻页（F9 切回选择模式）".into())
        } else {
            Some("鼠标已禁用：可拖拽选区复制（F9 切回滚轮模式）".into())
        };
    }

    /// 切换 CTF 题目面板可见性 / 聚焦（Ctrl+T）。
    /// 三态循环：不可见 → 可见+聚焦 → 隐藏；可见但不聚焦 → 聚焦。
    fn toggle_ctf_panel(&mut self) {
        if !self.ctf_enabled {
            self.toast = Some("CTF 模式未开启（/ctf enable）".into());
            return;
        }
        if self.ctf_panel_visible && self.ctf_panel_focused {
            // 面板可见且聚焦 → 隐藏面板
            self.ctf_panel_visible = false;
            self.ctf_panel_focused = false;
            self.ctf_panel_fullscreen = false;
        } else if self.ctf_panel_visible {
            // 面板可见但不聚焦 → 聚焦面板
            self.ctf_panel_focused = true;
        } else {
            // 面板不可见 → 显示并聚焦
            self.ctf_panel_visible = true;
            self.ctf_panel_focused = true;
        }
    }

    /// 处理 CTF 面板按键。返回 true 表示已消费（不传给 chat）。
    fn handle_ctf_panel_key(&mut self, k: KeyEvent) -> Option<bool> {
        use crossterm::event::{KeyCode, KeyModifiers};
        // Ctrl+Left: 面板全屏展开
        if k.code == KeyCode::Left && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.ctf_panel_fullscreen = true;
            return Some(true);
        }
        // Ctrl+Right: 恢复面板大小
        if k.code == KeyCode::Right && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.ctf_panel_fullscreen = false;
            return Some(true);
        }
        // Shift+Esc: 取消面板聚焦，直接回到对话（不退出详情视图 / 不还原全屏）
        if k.code == KeyCode::Esc && k.modifiers.contains(KeyModifiers::SHIFT) {
            self.ctf_panel_focused = false;
            return Some(true);
        }
        match k.code {
            KeyCode::Esc => {
                if self.ctf_panel_fullscreen {
                    // 全屏 → 先恢复面板大小
                    self.ctf_panel_fullscreen = false;
                } else if self.ctf_detail_view {
                    // 详情视图 → 返回列表
                    self.ctf_detail_view = false;
                    self.ctf_detail_scroll = 0;
                } else {
                    // 列表视图 → 取消聚焦（面板保持可见）
                    self.ctf_panel_focused = false;
                }
                Some(true)
            }
            KeyCode::Up => {
                if !self.ctf_detail_view && self.ctf_selected > 0 {
                    self.ctf_selected -= 1;
                } else if self.ctf_detail_view {
                    self.ctf_detail_scroll = self.ctf_detail_scroll.saturating_sub(1);
                }
                Some(true)
            }
            KeyCode::Down => {
                if !self.ctf_detail_view {
                    let len = self.ctf_challenges.lock().map(|l| l.len()).unwrap_or(0);
                    if self.ctf_selected + 1 < len {
                        self.ctf_selected += 1;
                    }
                } else {
                    self.ctf_detail_scroll += 1;
                }
                Some(true)
            }
            KeyCode::PageUp => {
                if !self.ctf_detail_view {
                    // 列表视图：上移 5 项
                    self.ctf_selected = self.ctf_selected.saturating_sub(5);
                } else {
                    self.ctf_detail_scroll = self.ctf_detail_scroll.saturating_sub(10);
                }
                Some(true)
            }
            KeyCode::PageDown => {
                if !self.ctf_detail_view {
                    // 列表视图：下移 5 项
                    let len = self.ctf_challenges.lock().map(|l| l.len()).unwrap_or(0);
                    self.ctf_selected = (self.ctf_selected + 5).min(len.saturating_sub(1));
                } else {
                    self.ctf_detail_scroll += 10;
                }
                Some(true)
            }
            KeyCode::Enter => {
                if !self.ctf_detail_view {
                    // 列表 → 进入详情
                    let len = self.ctf_challenges.lock().map(|l| l.len()).unwrap_or(0);
                    if self.ctf_selected < len {
                        self.ctf_detail_view = true;
                        self.ctf_detail_scroll = 0;
                    }
                }
                Some(true)
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                // 列表视图：删除选中题目
                if !self.ctf_detail_view {
                    let removed_name = self
                        .ctf_challenges
                        .lock()
                        .ok()
                        .and_then(|mut list| {
                            if self.ctf_selected < list.len() {
                                let name = list[self.ctf_selected].name.clone();
                                list.remove(self.ctf_selected);
                                Some(name)
                            } else {
                                None
                            }
                        });
                    if let Some(name) = removed_name {
                        // 调整选中索引
                        let len = self.ctf_challenges.lock().map(|l| l.len()).unwrap_or(0);
                        if self.ctf_selected >= len && len > 0 {
                            self.ctf_selected = len - 1;
                        }
                        self.toast = Some(format!("已删除「{name}」"));
                        self.save_history();
                    }
                }
                Some(true)
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                // 切换选中题目状态（进行中 ↔ 已完成）
                let toggled = self
                    .ctf_challenges
                    .lock()
                    .ok()
                    .and_then(|mut list| {
                        let c = list.get_mut(self.ctf_selected)?;
                        let was_solved = c.is_solved();
                        c.status = if was_solved {
                            CtfStatus::InProgress
                        } else {
                            CtfStatus::Solved
                        };
                        if !was_solved {
                            c.end_time = Some(current_time_str());
                        } else {
                            c.end_time = None;
                        }
                        Some((c.name.clone(), c.is_solved()))
                    });
                if let Some((name, solved)) = toggled {
                    self.toast = Some(format!(
                        "「{name}」→ {}",
                        if solved { "已完成" } else { "进行中" }
                    ));
                    self.save_history();
                }
                Some(true)
            }
            KeyCode::Char('g') => {
                // 切换选中题目的全局/Session 范围
                let toggled = self
                    .ctf_challenges
                    .lock()
                    .ok()
                    .and_then(|mut list| {
                        let c = list.get_mut(self.ctf_selected)?;
                        c.is_global = !c.is_global;
                        Some((c.name.clone(), c.is_global))
                    });
                if let Some((name, is_global)) = toggled {
                    self.toast = Some(format!(
                        "「{name}」→ {}",
                        if is_global { "全局 ★" } else { "仅本 session" }
                    ));
                    self.save_history();
                }
                Some(true)
            }
            KeyCode::Char('G') => {
                // 将当前 session 的所有题目（非全局）改为全局
                let count = self
                    .ctf_challenges
                    .lock()
                    .ok()
                    .map(|mut list| {
                        let n = list.iter().filter(|c| !c.is_global).count();
                        for c in list.iter_mut() {
                            c.is_global = true;
                        }
                        n
                    })
                    .unwrap_or(0);
                if count > 0 {
                    self.toast = Some(format!("已将 {count} 道题目设为全局"));
                    self.save_history();
                } else {
                    self.toast = Some("当前 session 无需转换的题目".into());
                }
                Some(true)
            }
            KeyCode::Char('e') | KeyCode::Char('E') => {
                // 列表视图：打开编辑表单
                if !self.ctf_detail_view {
                    let challenge = self
                        .ctf_challenges
                        .lock()
                        .ok()
                        .and_then(|list| list.get(self.ctf_selected).cloned());
                    if let Some(c) = challenge {
                        self.form_prev_mode = self.mode;
                        self.ctf_edit_form = Some(CtfEditFormState::from_challenge(&c));
                        self.mode = Mode::CtfEditForm;
                    }
                }
                Some(true)
            }
            KeyCode::Char('w') | KeyCode::Char('W') => {
                // 在详情视图中按 w 触发 writeup 生成
                if self.ctf_detail_view {
                    // 先取出题目名（释放锁后再调用可变方法，避免借用冲突）
                    let name_to_write = self
                        .ctf_challenges
                        .lock()
                        .ok()
                        .and_then(|list| {
                            list.get(self.ctf_selected)
                                .filter(|c| c.is_solved() && !c.has_writeup())
                                .map(|c| c.name.clone())
                        });
                    if let Some(name) = name_to_write {
                        self.handle_ctf_slash(&format!("writeup {name}"));
                        return Some(true);
                    }
                }
                Some(false)
            }
            _ => None, // 未识别键 → 不消费，交 chat 处理
        }
    }

    /// LogViewer 按键：Esc/Ctrl+L 关闭，Up/Down/PageUp/PageDown 翻滚。
    fn handle_log_viewer_key(&mut self, k: KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        // Ctrl+L / Esc 关闭
        if k.code == KeyCode::Char('l') && k.modifiers.contains(KeyModifiers::CONTROL)
            || k.code == KeyCode::Esc
        {
            self.mode = self.form_prev_mode;
            return;
        }
        let total = self.log_viewer.lines.len();
        match k.code {
            KeyCode::Down => {
                if self.log_viewer.scroll > 0 {
                    self.log_viewer.scroll -= 1;
                }
            }
            KeyCode::Up => {
                self.log_viewer.scroll = (self.log_viewer.scroll + 1).min(total.saturating_sub(1));
            }
            KeyCode::PageDown => {
                self.log_viewer.scroll = self.log_viewer.scroll.saturating_sub(20);
            }
            KeyCode::PageUp => {
                self.log_viewer.scroll = (self.log_viewer.scroll + 20).min(total.saturating_sub(1));
            }
            // Ctrl+R 刷新（重新读文件）
            KeyCode::Char('r') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.log_viewer.lines = read_log_tail(&self.paths.log_file, 1000);
                self.toast = Some("日志已刷新".into());
            }
            _ => {}
        }
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
                    // 无参数：打开 ModelPicker 面板选择 provider + model
                    self.open_model_picker();
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
                let mut lines = String::from("可用工具：");
                for s in self.registries.tools.schemas() {
                    lines.push_str(&format!("\n  {} — {}", s.name, s.description));
                }
                self.chat.entries.push(ChatEntry::System(lines));
            }
            SlashCommand::Skill(args) => {
                self.handle_skill_slash(&args);
            }
            SlashCommand::Mcp(args) => {
                self.handle_mcp_slash(&args);
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
            SlashCommand::Compact(instructions) => {
                if self.chat.streaming || self.compacting {
                    self.chat.entries.push(ChatEntry::System(
                        "生成中，无法压缩上下文（先 /cancel）".into(),
                    ));
                } else {
                    let history = self.chat.history();
                    if history.is_empty() {
                        self.chat
                            .entries
                            .push(ChatEntry::System("无消息可压缩".into()));
                    } else {
                        let custom = if instructions.trim().is_empty() {
                            None
                        } else {
                            Some(instructions.clone())
                        };
                        self.spawn_compact(history, custom);
                    }
                }
            }
            SlashCommand::MaxSteps(arg) => {
                if arg.is_empty() {
                    // 无参数：显示当前值
                    self.chat.entries.push(ChatEntry::System(format!(
                        "当前 max_steps = {}（用法：/max_steps <1-1000>）",
                        self.config.agent.max_steps
                    )));
                } else {
                    // 有参数：解析并更新
                    match arg.trim().parse::<u32>() {
                        Ok(n) if (1..=1000).contains(&n) => {
                            self.config.agent.max_steps = n;
                            self.chat.entries.push(ChatEntry::System(format!(
                                "max_steps 已设为 {n}"
                            )));
                        }
                        Ok(n) => {
                            self.chat.entries.push(ChatEntry::System(format!(
                                "max_steps 须在 1-1000 范围内（收到 {n}），当前 = {}",
                                self.config.agent.max_steps
                            )));
                        }
                        Err(_) => {
                            self.chat.entries.push(ChatEntry::System(format!(
                                "无效参数：{arg}（须为 1-1000 的整数，当前 = {}）",
                                self.config.agent.max_steps
                            )));
                        }
                    }
                }
            }
            SlashCommand::Think(arg) => {
                if arg.is_empty() {
                    self.chat.entries.push(ChatEntry::System(format!(
                        "当前思考强度 = {}（用法：/think <low|middle|high|max|auto>）",
                        self.config.agent.thinking_intensity.label()
                    )));
                } else {
                    match cyber_core::ThinkingIntensity::from_str(arg.trim()) {
                        Some(level) => {
                            self.config.agent.thinking_intensity = level;
                            self.chat.entries.push(ChatEntry::System(format!(
                                "思考强度已设为 {}（{}）",
                                level.as_str(),
                                level.label()
                            )));
                        }
                        None => {
                            self.chat.entries.push(ChatEntry::System(format!(
                                "无效参数：{arg}（可选值：low / middle / high / max / auto，当前 = {}）",
                                self.config.agent.thinking_intensity.label()
                            )));
                        }
                    }
                }
            }
            SlashCommand::Quit => {
                self.save_history();
                self.should_quit = true;
            }
            SlashCommand::New => {
                if self.chat.streaming {
                    self.chat.entries.push(ChatEntry::System(
                        "生成中，无法新建会话（先 /cancel）".into(),
                    ));
                } else {
                    self.create_session();
                }
            }
            SlashCommand::Sessions(args) => {
                self.handle_sessions_slash(&args);
            }
            SlashCommand::Memory(args) => {
                self.handle_memory_slash(&args);
            }
            SlashCommand::Ctf(args) => {
                self.handle_ctf_slash(&args);
            }
            SlashCommand::Unknown(name) => {
                self.chat.entries.push(ChatEntry::System(format!(
                    "未知命令：{name}（输入 /help 查看可用命令）"
                )));
            }
        }
    }

    /// 处理 `/memory [list|add <text>|project <text>]`：查看/追加用户记忆。
    fn handle_memory_slash(&mut self, args: &str) {
        let mut parts = args.splitn(2, char::is_whitespace);
        let sub = parts.next().unwrap_or("").trim().to_lowercase();
        let rest = parts.next().unwrap_or("").trim();

        let store = MemoryStore::new(
            self.paths.memory_file.clone(),
            self.paths.cwd.join(".cyber").join("memory.md"),
        );

        match sub.as_str() {
            "" | "list" => {
                let global = store.load_global();
                let project = store.load_project();
                if global.is_empty() && project.is_empty() {
                    self.chat.entries.push(ChatEntry::System(
                        "暂无用户记忆。用 /memory add <text> 添加全局记忆，或 /memory project <text> 添加项目级记忆。".into(),
                    ));
                    return;
                }
                let mut lines = String::new();
                if !global.is_empty() {
                    lines.push_str("全局记忆：\n");
                    lines.push_str(global.trim_end());
                }
                if !project.is_empty() {
                    if !global.is_empty() {
                        lines.push('\n');
                    }
                    lines.push_str("\n项目级记忆：\n");
                    lines.push_str(project.trim_end());
                }
                self.chat.entries.push(ChatEntry::System(lines));
            }
            "add" => {
                if rest.is_empty() {
                    self.chat
                        .entries
                        .push(ChatEntry::System("用法：/memory add <要记住的内容>".into()));
                    return;
                }
                match store.append(MemoryScope::Global, rest) {
                    Ok(()) => self.chat.entries.push(ChatEntry::System(format!(
                        "已保存到全局记忆：{rest}"
                    ))),
                    Err(e) => self
                        .chat
                        .entries
                        .push(ChatEntry::System(format!("保存记忆失败：{e}"))),
                }
            }
            "project" => {
                if rest.is_empty() {
                    self.chat.entries.push(ChatEntry::System(
                        "用法：/memory project <要记住的内容>".into(),
                    ));
                    return;
                }
                match store.append(MemoryScope::Project, rest) {
                    Ok(()) => self.chat.entries.push(ChatEntry::System(format!(
                        "已保存到项目级记忆：{rest}"
                    ))),
                    Err(e) => self
                        .chat
                        .entries
                        .push(ChatEntry::System(format!("保存记忆失败：{e}"))),
                }
            }
            other => {
                self.chat.entries.push(ChatEntry::System(format!(
                    "未知子命令：{other}（可选：list / add <text> / project <text>）"
                )));
            }
        }
    }

    /// 处理 `/ctf <subcommand>`：enable / disable / add / list / writeup / status。
    fn handle_ctf_slash(&mut self, args: &str) {
        let mut parts = args.splitn(2, char::is_whitespace);
        let sub = parts.next().unwrap_or("").trim();
        let rest = parts.next().unwrap_or("").trim();
        match sub.to_lowercase().as_str() {
            "enable" => {
                self.ctf_enabled = true;
                self.chat.entries.push(ChatEntry::System(
                    "CTF 模式已开启（Ctrl+T 切换题目面板，/ctf add 添加题目）".into(),
                ));
            }
            "disable" => {
                    self.ctf_enabled = false;
                    self.ctf_panel_visible = false;
                    self.ctf_panel_focused = false;
                    self.ctf_panel_fullscreen = false;
                    self.chat.entries.push(ChatEntry::System("CTF 模式已关闭".into()));
                }
            "add" => {
                // /ctf add <name> <category>
                let mut p = rest.splitn(2, char::is_whitespace);
                let name = p.next().unwrap_or("").trim();
                let cat_str = p.next().unwrap_or("").trim();
                if name.is_empty() {
                    self.chat.entries.push(ChatEntry::System(
                        "用法：/ctf add <题目名称> <分类(misc/web/reverse/pwn/crypto)>".into(),
                    ));
                    return;
                }
                let category = CtfCategory::from_str(cat_str).unwrap_or_default();
                let challenge = CtfChallenge::new(name.into(), category);
                if let Ok(mut list) = self.ctf_challenges.lock() {
                    list.push(challenge);
                }
                self.save_history();
                self.chat.entries.push(ChatEntry::System(format!(
                    "已添加题目 [{}] {}",
                    category, name
                )));
            }
            "list" => {
                if let Ok(list) = self.ctf_challenges.lock() {
                    if list.is_empty() {
                        self.chat.entries.push(ChatEntry::System("当前无 CTF 题目".into()));
                    } else {
                        let mut lines = String::from("CTF 题目列表：");
                        for (i, c) in list.iter().enumerate() {
                            lines.push_str(&format!(
                                "\n  {}. [{}] {} — {}",
                                i + 1,
                                c.category,
                                c.name,
                                c.status.label()
                            ));
                        }
                        self.chat.entries.push(ChatEntry::System(lines));
                    }
                }
            }
            "writeup" => {
                if rest.is_empty() {
                    self.chat.entries.push(ChatEntry::System(
                        "用法：/ctf writeup <题目名称>".into(),
                    ));
                    return;
                }
                if self.chat.streaming || self.compacting || self.ctf_writeup_pending.is_some() {
                    self.chat.entries.push(ChatEntry::System(
                        "当前有任务进行中，无法生成 writeup（先 /cancel）".into(),
                    ));
                    return;
                }
                // 查找题目并检查状态
                let challenge = if let Ok(list) = self.ctf_challenges.lock() {
                    list.iter().find(|c| c.name == rest).cloned()
                } else {
                    None
                };
                match challenge {
                    Some(c) if c.is_solved() => {
                        self.chat.entries.push(ChatEntry::System(format!(
                            "正在为题目 {} 生成 writeup…",
                            c.name
                        )));
                        self.spawn_writeup(&c);
                    }
                    Some(_) => {
                        self.chat.entries.push(ChatEntry::System(format!(
                            "题目 {} 尚未解出，无法生成 writeup",
                            rest
                        )));
                    }
                    None => {
                        self.chat.entries.push(ChatEntry::System(format!(
                            "题目 {} 不存在",
                            rest
                        )));
                    }
                }
            }
            "" | "status" => {
                let status = if self.ctf_enabled { "已开启" } else { "已关闭" };
                self.chat.entries.push(ChatEntry::System(format!(
                    "CTF 模式：{}（/ctf enable|disable 切换）",
                    status
                )));
            }
            other => {
                self.chat.entries.push(ChatEntry::System(format!(
                    "未知 CTF 子命令：{other}（可用：enable/disable/add/list/writeup）"
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
                self.form_prev_mode = Mode::Chat;
                self.provider_form = Some(ProviderFormState::empty());
                self.mode = Mode::ProviderForm;
            }
            "edit" => {
                if rest.is_empty() {
                    self.chat.entries.push(ChatEntry::System(
                        "用法：/provider edit <name>".into(),
                    ));
                } else if let Some(cfg) = self.providers.providers.get(rest).cloned() {
                    self.form_prev_mode = Mode::Chat;
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

    /// 处理 `/skill <name|list>`：list 列出全部 skill；非空 name 注入 body 为 System 条目。
    /// 对应 `skill_<name>` 工具的命令行入口（用户也可让 agent 自动调工具）。
    fn handle_skill_slash(&mut self, args: &str) {
        let name = args.trim();
        if name.is_empty() || name.eq_ignore_ascii_case("list") {
            if self.registries.skills.is_empty() {
                self.chat
                    .entries
                    .push(ChatEntry::System("（无可用 Skill）".into()));
            } else {
                let mut lines = String::from("可用 Skill：");
                for s in self.registries.skills.iter() {
                    let src = match s.source {
                        cyber_skills::SkillSource::Global => "全局",
                        cyber_skills::SkillSource::Project => "项目",
                    };
                    lines.push_str(&format!(
                        "\n  {} [{}] — {}",
                        s.name(),
                        src,
                        s.frontmatter.description
                    ));
                }
                self.chat.entries.push(ChatEntry::System(lines));
            }
            return;
        }
        match self.registries.skills.find(name) {
            Some(skill) => {
                let body = skill.body.clone();
                self.chat.entries.push(ChatEntry::System(format!(
                    "Skill「{name}」使用说明：\n{body}"
                )));
            }
            None => {
                self.chat.entries.push(ChatEntry::System(format!(
                    "未知 Skill：{name}（/skill list 查看全部）"
                )));
            }
        }
    }

    /// 处理 `/mcp <list|status>`：列出已连接的 MCP server。
    /// 空串 / list / status 均展示连接状态（v0.1 不做 reconnect）。
    fn handle_mcp_slash(&mut self, _args: &str) {
        match &self.registries.mcp {
            None => {
                self.chat
                    .entries
                    .push(ChatEntry::System("MCP 未启用（mock 模式或无 server 配置）".into()));
            }
            Some(mcp) if mcp.is_empty() => {
                self.chat
                    .entries
                    .push(ChatEntry::System("（无已连接的 MCP server）".into()));
            }
            Some(mcp) => {
                let names = mcp.server_names();
                let mut lines = String::from("已连接 MCP server：");
                for n in &names {
                    lines.push_str(&format!("\n  {n}"));
                }
                self.chat.entries.push(ChatEntry::System(lines));
            }
        }
    }

    /// 打开 `/model` 面板：重置状态 → 选中当前 default_provider → 自动拉取其模型列表。
    fn open_model_picker(&mut self) {
        self.form_prev_mode = self.mode;
        self.model_picker = ModelPickerState::default();
        // 选中当前 default_provider
        let names = self.providers.sorted_names();
        if let Some(idx) = names
            .iter()
            .position(|n| n == &self.config.agent.default_provider)
        {
            self.model_picker.provider_selected = idx;
        }
        self.mode = Mode::ModelPicker;
        // 自动拉取当前 provider 的模型列表
        if !names.is_empty() {
            self.start_model_fetch();
        }
    }

    /// 从 Settings Providers 段打开新增表单。
    fn open_provider_form_add(&mut self) {
        self.form_prev_mode = self.mode;
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
        self.form_prev_mode = self.mode;
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

    /// 删除当前选中的 MCP server（双击 d 确认）。
    fn delete_selected_mcp(&mut self) {
        let Some(idx) = self.settings.mcp_pending_delete_idx else {
            return;
        };
        let Some(spec) = self.mcp_config.servers.get(idx) else {
            self.settings.mcp_pending_delete_idx = None;
            return;
        };
        let name = spec.name.clone();
        self.mcp_config.remove(&name);
        self.settings.dirty_mcp = true;
        self.settings.mcp_pending_delete_idx = None;
        self.toast = Some(format!("已删除 MCP server：{name}（保存后重启生效）"));
        // clamp cursor
        let len = self.mcp_config.servers.len();
        if self.settings.mcp_selected >= len && len > 0 {
            self.settings.mcp_selected = len - 1;
        }
    }

    /// 从 Settings MCP 段打开新增表单。
    fn open_mcp_form_add(&mut self) {
        self.form_prev_mode = self.mode;
        self.mcp_form = Some(McpFormState::empty());
        self.mode = Mode::McpForm;
    }

    /// 从 Settings MCP 段打开编辑表单（按 mcp_selected 选中项）。
    fn open_mcp_form_edit(&mut self) {
        let Some(spec) = self.mcp_config.servers.get(self.settings.mcp_selected).cloned() else {
            return;
        };
        self.form_prev_mode = self.mode;
        self.mcp_form = Some(McpFormState::from_spec(&spec));
        self.mode = Mode::McpForm;
    }

    // ---- Env var 表单 ----

    /// EnvForm 模式按键分发。
    fn handle_env_form_key(&mut self, k: KeyEvent) {
        let Some(form) = self.env_form.as_mut() else {
            return;
        };
        let action = form.handle_key(k);
        match action {
            EnvFormAction::None => {}
            EnvFormAction::Cancel => {
                self.env_form = None;
                self.mode = self.form_prev_mode;
            }
            EnvFormAction::Save => self.save_env_form(),
        }
    }

    /// 保存 Env 表单：校验 → upsert → 标脏 → 返回 prev_mode。
    fn save_env_form(&mut self) {
        let form = match self.env_form.as_ref() {
            Some(f) => f,
            None => return,
        };
        if !form.is_valid() {
            self.toast = Some("key 不能为空".into());
            return;
        }
        let var = form.build_var();
        let key = var.key.clone();
        match form.edit_index {
            Some(idx) => {
                // 编辑已有：替换
                if let Some(slot) = self.config.env.vars.get_mut(idx) {
                    *slot = var;
                }
            }
            None => {
                // 新增：同 key 覆盖，否则追加
                if let Some(existing) = self.config.env.vars.iter_mut().find(|v| v.key == key) {
                    *existing = var;
                } else {
                    self.config.env.vars.push(var);
                }
            }
        }
        self.settings.dirty_env = true;
        self.toast = Some(format!("环境变量 '{key}' 已暂存（保存设置后生效）"));
        self.env_form = None;
        self.mode = self.form_prev_mode;
    }

    /// 从 Settings Env 段打开新增表单。
    fn open_env_form_add(&mut self) {
        self.form_prev_mode = self.mode;
        self.env_form = Some(EnvFormState::for_add());
        self.mode = Mode::EnvForm;
    }

    /// 从 Settings Env 段打开编辑表单。
    fn open_env_form_edit(&mut self) {
        let Some(var) = self.config.env.vars.get(self.settings.env_selected).cloned() else {
            return;
        };
        self.form_prev_mode = self.mode;
        self.env_form = Some(EnvFormState::for_edit(self.settings.env_selected, &var));
        self.mode = Mode::EnvForm;
    }

    /// 删除当前选中的 Env var（双击 d 确认）。
    fn delete_selected_env(&mut self) {
        let Some(idx) = self.settings.env_pending_delete_idx else {
            return;
        };
        if idx >= self.config.env.vars.len() {
            self.settings.env_pending_delete_idx = None;
            return;
        }
        let key = self.config.env.vars[idx].key.clone();
        self.config.env.vars.remove(idx);
        self.settings.dirty_env = true;
        self.settings.env_pending_delete_idx = None;
        self.toast = Some(format!("已删除环境变量：{key}（保存后生效）"));
        let len = self.config.env.vars.len();
        if self.settings.env_selected >= len && len > 0 {
            self.settings.env_selected = len - 1;
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
        // McpForm 模式：同 ProviderForm
        if self.mode == Mode::McpForm {
            if let Some(form) = self.mcp_form.as_mut() {
                form.prepare_render(&self.theme);
            }
            return;
        }
        // CtfEditForm 模式：同 ProviderForm
        if self.mode == Mode::CtfEditForm {
            if let Some(form) = self.ctf_edit_form.as_mut() {
                form.prepare_render(&self.theme);
            }
            return;
        }
        // LogViewer / Sessions / ModelPicker 模式：无 textarea，跳过
        if self.mode == Mode::LogViewer || self.mode == Mode::Sessions || self.mode == Mode::ModelPicker {
            return;
        }
        // 历史区可用宽度 = 终端宽 - 2(边框) - 2(左右 padding)，用于工具结果折叠阈值的
        // 可视行数计算。crossterm::terminal::size 失败时回退 80（不会 panic）。
        let chat_width = crossterm::terminal::size()
            .map(|(w, _)| w.saturating_sub(4))
            .unwrap_or(80);
        self.chat.prepare_render(&self.theme, chat_width);
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
        // Settings 下：除 Esc/Quit/Other/Enter 外的任何动作取消"待丢弃"状态
        //（Enter 在 pending_discard 态用于"保存并退出"，不应取消）
        if self.mode == Mode::Settings && !matches!(a, Action::Esc | Action::Quit | Action::Other | Action::Enter) {
            self.settings.pending_discard = false;
        }
        // Providers 段：非 DeleteProvider 的动作清除待删除确认（"任一其他键取消"）
        if self.mode == Mode::Settings
            && self.settings.on_providers_section()
            && !matches!(a, Action::DeleteProvider)
        {
            self.settings.pending_delete_idx = None;
        }
        // MCP 段：同理清除待删除确认
        if self.mode == Mode::Settings
            && self.settings.on_mcp_section()
            && !matches!(a, Action::DeleteProvider)
        {
            self.settings.mcp_pending_delete_idx = None;
        }
        // Env 段：同理清除待删除确认
        if self.mode == Mode::Settings
            && self.settings.on_env_section()
            && !matches!(a, Action::DeleteProvider)
        {
            self.settings.env_pending_delete_idx = None;
        }
        match a {
            Action::Quit => self.should_quit = true,
            Action::OpenSettings => {
                // 已在 Settings 时 no-op；否则记 prev_mode + 快照后进入
                if self.mode != Mode::Settings {
                    self.prev_mode = self.mode;
                    self.config_at_entry = self.config.clone();
                    self.providers_at_entry = self.providers.clone();
                    self.mcp_config_at_entry = self.mcp_config.clone();
                    self.settings.pending_discard = false;
                    self.mode = Mode::Settings;
                }
            }
            Action::ToggleLogs => self.toggle_log_viewer(),
            Action::Tab => {
                if self.mode == Mode::Settings {
                    self.settings.next_section();
                } else {
                    // Welcome 下 Tab 无效；其余三模式循环
                    self.mode = match self.mode {
                        Mode::Chat => Mode::Workflow,
                        Mode::Workflow => Mode::Dashboard,
                        Mode::Dashboard => Mode::Chat,
                        Mode::Welcome | Mode::Settings | Mode::ProviderForm | Mode::McpForm | Mode::EnvForm | Mode::CtfEditForm | Mode::ModelPicker | Mode::Sessions | Mode::LogViewer | Mode::About => {
                            self.mode
                        }
                    };
                }
            }
            Action::Esc => {
                if self.mode == Mode::Settings {
                    self.exit_settings();
                } else if self.mode == Mode::About {
                    // 关于页：返回进入前的模式
                    self.mode = self.prev_mode;
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
                    } else if self.settings.on_mcp_section() {
                        self.settings.prev_mcp(self.mcp_config.servers.len());
                    } else if self.settings.on_env_section() {
                        self.settings.prev_env(self.config.env.vars.len());
                    } else if self.settings.on_skills_section() {
                        self.settings.prev_skill(self.registries.skills.len());
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
                    } else if self.settings.on_mcp_section() {
                        self.settings.next_mcp(self.mcp_config.servers.len());
                    } else if self.settings.on_env_section() {
                        self.settings.next_env(self.config.env.vars.len());
                    } else if self.settings.on_skills_section() {
                        self.settings.next_skill(self.registries.skills.len());
                    } else {
                        self.settings.next_field();
                    }
                } else if self.mode == Mode::Welcome {
                    self.selected = (self.selected + 1) % WELCOME_OPTIONS;
                }
            }
            Action::Left => {
                if self.mode == Mode::Settings
                    && !self.settings.on_providers_section()
                    && !self.settings.on_mcp_section()
                    && !self.settings.on_env_section()
                    && !self.settings.on_skills_section()
                {
                    let live = self.settings.apply_edit(&mut self.config, &self.providers, false);
                    self.apply_live(live);
                }
            }
            Action::Right => {
                if self.mode == Mode::Settings
                    && !self.settings.on_providers_section()
                    && !self.settings.on_mcp_section()
                    && !self.settings.on_env_section()
                    && !self.settings.on_skills_section()
                {
                    let live = self.settings.apply_edit(&mut self.config, &self.providers, true);
                    self.apply_live(live);
                }
            }
            Action::Enter => {
                if self.mode == Mode::Settings {
                    if self.settings.pending_discard {
                        // Esc 确认态：Enter 保存并退出
                        self.save_settings();
                        let still_dirty = self.settings.dirty
                            || self.settings.dirty_providers
                            || self.settings.dirty_mcp
                            || self.settings.dirty_env;
                        if !still_dirty {
                            self.settings.pending_discard = false;
                            self.mode = self.prev_mode;
                        }
                    } else if self.settings.on_provider_save_row() {
                        // Providers 段保存按钮：保存设置
                        self.save_settings();
                    } else if self.settings.on_mcp_save_row() {
                        // MCP 段保存按钮：保存设置
                        self.save_settings();
                    } else if self.settings.on_env_save_row() {
                        // Env 段保存按钮：保存设置
                        self.save_settings();
                    } else if self.settings.on_providers_section() {
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
                            self.mcp_config_at_entry = self.mcp_config.clone();
                            self.settings.pending_discard = false;
                            self.mode = Mode::Settings;
                        }
                        4 => {
                            // 进入关于页
                            self.prev_mode = Mode::Welcome;
                            self.mode = Mode::About;
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
                if self.mode == Mode::Settings
                    && self.settings.on_providers_section()
                    && !self.settings.provider_on_save
                {
                    self.open_provider_form_add();
                } else if self.mode == Mode::Settings
                    && self.settings.on_mcp_section()
                    && !self.settings.mcp_on_save
                {
                    self.open_mcp_form_add();
                } else if self.mode == Mode::Settings
                    && self.settings.on_env_section()
                    && !self.settings.env_on_save
                {
                    self.open_env_form_add();
                }
            }
            Action::EditProvider => {
                if self.mode == Mode::Settings
                    && self.settings.on_providers_section()
                    && !self.settings.provider_on_save
                {
                    self.open_provider_form_edit();
                } else if self.mode == Mode::Settings
                    && self.settings.on_mcp_section()
                    && !self.settings.mcp_on_save
                {
                    self.open_mcp_form_edit();
                } else if self.mode == Mode::Settings
                    && self.settings.on_env_section()
                    && !self.settings.env_on_save
                {
                    self.open_env_form_edit();
                }
            }
            Action::DeleteProvider => {
                if self.mode == Mode::Settings
                    && self.settings.on_providers_section()
                    && !self.settings.provider_on_save
                {
                    let cur = self.settings.provider_selected;
                    if self.settings.pending_delete_idx == Some(cur) {
                        // 二次 d：执行删除
                        self.delete_selected_provider();
                    } else {
                        // 首次 d：标记待删除
                        self.settings.pending_delete_idx = Some(cur);
                    }
                } else if self.mode == Mode::Settings
                    && self.settings.on_mcp_section()
                    && !self.settings.mcp_on_save
                {
                    let cur = self.settings.mcp_selected;
                    if self.settings.mcp_pending_delete_idx == Some(cur) {
                        self.delete_selected_mcp();
                    } else {
                        self.settings.mcp_pending_delete_idx = Some(cur);
                    }
                } else if self.mode == Mode::Settings
                    && self.settings.on_env_section()
                    && !self.settings.env_on_save
                {
                    let cur = self.settings.env_selected;
                    if self.settings.env_pending_delete_idx == Some(cur) {
                        self.delete_selected_env();
                    } else {
                        self.settings.env_pending_delete_idx = Some(cur);
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
                // 鼠标捕获在 run 开始时已总是启用，此处不再动态切换。
                // config.ui.mouse 字段保留但不再控制鼠标捕获。
            }
        }
    }

    /// 保存配置到 `~/.cyber/config.toml` + providers 到 `~/.cyber/providers.toml`
    /// + MCP servers 到 `~/.cyber/mcp/servers.toml`。
    fn save_settings(&mut self) {
        let config_res = save_config(&self.config, &self.paths.config_file);
        let providers_res = if self.settings.dirty_providers {
            save_providers(&self.providers, &self.paths.providers_file)
        } else {
            Ok(())
        };
        let mcp_res = if self.settings.dirty_mcp {
            self.mcp_config.save(&self.paths.mcp_servers_file)
        } else {
            Ok(())
        };
        match (config_res, providers_res, mcp_res) {
            (Ok(()), Ok(()), Ok(())) => {
                self.settings.dirty = false;
                self.settings.dirty_providers = false;
                self.settings.dirty_mcp = false;
                self.settings.dirty_env = false;
                self.settings.pending_discard = false;
                self.config_at_entry = self.config.clone();
                self.providers_at_entry = self.providers.clone();
                self.mcp_config_at_entry = self.mcp_config.clone();
                self.toast = Some("配置已保存".into());
            }
            (Err(e), _, _) => {
                self.toast = Some(format!("配置保存失败: {e}"));
            }
            (_, Err(e), _) => {
                self.toast = Some(format!("providers 保存失败: {e}"));
            }
            (_, _, Err(e)) => {
                self.toast = Some(format!("MCP 配置保存失败: {e}"));
            }
        }
    }

    /// 退出设置：dirty（config / providers / mcp）时首次 Esc 提示（Enter 保存退出 / 再按 Esc 丢弃），
    /// 二次 Esc 回退到快照后返回 prev_mode。
    fn exit_settings(&mut self) {
        let dirty = self.settings.dirty
            || self.settings.dirty_providers
            || self.settings.dirty_mcp
            || self.settings.dirty_env;
        if dirty {
            if !self.settings.pending_discard {
                self.settings.pending_discard = true;
                self.toast = Some("Enter 保存退出 / 再按 Esc 丢弃退出".into());
                return;
            }
            // 二次 Esc：回退到进入时的快照（config + providers + mcp）
            self.config = self.config_at_entry.clone();
            self.providers = self.providers_at_entry.clone();
            self.mcp_config = self.mcp_config_at_entry.clone();
            self.apply_live(LiveApply::Theme);
            self.apply_live(LiveApply::Mouse);
            self.settings.dirty = false;
            self.settings.dirty_providers = false;
            self.settings.dirty_mcp = false;
            self.settings.dirty_env = false;
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
            Mode::Chat => {
                // 组合显示标签：provider 名 + model 显示名（alias 优先）
                let provider_label = self
                    .providers
                    .providers
                    .get(&self.config.agent.default_provider)
                    .map(|p| {
                        let display = p.model_display_name();
                        if display != p.model {
                            format!("{} · {}", self.config.agent.default_provider, display)
                        } else if !p.model.is_empty() {
                            format!("{} · {}", self.config.agent.default_provider, p.model)
                        } else {
                            self.config.agent.default_provider.clone()
                        }
                    })
                    .unwrap_or_else(|| self.config.agent.default_provider.clone());
                let effective_price = self
                    .providers
                    .providers
                    .get(&self.config.agent.default_provider)
                    .and_then(|p| p.effective_price());
                // 检查是否显示 CTF 面板
                if self.ctf_enabled && self.ctf_panel_visible {
                    let challenges = self
                        .ctf_challenges
                        .lock()
                        .map(|l| l.clone())
                        .unwrap_or_default();
                    if self.ctf_panel_fullscreen {
                        // 全屏模式：面板占满整个区域，不渲染 chat
                        ctf_panel::render(
                            frame,
                            area,
                            &self.theme,
                            &challenges,
                            self.ctf_selected,
                            self.ctf_detail_view,
                            self.ctf_detail_scroll,
                            self.ctf_panel_focused,
                            &self.ctf_list_scroll,
                        );
                    } else {
                        let chunks = Layout::horizontal([
                            Constraint::Min(0),
                            Constraint::Length(ctf_panel::CTF_PANEL_WIDTH),
                        ])
                        .split(area);
                        views::chat::render(
                            frame,
                            chunks[0],
                            &self.theme,
                            &self.chat,
                            self.project.as_ref(),
                            &provider_label,
                            &self.usage,
                            effective_price,
                            &self.context_usage,
                        );
                        ctf_panel::render(
                            frame,
                            chunks[1],
                            &self.theme,
                            &challenges,
                            self.ctf_selected,
                            self.ctf_detail_view,
                            self.ctf_detail_scroll,
                            self.ctf_panel_focused,
                            &self.ctf_list_scroll,
                        );
                    }
                } else {
                    views::chat::render(
                        frame,
                        area,
                        &self.theme,
                        &self.chat,
                        self.project.as_ref(),
                        &provider_label,
                        &self.usage,
                        effective_price,
                        &self.context_usage,
                    );
                }
            }
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
                &self.mcp_config,
                self.registries.mcp.as_deref(),
                &self.registries.skills,
                &self.settings,
                self.has_project_config,
                self.toast.as_deref(),
            ),
            Mode::ProviderForm => {
                if let Some(form) = &self.provider_form {
                    views::providers::render_form(frame, area, &self.theme, form);
                }
            }
            Mode::McpForm => {
                if let Some(form) = &self.mcp_form {
                    views::mcp_form::render_form(frame, area, &self.theme, form);
                }
            }
            Mode::EnvForm => {
                if let Some(form) = &self.env_form {
                    views::env_form::render_form(frame, area, &self.theme, form);
                }
            }
            Mode::CtfEditForm => {
                if let Some(form) = &self.ctf_edit_form {
                    views::ctf_edit_form::render_form(frame, area, &self.theme, form);
                }
            }
            Mode::ModelPicker => views::model_picker::render(
                frame,
                area,
                &self.theme,
                &self.model_picker,
                &self.providers,
                &self.config.agent.default_provider,
            ),
            Mode::Sessions => views::sessions::render(
                frame,
                area,
                &self.theme,
                &self.sessions_panel,
                &self.sessions,
            ),
            Mode::LogViewer => self.render_log_viewer(frame, area),
            Mode::About => views::about::render(frame, area, &self.theme),
        }
    }

    /// 渲染日志查看器面板（Ctrl+L 打开）。
    ///
    /// 全屏显示日志文件尾部，scroll=0 定位最末行。Up 向上翻、Down 向下翻。
    fn render_log_viewer(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.accent))
            .title(
                Line::from(format!(
                    " 日志 {} · {} 行 · Ctrl+R 刷新 · Esc/Ctrl+L 关闭 ",
                    self.paths.log_file.display(),
                    self.log_viewer.lines.len()
                ))
                .style(Style::default().fg(self.theme.title)),
            )
            .style(Style::default().bg(self.theme.bg).fg(self.theme.fg));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let total = self.log_viewer.lines.len();
        if total == 0 {
            frame.render_widget(
                Paragraph::new("（日志为空）").style(Style::default().fg(self.theme.muted)),
                inner,
            );
            return;
        }

        // 计算可见窗口：scroll=0 → 末尾，scroll增大 → 向上
        let visible_h = inner.height as usize;
        let end = total.saturating_sub(self.log_viewer.scroll);
        let start = end.saturating_sub(visible_h);
        // 解析 ANSI 颜色码 → ratatui Span（只解析可见行，避免全量解析开销）
        let visible: Vec<Line> = self.log_viewer.lines[start..end.min(total)]
            .iter()
            .map(|s| ansi_to_line(s, &self.theme))
            .collect();

        frame.render_widget(
            Paragraph::new(visible).style(Style::default().bg(self.theme.bg)),
            inner,
        );
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let hint = match self.mode {
            Mode::Welcome => " ↑/↓ 导航   Enter 确认   s 设置   q 退出",
            Mode::Settings => " ↑↓ 行  Tab 段  Enter 编辑/保存  ←→ 调整  Esc 返回  q 退出",
            Mode::Chat if self.chat.streaming => " ● 流式生成中… Esc 取消 · Ctrl+C 退出",
            Mode::Chat if self.mouse_capture => " ○ 就绪 · Enter 发送 · F9 切选择模式",
            Mode::Chat => " ○ 选择模式 · 可拖拽复制 · F9 切回滚轮",
            Mode::ProviderForm => " ↑↓ 选字段  Enter 编辑/确认  ←→ 切 kind  Esc 取消",
            Mode::McpForm => " ↑↓ 选字段  Enter 编辑/确认  ←→ 切 transport  Esc 取消",
            Mode::CtfEditForm => " ↑↓ 选字段  Enter 编辑/确认  ←→ 切枚举  Esc 取消",
            Mode::Sessions => " ↑↓ 选会话  Enter 切换  n 新建  d 删除  Esc 返回  q 退出",
            Mode::ModelPicker => " ↑↓ 导航  Tab 切换栏  Enter 选择/确认  Esc 返回  q 退出",
            Mode::LogViewer => " ↑↓ 翻滚  PageUp/PageDown 翻页  Ctrl+R 刷新  Esc/Ctrl+L 关闭",
            Mode::About => " Esc 返回   q 退出",
            _ => " Tab 切换模式   s 设置   Esc 返回 Welcome   q 退出",
        };

        // 右下角 Ctrl+L 热键提示（所有模式常驻）
        let right_hint = " Ctrl+L 日志 ";
        let right_len = right_hint.len() as u16;
        let chunks = Layout::horizontal([Constraint::Min(1), Constraint::Length(right_len)])
            .split(area);

        frame.render_widget(
            Paragraph::new(Line::from(hint))
                .style(Style::default().bg(self.theme.muted).fg(self.theme.bg)),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(right_hint))
                .style(Style::default().bg(self.theme.muted).fg(self.theme.bg)),
            chunks[1],
        );
    }
}

/// 格式化压缩前后的 token 计数（用于 Compacted 事件展示）。
/// < 1000 原样，≥ 1000 用 k 单位（如 12.3k）。
fn fmt_compact_tokens(n: usize) -> String {
    if n < 1000 {
        format!("{n}")
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// 剥离 writeup 文本中残留的工具调用标签。
///
/// DeepSeek 等模型在无工具场景下，仍可能自发输出 `<function_calls>...</function_calls>`
/// 这类 XML 标签而非纯文本正文。system 提示词虽已禁止，但此处兜底过滤，
/// 避免工具调用标签混入最终 writeup。
fn strip_tool_call_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tool_block = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("<function_calls>") || t.starts_with("<tool_calls>") {
            in_tool_block = true;
            continue;
        }
        if t.starts_with("</function_calls>") || t.starts_with("</tool_calls>") {
            in_tool_block = false;
            continue;
        }
        if in_tool_block {
            continue; // 块内 <invoke>/<parameter> 等标签跳过
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// 合并全局 + session 题目列表，按名称去重。
///
/// 如果同名题目同时存在于全局和 session，保留 session 版本（可能包含更新的数据）。
/// 同时也去除 session 列表内部的重名条目（保留最后一条）。
fn merge_challenges(global: Vec<CtfChallenge>, session: Vec<CtfChallenge>) -> Vec<CtfChallenge> {
    let mut result: Vec<CtfChallenge> = Vec::new();
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    // 先插入 session 题目（优先保留）
    for c in session {
        seen_names.insert(c.name.clone());
        result.push(c);
    }
    // 再插入全局题目中不与 session 重名的
    for c in global {
        if !seen_names.contains(&c.name) {
            seen_names.insert(c.name.clone());
            result.push(c);
        }
    }
    result
}

/// 读取日志文件尾部 `max_lines` 行。文件不存在或为空返回空 Vec。
fn read_log_tail(path: &std::path::Path, max_lines: usize) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return vec![format!("（无法读取日志文件: {}）", path.display())];
    };
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// 把带 ANSI SGR 转义码的字符串解析为 ratatui `Line`（多 `Span`，各带样式）。
///
/// 支持 tracing-subscriber 输出的常见码：dim(2)、bold(1)、italic(3)、
/// 前景色 30-37/90-97、reset(0)。不支持的码静默忽略。
fn ansi_to_line(s: &str, theme: &Theme) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default();
    let mut remaining = s;

    while let Some(esc_pos) = remaining.find('\x1b') {
        // 输出转义序列前的文本
        if esc_pos > 0 {
            spans.push(Span::styled(remaining[..esc_pos].to_string(), style));
        }
        remaining = &remaining[esc_pos + 1..]; // 跳过 ESC

        // 只处理 CSI 序列（\x1b[...m），其它 ESC 序列跳过
        if remaining.starts_with('[') {
            if let Some(m_pos) = remaining.find('m') {
                let codes = &remaining[1..m_pos];
                style = parse_sgr(codes, style, theme);
                remaining = &remaining[m_pos + 1..];
            } else {
                // 无 'm' 结束符，跳过剩余
                break;
            }
        }
        // 非 '[' 开头的 ESC 序列直接跳过（不影响 style）
    }

    if !remaining.is_empty() {
        spans.push(Span::styled(remaining.to_string(), style));
    }
    if spans.is_empty() {
        Line::from("")
    } else {
        Line::from(spans)
    }
}

/// 解析 SGR 码字符串（如 `"2"` 或 `"1;32"`）为 ratatui `Style`。
fn parse_sgr(codes: &str, base: Style, theme: &Theme) -> Style {
    let mut style = base;
    for code_str in codes.split(';') {
        let code: u16 = code_str.parse().unwrap_or(0);
        match code {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            30 => style = style.fg(Color::Black),
            31 => style = style.fg(theme.accent),
            32 => style = style.fg(Color::Green),
            33 => style = style.fg(Color::Yellow),
            34 => style = style.fg(Color::Blue),
            35 => style = style.fg(Color::Magenta),
            36 => style = style.fg(Color::Cyan),
            37 => style = style.fg(Color::Gray),
            90 => style = style.fg(Color::DarkGray),
            91 => style = style.fg(Color::LightRed),
            92 => style = style.fg(Color::LightGreen),
            93 => style = style.fg(Color::LightYellow),
            94 => style = style.fg(Color::LightBlue),
            95 => style = style.fg(Color::LightMagenta),
            96 => style = style.fg(Color::LightCyan),
            97 => style = style.fg(Color::White),
            _ => {}
        }
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyber_core::{Config, ProvidersConfig};

    #[test]
    fn strip_tool_call_tags_removes_function_calls_block() {
        let text = "# Writeup\n\n<function_calls>\n<invoke name=\"list_dir\">\n<parameter name=\"path\">.cyber/ctf/web</parameter>\n</invoke>\n</function_calls>\n\n## 解题过程\n";
        let out = strip_tool_call_tags(text);
        assert!(!out.contains("function_calls"), "应移除 function_calls 块: {out}");
        assert!(!out.contains("invoke"), "应移除 invoke 标签");
        assert!(!out.contains("list_dir"), "应移除工具名");
        assert!(out.contains("# Writeup"), "应保留正文标题");
        assert!(out.contains("## 解题过程"), "应保留工具块后的正文");
    }

    #[test]
    fn strip_tool_call_tags_keeps_plain_text() {
        let text = "# Writeup\n\n## 解题过程\n正常内容\n";
        let out = strip_tool_call_tags(text);
        assert_eq!(out, text, "无工具标签时应原样保留");
    }

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
        // 每个 app 独占 history_dir + cwd，避免并行测试共享 session 文件互相干扰。
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let history_dir =
            std::env::temp_dir().join(format!("cyber_app_hist_{}_{seed}", std::process::id()));
        let _ = std::fs::remove_dir_all(&history_dir);
        std::fs::create_dir_all(&history_dir).unwrap();
        let cwd = history_dir.join("proj");
        let mut app = App::new(
            Config::default(),
            ProvidersConfig::default(),
            McpServersConfig::default(),
            None,
            initial,
            false,
            AppPaths {
                config_file,
                providers_file: std::env::temp_dir()
                    .join(format!("cyber_test_providers_{seed}.toml")),
                mcp_servers_file: std::env::temp_dir()
                    .join(format!("cyber_test_mcp_{seed}.toml")),
                log_file: std::env::temp_dir()
                    .join(format!("cyber_test_log_{seed}.log")),
                history_dir: history_dir.clone(),
                cwd: cwd.clone(),
                ctf_dir: std::env::temp_dir().join("cyber_test_ctf_tmp"),
                ctf_writeup_dir: std::env::temp_dir().join("cyber_test_ctf_tmp").join("writeup"),
                memory_file: std::env::temp_dir().join(format!("cyber_test_memory_{seed}.md")),
            },
            false,
            false,
            tx,
            ftx,
            AppRegistries::with_builtins(),
        );
        // App::new 不加载历史（由 run() 负责）；测试需 sessions 已初始化才能 save_history。
        app.sessions = crate::history::load_index(&history_dir, &cwd);
        app
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
    fn esc_exits_after_provider_form_closed() {
        // 回归测试：从 Settings 打开 ProviderForm → 关闭表单 → Esc 应退出 Settings
        // 之前 bug：表单覆盖 prev_mode=Settings，导致 exit_settings 回到 Settings（卡住）
        let path = temp_config_path();
        let mut app = make_app(Mode::Chat, path.clone());
        app.handle_action(Action::OpenSettings);
        assert_eq!(app.prev_mode, Mode::Chat);
        // 打开 provider 表单（从 Settings）
        app.open_provider_form_add();
        assert_eq!(app.mode, Mode::ProviderForm);
        assert_eq!(app.form_prev_mode, Mode::Settings);
        // prev_mode 不应被覆盖
        assert_eq!(app.prev_mode, Mode::Chat, "打开表单不应覆盖 prev_mode");
        // 关闭表单（Cancel）
        app.provider_form = None;
        app.mode = app.form_prev_mode;
        assert_eq!(app.mode, Mode::Settings, "表单关闭后应回到 Settings");
        // Esc 应退出 Settings（无 dirty 时直接退出）
        app.handle_action(Action::Esc);
        assert_eq!(app.mode, Mode::Chat, "Esc 应退出到 Chat，不应卡在 Settings");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn esc_then_enter_saves_and_exits() {
        let path = temp_config_path();
        let mut app = make_app(Mode::Chat, path.clone());
        app.handle_action(Action::OpenSettings);
        app.handle_action(Action::Right); // theme → dracula, dirty
        assert!(app.settings.dirty);
        // 首次 Esc：进入确认态
        app.handle_action(Action::Esc);
        assert_eq!(app.mode, Mode::Settings, "首次 Esc 不应退出");
        assert!(app.settings.pending_discard);
        // Enter：保存并退出
        app.handle_action(Action::Enter);
        assert_eq!(app.mode, Mode::Chat, "Enter 应保存并退出");
        assert!(!app.settings.dirty, "保存后 dirty 应清");
        assert!(!app.settings.pending_discard);
        assert!(
            app.toast.as_deref().unwrap_or("").contains("已保存"),
            "应提示已保存，实际: {:?}",
            app.toast
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
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
            McpServersConfig::default(),
            None,
            Mode::Chat,
            false,
            AppPaths {
                config_file: temp_config_path(),
                providers_file: std::env::temp_dir().join("cyber_test_providers2.toml"),
                mcp_servers_file: std::env::temp_dir().join("cyber_test_mcp2.toml"),
                log_file: std::env::temp_dir().join("cyber_test_log2.log"),
                history_dir: std::env::temp_dir(),
                cwd: std::env::temp_dir(),
                ctf_dir: std::env::temp_dir().join("cyber_test_ctf2"),
                ctf_writeup_dir: std::env::temp_dir().join("cyber_test_ctf2").join("writeup"),
                memory_file: std::env::temp_dir().join("cyber_test_memory2.md"),
            },
            true,
            false,
            tx2,
            ftx2,
            AppRegistries::with_builtins(),
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
            McpServersConfig::default(),
            None,
            Mode::Chat,
            false,
            AppPaths {
                config_file: temp_config_path(),
                providers_file: std::env::temp_dir().join("cyber_test_providers3.toml"),
                mcp_servers_file: std::env::temp_dir().join("cyber_test_mcp3.toml"),
                log_file: std::env::temp_dir().join("cyber_test_log3.log"),
                history_dir: std::env::temp_dir(),
                cwd: std::env::temp_dir(),
                ctf_dir: std::env::temp_dir().join("cyber_test_ctf2"),
                ctf_writeup_dir: std::env::temp_dir().join("cyber_test_ctf2").join("writeup"),
                memory_file: std::env::temp_dir().join("cyber_test_memory3.md"),
            },
            false,
            true, // mock
            tx,
            ftx,
            AppRegistries::with_builtins(),
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
    fn chat_cancel_aborts_and_preserves_buffer() {
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
        // 取消应保留已生成的文本（flush 为 assistant 条目），而非丢弃——保留上下文
        assert_eq!(app.chat.entries.len(), 1, "取消应保留 buffer 为 assistant 条目");
        assert!(matches!(&app.chat.entries[0], crate::chat::ChatEntry::Assistant(c) if c == "部分"));
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
    fn save_history_persists_session_file() {
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
            McpServersConfig::default(),
            None,
            Mode::Chat,
            false,
            AppPaths {
                config_file: temp_config_path(),
                providers_file: std::env::temp_dir().join("cyber_test_providers4.toml"),
                mcp_servers_file: std::env::temp_dir().join("cyber_test_mcp4.toml"),
                log_file: std::env::temp_dir().join("cyber_test_log4.log"),
                history_dir: hist_dir.clone(),
                cwd: cwd.clone(),
                ctf_dir: std::env::temp_dir().join("cyber_test_ctf_tmp"),
                ctf_writeup_dir: std::env::temp_dir().join("cyber_test_ctf_tmp").join("writeup"),
                memory_file: std::env::temp_dir().join("cyber_test_memory4.md"),
            },
            false,
            false,
            tx,
            ftx,
            AppRegistries::with_builtins(),
        );
        // App::new 不加载历史；手动初始化 sessions 索引，使 save_history 写入合法 session。
        app.sessions = history::load_index(&hist_dir, &cwd);
        app.chat.entries.push(ChatEntry::User("你好".into()));
        app.chat.entries.push(ChatEntry::Assistant("收到".into()));
        app.save_history();

        let file = history::session_dir(&hist_dir, &cwd)
            .join(format!("{}.json", app.sessions.current));
        assert!(file.exists(), "save_history 应写入当前 session 文件");
        let loaded = history::load_entries(&hist_dir, &cwd, &app.sessions.current);
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
            McpServersConfig::default(),
            None,
            Mode::Chat,
            false,
            AppPaths {
                config_file: temp_config_path(),
                providers_file: std::env::temp_dir().join("cyber_test_providers5.toml"),
                mcp_servers_file: std::env::temp_dir().join("cyber_test_mcp5.toml"),
                log_file: std::env::temp_dir().join("cyber_test_log5.log"),
                history_dir: hist_dir.clone(),
                cwd: cwd.clone(),
                ctf_dir: std::env::temp_dir().join("cyber_test_ctf_tmp"),
                ctf_writeup_dir: std::env::temp_dir().join("cyber_test_ctf_tmp").join("writeup"),
                memory_file: std::env::temp_dir().join("cyber_test_memory5.md"),
            },
            false,
            false,
            tx,
            ftx,
            AppRegistries::with_builtins(),
        );
        // App::new 不加载历史；手动初始化 sessions 索引，使 save_history 写入合法 session。
        app.sessions = history::load_index(&hist_dir, &cwd);
        app.chat.streaming = true;
        app.generation = 0;
        app.chat.streaming_buffer.push_str("回复内容");
        app.handle_agent_event(0, AgentEvent::Done);
        assert!(!app.chat.streaming, "Done 应退出 streaming");
        let loaded = history::load_entries(&hist_dir, &cwd, &app.sessions.current);
        assert_eq!(loaded.len(), 1, "Done 应已持久化 assistant 条目");
        assert!(
            matches!(&loaded[0], ChatEntry::Assistant(c) if c == "回复内容"),
            "持久化的应为 finalize 后的 assistant 条目"
        );
        let _ = std::fs::remove_dir_all(&hist_dir);
    }

    // ── Session 管理（/new + /sessions 面板 + 跨读 + 删除） ─────────────────

    /// 构造一个 Press 态、无修饰的 KeyEvent（测试便利）。
    fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new_with_kind_and_state(
            code,
            crossterm::event::KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Press,
            crossterm::event::KeyEventState::NONE,
        )
    }

    #[test]
    fn new_command_creates_second_session_and_resets_chat() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        assert_eq!(app.sessions.sessions.len(), 1, "初始应仅 1 个默认 session");
        let old_current = app.sessions.current.clone();
        // 给当前 session 填点内容
        app.chat.entries.push(ChatEntry::User("在原会话".into()));
        // /new：记录命令 → 保存当前 → 新建并切到空会话
        app.handle_slash_command("/new");
        assert_eq!(app.sessions.sessions.len(), 2, "/new 后应有 2 个 session");
        assert_ne!(app.sessions.current, old_current, "current 应切到新 session");
        assert!(
            app.chat.entries.is_empty(),
            "新会话的 chat 应被重置为空"
        );
        assert!(
            app.toast.as_deref().unwrap_or("").contains("新会话"),
            "应 toast 新会话已创建"
        );
    }

    #[test]
    fn sessions_command_opens_panel() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_slash_command("/new"); // 凑出 2 个 session
        app.handle_slash_command("/sessions");
        assert_eq!(app.mode, Mode::Sessions, "/sessions 应进入 Sessions 面板");
        assert_eq!(app.form_prev_mode, Mode::Chat, "form_prev_mode 应记为 Chat");
        assert_eq!(
            app.sessions_panel.list.len(),
            app.sessions.sessions.len(),
            "面板 list 应为 sessions 快照"
        );
        // selected 应指向当前 session
        let cur_idx = app
            .sessions
            .sessions
            .iter()
            .position(|s| s.id == app.sessions.current)
            .unwrap();
        assert_eq!(app.sessions_panel.selected, cur_idx);
    }

    #[test]
    fn sessions_panel_enter_switches_session() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        let s1 = app.sessions.current.clone();
        // /new 后 current 变为 s2，s1 留有 "/new" User 条目
        app.handle_slash_command("/new");
        let _s2 = app.sessions.current.clone();
        app.chat.entries.push(ChatEntry::User("在 s2".into()));
        // 打开面板，selected 指向 s2（index 1），Up 选 s1
        app.handle_slash_command("/sessions");
        app.handle_sessions_key(key(crossterm::event::KeyCode::Up));
        assert_eq!(app.sessions_panel.selected, 0);
        // Enter 切换到 s1
        app.handle_sessions_key(key(crossterm::event::KeyCode::Enter));
        assert_eq!(app.mode, Mode::Chat, "Enter 后应返回 Chat");
        assert_eq!(app.sessions.current, s1, "应切回 s1");
        // s1 磁盘内容为 [User("/new")]（/new 时保存的）
        assert_eq!(app.chat.entries.len(), 1);
        assert!(matches!(&app.chat.entries[0], ChatEntry::User(c) if c == "/new"));
    }

    #[test]
    fn sessions_panel_delete_refuses_when_single() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        assert_eq!(app.sessions.sessions.len(), 1);
        app.handle_slash_command("/sessions");
        // 首次 d：标记
        app.handle_sessions_key(key(crossterm::event::KeyCode::Char('d')));
        assert_eq!(app.sessions_panel.pending_delete, Some(0));
        // 二次 d：拒绝删除（仅 1 个）
        app.handle_sessions_key(key(crossterm::event::KeyCode::Char('d')));
        assert_eq!(
            app.sessions.sessions.len(),
            1,
            "单 session 时不应删除"
        );
        assert!(
            app.toast.as_deref().unwrap_or("").contains("至少保留"),
            "应提示至少保留 1 个"
        );
    }

    #[test]
    fn sessions_panel_delete_with_two_sessions() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_slash_command("/new"); // 2 个 session，current=s2
        let s2 = app.sessions.current.clone();
        app.handle_slash_command("/sessions");
        // selected 指向 s2(idx1)，Up 选 s1(idx0)
        app.handle_sessions_key(key(crossterm::event::KeyCode::Up));
        assert_eq!(app.sessions_panel.selected, 0);
        // 双击 d 删除 s1
        app.handle_sessions_key(key(crossterm::event::KeyCode::Char('d')));
        app.handle_sessions_key(key(crossterm::event::KeyCode::Char('d')));
        assert_eq!(app.sessions.sessions.len(), 1, "应删到 1 个 session");
        assert_eq!(app.sessions.current, s2, "删非 current 不应改 current");
        assert_eq!(
            app.sessions_panel.list.len(),
            1,
            "面板 list 应刷新为 1 项"
        );
    }

    #[test]
    fn sessions_read_injects_cross_session_content() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        let s1 = app.sessions.current.clone();
        // 在 s1 写入对话并保存
        app.chat.entries.push(ChatEntry::User("hello".into()));
        app.chat.entries.push(ChatEntry::Assistant("hi there".into()));
        app.save_history();
        // 新建 s2（current 切走），chat 重置为空
        app.handle_slash_command("/new");
        assert!(app.chat.entries.is_empty());
        // /sessions read <s1> 跨读注入
        app.handle_slash_command(&format!("/sessions read {s1}"));
        // 末条应为 System，含 s1 的内容
        let last = app.chat.entries.last().expect("应有注入条目");
        assert!(
            matches!(last, ChatEntry::System(t) if t.contains("hello") && t.contains("hi there")),
            "跨读应注入 s1 内容为 System 条目，实际: {last:?}"
        );
        // current 仍是 s2（跨读不切换）
        assert_ne!(app.sessions.current, s1);
    }

    #[test]
    fn sessions_panel_esc_returns_without_switching() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        let cur = app.sessions.current.clone();
        app.handle_slash_command("/new");
        let s2 = app.sessions.current.clone();
        app.handle_slash_command("/sessions");
        app.handle_sessions_key(key(crossterm::event::KeyCode::Esc));
        assert_eq!(app.mode, Mode::Chat, "Esc 应返回 Chat");
        assert_eq!(app.sessions.current, s2, "Esc 不应切换 session");
        assert_ne!(app.sessions.current, cur);
    }

    #[test]
    fn new_blocked_during_streaming() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.chat.streaming = true;
        let before = app.sessions.sessions.len();
        app.handle_slash_command("/new");
        assert_eq!(
            app.sessions.sessions.len(),
            before,
            "流式期 /new 应被阻止"
        );
        assert!(app.chat.streaming, "流式态不应被改变");
    }

    #[test]
    fn sessions_panel_renders_without_panic() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_slash_command("/new"); // 2 个 session
        app.handle_slash_command("/sessions");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
    }

    // ── /max_steps 命令 ─────────────────────────────────────────────────────

    #[test]
    fn max_steps_no_arg_shows_current() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_slash_command("/max_steps");
        let last = app.chat.entries.last().unwrap();
        assert!(
            matches!(last, ChatEntry::System(t) if t.contains("max_steps = 50")),
            "无参数应显示当前值 50: {last:?}"
        );
    }

    #[test]
    fn max_steps_sets_valid_value() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_slash_command("/max_steps 100");
        assert_eq!(app.config.agent.max_steps, 100);
        let last = app.chat.entries.last().unwrap();
        assert!(
            matches!(last, ChatEntry::System(t) if t.contains("100")),
            "应确认设为 100: {last:?}"
        );
    }

    #[test]
    fn max_steps_rejects_out_of_range() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_slash_command("/max_steps 9999");
        assert_eq!(app.config.agent.max_steps, 500, "超范围不应更新");
        let last = app.chat.entries.last().unwrap();
        assert!(
            matches!(last, ChatEntry::System(t) if t.contains("1-1000")),
            "应提示范围: {last:?}"
        );
    }

    #[test]
    fn max_steps_rejects_non_number() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_slash_command("/max_steps abc");
        assert_eq!(app.config.agent.max_steps, 500, "非数字不应更新");
        let last = app.chat.entries.last().unwrap();
        assert!(
            matches!(last, ChatEntry::System(t) if t.contains("无效参数")),
            "应提示无效参数: {last:?}"
        );
    }

    #[test]
    fn max_steps_accepts_minimum_one() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_slash_command("/max_steps 1");
        assert_eq!(app.config.agent.max_steps, 1);
    }

    // ── /model 面板（ModelPicker） ──────────────────────────────────────────

    fn make_app_with_providers(initial: Mode, config_file: PathBuf) -> App {
        let mut app = make_app(initial, config_file);
        app.providers = ProvidersConfig::default_template();
        app.config.agent.default_provider = "openai".into();
        app
    }

    #[tokio::test]
    async fn model_no_arg_opens_picker_panel() {
        let mut app = make_app_with_providers(Mode::Chat, temp_config_path());
        app.handle_slash_command("/model");
        assert_eq!(app.mode, Mode::ModelPicker, "/model 无参数应打开 ModelPicker 面板");
        assert_eq!(app.form_prev_mode, Mode::Chat);
        // 选中项应指向当前 default_provider（openai）
        // sorted_names = ["anthropic", "ollama", "openai"] → openai 在 idx=2
        let names = app.providers.sorted_names();
        let expected_idx = names.iter().position(|n| n == "openai").unwrap();
        assert_eq!(app.model_picker.provider_selected, expected_idx);
        // 应自动发起拉取
        assert!(app.model_picker.fetching, "打开面板应自动拉取当前 provider 模型");
    }

    #[test]
    fn model_with_arg_switches_provider_directly() {
        let mut app = make_app_with_providers(Mode::Chat, temp_config_path());
        app.handle_slash_command("/model ollama");
        assert_eq!(app.mode, Mode::Chat, "/model <provider> 不应打开面板");
        assert_eq!(app.config.agent.default_provider, "ollama");
    }

    #[tokio::test]
    async fn model_picker_esc_returns_to_prev() {
        let mut app = make_app_with_providers(Mode::Chat, temp_config_path());
        app.handle_slash_command("/model");
        assert_eq!(app.mode, Mode::ModelPicker);
        app.handle_model_picker_key(key(crossterm::event::KeyCode::Esc));
        assert_eq!(app.mode, Mode::Chat, "Esc 应返回 prev_mode");
    }

    #[tokio::test]
    async fn model_picker_tab_toggles_focus() {
        let mut app = make_app_with_providers(Mode::Chat, temp_config_path());
        app.handle_slash_command("/model");
        assert!(!app.model_picker.focus_models, "初始焦点应在 provider 栏");
        app.handle_model_picker_key(key(crossterm::event::KeyCode::Tab));
        assert!(app.model_picker.focus_models, "Tab 应切到 model 栏");
        app.handle_model_picker_key(key(crossterm::event::KeyCode::Tab));
        assert!(!app.model_picker.focus_models, "再 Tab 切回 provider 栏");
    }

    #[tokio::test]
    async fn model_picker_confirm_saves_provider_and_model() {
        let mut app = make_app_with_providers(Mode::Chat, temp_config_path());
        // 打开面板
        app.handle_slash_command("/model");
        // 模拟拉取成功
        let fid = app.model_picker.fetch_id;
        app.model_picker.deliver_fetch(fid, Ok(vec!["gpt-4o".into(), "gpt-4o-mini".into()]));
        assert!(!app.model_picker.fetching);
        assert_eq!(app.model_picker.models.len(), 2);
        // 切到 model 栏选第二个模型
        app.handle_model_picker_key(key(crossterm::event::KeyCode::Tab));
        app.handle_model_picker_key(key(crossterm::event::KeyCode::Down));
        assert_eq!(app.model_picker.model_selected, 1);
        // Enter 确认
        app.handle_model_picker_key(key(crossterm::event::KeyCode::Enter));
        assert_eq!(app.mode, Mode::Chat, "确认后应返回 Chat");
        assert_eq!(app.config.agent.default_provider, "openai");
        assert_eq!(app.providers.providers["openai"].model, "gpt-4o-mini");
    }

    #[tokio::test]
    async fn model_picker_provider_nav_triggers_fetch() {
        let mut app = make_app_with_providers(Mode::Chat, temp_config_path());
        app.handle_slash_command("/model");
        let fid0 = app.model_picker.fetch_id;
        // openai 在 sorted_names idx=2，Down 后 wrap 到 idx=0（anthropic）
        app.handle_model_picker_key(key(crossterm::event::KeyCode::Down));
        let names = app.providers.sorted_names();
        assert_eq!(app.model_picker.provider_selected, 0, "Down 应切到 sorted_names[0]");
        assert_eq!(names[0], "anthropic");
        assert!(app.model_picker.fetching, "切换 provider 应触发拉取");
        assert_ne!(app.model_picker.fetch_id, fid0, "fetch_id 应 bump");
        // models 应被清空（旧结果清除）
        assert!(app.model_picker.models.is_empty());
    }

    #[tokio::test]
    async fn model_picker_enter_on_empty_models_toasts() {
        let mut app = make_app_with_providers(Mode::Chat, temp_config_path());
        app.handle_slash_command("/model");
        // 模拟拉取失败
        let fid = app.model_picker.fetch_id;
        app.model_picker.deliver_fetch(fid, Err("timeout".into()));
        // 切到 model 栏，Enter 应 toast 而非确认
        app.handle_model_picker_key(key(crossterm::event::KeyCode::Tab));
        app.handle_model_picker_key(key(crossterm::event::KeyCode::Enter));
        assert_eq!(app.mode, Mode::ModelPicker, "无模型时 Enter 不应退出");
        assert!(app.toast.as_deref().unwrap_or("").contains("无模型"));
    }

    #[test]
    fn model_picker_state_start_fetch_bumps_id() {
        let mut s = ModelPickerState::default();
        s.models = vec!["old".into()];
        let id1 = s.start_fetch();
        assert!(s.fetching);
        assert!(s.models.is_empty(), "start_fetch 应清空旧 models");
        let id2 = s.start_fetch();
        assert_ne!(id1, id2);
    }

    #[test]
    fn model_picker_state_deliver_stale_ignored() {
        let mut s = ModelPickerState::default();
        let id = s.start_fetch();
        s.deliver_fetch(id.wrapping_sub(1), Ok(vec!["m".into()]));
        assert!(s.fetching, "stale 结果应被忽略");
        assert!(s.models.is_empty());
    }

    #[test]
    fn model_picker_state_deliver_success_populates() {
        let mut s = ModelPickerState::default();
        let id = s.start_fetch();
        s.deliver_fetch(id, Ok(vec!["a".into(), "b".into()]));
        assert!(!s.fetching);
        assert_eq!(s.models.len(), 2);
        assert_eq!(s.model_selected, 0);
    }

    #[test]
    fn model_picker_state_deliver_error_stores() {
        let mut s = ModelPickerState::default();
        let id = s.start_fetch();
        s.deliver_fetch(id, Err("boom".into()));
        assert!(!s.fetching);
        assert_eq!(s.fetch_error.as_deref(), Some("boom"));
    }

    #[test]
    fn model_picker_state_deliver_empty_errors() {
        let mut s = ModelPickerState::default();
        let id = s.start_fetch();
        s.deliver_fetch(id, Ok(vec![]));
        assert!(!s.fetching);
        assert!(s.fetch_error.is_some(), "空模型列表应记为错误");
    }

    #[test]
    fn model_picker_blocked_during_streaming() {
        let mut app = make_app_with_providers(Mode::Chat, temp_config_path());
        app.chat.streaming = true;
        app.handle_slash_command("/model");
        assert_eq!(app.mode, Mode::Chat, "流式期 /model 应被阻止");
        assert!(app.chat.streaming);
    }

    #[tokio::test]
    async fn model_picker_render_does_not_panic() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = make_app_with_providers(Mode::Chat, temp_config_path());
        app.handle_slash_command("/model");
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
    }
}
