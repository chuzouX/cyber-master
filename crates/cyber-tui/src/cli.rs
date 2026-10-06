//! Full-screen coding CLI: project header, conversation, and a pinned composer.

use std::io::{self, IsTerminal};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Once,
};
use std::time::Duration;

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
        MouseEventKind,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
    },
};
use cyber_agent::{
    estimate_messages_tokens, AgentEvent, ApprovalChoice, BackgroundJob, BackgroundRegistry,
    JobKind, JobStatus, PermissionBroker, PermissionDecision, PermissionMode, PermissionRequest,
    SubagentArchive, SubagentRun, SubagentStatus,
};
use cyber_core::{
    Config, MemoryEntry, MemoryScope, MemoryStore, Paths, ProvidersConfig, ThinkingIntensity,
};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
    Frame, Terminal,
};
use tokio::sync::{mpsc, oneshot};
use tui_textarea::TextArea;

use crate::chat::{entries_to_messages, ChatEntry, KeyDisposition, PasteDetector};
use crate::cli_commands::{
    self, CliAction, CommandForm, CommandPicker, CompletionItem, FormKind, PickerKind,
};
use crate::headless::{HeadlessOutcome, SessionRunner, ViewCtx};
#[allow(unused_imports)]
use crate::selection::default_selection_style;
use crate::selection::{ContentCoord, TextSelection};
use crate::theme::Theme;
use crate::views::{clipped_spans, todo_visible_window};

const CY_LOGO: [&str; 5] = [
    "  ___       ",
    " / __| _  _ ",
    "| (__ | || |",
    " \\___| \\_, |",
    "       |__/ ",
];
const FG: Color = Color::Rgb(204, 204, 204);
const MUTED: Color = Color::Rgb(119, 125, 136);
const DIM: Color = Color::Rgb(95, 102, 115);
const ACCENT: Color = Color::Rgb(254, 188, 56);
const AMBER: Color = ACCENT;
const WARNING: Color = AMBER;
const USER_BG: Color = Color::Rgb(34, 29, 26);
const PENDING_BG: Color = Color::Rgb(29, 29, 33);
const SUCCESS_BG: Color = Color::Rgb(22, 26, 31);
const ERROR_BG: Color = Color::Rgb(41, 30, 29);
const SUCCESS: Color = Color::Rgb(137, 210, 129);
const ERROR: Color = Color::Rgb(252, 58, 75);
const CODE: Color = Color::Rgb(229, 193, 255);
const CODE_BG: Color = Color::Rgb(30, 30, 36);
const LINK: Color = Color::Rgb(0, 136, 250);
const CLI_THEME: Theme = Theme {
    bg: Color::Reset,
    fg: FG,
    accent: ACCENT,
    muted: MUTED,
    border: DIM,
    sel_bg: Color::Reset,
    sel_fg: ACCENT,
    title: CODE,
};

struct TerminalSession;

static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn restore_terminal() {
    if !TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        SetTitle("Cyber Master"),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        static HOOK: Once = Once::new();
        HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore_terminal();
                previous(info);
            }));
        });
        enable_raw_mode()?;
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen)?;
        // Legacy Windows consoles may not support bracketed paste. Key bursts
        // are also buffered below instead of treating pasted newlines as sends.
        let _ = execute!(io::stdout(), EnableBracketedPaste, EnableMouseCapture);
        Ok(guard)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        restore_terminal();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Panel {
    Shortcuts,
    Ctf,
    Settings,
    Subagents,
    Jobs,
    ModelPicker,
    Mcp,
    About,
}

impl Panel {
    pub fn is_fullscreen(self) -> bool {
        matches!(
            self,
            Self::Settings | Self::Mcp | Self::ModelPicker | Self::About
        )
    }
}

/// 设置面板标签页（共 9 个大类，全面覆盖所有设置）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    AgentModel, // 1. Agent 与核心模型
    UiWorkflow,    // 2. 界面与交互
    Subagents,     // 3. 子任务与并发
    ToolsMcp,      // 4. 工具扩展与 MCP
    Providers,     // 5. 服务商管理
    EnvMemory,     // 6. 环境变量与记忆
    StorageSystem, // 7. 系统与存储日志
    Toolbox,       // 8. 自定义工具库
    About,         // 9. 关于（版本 / 项目 / 运行环境 / 快捷键说明书）
}

impl SettingsTab {
    pub fn all() -> &'static [SettingsTab] {
        &[
            SettingsTab::AgentModel,
            SettingsTab::UiWorkflow,
            SettingsTab::Subagents,
            SettingsTab::ToolsMcp,
            SettingsTab::Providers,
            SettingsTab::EnvMemory,
            SettingsTab::StorageSystem,
            SettingsTab::Toolbox,
            SettingsTab::About,
        ]
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::AgentModel => "1. Agent & 模型",
            Self::UiWorkflow => "2. 界面与交互",
            Self::Subagents => "3. 子任务并发",
            Self::ToolsMcp => "4. 工具与 MCP",
            Self::Providers => "5. 服务商管理",
            Self::EnvMemory => "6. 环境与记忆",
            Self::StorageSystem => "7. 系统与存储",
            Self::Toolbox => "8. 工具库",
            Self::About => "9. 关于",
        }
    }

    pub fn short_title(self) -> &'static str {
        match self {
            Self::AgentModel => "1.Agent模型",
            Self::UiWorkflow => "2.界面交互",
            Self::Subagents => "3.子任务",
            Self::ToolsMcp => "4.工具MCP",
            Self::Providers => "5.服务商",
            Self::EnvMemory => "6.环境记忆",
            Self::StorageSystem => "7.系统存储",
            Self::Toolbox => "8.工具库",
            Self::About => "9.关于",
        }
    }

    pub fn compact_title(self) -> &'static str {
        match self {
            Self::AgentModel => "1.模型",
            Self::UiWorkflow => "2.界面",
            Self::Subagents => "3.并发",
            Self::ToolsMcp => "4.工具",
            Self::Providers => "5.服务商",
            Self::EnvMemory => "6.环境",
            Self::StorageSystem => "7.存储",
            Self::Toolbox => "8.工具",
            Self::About => "9.关于",
        }
    }

    pub fn max_row(self, state: &CliSettingsState) -> usize {
        match self {
            Self::AgentModel => 11, // 0..=11 (12 rows)
            Self::UiWorkflow => 6,  // 0..=6 (7 rows)
            Self::Subagents => 4,   // 0..=4 (5 rows)
            Self::ToolsMcp => 2 + state.skills.len(),
            Self::Providers => state.providers_draft.providers.len().saturating_sub(1),
            Self::EnvMemory => {
                let env_count = state.config_draft.env.vars.len();
                let mem_count = state.config_draft.memory.rules.len();
                let env_slots = env_count.max(1);
                let mem_slots = mem_count.max(1);
                env_slots + mem_slots - 1 + state.total_memories()
            }
            Self::StorageSystem => 1, // 0..=1 (2 rows: retention, log_level)
            // N 个工具行 + 1 个 AI 扫描行
            Self::Toolbox => state.custom_tools.len(),
            // 「关于」页只读，无可编辑行（滚动由 `about_scroll` 承担）。
            Self::About => 0,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct McpServerSummary {
    pub name: String,
    pub connected: bool,
    pub tool_count: usize,
    pub detail: String,
}

#[derive(Clone, Debug, Default)]
pub struct SkillSummary {
    pub name: String,
    pub source: String,
    pub description: String,
    pub triggers: Vec<String>,
    pub tools: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub disable_model_invocation: bool,
    pub path: String,
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct SkillDetailModal {
    /// 当前查看的技能在 `settings.skills` 中的下标索引
    pub skill_index: usize,
    /// 正文滚屏偏移行数
    pub scroll: usize,
}

/// Memory List 分组：全局在前、项目级在后。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryGroup {
    Global,
    Project,
}

/// Memory List 详情弹窗状态。
#[derive(Clone, Debug)]
pub struct MemoryDetailModal {
    /// 所属分组。
    pub group: MemoryGroup,
    /// 组内 0-based 索引。
    pub entry: usize,
    /// 正文滚屏偏移行数。
    pub scroll: usize,
}
/// CLI 设置面板运行时状态
#[derive(Clone, Debug)]
pub struct CliSettingsState {
    pub tab: SettingsTab,
    pub selected_row: usize,
    pub dirty: bool,
    /// 焦点行是否处于编辑态：Enter 进入；Enter/Esc 退出；只读态 ←/→ 改为切换标签页。
    pub editing: bool,
    /// 编辑中的配置草稿（保存时落盘并同步至 AppContext；退出时可丢弃）
    pub config_draft: Config,
    /// 编辑中的服务商配置草稿
    pub providers_draft: ProvidersConfig,
    /// 列表子项选中索引（用于 Providers/Env/Memory 列表的垂直滚动）
    pub list_selected: usize,
    /// 未保存退出时的确认拦截态
    pub pending_discard_confirm: bool,
    pub mcp_servers: Vec<McpServerSummary>,
    pub skills: Vec<SkillSummary>,
    /// 自定义安全工具库（`~/.cyber/tools/*.toml`，启动时快照）。
    pub custom_tools: Vec<cyber_core::CustomToolConfig>,
    /// 「工具库」页连按两次 D 删除确认的待删行下标。
    pub tools_pending_delete: Option<usize>,
    pub skill_detail: Option<SkillDetailModal>,
    /// 全局记忆条目（~/.cyber/memory.md）。
    pub memories_global: Vec<MemoryEntry>,
    /// 项目级记忆条目（<cwd>/.cyber/memory.md）。
    pub memories_project: Vec<MemoryEntry>,
    /// 全局记忆文件路径（展示用）。
    pub memory_global_path: String,
    /// 项目级记忆文件路径（展示用）。
    pub memory_project_path: String,
    /// Memory List 详情弹窗。
    pub memory_detail: Option<MemoryDetailModal>,
    pub config_path: String,
    pub providers_path: String,
    pub sessions_dir: String,
    pub sessions_count: usize,
    pub has_project_config: bool,
    pub provider_test_results: std::collections::HashMap<String, (bool, String, u128)>,
}

impl CliSettingsState {
    pub fn new(config: &Config, providers: &ProvidersConfig) -> Self {
        Self {
            tab: SettingsTab::AgentModel,
            selected_row: 0,
            dirty: false,
            editing: false,
            config_draft: config.clone(),
            providers_draft: providers.clone(),
            list_selected: 0,
            pending_discard_confirm: false,
            provider_test_results: std::collections::HashMap::new(),
            mcp_servers: Vec::new(),
            skills: Vec::new(),
            custom_tools: Vec::new(),
            tools_pending_delete: None,
            skill_detail: None,
            memories_global: Vec::new(),
            memories_project: Vec::new(),
            memory_global_path: "~/.cyber/memory.md".into(),
            memory_project_path: "<cwd>/.cyber/memory.md".into(),
            memory_detail: None,
            config_path: "~/.cyber/config.toml".into(),
            providers_path: "~/.cyber/providers.toml".into(),
            sessions_dir: "~/.cyber/sessions".into(),
            sessions_count: 0,
            has_project_config: false,
        }
    }

    pub(crate) fn from_view(view: &ViewCtx<'_>) -> Self {
        let mut state = Self::new(view.config, view.providers);
        state.config_path = view.paths.config_file.display().to_string();
        state.providers_path = view.paths.providers_file.display().to_string();
        state.sessions_dir = view.paths.history_dir.display().to_string();
        state.sessions_count = view.sessions.len();
        state.has_project_config = view.has_project;
        if let Ok(mcp_cfg) = cyber_mcp::McpServersConfig::load(&view.paths.mcp_servers_file) {
            state.mcp_servers = mcp_cfg
                .servers
                .into_iter()
                .map(|s| {
                    let (connected, tool_count) = match view.mcp {
                        Some(m) => match m.tool_count(&s.name) {
                            Some(c) => (true, c),
                            None => (false, 0),
                        },
                        None => (false, 0),
                    };
                    let detail = match &s.transport {
                        cyber_mcp::McpTransport::Stdio => {
                            format!("stdio: {}", s.command.as_deref().unwrap_or(""))
                        }
                        cyber_mcp::McpTransport::Sse | cyber_mcp::McpTransport::Http => {
                            s.url.clone().unwrap_or_default()
                        }
                    };
                    McpServerSummary {
                        name: s.name,
                        connected,
                        tool_count,
                        detail,
                    }
                })
                .collect();
        }
        state.custom_tools = cyber_core::load_custom_tools(&view.paths.tools_dir)
            .0
            .into_iter()
            .map(|tool| tool.config)
            .collect();
        state.skills = view
            .skills
            .iter()
            .map(|s| SkillSummary {
                name: s.name().to_string(),
                source: match s.source {
                    cyber_skills::SkillSource::Global => "全局".into(),
                    cyber_skills::SkillSource::Project => "项目级".into(),
                },
                description: s.frontmatter.description.clone(),
                triggers: s.frontmatter.triggers.clone(),
                tools: s.frontmatter.tools.clone(),
                allowed_tools: s.frontmatter.allowed_tools.clone(),
                disable_model_invocation: s.frontmatter.disable_model_invocation,
                path: s.path.display().to_string(),
                body: s.body.clone(),
            })
            .collect();
        let memory_store = MemoryStore::new(
            view.paths.memory_file.clone(),
            Paths::project_memory_file(view.cwd),
        );
        state.memory_global_path = view.paths.memory_file.display().to_string();
        state.memory_project_path = Paths::project_memory_file(view.cwd).display().to_string();
        state.memories_global = memory_store.entries(MemoryScope::Global);
        state.memories_project = memory_store.entries(MemoryScope::Project);
        state
    }

    /// 记忆列表总条数（全局 + 项目级）。
    pub fn total_memories(&self) -> usize {
        self.memories_global.len() + self.memories_project.len()
    }

    /// 记忆列表第 `flat` 条（0-based，全局在前）→ (分组, 组内 0-based 索引)。
    pub fn memory_at(&self, flat: usize) -> Option<(MemoryGroup, usize)> {
        if flat < self.memories_global.len() {
            Some((MemoryGroup::Global, flat))
        } else {
            let p = flat - self.memories_global.len();
            (p < self.memories_project.len()).then_some((MemoryGroup::Project, p))
        }
    }

    pub fn next_tab(&mut self) {
        let all = SettingsTab::all();
        let idx = all.iter().position(|&t| t == self.tab).unwrap_or(0);
        self.tab = all[(idx + 1) % all.len()];
        self.selected_row = 0;
        self.list_selected = 0;
        self.skill_detail = None;
        self.memory_detail = None;
        self.editing = false;
    }

    pub fn prev_tab(&mut self) {
        let all = SettingsTab::all();
        let idx = all.iter().position(|&t| t == self.tab).unwrap_or(0);
        self.tab = all[(idx + all.len() - 1) % all.len()];
        self.selected_row = 0;
        self.list_selected = 0;
        self.skill_detail = None;
        self.memory_detail = None;
        self.editing = false;
    }

    pub fn set_tab(&mut self, tab: SettingsTab) {
        self.tab = tab;
        self.selected_row = 0;
        self.list_selected = 0;
        self.skill_detail = None;
        self.memory_detail = None;
        self.editing = false;
    }

    pub fn reset_current_tab(&mut self) {
        let default = Config::default();
        match self.tab {
            SettingsTab::AgentModel => {
                self.config_draft.agent.default_provider = default.agent.default_provider;
                self.config_draft.agent.auto_tool_call = default.agent.auto_tool_call;
                self.config_draft.agent.permission_mode = default.agent.permission_mode;
                self.config_draft.agent.max_steps = default.agent.max_steps;
                self.config_draft.agent.thinking_intensity = default.agent.thinking_intensity;
                self.config_draft.tools.web_search = default.tools.web_search;
                self.config_draft.agent.vision = default.agent.vision;
                self.config_draft.agent.retry_attempts = default.agent.retry_attempts;
                self.config_draft.agent.retry_delay_secs = default.agent.retry_delay_secs;
            }
            SettingsTab::UiWorkflow => {
                self.config_draft.ui = default.ui;
                self.config_draft.workflow = default.workflow;
            }
            SettingsTab::Subagents => {
                self.config_draft.agent.subagents = default.agent.subagents;
            }
            SettingsTab::ToolsMcp => {
                self.config_draft.tools.prefer_docker = default.tools.prefer_docker;
            }
            SettingsTab::Providers => {}
            SettingsTab::EnvMemory => {
                self.config_draft.memory = default.memory;
            }
            SettingsTab::StorageSystem => {
                self.config_draft.storage = default.storage;
            }
            // 工具是磁盘上的文件，不是 config 草稿，无法“恢复默认”。
            SettingsTab::Toolbox => {}
            // 只读页：提前返回，避免下方统一置 `dirty` 触发「未保存丢弃确认」。
            SettingsTab::About => return,
        }
        self.editing = false;
        self.dirty = true;
    }

    /// 焦点行是否「可编辑值行」：Enter 进入编辑态；只读态 ←/→ 不作用于该行。
    ///
    /// 可编辑值行 = bool / enum / number 等就地改值行；
    /// 打开弹窗/表单或执行动作的行（Providers、MCP、Skills、Toolbox、记忆列表）返回 false。
    pub fn focused_row_is_value(&self) -> bool {
        let env_slots = self.config_draft.env.vars.len().max(1);
        let mem_slots = self.config_draft.memory.rules.len().max(1);
        match self.tab {
            SettingsTab::AgentModel => !matches!(self.selected_row, 1 | 9),
            SettingsTab::UiWorkflow => true,
            SettingsTab::Subagents => true,
            SettingsTab::ToolsMcp => self.selected_row <= 1,
            SettingsTab::Providers => false,
            SettingsTab::EnvMemory => self.selected_row < env_slots + mem_slots,
            SettingsTab::StorageSystem => true,
            SettingsTab::Toolbox => false,
            SettingsTab::About => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolCardState {
    Pending,
    Success,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolCardKind {
    Read,
    Edit,
    Write,
    Download,
    Shell,
    Fetch,
    List,
    Find,
    Delegate,
    Generic,
    Todo,
}

#[derive(Debug)]
struct ToolCard {
    id: String,
    name: String,
    arguments: String,
    progress: String,
    output: String,
    state: ToolCardState,
    start: usize,
    end: usize,
}

/// 排队输入的种类（同一队列保序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuedKind {
    Prompt,
    Command,
}

#[derive(Debug, Clone)]
pub struct QueuedPrompt {
    pub text: String,
    pub displayed: bool,
    pub kind: QueuedKind,
}

/// 子代理面板交互状态：仅列表选择（Enter 进入覆盖对话视图）。
#[derive(Clone, Debug, Default)]
struct SubagentPanelState {
    selected: usize,
}

/// 子代理覆盖视图状态：`run_id` 指向 `SubagentArchive` 条目，转录以对话样式
/// 覆盖主对话区渲染（Esc 返回）。跨会话/回合保留（归档条目会话级不回删）。
#[derive(Clone, Debug)]
struct SubagentViewState {
    run_id: u64,
    /// true=跟随底部（新行到达自动下滚）。
    follow_bottom: bool,
    /// 自底部上滚行数（语义同旧面板 detail_scroll）。
    scroll: usize,
    /// Ctrl+O；独立于对话的 tools_expanded。
    tools_expanded: bool,
    /// 上次重建视图行的 (width, tools_expanded, 转录内容指纹)；变化才重建。
    /// 指纹基于内容而非行数：行数到 MAX_LINES 上限后仍会丢最旧/追加，
    /// 只用 len 会导致运行后期视图停止更新。
    built: Option<(u16, bool, u64)>,
    viewport: WrappedViewport,
    max_scroll: usize,
}

/// 后台任务面板交互状态（结构同子代理面板）。
#[derive(Clone, Debug, Default)]
struct JobsPanelState {
    selected: usize,
    detail: Option<u64>,
    detail_scroll: usize,
    follow_bottom: bool,
}

/// 模型选择面板交互状态（双栏选择服务商与模型）。
#[derive(Clone, Debug, Default)]
pub struct CliModelPickerState {
    /// 焦点所在栏：false = 左栏服务商 (Providers)，true = 右栏模型 (Models)
    pub focus_models: bool,
    /// 左栏当前选中的服务商索引
    pub provider_selected: usize,
    /// 右栏当前选中的模型索引
    pub model_selected: usize,
    /// 所有可用服务商配置快照
    pub providers: cyber_core::ProvidersConfig,
    /// 默认服务商名称
    pub default_provider: String,
    /// 当前所选服务商下的模型列表缓存
    pub models: Vec<String>,
    /// 服务商列表垂直滚动偏移
    pub provider_scroll: usize,
    /// 模型列表垂直滚动偏移
    pub model_scroll: usize,
    /// 是否正在实测识图能力探针
    pub probing_model: Option<String>,
    /// 正在从接口拉取模型列表的服务商名（None = 空闲）
    pub fetching: Option<String>,
    /// 最近一次拉取失败原因（成功/未拉取为 None）
    pub fetch_error: Option<String>,
    /// 右栏当前列表是否来自接口（false = 仅本地配置，尚未按 Enter 拉取）。
    pub fetched: bool,
    /// 拉取序号：仅接受与当前序号一致的结果，防止切换服务商后旧结果串台
    fetch_id: u64,
}

impl CliModelPickerState {
    pub(crate) fn from_view(view: &ViewCtx<'_>) -> Self {
        let default_prov = view.config.agent.default_provider.clone();
        let providers = view.providers.clone();
        let names = providers.sorted_names();
        let provider_selected = names.iter().position(|n| n == &default_prov).unwrap_or(0);
        let mut state = Self {
            focus_models: false,
            provider_selected,
            model_selected: 0,
            providers,
            default_provider: default_prov,
            models: Vec::new(),
            provider_scroll: 0,
            model_scroll: 0,
            probing_model: None,
            fetching: None,
            fetch_error: None,
            fetched: false,
            fetch_id: 0,
        };
        state.refresh_models();
        state
    }

    /// 本地配置（providers 快照）中当前服务商的模型清单：`models` 映射键 + 当前 `model`。
    fn config_models(&self) -> Vec<String> {
        let names = self.providers.sorted_names();
        let Some(cfg) = names
            .get(self.provider_selected)
            .and_then(|name| self.providers.providers.get(name))
        else {
            return Vec::new();
        };
        let mut list: Vec<String> = cfg.models.keys().cloned().collect();
        if !cfg.model.is_empty() && !list.contains(&cfg.model) {
            list.push(cfg.model.clone());
        }
        list
    }

    /// 当前服务商在配置里指定的模型（`providers[name].model`）。
    fn configured_model(&self) -> Option<String> {
        let names = self.providers.sorted_names();
        names
            .get(self.provider_selected)
            .and_then(|name| self.providers.providers.get(name))
            .map(|cfg| cfg.model.clone())
            .filter(|m| !m.is_empty())
    }

    /// 本地配置刷新：切换/打开服务商时的即时列表（接口结果到达前的兜底）。
    pub fn refresh_models(&mut self) {
        let list = self.config_models();
        self.apply_models(list, false);
        self.fetched = false;
        self.fetch_error = None;
    }

    /// 写入模型列表：trim + 去重 + 排序；`keep_selection` 为真时优先保持当前选中项
    /// （接口结果到达时不打断用户移动光标），否则回落到配置指定的模型。
    fn apply_models(&mut self, list: Vec<String>, keep_selection: bool) {
        let previous = if keep_selection {
            self.models.get(self.model_selected).cloned()
        } else {
            None
        };
        let configured = self.configured_model();
        let mut models: Vec<String> = Vec::new();
        for model in list {
            let model = model.trim();
            if !model.is_empty() && !models.iter().any(|m| m == model) {
                models.push(model.to_string());
            }
        }
        sort_model_ids(&mut models);
        self.models = models;
        self.model_scroll = 0;
        let mut target = previous
            .as_deref()
            .and_then(|m| self.models.iter().position(|x| x == m));
        if target.is_none() {
            target = configured
                .as_deref()
                .and_then(|m| self.models.iter().position(|x| x == m));
        }
        self.model_selected = target.unwrap_or(0);
    }

    /// 异步拉取当前服务商的模型列表（端点构造见 `cyber_agent::fetch_models`，已含版本段的
    /// base_url 不会再补 `/v1`）。
    ///
    /// 结果经 `tx` 回传 CLI 主循环，由 `deliver_fetch` 归并；期间（以及失败后）右栏
    /// 仍显示 `refresh_models` 提供的本地配置清单。
    fn begin_fetch(&mut self, tx: &mpsc::UnboundedSender<ModelFetchResult>) {
        let names = self.providers.sorted_names();
        let Some(name) = names.get(self.provider_selected).cloned() else {
            return;
        };
        let Some(cfg) = self.providers.providers.get(&name).cloned() else {
            return;
        };
        self.fetch_id = self.fetch_id.wrapping_add(1);
        let fetch_id = self.fetch_id;
        self.fetching = Some(name.clone());
        self.fetch_error = None;
        self.fetched = false;
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = cyber_agent::fetch_models(&cfg)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(ModelFetchResult {
                fetch_id,
                provider: name,
                result,
            });
        });
    }

    /// 接收拉取结果：`fetch_id` / 服务商与当前状态不符（已切换服务商或面板重开）则丢弃。
    fn deliver_fetch(&mut self, fetch: ModelFetchResult) {
        if fetch.fetch_id != self.fetch_id
            || self.fetching.as_deref() != Some(fetch.provider.as_str())
        {
            return;
        }
        self.fetching = None;
        match fetch.result {
            Ok(models) if !models.is_empty() => {
                self.fetch_error = None;
                self.fetched = true;
                // 接口结果为基准，本地已配置的模型（别名/手写清单）一并保留：
                // 接口不可用或未覆盖时列表不会退化成空。
                let mut merged = models;
                merged.extend(self.config_models());
                self.apply_models(merged, true);
            }
            Ok(_) => self.fetch_error = Some("接口未返回任何模型".into()),
            Err(err) => self.fetch_error = Some(err),
        }
    }
}

/// 模型列表异步拉取结果：`fetch_models` 任务 → CLI 主循环（`CliScreen::model_fetch_tx`）。
struct ModelFetchResult {
    fetch_id: u64,
    provider: String,
    result: Result<Vec<String>, String>,
}

/// 安装脚本启动器：`(version, wait_for_exit) -> 脚本路径`。
///
/// 生产实现为 `cyber_core::update::launch_detached_install`；测试注入假实现。
pub(crate) type UpdateLauncher =
    Arc<dyn Fn(&str, bool) -> std::io::Result<std::path::PathBuf> + Send + Sync>;

/// `/update` 检查结果：`check_for_updates(true)` 任务 → CLI 主循环。
struct UpdateCheckResult {
    kind: cli_commands::CliUpdate,
    info: Option<cyber_core::update::ReleaseInfo>,
}

/// CLI 对话页 todo 清单三态视图（原 `todo_closed: bool` 二元态的扩展）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TodoView {
    /// 完整收起：不画表格，只在对话区底部保留 1 行进度条。
    Collapsed,
    /// 精简：表格高度 clamp(3, 7)（内区最多 5 条 + 折叠提示），与改动前一致。
    #[default]
    Summary,
    /// 全量：表格按内容要高度，上限为可用空间；超出仍由 `todo_visible_window` 窗口化。
    Full,
}

/// 回合期间（runner 已被 `take()` 走）只读指令的数据快照，在 `CliScreen::sync` 刷新。
///
/// 只做克隆、不做任何磁盘 I/O：busy 期间每条只读指令都不得引入 per-turn 读盘。
struct ViewSnapshot {
    config: Config,
    providers: ProvidersConfig,
    paths: Paths,
    cwd: std::path::PathBuf,
    sessions: Vec<crate::history::SessionMeta>,
    current_session_id: String,
    has_project: bool,
    ctf_enabled: bool,
    ctf_challenges: Option<Arc<std::sync::Mutex<Vec<cyber_core::CtfChallenge>>>>,
    tools: Arc<cyber_agent::ToolRegistry>,
    skills: Arc<cyber_skills::SkillRegistry>,
    mcp: Option<Arc<cyber_mcp::McpRegistry>>,
}

impl ViewSnapshot {
    /// 快照当前 runner 的只读数据（无磁盘 I/O）。
    fn refresh_from(runner: &SessionRunner) -> Self {
        Self {
            config: runner.ctx.config.clone(),
            providers: runner.ctx.providers.clone(),
            paths: runner.ctx.paths.clone(),
            cwd: runner.cwd.clone(),
            sessions: runner.index.sessions.clone(),
            current_session_id: runner.index.current.clone(),
            has_project: runner.ctx.project.is_some(),
            ctf_enabled: runner.ctf_enabled,
            ctf_challenges: runner.registries.ctf_challenges.clone(),
            tools: Arc::clone(&runner.registries.tools),
            skills: Arc::clone(&runner.registries.skills),
            mcp: runner.registries.mcp.clone(),
        }
    }

    fn view(&self) -> crate::headless::ViewCtx<'_> {
        crate::headless::ViewCtx {
            config: &self.config,
            providers: &self.providers,
            paths: &self.paths,
            cwd: &self.cwd,
            sessions: &self.sessions,
            current_session_id: &self.current_session_id,
            has_project: self.has_project,
            ctf_enabled: self.ctf_enabled,
            ctf_challenges: self.ctf_challenges.as_deref(),
            tools: &self.tools,
            skills: &self.skills,
            mcp: self.mcp.as_deref(),
        }
    }
}

/// 「关于」页内容快照：进入该页时一次性采集，渲染期不做磁盘 I/O。
#[derive(Clone, Debug)]
struct AboutInfo {
    version: &'static str,
    latest: Option<String>,
    binary: Option<String>,
    provider: String,
    model: String,
    effort: String,
    cwd: String,
    config_path: String,
    providers_path: String,
    sessions_dir: String,
    tools_dir: String,
    sessions_count: usize,
    has_project_config: bool,
    log_level: String,
    tool_count: usize,
    skill_count: usize,
    custom_tool_count: usize,
}

impl AboutInfo {
    /// 采集当前运行环境（`view_snapshot` + 顶栏字段 + `new_version`）。
    /// 唯一 I/O：统计自定义工具目录（`cyber_core::load_custom_tools`，每次进入页面一次）。
    fn collect(screen: &CliScreen) -> Self {
        let snap = &screen.view_snapshot;
        Self {
            version: cyber_core::update::CURRENT_VERSION,
            latest: screen.new_version.clone(),
            binary: std::env::current_exe()
                .ok()
                .map(|p| p.display().to_string()),
            provider: screen.provider.clone(),
            model: screen.model.clone(),
            effort: effort_label(screen.effort).to_string(),
            cwd: screen.cwd.clone(),
            config_path: snap.paths.config_file.display().to_string(),
            providers_path: snap.paths.providers_file.display().to_string(),
            sessions_dir: snap.paths.history_dir.display().to_string(),
            tools_dir: snap.paths.tools_dir.display().to_string(),
            sessions_count: snap.sessions.len(),
            has_project_config: snap.has_project,
            log_level: snap.config.storage.log_level.clone(),
            tool_count: snap.tools.all_schemas().len(),
            skill_count: snap.skills.len(),
            custom_tool_count: cyber_core::load_custom_tools(&snap.paths.tools_dir).0.len(),
        }
    }
}

struct CliScreen {
    provider: String,
    model: String,
    effort: ThinkingIntensity,
    cwd: String,
    session: String,
    session_title: String,
    input: TextArea<'static>,
    approval_input: TextArea<'static>,
    approval: Option<PermissionRequest>,
    approval_choice: Option<ApprovalChoice>,
    messages: Vec<Line<'static>>,
    response_start: Option<usize>,
    // At most one answer and one thinking segment per uninterrupted response.
    stream: Vec<ResponseSegment>,
    used_tokens: usize,
    context_length: Option<u32>,
    usage_reported: bool,
    prompt_tokens: u128,
    completion_tokens: u128,
    cache_hit_tokens: u128,
    cache_miss_tokens: u128,
    busy: bool,
    status: String,
    scroll: usize,
    max_scroll: usize,
    pub auto_scroll: bool,
    history_view: WrappedViewport,
    approval_scroll: usize,
    approval_max_scroll: usize,
    approval_nonce: String,
    approval_arguments: Vec<Line<'static>>,
    approval_view: WrappedViewport,
    approval_visible: bool,
    panel: Option<Panel>,
    pub settings: Option<CliSettingsState>,
    /// 首次配置向导模式（`cyber setup`）：Esc 只在配置可用后允许退出。
    setup_mode: bool,
    pub settings_return_tab: Option<SettingsTab>,
    pub model_picker: Option<CliModelPickerState>,
    pub mcp_panel: Option<crate::views::mcp_panel::McpPanelState>,
    pub ctf_edit_form: Option<crate::views::ctf_edit_form::CtfEditFormState>,
    ctf_enabled: bool,
    ctf_challenges: Arc<std::sync::Mutex<Vec<cyber_core::CtfChallenge>>>,
    ctf_selected: usize,
    ctf_detail_view: bool,
    ctf_detail_scroll: usize,
    ctf_list_scroll: std::cell::Cell<usize>,
    completions: Vec<CompletionItem>,
    completion_selected: usize,
    completion_closed: bool,
    completion_accepted: bool,
    form: Option<FormState>,
    picker: Option<CommandPicker>,
    picker_selected: usize,
    delete_pending: Option<String>,
    tools: Vec<ToolCard>,
    tools_expanded: bool,
    assistant_label_shown: bool,
    recent_sessions: Vec<String>,
    permission_mode: PermissionMode,
    prompt_history: Vec<String>,
    history_index: Option<usize>,
    saved_draft: String,
    thinking_started: Option<std::time::Instant>,
    has_run: bool,
    todos: Arc<std::sync::Mutex<Vec<cyber_core::TodoItem>>>,
    pub(crate) todo_view: TodoView,
    last_window_title: String,
    pub needs_clear: bool,
    pub queued_prompts: std::collections::VecDeque<QueuedPrompt>,
    pub active_steering_tx: Option<cyber_agent::SteeringSender>,
    /// 模型列表异步拉取结果通道。仅 `run_cli` 注入（测试为 `None` → 不发网络请求）。
    model_fetch_tx: Option<mpsc::UnboundedSender<ModelFetchResult>>,
    /// `/update` 联网检查通道（仅 `run_cli_inner` 注入；测试为 `None` → 只读本地缓存，不联网）。
    update_check_tx: Option<mpsc::UnboundedSender<UpdateCheckResult>>,
    /// 正在联网检查（占位，避免重复发起）。
    update_checking: bool,
    /// 已发现新版本、等待用户按 `y` 确认。
    update_pending: Option<cyber_core::update::ReleaseInfo>,
    /// 安装脚本启动器（测试注入点；生产为 `cyber_core::update::launch_detached_install`）。
    update_launcher: UpdateLauncher,
    /// 子代理转录注册表（Ctrl+G 面板数据源）。
    subagents: Arc<cyber_agent::SubagentArchive>,
    /// 后台任务注册表（Ctrl+B 面板 + /bg 命令数据源）。
    background: Arc<cyber_agent::BackgroundRegistry>,
    /// 最近一次回合的 (cwd, env) 快照（`/bg shell` busy 时可用）。
    background_env: Option<(std::path::PathBuf, Vec<(String, String)>)>,
    /// 回合中完成的后台子代理结果，`sync` 时 flush 进会话。
    pending_job_results: std::collections::VecDeque<String>,
    subagent_panel: Option<SubagentPanelState>,
    jobs_panel: Option<JobsPanelState>,
    /// 子代理覆盖对话视图（Ctrl+G 面板 Enter 进入；Esc 返回）。
    subagent_view: Option<SubagentViewState>,
    /// 检测到的新版本（若有新版本则为 Some(ver)，在顶栏黄色标出）
    pub new_version: Option<String>,
    /// 「关于」页内容快照（进入该页时采集；`Panel::About` 与设置中心「9. 关于」共用）。
    about: Option<AboutInfo>,
    /// 「关于」页滚动行偏移（绘制时按内容/可视高度收敛）。
    about_scroll: usize,
    /// 回合期间只读指令的数据快照（sync 刷新）。
    view_snapshot: ViewSnapshot,
    pub attached_images: Vec<cyber_agent::AttachedImage>,
    pub next_image_id: usize,
    pub last_ctrl_c: Option<std::time::Instant>,
    pub last_paste_instant: Option<std::time::Instant>,
    pub history_area: Rect,
    #[allow(dead_code)]
    pub selection: Option<TextSelection>,
    pub is_dragging_scrollbar: bool,
    #[allow(dead_code)]
    pub question_area: Rect,
    #[allow(dead_code)]
    pub question_state: Option<crate::question_ui::QuestionUiState>,
}

struct FormState {
    form: CommandForm,
    inputs: Vec<TextArea<'static>>,
    selected: usize,
    /// 模型列表浮层（`Some` = 打开；`manual` = 手输兜底模式）。
    model_picker: Option<FormModelPicker>,
    /// 最近一次成功拉取到的模型 id 列表（关闭浮层后再按 Enter 无需重新拉取）。
    last_models: Vec<String>,
}

/// CLI 全屏 provider 表单的模型列表面板状态。
struct FormModelPicker {
    /// 带能力标签的模型条目（打开面板时一次性解析，避免渲染期读盘）。
    entries: Vec<FormModelEntry>,
    selected: usize,
    /// 拉取失败/端点缺失时的错误信息。
    error: Option<String>,
    /// 手输兜底模式：true = 不画列表浮层，model 字段恢复可编辑。
    manual: bool,
}

/// 模型列表条目：id + 已解析出的能力标签。
struct FormModelEntry {
    id: String,
    vision: cyber_core::VisionCapability,
    reasoning: cyber_core::ReasoningCapability,
}

/// 同步探针实测结果的局部统一类型（vision / reasoning 返回类型不同，无法直接共用）。
enum ProbeKindLite {
    Vision(cyber_core::VisionCapability),
    Reasoning(cyber_core::ReasoningCapability),
}

/// 在给定候选值上循环 provider 表单当前选中字段（候选索引 0 表示「未设置」）。
///
/// 与 model/kind 等字段的循环共用；`backwards` = 按了 ←。
fn cycle_field_value(form: &mut FormState, options: &[&str], backwards: bool) {
    let Some(name) = form.form.fields.get(form.selected).map(|f| f.name.clone()) else {
        return;
    };
    let cur = form.input_value(&name).trim().to_ascii_lowercase();
    let cur_idx = options.iter().position(|o| *o == cur).unwrap_or(0);
    let next = if backwards {
        (cur_idx + options.len() - 1) % options.len()
    } else {
        (cur_idx + 1) % options.len()
    };
    let value = options[next];
    form.set_input(&name, value);
}

/// 由 CLI 表单当前值构造临时 `ProviderConfig`（拉取模型 / 探针用；不落盘）。
///
/// 以输入框文本为准（`inputs` 是用户正在编辑的真值；`form.fields[].value` 只在保存前 `capture` 时同步）。
fn provider_config_from_form(form: &FormState) -> cyber_core::ProviderConfig {
    let opt = |n: &str| {
        let v = form.input_value(n);
        let v = v.trim();
        (!v.is_empty()).then(|| v.to_string())
    };
    cyber_core::ProviderConfig {
        kind: form.input_value("kind").trim().to_ascii_lowercase(),
        base_url: form.input_value("endpoint").trim().to_string(),
        api_key: form.input_value("apikey").trim().to_string(),
        model: form.input_value("model").trim().to_string(),
        max_tokens: form
            .input_value("maxtokens")
            .trim()
            .parse()
            .unwrap_or(cyber_core::DEFAULT_MAX_TOKENS),
        temperature: form
            .input_value("temperature")
            .trim()
            .parse()
            .unwrap_or(0.7),
        chat_endpoint: opt("chat_endpoint"),
        models_endpoint: opt("models_endpoint"),
        ..Default::default()
    }
}

/// 表单当前服务商名：Provider 表单取 `name` 字段，工具库扫描表单取 `provider` 字段。
fn form_provider_name(form: &FormState) -> String {
    let field = match form.form.kind {
        FormKind::ToolboxScan => "provider",
        _ => "name",
    };
    form.input_value(field).trim().to_string()
}

/// 表单当前服务商对应的配置：Provider 表单取正在编辑的输入值；
/// 工具库扫描表单按 `provider` 名查已保存的 `providers.toml`（拉取模型 / 探针用）。
fn provider_config_for_form(
    form: &FormState,
    runner: Option<&SessionRunner>,
) -> cyber_core::ProviderConfig {
    if matches!(form.form.kind, FormKind::ToolboxScan) {
        let name = form_provider_name(form);
        return runner
            .and_then(|r| r.ctx.providers.providers.get(&name).cloned())
            .unwrap_or_default();
    }
    provider_config_from_form(form)
}

/// 「AI 智能扫描」表单的默认服务商与模型：当前默认 Provider（不可用时取首个已配置服务商）
/// 及其 `providers.toml` 中配置的模型。
fn scan_form_defaults(runner: Option<&SessionRunner>) -> (String, String) {
    let default_provider = runner
        .map(|r| r.ctx.config.agent.default_provider.clone())
        .unwrap_or_default();
    let provider = if default_provider.is_empty()
        || !runner.is_some_and(|r| r.ctx.providers.providers.contains_key(&default_provider))
    {
        runner
            .map(|r| r.ctx.providers.sorted_names())
            .and_then(|names| names.first().cloned())
            .unwrap_or(default_provider)
    } else {
        default_provider
    };
    let model = runner
        .and_then(|r| r.ctx.providers.providers.get(&provider))
        .map(|p| p.model.clone())
        .unwrap_or_default();
    (provider, model)
}

impl FormState {
    fn new(form: CommandForm) -> Self {
        let inputs = form
            .fields
            .iter()
            .map(|field| {
                let mut input = composer();
                input.set_placeholder_text("");
                input.insert_str(&field.value);
                input
            })
            .collect();
        Self {
            form,
            inputs,
            selected: 0,
            model_picker: None,
            last_models: Vec::new(),
        }
    }

    fn capture(&mut self) {
        for (field, input) in self.form.fields.iter_mut().zip(&self.inputs) {
            field.value = input.lines().join("\n");
        }
    }

    /// 按字段名读取输入框文本（不存在则空串）。
    fn input_value(&self, name: &str) -> String {
        self.form
            .fields
            .iter()
            .position(|f| f.name == name)
            .and_then(|idx| self.inputs.get(idx))
            .map(|input| input.lines().join("\n"))
            .unwrap_or_default()
    }

    /// 当前 `model` 字段文本。
    fn model_input(&self) -> String {
        self.input_value("model")
    }

    /// 写回指定字段的输入框文本（同步 `form.fields`，保证随后 `capture`/`submit_form` 一致）。
    fn set_input(&mut self, name: &str, value: &str) {
        if let Some(idx) = self.form.fields.iter().position(|f| f.name == name) {
            if let Some(input) = self.inputs.get_mut(idx) {
                *input = composer();
                input.insert_str(value);
            }
            self.form.fields[idx].value = value.to_string();
        }
    }

    /// 写回 `model` 字段。
    fn set_model_input(&mut self, value: &str) {
        self.set_input("model", value);
    }

    /// 以拉取结果打开模型选择浮层；`Err` / 空列表进入手输兜底模式。
    ///
    /// 能力标签在此一次性解析（显式 models 配置 → 持久化实测缓存 → 名称规则表），
    /// 渲染期不再读盘。`models_map` 取自当前已保存的 provider 配置（未保存时传 `None`）。
    fn open_model_picker(
        &mut self,
        result: Result<Vec<String>, String>,
        models_map: Option<&std::collections::HashMap<String, cyber_core::ModelConfig>>,
        provider_name: &str,
    ) {
        let (mut ids, error, manual) = match result {
            Ok(list) if !list.is_empty() => (list, None, false),
            Ok(_) => (Vec::new(), Some("未返回任何模型".to_string()), true),
            Err(e) => (Vec::new(), Some(e), true),
        };
        // 接口返回顺序不保证有序，按名称首字母排序，避免列表看起来乱序。
        sort_model_ids(&mut ids);
        let store = cyber_core::CapabilityStore::load();
        let entries: Vec<FormModelEntry> = ids
            .iter()
            .map(|id| FormModelEntry {
                id: id.clone(),
                vision: cyber_core::resolve_vision_capability(
                    models_map,
                    provider_name,
                    id,
                    &store,
                ),
                reasoning: cyber_core::resolve_reasoning_capability(
                    models_map,
                    provider_name,
                    id,
                    &store,
                ),
            })
            .collect();
        let current = self.model_input();
        let selected = entries.iter().position(|e| e.id == current).unwrap_or(0);
        if !ids.is_empty() {
            self.last_models = ids;
        }
        self.model_picker = Some(FormModelPicker {
            entries,
            selected,
            error,
            manual,
        });
    }
}

struct ResponseSegment {
    thinking: bool,
    text: String,
    lines: Vec<Line<'static>>,
}

fn composer() -> TextArea<'static> {
    let mut input = TextArea::default();
    input.set_placeholder_text("Try \"refactor <filepath>\"");
    input.set_style(Style::default().fg(FG));
    input.set_placeholder_style(Style::default().fg(DIM));
    input.set_cursor_line_style(Style::default());
    input.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    input
}

impl CliScreen {
    fn insert_text(&mut self, text: &str) {
        self.delete_pending = None;
        if self.approval.is_some() {
            // Approval is keyboard-only. Ignore paste so it cannot select or confirm an action.
        } else if let Some(q_state) = self.question_state.as_mut() {
            if q_state.custom_editing {
                let single_line = clean(text).replace(['\r', '\n'], " ");
                q_state.custom_textarea.insert_str(&single_line);
            }
        } else if let Some(form) = &mut self.form {
            if let Some(input) = form.inputs.get_mut(form.selected) {
                input.insert_str(clean(text));
            }
        } else if let Some(form) = &mut self.ctf_edit_form {
            if form.editing {
                form.textarea.insert_str(clean(text));
            }
        } else if self.panel.is_none() && self.picker.is_none() {
            self.input.insert_str(clean(text));
            self.completion_closed = false;
            self.completion_accepted = false;
        }
    }

    fn attach_image(
        &mut self,
        path: impl Into<std::path::PathBuf>,
        display_name: impl Into<String>,
    ) -> usize {
        let id = self.next_image_id;
        self.next_image_id += 1;
        self.attached_images
            .push(cyber_agent::AttachedImage::new(id, path, display_name));
        self.input.insert_str(format!("[image:{id}]"));
        self.completion_closed = false;
        self.completion_accepted = false;
        id
    }

    fn expand_attached_placeholders(&mut self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut last_end = 0;
        let bytes = text.as_bytes();
        let len = bytes.len();
        let mut i = 0;
        let mut referenced_ids = std::collections::HashSet::new();

        while i < len {
            if bytes[i] == b'[' {
                let rest = &text[i + 1..];
                if rest.to_ascii_lowercase().starts_with("image:") {
                    let prefix_len = 1 + "image:".len();
                    if let Some(close) = text[i + prefix_len..].find(']') {
                        let end = i + prefix_len + close + 1;
                        let spec = text[i + prefix_len..end - 1].trim();
                        if let Ok(id) = spec.parse::<usize>() {
                            if let Some(att) = self.attached_images.iter().find(|a| a.id == id) {
                                out.push_str(&text[last_end..i]);
                                out.push_str(&format!("[image: {}]", att.path.display()));
                                referenced_ids.insert(id);
                                last_end = end;
                                i = end;
                                continue;
                            }
                        }
                    }
                }
            }
            i += 1;
        }
        out.push_str(&text[last_end..]);
        self.attached_images
            .retain(|a| referenced_ids.contains(&a.id));
        out
    }

    fn update_completions(&mut self, runner: Option<&SessionRunner>) {
        let input = &self.input.lines()[0];
        self.completions = if input.starts_with('/') {
            cli_commands::suggestions(runner, input)
        } else {
            Vec::new()
        };
        self.completion_selected = self
            .completion_selected
            .min(self.completions.len().saturating_sub(1));
    }

    fn complete(&mut self) -> bool {
        let Some(item) = self.completions.get(self.completion_selected) else {
            return false;
        };
        let current = self.input.lines().join("\n");
        if current.trim() == item.value.trim() {
            if current != item.value.trim() {
                self.input = composer();
                self.input.insert_str(item.value.trim());
            }
            return false;
        }
        let changed = current != item.value;
        if changed {
            self.input = composer();
            self.input.insert_str(&item.value);
        }
        changed
    }

    fn new(runner: &SessionRunner) -> Self {
        let mut screen = Self {
            provider: String::new(),
            model: String::new(),
            effort: runner.ctx.config.agent.thinking_intensity,
            cwd: runner.cwd.display().to_string(),
            session: runner.index.current.clone(),
            session_title: runner
                .index
                .get(&runner.index.current)
                .map(|meta| meta.title.clone())
                .unwrap_or_else(|| "新会话".into()),
            input: composer(),
            approval_input: composer(),
            approval: None,
            approval_choice: None,
            messages: Vec::new(),
            response_start: None,
            stream: Vec::new(),
            used_tokens: 0,
            context_length: None,
            usage_reported: false,
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
            busy: false,
            status: String::new(),
            scroll: 0,
            max_scroll: 0,
            auto_scroll: true,
            history_view: WrappedViewport {
                padding: 1,
                ..WrappedViewport::default()
            },
            approval_scroll: 0,
            approval_max_scroll: 0,
            approval_nonce: String::new(),
            approval_arguments: Vec::new(),
            approval_view: WrappedViewport::default(),
            approval_visible: false,
            panel: None,
            settings: None,
            setup_mode: false,
            settings_return_tab: None,
            model_picker: None,
            mcp_panel: None,
            ctf_edit_form: None,
            ctf_enabled: runner.ctf_enabled,
            ctf_challenges: runner
                .registries
                .ctf_challenges
                .clone()
                .unwrap_or_else(|| Arc::new(std::sync::Mutex::new(Vec::new()))),
            ctf_selected: 0,
            ctf_detail_view: false,
            ctf_detail_scroll: 0,
            ctf_list_scroll: std::cell::Cell::new(0),
            completions: Vec::new(),
            completion_selected: 0,
            completion_closed: false,
            completion_accepted: false,
            form: None,
            picker: None,
            picker_selected: 0,
            delete_pending: None,
            tools: Vec::new(),
            tools_expanded: false,
            assistant_label_shown: false,
            recent_sessions: Vec::new(),
            permission_mode: runner
                .ctx
                .config
                .agent
                .permission_mode
                .as_deref()
                .and_then(PermissionMode::parse)
                .unwrap_or(if runner.ctx.config.agent.auto_tool_call {
                    PermissionMode::Auto
                } else {
                    PermissionMode::Manual
                }),
            prompt_history: Vec::new(),
            history_index: None,
            saved_draft: String::new(),
            thinking_started: None,
            todos: Arc::clone(&runner.registries.todos),
            has_run: false,
            todo_view: TodoView::Summary,
            last_window_title: String::new(),
            needs_clear: false,
            queued_prompts: std::collections::VecDeque::new(),
            active_steering_tx: None,
            model_fetch_tx: None,
            update_check_tx: None,
            update_checking: false,
            update_pending: None,
            update_launcher: Arc::new(cyber_core::update::launch_detached_install),
            subagents: Arc::clone(&runner.registries.subagents),
            background: Arc::clone(&runner.registries.background),
            background_env: Some((
                runner.cwd.clone(),
                runner
                    .ctx
                    .config
                    .env
                    .vars
                    .iter()
                    .map(|value| (value.key.clone(), value.value.clone()))
                    .collect(),
            )),
            pending_job_results: std::collections::VecDeque::new(),
            subagent_panel: None,
            jobs_panel: None,
            subagent_view: None,
            new_version: None,
            about: None,
            about_scroll: 0,
            view_snapshot: ViewSnapshot::refresh_from(runner),
            attached_images: Vec::new(),
            next_image_id: 1,
            last_ctrl_c: None,
            last_paste_instant: None,
            history_area: Rect::default(),
            is_dragging_scrollbar: false,
            selection: None,
            question_area: Rect::default(),
            question_state: None,
        };
        if let Some(info) = cyber_core::update::cached_latest_version() {
            if cyber_core::update::is_newer(cyber_core::update::CURRENT_VERSION, &info.version) {
                screen.new_version = Some(info.version);
            }
        }
        screen.sync(runner);
        screen
    }

    fn sync(&mut self, runner: &SessionRunner) {
        let provider = runner
            .ctx
            .providers
            .providers
            .get(&runner.ctx.config.agent.default_provider);
        let model = provider
            .map(|provider| provider.model.clone())
            .unwrap_or_else(|| "not configured".into());
        let changed_session = self.session != runner.index.current;
        let changed_model =
            self.provider != runner.ctx.config.agent.default_provider || self.model != model;
        if changed_session {
            self.reset_usage();
        }
        if changed_session || changed_model {
            self.used_tokens = estimate_messages_tokens(&entries_to_messages(&runner.entries));
            self.context_length = provider.and_then(|provider| provider.effective_context_length());
        }
        self.provider
            .clone_from(&runner.ctx.config.agent.default_provider);
        self.model = model;
        self.effort = runner.ctx.config.agent.thinking_intensity;
        self.session = runner.index.current.clone();
        self.session_title = runner
            .index
            .get(&self.session)
            .map(|meta| meta.title.clone())
            .unwrap_or_else(|| "新会话".into());
        self.ctf_enabled = runner.ctf_enabled;
        if let Some(challenges) = &runner.registries.ctf_challenges {
            self.ctf_challenges = Arc::clone(challenges);
        }
        let challenge_count = self.ctf_challenges_count();
        if self.ctf_selected >= challenge_count {
            self.ctf_selected = challenge_count.saturating_sub(1);
        }
        let mut recent = runner
            .index
            .sessions
            .iter()
            .filter(|session| session.message_count > 0)
            .collect::<Vec<_>>();
        recent.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        self.recent_sessions = recent
            .into_iter()
            .take(4)
            .map(|session| format!("{}  {}", clean(&session.id), clean(&session.title)))
            .collect();
        self.todos = Arc::clone(&runner.registries.todos);
        self.prompt_history = runner
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ChatEntry::User(text) => {
                    let cleaned = clean(text);
                    if !cleaned.trim().is_empty() {
                        Some(cleaned)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect();
        self.thinking_started = None;
        self.has_run = !runner.entries.is_empty() || self.usage_reported;
        self.history_index = None;
        self.messages.clear();
        self.assistant_label_shown = false;
        self.tools.clear();
        self.response_start = None;
        self.stream.clear();
        for entry in &runner.entries {
            match entry {
                ChatEntry::User(text) => self.message("You", text, ACCENT),
                ChatEntry::Assistant(text) => {
                    if !text.trim().is_empty() {
                        self.response_text(false, text);
                    }
                }
                ChatEntry::Thinking(text) => {
                    if !text.trim().is_empty() {
                        self.response_text(true, text);
                    }
                }
                ChatEntry::ToolCall {
                    id,
                    name,
                    arguments,
                } => self.tool_call(id, name, arguments),
                ChatEntry::ToolResult {
                    id,
                    name,
                    output,
                    is_error,
                } => self.tool_result(id, name, output, *is_error),
                ChatEntry::TurnSummary {
                    elapsed_ms,
                    finished_at,
                    status,
                    summary,
                } => {
                    self.turn_summary(*elapsed_ms, *finished_at, status, summary.as_deref());
                }
                ChatEntry::System(text) => self.message("Notice", text, MUTED),
            }
        }
        self.stream.clear();
        self.scroll = 0;
        // 回合期间 runner 已被 take 走，只读指令依赖此快照。
        self.view_snapshot = ViewSnapshot::refresh_from(runner);
    }

    /// 只读视图：空闲时取 runner 快照，回合期间取 `sync` 缓存的快照。
    pub(crate) fn view(&self) -> crate::headless::ViewCtx<'_> {
        self.view_snapshot.view()
    }

    fn reset_usage(&mut self) {
        self.usage_reported = false;
        self.prompt_tokens = 0;
        self.completion_tokens = 0;
        self.cache_hit_tokens = 0;
        self.cache_miss_tokens = 0;
    }

    fn message(&mut self, label: &str, text: &str, color: Color) {
        self.stream.clear();
        self.response_start = None;
        self.messages.push(Line::default());
        if label == "You" {
            self.has_run = true;
            self.assistant_label_shown = false;
            let style = Style::default().fg(FG).bg(USER_BG);
            self.messages.push(Line::styled("", style));
            for line in clean(text).lines() {
                if line.contains("[image:") {
                    let mut spans = Vec::new();
                    Self::render_cli_text_with_image_badges(&mut spans, line, style);
                    self.messages.push(Line::from(spans).style(style));
                } else {
                    self.messages.push(Line::styled(line.to_owned(), style));
                }
            }
            self.messages.push(Line::styled("", style));
            return;
        }
        self.messages.push(Line::styled(
            clean(label),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        // 系统消息与通知（含后台子代理注入结果）走 Markdown 渲染，保证表格/粗体/代码块正常显示。
        if label == "System" || label == "Notice" {
            let rendered = crate::markdown::render(&clean(text), &CLI_THEME);
            self.messages.extend(rendered);
            return;
        }
        self.messages.extend(clean(text).lines().map(|line| {
            Line::styled(
                line.to_owned(),
                Style::default().fg(if label == "You" { CLI_THEME.fg } else { color }),
            )
        }));
    }

    fn render_cli_text_with_image_badges(
        spans: &mut Vec<Span<'static>>,
        text: &str,
        default_style: Style,
    ) {
        let mut last_end = 0;
        let bytes = text.as_bytes();
        let len = bytes.len();
        let mut i = 0;

        while i < len {
            if bytes[i] == b'[' {
                let rest = &text[i + 1..];
                if rest.to_ascii_lowercase().starts_with("image:") {
                    let prefix_len = 1 + "image:".len();
                    if let Some(close) = text[i + prefix_len..].find(']') {
                        let end = i + prefix_len + close + 1;
                        let spec = text[i + prefix_len..end - 1].trim();
                        if let Ok(id) = spec.parse::<usize>() {
                            if i > last_end {
                                spans.push(Span::styled(
                                    text[last_end..i].to_string(),
                                    default_style,
                                ));
                            }
                            spans.push(Span::styled(
                                format!("🖼️ [image:{id}]"),
                                Style::default()
                                    .fg(Color::White)
                                    .bg(Color::Rgb(112, 48, 160))
                                    .add_modifier(Modifier::BOLD),
                            ));
                            last_end = end;
                            i = end;
                            continue;
                        }
                    }
                }
            }
            i += 1;
        }

        if last_end < text.len() {
            spans.push(Span::styled(text[last_end..].to_string(), default_style));
        }
    }

    fn handle_text_image_paste(&mut self, text: &str) -> bool {
        let trimmed = text.trim();
        let unquoted = trimmed
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| {
                trimmed
                    .strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''))
            })
            .or_else(|| trimmed.strip_prefix('“').and_then(|s| s.strip_suffix('”')))
            .unwrap_or(trimmed)
            .trim();

        if unquoted.is_empty() || unquoted.contains('\n') {
            return false;
        }

        let is_url = unquoted.starts_with("http://") || unquoted.starts_with("https://");
        let p = std::path::Path::new(unquoted);
        if is_url && cyber_agent::vision::is_image_extension(p) {
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("image.png");
            let id = self.attach_image(unquoted, name);
            self.status = format!("🖼️ 已记录图片，生成标识符 [image:{id}]");
            return true;
        }
        if cyber_agent::vision::is_image_extension(p) {
            let full = if p.is_absolute() {
                p.to_path_buf()
            } else {
                std::path::Path::new(&self.cwd).join(p)
            };
            if full.exists() && full.is_file() {
                let name = full
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("image.png");
                let id = self.attach_image(&full, name);
                self.status = format!("🖼️ 已记录图片，生成标识符 [image:{id}]");
                return true;
            }
        }
        false
    }

    fn handle_clipboard_image_only(&mut self) -> bool {
        if self
            .last_paste_instant
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(300))
        {
            return false;
        }
        let mut cb_res = arboard::Clipboard::new();
        for _ in 0..5 {
            if cb_res.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
            cb_res = arboard::Clipboard::new();
        }
        if let Ok(mut cb) = cb_res {
            if let Ok(img) = cb.get_image() {
                let width = img.width as u32;
                let height = img.height as u32;
                let png_bytes = crate::chat::encode_rgba_to_png(width, height, &img.bytes);

                let cache_dir = cyber_core::Paths::detect()
                    .map(|p| p.cyber_home)
                    .unwrap_or_else(|_| std::path::PathBuf::from("."))
                    .join("cache")
                    .join("images");
                let _ = std::fs::create_dir_all(&cache_dir);
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let id = self.next_image_id;
                let file_name = format!("clip_{}_{}.png", timestamp, id);
                let file_path = cache_dir.join(&file_name);

                if let Ok(()) = std::fs::write(&file_path, png_bytes) {
                    self.attach_image(&file_path, file_name);
                    self.status = format!("🖼️ 已粘贴图片，生成标识符 [image:{id}]");
                    self.last_paste_instant = Some(std::time::Instant::now());
                    return true;
                }
            }
        }
        false
    }

    fn handle_clipboard_image_or_text(&mut self) -> bool {
        if self
            .last_paste_instant
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(300))
        {
            return false;
        }
        if self.handle_clipboard_image_only() {
            return true;
        }
        let mut cb_res = arboard::Clipboard::new();
        for _ in 0..5 {
            if cb_res.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
            cb_res = arboard::Clipboard::new();
        }
        if let Ok(mut cb) = cb_res {
            if let Ok(text) = cb.get_text() {
                self.last_paste_instant = Some(std::time::Instant::now());
                if self.handle_text_image_paste(&text) {
                    return true;
                }
                self.insert_text(&text);
                return true;
            }
        }
        false
    }

    fn tool_width(&self) -> u16 {
        if self.history_view.width == 0 {
            80
        } else {
            self.history_view.width
        }
    }

    fn tool_call(&mut self, id: &str, name: &str, arguments: &str) {
        self.stream.clear();
        self.response_start = None;
        let start = self.messages.len();
        self.tools.push(ToolCard {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
            progress: String::new(),
            output: String::new(),
            state: ToolCardState::Pending,
            start,
            end: start,
        });
        self.replace_tool_card(self.tools.len() - 1, self.tool_width());
    }

    fn pending_tool_index(&self, id: &str) -> Option<usize> {
        self.tools
            .iter()
            .rposition(|tool| tool.id == id && tool.state == ToolCardState::Pending)
    }

    fn tool_progress(&mut self, id: &str, name: &str, chunk: &str) {
        let index = self.pending_tool_index(id).unwrap_or_else(|| {
            self.tool_call(id, name, "{}");
            self.tools.len() - 1
        });
        self.tools[index].name = name.to_owned();
        let cleaned = clean(chunk);
        if !self.tools[index].progress.is_empty() && !self.tools[index].progress.ends_with('\n') {
            self.tools[index].progress.push('\n');
        }
        self.tools[index].progress.push_str(&cleaned);
        self.replace_tool_card(index, self.tool_width());
    }

    fn tool_result(&mut self, id: &str, name: &str, output: &str, is_error: bool) {
        let index = self.pending_tool_index(id).unwrap_or_else(|| {
            self.tool_call(id, name, "{}");
            self.tools.len() - 1
        });
        let tool = &mut self.tools[index];
        tool.name = name.to_owned();
        tool.output = clean(output);
        tool.progress.clear();
        tool.state = if is_error {
            ToolCardState::Error
        } else {
            ToolCardState::Success
        };
        self.replace_tool_card(index, self.tool_width());
        self.needs_clear = true;
    }

    fn toggle_tools(&mut self) {
        self.tools_expanded = !self.tools_expanded;
        self.refresh_tools(self.tool_width());
    }

    fn replace_tool_card(&mut self, index: usize, width: u16) {
        let replacement = render_tool_card(&self.tools[index], self.tools_expanded, width, false);
        let start = self.tools[index].start;
        let end = self.tools[index].end;
        let delta = replacement.len() as isize - (end - start) as isize;
        self.messages.splice(start..end, replacement);
        self.tools[index].end = (end as isize + delta) as usize;
        for later in &mut self.tools[index + 1..] {
            later.start = (later.start as isize + delta) as usize;
            later.end = (later.end as isize + delta) as usize;
        }
        if let Some(response_start) = &mut self.response_start {
            if *response_start >= end {
                *response_start = (*response_start as isize + delta) as usize;
            }
        }
    }

    fn refresh_tools(&mut self, width: u16) {
        for index in (0..self.tools.len()).rev() {
            self.replace_tool_card(index, width);
        }
    }

    fn response_text(&mut self, thinking: bool, text: &str) {
        let text = clean(text);
        if text.is_empty() {
            return;
        }
        let mut index = self
            .stream
            .iter()
            .position(|segment| segment.thinking == thinking)
            .unwrap_or_else(|| {
                self.stream.push(ResponseSegment {
                    thinking,
                    text: String::new(),
                    lines: Vec::new(),
                });
                self.stream.len() - 1
            });
        // A leading-space-only segment has not appeared on screen yet. Its
        // first real content belongs after any already visible reasoning.
        if self.stream[index].text.trim().is_empty()
            && !text.trim().is_empty()
            && index + 1 < self.stream.len()
        {
            let segment = self.stream.remove(index);
            self.stream.push(segment);
            index = self.stream.len() - 1;
        }
        let segment = &mut self.stream[index];
        segment.text.push_str(&text);
        if segment.text.trim().is_empty() {
            return;
        }
        let theme = if thinking {
            Theme {
                fg: MUTED,
                title: MUTED,
                ..CLI_THEME
            }
        } else {
            CLI_THEME
        };
        // Reparse only the changed segment. The other segment keeps its styled lines.
        segment.lines = crate::markdown::render(&segment.text, &theme);
        for line in &mut segment.lines {
            let code_block = line
                .spans
                .first()
                .is_some_and(|span| span.content == "│ " && span.style.fg == Some(theme.title));
            if code_block && !thinking {
                line.style = line.style.bg(CODE_BG);
            }
            for (index, span) in line.spans.iter_mut().enumerate() {
                if thinking {
                    span.style = span.style.fg(MUTED).add_modifier(Modifier::ITALIC);
                } else if code_block {
                    span.style = span
                        .style
                        .fg(if index == 0 {
                            MUTED
                        } else {
                            Color::Rgb(156, 220, 254)
                        })
                        .bg(CODE_BG);
                } else if span.style.fg == Some(ACCENT)
                    && span.style.add_modifier.contains(Modifier::UNDERLINED)
                {
                    span.style = span.style.fg(LINK);
                } else if span.style.fg == Some(CODE) {
                    span.style = span.style.remove_modifier(Modifier::DIM);
                }
            }
        }
        let start = *self.response_start.get_or_insert_with(|| {
            self.messages.push(Line::default());
            if !self.assistant_label_shown {
                self.messages
                    .push(Line::styled("Cyber", Style::default().fg(DIM)));
                self.assistant_label_shown = true;
            }
            self.messages.len()
        });
        self.messages.truncate(start);
        for (index, segment) in self.stream.iter().enumerate() {
            if segment.text.trim().is_empty() {
                continue;
            }
            if index > 0 {
                self.messages.push(Line::default());
            }
            if segment.thinking {
                self.messages.push(Line::styled(
                    "Thinking",
                    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                ));
            }
            self.messages.extend(segment.lines.iter().cloned());
        }
    }

    fn turn_summary(
        &mut self,
        elapsed_ms: u64,
        finished_at: u64,
        status: &str,
        summary: Option<&str>,
    ) {
        self.stream.clear();
        self.has_run = true;
        self.response_start = None;
        self.assistant_label_shown = false;
        self.messages.push(Line::default());
        self.messages.push(Line::styled(
            crate::chat::turn_summary_text(elapsed_ms, finished_at, &clean(status)),
            Style::default().fg(DIM),
        ));
        if let Some(summary_text) = summary {
            if !summary_text.trim().is_empty() {
                self.messages.push(Line::styled(
                    format!("※summary：{summary_text}"),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ));
            }
        }
    }

    fn todo_summary(&self) -> String {
        let todos = self.todos.lock().map(|g| g.clone()).unwrap_or_default();
        if todos.is_empty() {
            return String::new();
        }
        let total = todos.len();
        let completed = todos
            .iter()
            .filter(|t| t.status == cyber_core::TodoStatus::Completed)
            .count();
        let active_desc = todos
            .iter()
            .find(|t| t.status == cyber_core::TodoStatus::InProgress)
            .or_else(|| {
                todos
                    .iter()
                    .find(|t| t.status == cyber_core::TodoStatus::Pending)
            })
            .map(|t| format!(" (▶ {})", t.title))
            .unwrap_or_default();
        format!("Todo: [{completed}/{total}]{active_desc}")
    }

    /// `/update`：发起（或退化执行）一次更新检查。
    fn begin_update_check(&mut self, kind: cli_commands::CliUpdate) {
        if self.update_checking {
            self.message("更新", "正在检查更新，请稍候…", MUTED);
            return;
        }
        if let (Some(tx), Ok(handle)) = (
            self.update_check_tx.clone(),
            tokio::runtime::Handle::try_current(),
        ) {
            self.update_checking = true;
            self.status = "正在检查更新…".into();
            handle.spawn(async move {
                let info = cyber_core::update::check_for_updates(true).await;
                let _ = tx.send(UpdateCheckResult { kind, info });
            });
            return;
        }
        // 未注入通道（单元测试 / mock 模式）或无 tokio 运行时：退化为本地缓存，不联网。
        let info = cyber_core::update::cached_latest_version();
        self.deliver_update(UpdateCheckResult { kind, info });
    }

    /// 检查结果落地：打印版本信息，并按 `kind` 进入确认态或直接开始安装。
    fn deliver_update(&mut self, result: UpdateCheckResult) {
        self.update_checking = false;
        let current = cyber_core::update::CURRENT_VERSION;
        match result.info {
            None => {
                self.status = "更新检查失败".into();
                self.message(
                    "更新检查",
                    &format!(
                        "⚠ 检查失败：无法连接版本源（GitHub / CNB 可能超时或不可达）。\n当前版本 V{current}；CNB 镜像：{}",
                        cyber_core::update::CNB_RELEASES_URL
                    ),
                    ERROR,
                );
            }
            Some(info) if cyber_core::update::is_newer(current, &info.version) => {
                self.new_version = Some(info.version.clone());
                // 关于页已打开时同步刷新「可更新版本」行（面板内看不到对话区的状态提示）。
                if let Some(about) = self.about.as_mut() {
                    about.latest = Some(info.version.clone());
                }
                let mut text = format!(
                    "✨ 发现新版本 V{}（当前 V{current}）\n发布页面：{}\nCNB 镜像：{}",
                    info.version,
                    info.html_url,
                    cyber_core::update::CNB_RELEASES_URL
                );
                if let Some(at) = info
                    .published_at
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                {
                    text.push_str(&format!("\n发布时间：{at}"));
                }
                if let Some(notes) = info
                    .release_notes
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                {
                    text.push_str("\n发布说明：");
                    let lines: Vec<&str> = notes.lines().collect();
                    for line in lines.iter().take(15) {
                        text.push_str(&format!("\n  {line}"));
                    }
                    if lines.len() > 15 {
                        text.push_str("\n  ...（更多更新说明见发布页面）");
                    }
                }
                match result.kind {
                    cli_commands::CliUpdate::Check => {
                        self.status =
                            format!("发现新版本 V{}（/update apply 可立即更新）", info.version);
                        self.message("更新检查", &text, ACCENT);
                    }
                    cli_commands::CliUpdate::Prompt => {
                        self.status = format!(
                            "发现新版本 V{}：按 y 用安装脚本更新 · 其它键取消",
                            info.version
                        );
                        self.message("更新检查", &text, ACCENT);
                        self.update_pending = Some(info);
                    }
                    cli_commands::CliUpdate::Apply => {
                        self.message("更新检查", &text, ACCENT);
                        self.start_update(&info.version);
                    }
                }
            }
            Some(info) => {
                if let Some(about) = self.about.as_mut() {
                    about.latest = None;
                }
                self.status.clear();
                self.message(
                    "更新检查",
                    &format!(
                        "✅ 当前已是最新版本 V{current}（版本源最新：V{}）",
                        info.version
                    ),
                    SUCCESS,
                );
            }
        }
    }

    /// 启动安装脚本。返回 `true` = 需要退出（Windows 上运行中的二进制无法被覆盖）。
    fn start_update(&mut self, version: &str) -> bool {
        self.update_pending = None;
        if self.busy {
            self.message(
                "更新",
                "回合进行中：请先 /cancel 或等回合结束后再更新",
                ERROR,
            );
            self.status.clear();
            return false;
        }
        let wait_for_exit = cyber_core::update::needs_exit_before_install();
        match (self.update_launcher)(version, wait_for_exit) {
            Ok(script) if wait_for_exit => {
                self.message(
                    "更新",
                    &format!(
                        "更新已安排：退出 cyber 后自动安装 V{version}（脚本：{}）。正在退出…",
                        script.display()
                    ),
                    ACCENT,
                );
                true
            }
            Ok(script) => {
                self.message(
                    "更新",
                    &format!(
                        "已开始后台安装 V{version}（脚本：{}）。安装完成后重启 cyber 生效。",
                        script.display()
                    ),
                    ACCENT,
                );
                self.status = format!("后台安装 V{version} 中 · 重启后生效");
                false
            }
            Err(error) => {
                self.message(
                    "更新",
                    &format!("无法启动安装脚本：{error}\n请在终端运行 `cyber update` 完成升级。"),
                    ERROR,
                );
                self.status.clear();
                false
            }
        }
    }
    fn ctf_challenges_list(&self) -> Vec<cyber_core::CtfChallenge> {
        self.ctf_challenges
            .lock()
            .map(|list| list.clone())
            .unwrap_or_default()
    }

    fn ctf_challenges_count(&self) -> usize {
        self.ctf_challenges
            .lock()
            .map(|list| list.len())
            .unwrap_or(0)
    }

    pub(crate) fn ctf_challenge_at(&self, idx: usize) -> Option<cyber_core::CtfChallenge> {
        self.ctf_challenges
            .lock()
            .ok()
            .and_then(|list| list.get(idx).cloned())
    }

    pub(crate) fn ctf_remove_selected(&mut self) -> Option<String> {
        let removed_name = self.ctf_challenges.lock().ok().and_then(|mut list| {
            if self.ctf_selected < list.len() {
                let name = list[self.ctf_selected].name.clone();
                list.remove(self.ctf_selected);
                Some(name)
            } else {
                None
            }
        });
        if removed_name.is_some() {
            let len = self.ctf_challenges_count();
            if self.ctf_selected >= len && len > 0 {
                self.ctf_selected = len - 1;
            }
        }
        removed_name
    }

    pub(crate) fn ctf_toggle_status_selected(&mut self) -> Option<(String, bool)> {
        self.ctf_challenges.lock().ok().and_then(|mut list| {
            let c = list.get_mut(self.ctf_selected)?;
            let was_solved = c.is_solved();
            c.status = if was_solved {
                cyber_core::CtfStatus::InProgress
            } else {
                cyber_core::CtfStatus::Solved
            };
            if !was_solved {
                c.end_time = Some(cyber_core::current_time_str());
            } else {
                c.end_time = None;
            }
            Some((c.name.clone(), c.is_solved()))
        })
    }

    pub(crate) fn ctf_toggle_global_selected(&mut self) -> Option<(String, bool)> {
        self.ctf_challenges.lock().ok().and_then(|mut list| {
            let c = list.get_mut(self.ctf_selected)?;
            c.is_global = !c.is_global;
            Some((c.name.clone(), c.is_global))
        })
    }

    pub(crate) fn ctf_set_all_global(&mut self) -> usize {
        self.ctf_challenges
            .lock()
            .ok()
            .map(|mut list| {
                let mut count = 0;
                for c in list.iter_mut() {
                    if !c.is_global {
                        c.is_global = true;
                        count += 1;
                    }
                }
                count
            })
            .unwrap_or(0)
    }

    pub(crate) fn ctf_update_selected(
        &mut self,
        updated: cyber_core::CtfChallenge,
    ) -> Option<String> {
        let name = updated.name.clone();
        let success = self
            .ctf_challenges
            .lock()
            .map(|mut list| {
                let mut done = false;
                for c in list.iter_mut() {
                    if c.id == updated.id {
                        *c = updated.clone();
                        done = true;
                        break;
                    }
                }
                done
            })
            .unwrap_or(false);
        if success {
            Some(name)
        } else {
            None
        }
    }

    #[allow(dead_code)]
    fn footer(&self) -> String {
        let todo_sum = self.todo_summary();
        if !self.has_run {
            if self.approval.is_some() {
                return "  permission required · 1/2/3 select · Enter confirm · Esc deny".into();
            } else if !todo_sum.is_empty() {
                if !self.status.is_empty() {
                    return format!("  {todo_sum} │ {}", clean(&self.status));
                }
                return format!("  {todo_sum}");
            } else if !self.status.is_empty() {
                return format!("  {}", clean(&self.status));
            } else {
                return String::new();
            }
        }
        let context = self
            .context_length
            .filter(|length| *length > 0)
            .map(|length| {
                let length = u128::from(length);
                format!(
                    "{}%",
                    (length.saturating_sub(self.used_tokens as u128) * 100) / length
                )
            })
            .unwrap_or_else(|| "--".into());
        let cache_total = self.cache_hit_tokens.saturating_add(self.cache_miss_tokens);
        let cache = if cache_total > 0 {
            format!(
                "{:.1}%",
                self.cache_hit_tokens as f64 / cache_total as f64 * 100.0
            )
        } else {
            "--".into()
        };
        let (input, output) = if self.usage_reported {
            (
                token_count(self.prompt_tokens),
                token_count(self.completion_tokens),
            )
        } else {
            ("--".into(), "--".into())
        };
        let mode_label = self.permission_mode.label();
        let mut footer = format!(
            "  {} · {} │ ctx {context} │ cache {cache} │ ↑{input} ↓{output} │ 模式: {mode_label} (F2)",
            clean(&self.provider),
            clean(&self.model)
        );
        if !todo_sum.is_empty() {
            footer.push_str(&format!(" │ {todo_sum}"));
        }
        if self.approval.is_some() {
            footer.push_str(" · permission required · 1/2/3 select · Enter confirm · Esc deny");
            footer.push_str(&format!(" · {} · Ctrl+C to cancel", clean(&self.status)));
        } else if !self.status.is_empty() {
            footer.push_str(&format!(" · {}", clean(&self.status)));
        }
        footer
    }

    fn footer_status_style(&self) -> Style {
        let s = self.status.as_str();
        if s.contains("Cancelling")
            || s.contains("失败")
            || s.contains("Error")
            || s.contains("Cannot")
        {
            Style::default().fg(ERROR).add_modifier(Modifier::BOLD)
        } else if s == "Working"
            || s == "Thinking"
            || s == "Responding"
            || s.starts_with("Tool")
            || s.starts_with("Running")
            || s.starts_with("Compacting")
            || s.contains("重试")
        {
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD)
        } else {
            // 操作反馈提示（如 Model selected, CTF mode updated, Vision engine enabled 等）：
            // 采用鲜明的高亮电光青色 (RGB 80, 240, 220) 配合粗体，形成极高对比度，清晰醒目！
            Style::default()
                .fg(Color::Rgb(80, 240, 220))
                .add_modifier(Modifier::BOLD)
        }
    }

    fn footer_line(&self) -> Line<'static> {
        let todo_sum = self.todo_summary();
        let status_style = self.footer_status_style();

        if !self.has_run {
            if self.approval.is_some() {
                return Line::from(vec![Span::styled(
                    "  permission required · 1/2/3 select · Enter confirm · Esc deny",
                    Style::default().fg(DIM),
                )]);
            } else if !todo_sum.is_empty() {
                let mut spans = vec![Span::styled(
                    format!("  {todo_sum}"),
                    Style::default().fg(DIM),
                )];
                if !self.status.is_empty() {
                    spans.push(Span::styled(" │ ", Style::default().fg(DIM)));
                    spans.push(Span::styled(clean(&self.status), status_style));
                }
                if !self.auto_scroll && self.scroll > 0 {
                    spans.push(Span::styled(
                        " · [↑自动滚动已暂停 · 按 End 恢复]",
                        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                    ));
                }
                return Line::from(spans);
            } else if !self.status.is_empty() {
                let mut spans = vec![
                    Span::styled("  ", Style::default().fg(DIM)),
                    Span::styled(clean(&self.status), status_style),
                ];
                if !self.auto_scroll && self.scroll > 0 {
                    spans.push(Span::styled(
                        " · [↑自动滚动已暂停 · 按 End 恢复]",
                        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                    ));
                }
                return Line::from(spans);
            } else if !self.auto_scroll && self.scroll > 0 {
                return Line::from(vec![
                    Span::styled("  ", Style::default().fg(DIM)),
                    Span::styled(
                        "[↑自动滚动已暂停 · 按 End 恢复]",
                        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                    ),
                ]);
            } else {
                return Line::default();
            }
        }

        let context = self
            .context_length
            .filter(|length| *length > 0)
            .map(|length| {
                let length = u128::from(length);
                format!(
                    "{}%",
                    (length.saturating_sub(self.used_tokens as u128) * 100) / length
                )
            })
            .unwrap_or_else(|| "--".into());
        let cache_total = self.cache_hit_tokens.saturating_add(self.cache_miss_tokens);
        let cache = if cache_total > 0 {
            format!(
                "{:.1}%",
                self.cache_hit_tokens as f64 / cache_total as f64 * 100.0
            )
        } else {
            "--".into()
        };
        let (input, output) = if self.usage_reported {
            (
                token_count(self.prompt_tokens),
                token_count(self.completion_tokens),
            )
        } else {
            ("--".into(), "--".into())
        };
        let mode_label = self.permission_mode.label();

        let mut spans = vec![Span::styled(
            format!(
                "  {} · {} │ ctx {context} │ cache {cache} │ ↑{input} ↓{output} │ 模式: {mode_label} (F2)",
                clean(&self.provider),
                clean(&self.model)
            ),
            Style::default().fg(DIM),
        )];

        if !todo_sum.is_empty() {
            spans.push(Span::styled(
                format!(" │ {todo_sum}"),
                Style::default().fg(DIM),
            ));
        }

        if self.approval.is_some() {
            spans.push(Span::styled(
                " · permission required · 1/2/3 select · Enter confirm · Esc deny",
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!(" · {} · Ctrl+C to cancel", clean(&self.status)),
                status_style,
            ));
        } else if !self.status.is_empty() {
            spans.push(Span::styled(" · ", Style::default().fg(DIM)));
            spans.push(Span::styled(clean(&self.status), status_style));
        }

        if !self.auto_scroll && self.scroll > 0 {
            spans.push(Span::styled(
                " · [↑自动滚动已暂停 · 按 End 恢复]",
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ));
        }

        Line::from(spans)
    }
    fn append_ctf_mode_badge(&self, line: Line<'static>, width: usize) -> Line<'static> {
        let badge_text = "CTF model";
        let badge_style = Style::default()
            .fg(Color::Rgb(254, 188, 56))
            .add_modifier(Modifier::BOLD);
        let line_w = line.width();
        let badge_w = 9;
        let mut spans = line.spans;
        if width > line_w + badge_w + 3 {
            let pad = width - line_w - badge_w - 3;
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(badge_text, badge_style));
            spans.push(Span::raw("   "));
        } else if width > line_w + badge_w {
            let pad = width.saturating_sub(line_w + badge_w);
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(badge_text, badge_style));
        } else {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(badge_text, badge_style));
        }
        Line::from(spans)
    }

    fn event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Token(token) => {
                self.has_run = true;
                self.response_text(false, &token);
                self.status = "Responding".into();
            }
            AgentEvent::Reasoning(token) => {
                self.has_run = true;
                if self.thinking_started.is_none() {
                    self.thinking_started = Some(std::time::Instant::now());
                }
                self.response_text(true, &token);
                self.status = "Thinking".into();
            }
            AgentEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                self.has_run = true;
                self.tool_call(&id, &name, &arguments);
                self.status = format!("Tool · {name}");
            }
            AgentEvent::ToolProgress { id, name, chunk } => {
                self.has_run = true;
                self.tool_progress(&id, &name, &chunk);
                self.status = format!("Running · {name}");
            }
            AgentEvent::ToolResult {
                id,
                name,
                output,
                is_error,
            } => {
                self.has_run = true;
                if name.eq_ignore_ascii_case("todo") && !is_error {
                    self.todo_view = TodoView::Summary;
                }
                self.tool_result(&id, &name, &output, is_error);
            }
            AgentEvent::ContextUpdate {
                used_tokens,
                effective_context_length,
            } => {
                self.used_tokens = used_tokens;
                self.context_length = effective_context_length;
            }
            AgentEvent::Usage(usage) => {
                self.has_run = true;
                self.usage_reported = true;
                self.prompt_tokens = self
                    .prompt_tokens
                    .saturating_add(u128::from(usage.prompt_tokens));
                self.completion_tokens = self
                    .completion_tokens
                    .saturating_add(u128::from(usage.completion_tokens));
                self.cache_hit_tokens = self
                    .cache_hit_tokens
                    .saturating_add(u128::from(usage.cache_hit_tokens));
                self.cache_miss_tokens = self
                    .cache_miss_tokens
                    .saturating_add(u128::from(usage.cache_miss_tokens));
            }
            AgentEvent::Compacting { .. } => {
                self.has_run = true;
                self.status = "Compacting context".into();
            }
            AgentEvent::SteeringReceived(text) => {
                self.queued_prompts.retain(|q| q.text != text);
                self.status = "已读取追加指示".into();
            }
            AgentEvent::Done => {
                self.busy = false;
                self.thinking_started = None;
            }
            AgentEvent::Notice(text) => {
                self.has_run = true;
                self.message("Notice", &text, AMBER);
            }
            AgentEvent::Error(error) => {
                self.has_run = true;
                self.busy = false;
                self.thinking_started = None;
                self.message("Error", &error, ERROR);
            }
            AgentEvent::Retry {
                attempt,
                max_retries,
                delay_secs,
                error,
            } => {
                self.has_run = true;
                if let Some(start) = self.response_start {
                    let rollback_to =
                        start.saturating_sub(if self.assistant_label_shown { 2 } else { 0 });
                    self.messages.truncate(rollback_to);
                    self.stream.clear();
                    self.response_start = None;
                    self.assistant_label_shown = false;
                }
                self.thinking_started = None;
                self.status = format!(
                    "连接中断，正在重试 ({attempt}/{max_retries}) · {delay_secs}s 后重试: {}",
                    clean(&error)
                );
            }
            _ => {}
        }
    }

    fn reply(&mut self, decision: PermissionDecision) {
        if let Some(request) = self.approval.take() {
            let _ = request.reply.send(decision);
        }
        self.approval_choice = None;
        self.approval_input = composer();
    }

    fn select_approval(&mut self, choice: ApprovalChoice) {
        self.approval_choice = Some(choice);
        self.approval_input = composer();
        self.approval_input.insert_str(choice.label());
    }

    pub(crate) fn apply_scrollbar_click(&mut self, row: u16) {
        if self.approval.is_some()
            || self.panel.is_some()
            || self.form.is_some()
            || self.settings.is_some()
        {
            return;
        }
        let area = self.history_area;
        if area.height == 0 {
            return;
        }
        let track_h = area.height.saturating_sub(1).max(1) as f64;
        let rel_y = (row.saturating_sub(area.y) as f64).clamp(0.0, track_h);
        let progress = rel_y / track_h;

        if let Some(view) = self.subagent_view.as_mut() {
            if view.max_scroll > 0 {
                let target_pos = (progress * view.max_scroll as f64).round() as usize;
                view.scroll = view.max_scroll.saturating_sub(target_pos);
                view.follow_bottom = view.scroll == 0;
            }
        } else if self.max_scroll > 0 {
            let target_pos = (progress * self.max_scroll as f64).round() as usize;
            self.scroll = self.max_scroll.saturating_sub(target_pos);
            self.auto_scroll = self.scroll == 0;
        }
    }

    #[allow(dead_code)]
    pub fn has_selection(&self) -> bool {
        self.selection.as_ref().is_some_and(|s| !s.is_empty())
    }

    #[allow(dead_code)]
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    #[allow(dead_code)]
    pub fn copy_selection_to_clipboard(&mut self) -> bool {
        if let Some(sel) = self.selection.as_ref() {
            if !sel.is_empty() {
                let lines: &[Line<'static>] = if let Some(view) = self.subagent_view.as_ref() {
                    &view.viewport.source
                } else {
                    &self.messages
                };
                let text = crate::selection::extract_text(lines, sel);
                if !text.is_empty() {
                    let ok = crate::selection::set_clipboard_text(&text);
                    self.status = "✔ 已选中文本并复制到剪贴板".into();
                    return ok;
                }
            }
        }
        false
    }

    #[allow(dead_code)]
    pub fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> bool {
        if self.panel.is_some()
            || self.settings.is_some()
            || self.approval.is_some()
            || self.form.is_some()
            || self.picker.is_some()
        {
            return false;
        }
        if let Some(q_state) = self.question_state.as_mut() {
            if mouse.row >= self.question_area.top() && mouse.row < self.question_area.bottom() {
                match q_state.handle_mouse(mouse, self.question_area) {
                    crate::question_ui::QuestionUiResult::Continue => return true,
                    crate::question_ui::QuestionUiResult::Submit(resp) => {
                        if let Some(q) = self.question_state.take() {
                            let _ = q.request.reply.send(resp);
                        }
                        return true;
                    }
                    crate::question_ui::QuestionUiResult::Cancel => {
                        if let Some(q) = self.question_state.take() {
                            let _ = q.request.reply.send(cyber_agent::QuestionResponse {
                                answers: Vec::new(),
                                cancelled: true,
                            });
                        }
                        return true;
                    }
                }
            }
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if mouse.row >= self.history_area.top()
                    && mouse.row < self.history_area.bottom()
                    && mouse.column >= self.history_area.right().saturating_sub(2)
                    && mouse.column < self.history_area.right()
                {
                    self.is_dragging_scrollbar = true;
                    self.apply_scrollbar_click(mouse.row);
                    return true;
                }

                let in_history = mouse.row >= self.history_area.top()
                    && mouse.row < self.history_area.bottom()
                    && mouse.column >= self.history_area.left()
                    && mouse.column < self.history_area.right();

                let is_shift = mouse.modifiers.contains(KeyModifiers::SHIFT);

                if in_history {
                    let coord_opt = if let Some(view) = self.subagent_view.as_ref() {
                        let start = view.max_scroll
                            - if view.follow_bottom {
                                0
                            } else {
                                view.scroll.min(view.max_scroll)
                            };
                        let rel_row = (mouse.row - self.history_area.y) as usize;
                        let rel_col = mouse.column.saturating_sub(self.history_area.x);
                        let viewport_row =
                            (start + rel_row).min(view.viewport.rows.len().saturating_sub(1));
                        view.viewport.coord_from_viewport(viewport_row, rel_col)
                    } else {
                        let start = self.max_scroll - self.scroll;
                        let rel_row = (mouse.row - self.history_area.y) as usize;
                        let rel_col = mouse.column.saturating_sub(self.history_area.x);
                        let viewport_row =
                            (start + rel_row).min(self.history_view.rows.len().saturating_sub(1));
                        self.history_view.coord_from_viewport(viewport_row, rel_col)
                    };

                    if let Some(coord) = coord_opt {
                        if is_shift {
                            if let Some(mut sel) = self.selection.take() {
                                sel.cursor = coord;
                                sel.selecting = true;
                                let lines: &[Line<'static>] =
                                    if let Some(view) = self.subagent_view.as_ref() {
                                        &view.viewport.source
                                    } else {
                                        &self.messages
                                    };
                                let text = crate::selection::extract_text(lines, &sel);
                                if !text.is_empty() {
                                    crate::selection::set_clipboard_text(&text);
                                    self.status = "✔ 已扩展选区并复制到剪贴板".into();
                                }
                                self.selection = Some(sel);
                            } else {
                                self.selection = Some(TextSelection::new(coord));
                                self.auto_scroll = false;
                            }
                        } else {
                            self.selection = Some(TextSelection::new(coord));
                            self.auto_scroll = false;
                        }
                        return true;
                    }
                } else {
                    self.clear_selection();
                    return true;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.is_dragging_scrollbar {
                    self.apply_scrollbar_click(mouse.row);
                    return true;
                }

                if let Some(mut sel) = self.selection.take() {
                    if sel.selecting {
                        if mouse.row < self.history_area.y {
                            let delta = (self.history_area.y.saturating_sub(mouse.row)) as usize;
                            let speed = delta.clamp(1, 10);
                            if let Some(view) = self.subagent_view.as_mut() {
                                view.follow_bottom = false;
                                view.scroll = (view.scroll + speed).min(view.max_scroll);
                                let start = view.max_scroll - view.scroll;
                                if let Some(coord) = view.viewport.coord_from_viewport(start, 0) {
                                    sel.cursor = coord;
                                }
                            } else {
                                self.auto_scroll = false;
                                self.scroll =
                                    self.scroll.saturating_add(speed).min(self.max_scroll);
                                let viewport_row = self.max_scroll - self.scroll;
                                if let Some(coord) =
                                    self.history_view.coord_from_viewport(viewport_row, 0)
                                {
                                    sel.cursor = coord;
                                }
                            }
                        } else if mouse.row >= self.history_area.y + self.history_area.height {
                            let delta = (mouse
                                .row
                                .saturating_sub(self.history_area.y + self.history_area.height)
                                + 1) as usize;
                            let speed = delta.clamp(1, 10);
                            if let Some(view) = self.subagent_view.as_mut() {
                                view.scroll = view.scroll.saturating_sub(speed);
                                if view.scroll == 0 {
                                    view.follow_bottom = true;
                                }
                                let start = view.max_scroll
                                    - if view.follow_bottom { 0 } else { view.scroll };
                                let bottom_row = (start + self.history_area.height as usize)
                                    .saturating_sub(1)
                                    .min(view.viewport.rows.len().saturating_sub(1));
                                if let Some(coord) =
                                    view.viewport.coord_from_viewport(bottom_row, u16::MAX)
                                {
                                    sel.cursor = coord;
                                }
                            } else {
                                self.scroll = self.scroll.saturating_sub(speed);
                                if self.scroll == 0 {
                                    self.auto_scroll = true;
                                }
                                let start = self.max_scroll - self.scroll;
                                let bottom_row = (start + self.history_area.height as usize)
                                    .saturating_sub(1)
                                    .min(self.history_view.rows.len().saturating_sub(1));
                                if let Some(coord) =
                                    self.history_view.coord_from_viewport(bottom_row, u16::MAX)
                                {
                                    sel.cursor = coord;
                                }
                            }
                        } else {
                            let rel_row = (mouse.row - self.history_area.y) as usize;
                            let rel_col = if mouse.column < self.history_area.x {
                                0
                            } else if mouse.column >= self.history_area.right() {
                                u16::MAX
                            } else {
                                mouse.column - self.history_area.x
                            };
                            if let Some(view) = self.subagent_view.as_ref() {
                                let start = view.max_scroll
                                    - if view.follow_bottom {
                                        0
                                    } else {
                                        view.scroll.min(view.max_scroll)
                                    };
                                let viewport_row = (start + rel_row)
                                    .min(view.viewport.rows.len().saturating_sub(1));
                                if let Some(coord) =
                                    view.viewport.coord_from_viewport(viewport_row, rel_col)
                                {
                                    sel.cursor = coord;
                                }
                            } else {
                                let start = self.max_scroll - self.scroll;
                                let viewport_row = (start + rel_row)
                                    .min(self.history_view.rows.len().saturating_sub(1));
                                if let Some(coord) =
                                    self.history_view.coord_from_viewport(viewport_row, rel_col)
                                {
                                    sel.cursor = coord;
                                }
                            }
                        }
                        self.selection = Some(sel);
                        return true;
                    }
                    self.selection = Some(sel);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.is_dragging_scrollbar {
                    self.is_dragging_scrollbar = false;
                    return true;
                }

                if let Some(mut sel) = self.selection.take() {
                    // Preserve selecting flag if Shift is held, otherwise clear it
                    if !mouse.modifiers.contains(KeyModifiers::SHIFT) {
                        sel.selecting = false;
                    }
                    if !sel.is_empty() {
                        let lines: &[Line<'static>] =
                            if let Some(view) = self.subagent_view.as_ref() {
                                &view.viewport.source
                            } else {
                                &self.messages
                            };
                        let text = crate::selection::extract_text(lines, &sel);
                        if !text.is_empty() {
                            crate::selection::set_clipboard_text(&text);
                            self.status = "✔ 已选中文本并复制到剪贴板".into();
                        }
                    }
                    self.selection = Some(sel);
                    return true;
                }
            }
            MouseEventKind::Down(MouseButton::Right) if self.has_selection() => {
                self.copy_selection_to_clipboard();
                return true;
            }
            _ => {}
        }

        false
    }

    /// 设置面板下的鼠标滚轮：语义等同于按 ↑（`up=true`）/ ↓ 键。
    ///
    /// 依次处理：丢弃确认弹层 → 技能详情弹窗滚屏 → 记忆详情弹窗滚屏 → 主列表光标移动。
    /// 返回 `true` 表示事件已被设置面板消费（调用方不应再退回聊天区滚动）。
    fn settings_scroll(&mut self, up: bool) -> bool {
        if self
            .settings
            .as_ref()
            .is_some_and(|s| s.tab == SettingsTab::About)
        {
            if up {
                self.about_scroll = self.about_scroll.saturating_sub(1);
            } else {
                self.about_scroll = self.about_scroll.saturating_add(1);
            }
            return true;
        }
        let Some(settings) = self.settings.as_mut() else {
            return false;
        };
        if settings.pending_discard_confirm {
            settings.pending_discard_confirm = false;
            return true;
        }
        if let Some(detail) = settings.skill_detail.as_mut() {
            if up {
                detail.scroll = detail.scroll.saturating_sub(1);
            } else {
                detail.scroll = detail.scroll.saturating_add(1);
            }
            return true;
        }
        if let Some(detail) = settings.memory_detail.as_mut() {
            if up {
                detail.scroll = detail.scroll.saturating_sub(1);
            } else {
                detail.scroll = detail.scroll.saturating_add(1);
            }
            return true;
        }
        let max_row = settings.tab.max_row(settings);
        settings.editing = false;
        if up {
            settings.selected_row = settings.selected_row.saturating_sub(1);
        } else {
            settings.selected_row = (settings.selected_row + 1).min(max_row);
        }
        true
    }

    /// 页签切到「关于」时刷新内容快照并复位滚动（其它页签不动）。
    fn sync_about_if_active(&mut self) {
        if self
            .settings
            .as_ref()
            .is_some_and(|s| s.tab == SettingsTab::About)
        {
            self.about_scroll = 0;
            let info = AboutInfo::collect(self);
            self.about = Some(info);
        }
    }

    /// 是否为「由设置面板打开的弹层」（自定义工具 / 环境变量 / 记忆规则 / 工具库扫描表单、
    /// Models 模型选择器）。
    ///
    /// 这类弹层打开时 `panel` 仍为 `None`（`handle_key` 的按键路由、Esc/保存后的返回语义
    /// 都保持不变），但绘制时下层背景必须是设置中心，而不是对话界面。
    fn settings_opened_dialog(&self) -> bool {
        if self.panel.is_some() || self.settings.is_none() || self.settings_return_tab.is_none() {
            return false;
        }
        match (&self.form, &self.picker) {
            // Provider 表单与 Provider/Protocol/Preset 向导选择器由 `draw()` 更早的全屏分支
            // 接管（cli.rs 约 2764–2783），永远走不到这里；其余表单只要是从设置中心打开的
            // （`settings_return_tab` 非空）就必须以设置中心为背景。
            (Some(form), _) => !matches!(form.form.kind, FormKind::Provider { .. }),
            (None, Some(picker)) => picker.title.starts_with("Models ("),
            _ => false,
        }
    }

    /// 模型选择面板：左栏服务商变化（打开面板 / 切换选中）后刷新右栏模型列表。
    ///
    /// 先用本地配置立即刷新（离线可用），再异步从接口拉取该服务商的模型列表
    /// （结果经 `model_fetch_tx` 回传；未注入通道时——如单元测试——只做本地刷新）。
    /// 只刷新右栏的本地配置清单（不联网）。用于打开面板与左栏切换 provider。
    fn refresh_model_picker_local(&mut self) {
        if let Some(state) = self.model_picker.as_mut() {
            state.refresh_models();
        }
    }

    /// 从接口拉取当前 provider 的模型列表（联网）。仅在左栏 `Enter`（选定 provider）或 `r` 时调用。
    fn fetch_model_picker_models(&mut self) {
        let tx = self.model_fetch_tx.clone();
        let Some(state) = self.model_picker.as_mut() else {
            return;
        };
        state.refresh_models();
        if let Some(tx) = tx {
            state.begin_fetch(&tx);
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let win_title = crate::history::terminal_window_title(
            self.busy,
            self.thinking_started,
            &self.session_title,
            &self.model,
        );
        if win_title != self.last_window_title {
            let _ = execute!(io::stdout(), SetTitle(&win_title));
            self.last_window_title = win_title;
        }
        let area = frame.area();
        if area.width < 4 || area.height < 4 {
            return;
        }
        if self.approval.is_none() {
            if let Some(form) = &mut self.form {
                if matches!(form.form.kind, FormKind::Provider { .. }) {
                    self.history_area = area;
                    draw_fullscreen_provider_form(frame, area, form, &self.status);
                    return;
                }
            }
            if let Some(picker) = &self.picker {
                if picker.title.contains("Provider")
                    || picker.title.contains("Protocol")
                    || picker.title.contains("Preset")
                {
                    self.history_area = area;
                    draw_fullscreen_provider_picker(frame, area, picker, self.picker_selected);
                    return;
                }
            }
            // 由设置面板打开的弹层：面板状态仍是 None，但背景绘制为设置中心，而不是对话界面。
            if self.settings_opened_dialog() {
                self.history_area = area;
                if let Some(settings) = &self.settings {
                    draw_settings_panel(frame, area, settings, self);
                }
                if let Some(form) = &self.form {
                    draw_form_dialog(frame, area, form, &self.status);
                } else if let Some(picker) = &self.picker {
                    draw_picker_dialog(
                        frame,
                        area,
                        picker,
                        self.picker_selected,
                        self.delete_pending.is_some(),
                        &self.status,
                    );
                }
                return;
            }
            if let Some(panel) = self.panel {
                if panel.is_fullscreen() {
                    self.history_area = area;
                    match panel {
                        Panel::Settings => {
                            if let Some(settings) = &self.settings {
                                draw_settings_panel(frame, area, settings, self);
                            }
                        }
                        Panel::Mcp => {
                            if let Some(mcp_state) = self.mcp_panel.as_mut() {
                                crate::views::mcp_panel::render_mcp_panel(
                                    frame, area, mcp_state, &CLI_THEME,
                                );
                            }
                        }
                        Panel::ModelPicker => {
                            let active_provider = self.provider.clone();
                            if let Some(state) = self.model_picker.as_mut() {
                                draw_model_picker(frame, area, state, &active_provider);
                            }
                        }
                        Panel::About => {
                            if self.about.is_none() {
                                self.about = Some(AboutInfo::collect(self));
                            }
                            if let Some(info) = &self.about {
                                draw_about_panel(frame, area, info, self.about_scroll);
                            }
                        }
                        _ => {}
                    }
                    return;
                }
            }
        }
        if !self.tools_expanded && self.history_view.width != area.width {
            self.refresh_tools(area.width);
        }
        let approval_width = area.width.saturating_sub(6).clamp(36, 76);
        let approval_controls = self.approval.as_ref().map(|request| {
            let button = |num: &'static str,
                          label: &'static str,
                          choice: ApprovalChoice,
                          base_color: Color| {
                let selected = self.approval_choice == Some(choice);
                let (bg_color, fg_color, num_fg) = if selected {
                    (base_color, Color::Rgb(18, 18, 22), Color::Rgb(18, 18, 22))
                } else {
                    (
                        Color::Rgb(36, 36, 44),
                        Color::Rgb(210, 210, 220),
                        base_color,
                    )
                };
                vec![
                    Span::styled(
                        format!(" [{num} "),
                        Style::default()
                            .fg(num_fg)
                            .bg(bg_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{label}] "),
                        Style::default()
                            .fg(fg_color)
                            .bg(bg_color)
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                ]
            };
            let mut button_spans = Vec::new();
            button_spans.extend(button("1", "Allow once", ApprovalChoice::Once, SUCCESS));
            button_spans.push(Span::raw(" "));
            button_spans.extend(button("2", "Session", ApprovalChoice::Session, ACCENT));
            button_spans.push(Span::raw(" "));
            button_spans.extend(button("3", "Deny", ApprovalChoice::Deny, ERROR));

            let mut tool_header_spans = vec![
                Span::styled(
                    " TOOL ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(AMBER)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    clean(&request.tool),
                    Style::default().fg(FG).add_modifier(Modifier::BOLD),
                ),
            ];
            if !request.risk_reason.is_empty() {
                tool_header_spans.push(Span::raw("  "));
                let conf_pct = (request.confidence * 100.0).clamp(0.0, 100.0);
                let badge_color = if request.confidence >= 0.70 {
                    SUCCESS
                } else if request.confidence >= 0.40 {
                    AMBER
                } else {
                    ERROR
                };
                tool_header_spans.push(Span::styled(
                    format!(
                        "[安全置信度: {conf_pct:.0}%] {}",
                        clean(&request.risk_reason)
                    ),
                    Style::default().fg(badge_color),
                ));
            }

            vec![
                Line::from(tool_header_spans),
                Line::default(),
                Line::from(button_spans),
                Line::styled(
                    self.approval_choice.map_or(
                        " Select with [1 / 2 / 3] or Left/Right/Tab, then press Enter.",
                        |choice| match choice {
                            ApprovalChoice::Once => {
                                " > Allow this single tool call once. Press Enter to confirm."
                            }
                            ApprovalChoice::Session => {
                                " > Allow identical arguments for the entire session. Press Enter."
                            }
                            ApprovalChoice::Deny => {
                                " > Deny this tool call. Press Enter or Esc to confirm."
                            }
                        },
                    ),
                    Style::default().fg(if self.approval_choice.is_some() {
                        FG
                    } else {
                        MUTED
                    }),
                ),
                Line::default(),
                Line::from(vec![
                    Span::styled(
                        " ARGUMENTS ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(CODE)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  PgUp/PgDn scroll", Style::default().fg(DIM)),
                ]),
            ]
        });
        let controls_height = approval_controls.as_ref().map_or(0, |lines| {
            Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false })
                .line_count(approval_width.saturating_sub(4).max(1))
        });
        self.approval_visible = false;
        let active_input = if self.approval.is_some() {
            &self.approval_input
        } else if let Some(input) = self
            .form
            .as_ref()
            .and_then(|form| form.inputs.get(form.selected))
        {
            input
        } else {
            &self.input
        };
        let input_height = (active_input.lines().len().clamp(1, 5) as u16)
            .min(area.height.saturating_sub(2).max(1));
        let normal_header = if area.height >= 16 && area.width >= 38 {
            6
        } else {
            area.height.saturating_sub(3).min(3)
        };
        let header_height = if self.approval.is_some() {
            normal_header.min(area.height.saturating_sub(
                (controls_height + 3 + input_height as usize + 2).min(u16::MAX as usize) as u16,
            ))
        } else {
            normal_header
        };
        let todo_items = self.todos.lock().map(|g| g.clone()).unwrap_or_default();
        let has_todos = !todo_items.is_empty();
        let show_todo_table = has_todos && self.todo_view != TodoView::Collapsed;
        let show_todo_strip = has_todos && self.todo_view == TodoView::Collapsed;
        let question_height = self
            .question_state
            .as_ref()
            .map_or(0, |q| q.height_needed());
        // 其它区域（头部/对话/提问/输入/状态栏）至少留 4 行，超出部分不作为 todo 高度。
        let todo_budget = area
            .height
            .saturating_sub(header_height + question_height + input_height + 4);
        let todo_height = if show_todo_table {
            let desired = match self.todo_view {
                TodoView::Summary => (todo_items.len() as u16 + 2).clamp(3, 7),
                TodoView::Full => (todo_items.len() as u16).saturating_add(2),
                TodoView::Collapsed => 0,
            };
            desired.min(todo_budget)
        } else if show_todo_strip {
            1.min(todo_budget)
        } else {
            0
        };
        let sections = Layout::vertical([
            Constraint::Length(header_height),
            Constraint::Min(0),
            Constraint::Length(todo_height),
            Constraint::Length(question_height),
            Constraint::Length(input_height + 1),
            Constraint::Length(1),
        ])
        .split(area);
        self.history_area = sections[1];
        self.question_area = sections[3];
        // 子代理覆盖视图：panel/approval 打开时隐藏但不丢状态；run 从快照消失
        // 时防御性退出（归档条目会话级不回删，属纯防御）。
        let mut view_run: Option<SubagentRun> = None;
        let mut view_lost = false;
        if let Some(view) = &self.subagent_view {
            if self.panel.is_none() && self.approval.is_none() {
                view_run = self
                    .subagents
                    .snapshot()
                    .into_iter()
                    .find(|run| run.id == view.run_id);
                view_lost = view_run.is_none();
            }
        }
        if view_lost {
            self.subagent_view = None;
        }
        if let Some(run) = view_run.as_ref() {
            self.draw_subagent_view_header(frame, sections[0], run);
        } else {
            self.draw_header(frame, sections[0]);
        }
        // 对话自身的 history_view/scroll 照常更新（视图期间不渲染，但返回时
        // 保证对话内容最新）。
        self.history_view.update(&self.messages, sections[1].width);
        let new_max_scroll = self
            .history_view
            .rows
            .len()
            .saturating_sub(sections[1].height as usize);

        if new_max_scroll > self.max_scroll {
            let delta = new_max_scroll - self.max_scroll;
            if self.auto_scroll {
                // 处于自动滚动跟随态：保持紧贴最新底部
                self.scroll = 0;
            } else {
                // 处于上滚阅读态（自动滚动已暂停）：
                // 视口起始行 start = max_scroll - scroll 必须保持完全静止！
                // 由于 max_scroll 增加了 delta，将 self.scroll 同步增加 delta，
                // 使得 start_new = (max_scroll + delta) - (scroll + delta) = max_scroll - scroll = start_old
                self.scroll = (self.scroll + delta).min(new_max_scroll);
            }
        }
        self.max_scroll = new_max_scroll;
        self.scroll = self.scroll.min(self.max_scroll);
        if self.scroll == 0 {
            self.auto_scroll = true;
        }
        if let (Some(view), Some(run)) = (self.subagent_view.as_mut(), view_run.as_ref()) {
            let key = (
                sections[1].width,
                view.tools_expanded,
                subagent_lines_fingerprint(&run.lines),
            );
            if view.built != Some(key) {
                let lines = build_subagent_view_lines(run, view.tools_expanded, sections[1].width);
                view.viewport.update(&lines, sections[1].width);
                view.built = Some(key);
            }
            let new_max = view
                .viewport
                .rows
                .len()
                .saturating_sub(sections[1].height as usize);
            if new_max > view.max_scroll {
                let delta = new_max - view.max_scroll;
                if view.follow_bottom {
                    view.scroll = 0;
                } else {
                    view.scroll = (view.scroll + delta).min(new_max);
                }
            }
            view.max_scroll = new_max;
            view.scroll = view.scroll.min(view.max_scroll);
            if view.scroll == 0 {
                view.follow_bottom = true;
            }
            let start = view.max_scroll
                - if view.follow_bottom {
                    0
                } else {
                    view.scroll.min(view.max_scroll)
                };
            let window = view.viewport.window_with_selection(
                start,
                sections[1].height,
                self.selection.as_ref(),
                default_selection_style(),
            );
            frame.render_widget(Paragraph::new(window), sections[1]);
            if view.max_scroll > 0 {
                let mut scrollbar_state = ScrollbarState::new(view.max_scroll)
                    .position(view.max_scroll.saturating_sub(view.scroll));
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None)
                    .track_symbol(Some("│"))
                    .thumb_symbol("█")
                    .track_style(Style::default().fg(Color::DarkGray))
                    .thumb_style(Style::default().fg(ACCENT));
                frame.render_stateful_widget(scrollbar, sections[1], &mut scrollbar_state);
            }
        } else {
            let start = self.max_scroll - self.scroll;
            frame.render_widget(
                Paragraph::new(self.history_view.window_with_selection(
                    start,
                    sections[1].height,
                    self.selection.as_ref(),
                    default_selection_style(),
                )),
                sections[1],
            );
            if self.messages.is_empty() && !self.busy && sections[1].height >= 4 {
                let mut welcome = vec![
                    Line::styled(
                        " Start with a question or a file to inspect.",
                        Style::default().fg(FG),
                    ),
                    Line::styled(
                        " / commands · Tab complete · ? shortcuts",
                        Style::default().fg(DIM),
                    ),
                ];
                if let Some(newer) = &self.new_version {
                    welcome.push(Line::default());
                    welcome.push(Line::styled(
                        format!(" 💡 检测到新版本 V{newer}，可运行 /update 更新（或退出后 cyber update）"),
                        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                    ));
                }
                if !self.recent_sessions.is_empty() && sections[1].height >= 7 {
                    welcome.push(Line::default());
                    welcome.push(Line::styled(
                        " Recent sessions",
                        Style::default().fg(ACCENT),
                    ));
                    welcome.extend(
                        self.recent_sessions
                            .iter()
                            .take(sections[1].height.saturating_sub(4).min(4) as usize)
                            .map(|session| {
                                Line::styled(
                                    format!(" {}", single_line(session)),
                                    Style::default().fg(MUTED),
                                )
                            }),
                    );
                }
                frame.render_widget(Paragraph::new(welcome), sections[1]);
            }
            if self.max_scroll > 0 {
                let mut scrollbar_state = ScrollbarState::new(self.max_scroll)
                    .position(self.max_scroll.saturating_sub(self.scroll));
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None)
                    .track_symbol(Some("│"))
                    .thumb_symbol("█")
                    .track_style(Style::default().fg(Color::DarkGray))
                    .thumb_style(Style::default().fg(ACCENT));
                frame.render_stateful_widget(scrollbar, sections[1], &mut scrollbar_state);
            }
        }
        if self.approval.is_none()
            && self.form.is_none()
            && self.picker.is_none()
            && self.panel.is_none()
            && !self.completion_closed
            && !self.completions.is_empty()
            && sections[1].height > 0
        {
            let height = (self.completions.len().min(10) as u16).min(sections[1].height);
            let menu = Rect::new(
                sections[1].x,
                sections[1].bottom() - height,
                sections[1].width,
                height,
            );
            frame.render_widget(Clear, menu);
            let offset = self
                .completion_selected
                .saturating_sub(menu.height.saturating_sub(1) as usize);
            let lines = self
                .completions
                .iter()
                .enumerate()
                .skip(offset)
                .take(menu.height as usize)
                .map(|(index, item)| {
                    let main = item.value.trim();
                    select_row(
                        main,
                        &item.description,
                        index == self.completion_selected,
                        menu.width,
                    )
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), menu);
        }
        if show_todo_table && sections[2].height >= 2 {
            draw_todo_table(frame, sections[2], &todo_items, self.todo_view);
        }
        if show_todo_strip && sections[2].height >= 1 {
            draw_todo_strip(frame, sections[2], &todo_items);
        }
        if let Some(q) = &self.question_state {
            if sections[3].height >= 4 {
                crate::question_ui::render_question_box(frame, sections[3], q, Some(ACCENT));
            }
        }
        let input_area = sections[4];
        let border = if self.approval.is_some() || self.form.is_some() || self.busy {
            ACCENT
        } else {
            effort_color(self.effort)
        };
        let title = if self.approval.is_some() {
            " Select a permission action ".to_owned()
        } else if let Some(form) = &self.form {
            format!(
                " {} · Ctrl+S save ",
                form.form
                    .fields
                    .get(form.selected)
                    .map(|field| single_line(&field.name))
                    .unwrap_or_default()
            )
        } else {
            let model_effort = format!(
                "{} · {}",
                single_line(&self.model),
                effort_label(self.effort)
            );
            if self.busy {
                let secs = self
                    .thinking_started
                    .map_or(0, |started| started.elapsed().as_secs());
                format!(" Cy > {secs}s > {model_effort} ")
            } else {
                format!(" {model_effort} ")
            }
        };
        let secret = self.approval.is_none()
            && self.form.as_ref().is_some_and(|form| {
                form.form
                    .fields
                    .get(form.selected)
                    .is_some_and(|field| field.secret)
            });
        draw_composer(
            frame,
            input_area,
            active_input,
            &title,
            border,
            secret,
            self.busy,
        );
        let mut footer: Vec<Line<'static>> = vec![self.footer_line()];
        if self.approval.is_some() {
            footer = vec![Line::from(
                "  permission required · 1/2/3 select · Enter confirm · Esc deny",
            )];
        } else if let Some(run) = view_run.as_ref() {
            let (badge, badge_color) = subagent_badge(run.status);
            let mut spans = vec![
                Span::styled(
                    format!(" #{} {} · ", run.id, single_line(&run.name)),
                    Style::default().fg(DIM),
                ),
                Span::styled(badge.to_string(), Style::default().fg(badge_color)),
                Span::styled(
                    format!(
                        " · {} 行 · ↑/↓ 滚动 · End 回底 · Esc 返回对话 · Ctrl+O 工具详情 ",
                        run.lines.len()
                    ),
                    Style::default().fg(DIM),
                ),
            ];
            if let Some(view) = &self.subagent_view {
                if !view.follow_bottom && view.scroll > 0 {
                    spans.push(Span::styled(
                        "· [↑自动滚动已暂停 · 按 End 恢复]",
                        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                    ));
                }
            }
            footer = vec![Line::from(spans)];
        }
        if self.ctf_enabled {
            let last_line = footer.pop().unwrap_or_default();
            footer.push(self.append_ctf_mode_badge(last_line, sections[5].width as usize));
        }
        frame.render_widget(Paragraph::new(footer), sections[5]);
        if let Some(request) = &self.approval {
            if self.approval_nonce != request.nonce {
                self.approval_nonce.clone_from(&request.nonce);
                self.approval_scroll = 0;
                let arguments = serde_json::to_string_pretty(&request.arguments)
                    .unwrap_or_else(|_| request.arguments.to_string());
                self.approval_arguments = clean(&arguments)
                    .lines()
                    .map(|line| Line::raw(line.to_owned()))
                    .collect();
            }
            let bounds = sections[1];
            if approval_width < 4 || (bounds.height as usize) < controls_height + 3 {
                self.approval_max_scroll = 0;
                self.approval_scroll = 0;
                frame.render_widget(Clear, bounds);
                frame.render_widget(
                    Paragraph::new(
                        "Expand the terminal to review all permission details.\nConfirmation is disabled; Esc denies safely.",
                    )
                    .style(Style::default().fg(AMBER))
                    .wrap(Wrap { trim: false }),
                    bounds,
                );
            } else {
                let inner_width = approval_width.saturating_sub(2);
                self.approval_view
                    .update(&self.approval_arguments, inner_width);
                let max_popup_height = bounds.height.saturating_sub(2);
                let arguments_height = self.approval_view.rows.len().clamp(1, 8) as u16;
                let popup_height = (controls_height as u16)
                    .saturating_add(arguments_height)
                    .saturating_add(2)
                    .min(max_popup_height)
                    .max(8.min(max_popup_height));
                let popup_area = Rect::new(
                    bounds.x + (bounds.width.saturating_sub(approval_width)) / 2,
                    bounds.y + (bounds.height.saturating_sub(popup_height)) / 2,
                    approval_width,
                    popup_height,
                );
                let block = Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(" Permission Required ")
                    .title_style(Style::default().fg(AMBER).add_modifier(Modifier::BOLD))
                    .border_style(Style::default().fg(AMBER));
                let inner = block.inner(popup_area);
                frame.render_widget(Clear, popup_area);
                frame.render_widget(block, popup_area);
                let controls_height = (controls_height as u16).min(inner.height);
                frame.render_widget(
                    Paragraph::new(approval_controls.as_ref().unwrap().clone())
                        .wrap(Wrap { trim: false }),
                    Rect::new(inner.x, inner.y, inner.width, controls_height),
                );
                let arguments_area = Rect::new(
                    inner.x,
                    inner.y + controls_height,
                    inner.width,
                    inner.height.saturating_sub(controls_height),
                );
                self.approval_max_scroll = self
                    .approval_view
                    .rows
                    .len()
                    .saturating_sub(arguments_area.height as usize);
                self.approval_scroll = self.approval_scroll.min(self.approval_max_scroll);
                frame.render_widget(
                    Paragraph::new(
                        self.approval_view
                            .window(self.approval_scroll, arguments_area.height),
                    )
                    .style(Style::default().fg(MUTED)),
                    arguments_area,
                );
                self.approval_visible = true;
            }
        } else if let Some(panel) = self.panel {
            match panel {
                Panel::Shortcuts => {
                    let title = " Shortcuts ";
                    let keys = SHORTCUT_KEYS
                        .iter()
                        .map(|(key, desc)| format!("{key:<16}{desc}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let text = format!(
                        "{keys}\n\n{}\n\nEsc closes this panel.",
                        cli_commands::commands()
                            .iter()
                            .map(|spec| format!("{:<30} {}", spec.usage, spec.desc))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                    popup(frame, sections[1], title, &text);
                }
                Panel::Ctf => {
                    if let Some(popup_area) = panel_geometry(sections[1]) {
                        blank_frame_surround(frame, sections[1], popup_area);
                        frame.render_widget(Clear, popup_area);
                        crate::views::ctf_panel::render(
                            frame,
                            popup_area,
                            &CLI_THEME,
                            &self.ctf_challenges_list(),
                            self.ctf_selected,
                            self.ctf_detail_view,
                            self.ctf_detail_scroll,
                            true,
                            &self.ctf_list_scroll,
                        );
                    }
                }
                Panel::Settings => {
                    if let Some(settings) = &self.settings {
                        draw_settings_panel(frame, sections[1], settings, self);
                    }
                }
                Panel::Subagents => {
                    let snapshot = self.subagents.snapshot();
                    if let Some(state) = self.subagent_panel.as_mut() {
                        draw_subagent_panel(frame, sections[1], state, &snapshot);
                    }
                }
                Panel::Jobs => {
                    let jobs = self.background.snapshot();
                    let subagents = Arc::clone(&self.subagents);
                    if let Some(state) = self.jobs_panel.as_mut() {
                        draw_jobs_panel(frame, sections[1], state, &jobs, &subagents);
                    }
                }
                Panel::ModelPicker => {}
                Panel::Mcp => {}
                Panel::About => {}
            }
        } else if let Some(form) = &self.form {
            draw_form_dialog(frame, sections[1], form, &self.status);
        } else if let Some(picker) = &self.picker {
            draw_picker_dialog(
                frame,
                sections[1],
                picker,
                self.picker_selected,
                self.delete_pending.is_some(),
                &self.status,
            );
        }
        if let Some(form) = self.ctf_edit_form.as_mut() {
            form.prepare_render(&CLI_THEME);
            crate::views::ctf_edit_form::render_form(frame, area, &CLI_THEME, form);
        }
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let logo_width = draw_logo(frame, area);
        let info = Rect::new(
            area.x + logo_width,
            area.y + u16::from(logo_width != 0),
            area.width.saturating_sub(logo_width),
            area.height.saturating_sub(u16::from(logo_width != 0)),
        );
        let mut title_spans = vec![Span::styled(
            format!("Cyber Master V{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        )];
        if let Some(newer) = &self.new_version {
            title_spans.push(Span::styled(
                format!(" (新版本 V{newer})"),
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ));
        }
        let mut lines = vec![
            Line::from(title_spans),
            Line::styled(
                format!(
                    "{} with {} effort",
                    clean(&self.model),
                    effort_label(self.effort)
                ),
                Style::default().fg(MUTED),
            ),
            Line::styled(clean(&self.cwd), Style::default().fg(MUTED)),
        ];
        push_session_title_line(&mut lines, &self.session_title);
        frame.render_widget(Paragraph::new(lines), info);
    }

    /// 子代理覆盖视图顶栏：logo 同 `draw_header`，信息行替换为子代理标识。
    fn draw_subagent_view_header(&self, frame: &mut Frame, area: Rect, run: &SubagentRun) {
        let logo_width = draw_logo(frame, area);
        let info = Rect::new(
            area.x + logo_width,
            area.y + u16::from(logo_width != 0),
            area.width.saturating_sub(logo_width),
            area.height.saturating_sub(u16::from(logo_width != 0)),
        );
        let (badge, badge_color) = subagent_badge(run.status);
        let elapsed = std::time::Instant::now()
            .duration_since(run.started)
            .as_secs();
        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    format!("#{} · {}", run.id, clean(&run.name)),
                    Style::default().fg(FG).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" · {badge}"), Style::default().fg(badge_color)),
                Span::styled(format!(" · {elapsed}s"), Style::default().fg(MUTED)),
            ]),
            Line::styled(
                format!(
                    "{} with {} effort",
                    clean(&self.model),
                    effort_label(self.effort)
                ),
                Style::default().fg(MUTED),
            ),
            Line::styled(clean(&self.cwd), Style::default().fg(MUTED)),
        ];
        push_session_title_line(&mut lines, &self.session_title);
        frame.render_widget(Paragraph::new(lines), info);
    }
}

/// 左 logo（与对话顶栏完全一致）；返回 logo 宽度（空间不足时 0）。
fn draw_logo(frame: &mut Frame, area: Rect) -> u16 {
    let logo_width = if area.width >= 38 && area.height >= 5 {
        15
    } else {
        0
    };
    if logo_width != 0 {
        frame.render_widget(
            Paragraph::new(
                CY_LOGO
                    .iter()
                    .map(|line| {
                        Line::from(
                            line.chars()
                                .enumerate()
                                .map(|(index, ch)| {
                                    Span::styled(
                                        ch.to_string(),
                                        Style::default()
                                            .fg(match index {
                                                0..=3 => Color::Rgb(215, 135, 175),
                                                4..=7 => CODE,
                                                _ => Color::Rgb(0, 175, 175),
                                            })
                                            .add_modifier(Modifier::BOLD),
                                    )
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect::<Vec<_>>(),
            ),
            Rect::new(
                area.x,
                area.y + area.height.saturating_sub(5),
                logo_width,
                5,
            ),
        );
    }
    logo_width
}

/// 顶栏会话行（与 `draw_header` 条件一致：非空且非默认标题才显示）。
fn push_session_title_line(lines: &mut Vec<Line<'static>>, session_title: &str) {
    if !session_title.is_empty() && session_title != "新会话" && session_title != "默认会话"
    {
        lines.push(Line::from(vec![
            Span::styled("Session · ", Style::default().fg(ACCENT)),
            Span::styled(
                clean(session_title),
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            ),
        ]));
    }
}

/// ` (▶ 标题) ` 片段：优先 InProgress，其次第一条 Pending；无则空串。
///
/// 底栏 `todo_summary` 与收起态 1 行进度条（`draw_todo_strip`）共用，保证两处当前任务一致。
fn todo_active_desc(items: &[cyber_core::TodoItem]) -> String {
    items
        .iter()
        .find(|t| t.status == cyber_core::TodoStatus::InProgress)
        .or_else(|| {
            items
                .iter()
                .find(|t| t.status == cyber_core::TodoStatus::Pending)
        })
        .map(|t| format!(" (▶ {})", t.title))
        .unwrap_or_default()
}

fn clean(text: &str) -> String {
    if !text.contains('\r') {
        return text
            .chars()
            .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
            .collect();
    }
    text.split('\n')
        .map(|line| {
            let effective = if line.contains('\r') {
                line.rsplit('\r').find(|s| !s.is_empty()).unwrap_or("")
            } else {
                line
            };
            effective
                .chars()
                .filter(|c| !c.is_control() || *c == '\t')
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn single_line(text: &str) -> String {
    clean(text).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// unicode-width 补齐空格至 `width`；已达/超过宽度时原样返回（不截断）。
fn pad_cells(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let used = UnicodeWidthStr::width(text);
    if used >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - used))
    }
}

fn clip_cells(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut used = 0;
    text.chars()
        .take_while(|ch| {
            used += if *ch == '\t' {
                4
            } else {
                ch.width().unwrap_or(0)
            };
            used <= width
        })
        .collect()
}

fn clip_cells_ellipsis(text: &str, width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let target = width.saturating_sub(1);
    let mut used = 0;
    let mut result: String = text
        .chars()
        .take_while(|ch| {
            used += if *ch == '\t' {
                4
            } else {
                ch.width().unwrap_or(0)
            };
            used <= target
        })
        .collect();
    result.push('…');
    result
}

/// 模型 id 列表排序：按名称首字母（不区分大小写）升序；仅大小写不同的条目按原串稳定排序。
///
/// 用于所有「选择模型」列表（`/model` 双栏面板、设置中心 Providers 的 `M` 模型选择、
/// Provider 表单的「选择默认模型」浮层），保证同一份模型清单在任意入口顺序一致。
fn sort_model_ids(ids: &mut [String]) {
    ids.sort_by_cached_key(|id| (id.to_ascii_lowercase(), id.clone()));
}

fn select_row(main: &str, detail: &str, selected: bool, width: u16) -> Line<'static> {
    let available = width.saturating_sub(2) as usize;
    let primary_width = if available >= 44 { 32 } else { available };
    let primary = clip_cells(&single_line(main), primary_width);
    let primary_cells = Line::raw(primary.as_str()).width();
    let mut spans = vec![
        Span::raw(if selected { "> " } else { "  " }),
        Span::styled(
            primary,
            Style::default().fg(if selected { ACCENT } else { FG }),
        ),
    ];
    if available >= 44 {
        spans.push(Span::raw(
            " ".repeat(primary_width.saturating_sub(primary_cells) + 2),
        ));
        spans.push(Span::styled(
            clip_cells(&single_line(detail), available - primary_width - 2),
            Style::default().fg(if selected { ACCENT } else { MUTED }),
        ));
    }
    Line::from(spans).style(
        Style::default()
            .fg(if selected { ACCENT } else { MUTED })
            .bg(Color::Reset),
    )
}

fn tool_card_kind(name: &str) -> ToolCardKind {
    if name.eq_ignore_ascii_case("read") || name.eq_ignore_ascii_case("read_file") {
        ToolCardKind::Read
    } else if name.eq_ignore_ascii_case("edit") || name.eq_ignore_ascii_case("apply_patch") {
        ToolCardKind::Edit
    } else if name.eq_ignore_ascii_case("write") || name.eq_ignore_ascii_case("write_file") {
        ToolCardKind::Write
    } else if name.eq_ignore_ascii_case("download") || name.eq_ignore_ascii_case("download_file") {
        ToolCardKind::Download
    } else if name.eq_ignore_ascii_case("shell") || name.eq_ignore_ascii_case("bash") {
        ToolCardKind::Shell
    } else if name.eq_ignore_ascii_case("web_fetch") || name.eq_ignore_ascii_case("fetch") {
        ToolCardKind::Fetch
    } else if name.eq_ignore_ascii_case("list_dir") || name.eq_ignore_ascii_case("list") {
        ToolCardKind::List
    } else if name.eq_ignore_ascii_case("find_file")
        || name.eq_ignore_ascii_case("find")
        || name.eq_ignore_ascii_case("grep")
    {
        ToolCardKind::Find
    } else if name.eq_ignore_ascii_case("delegate_tasks") || name.eq_ignore_ascii_case("delegate") {
        ToolCardKind::Delegate
    } else if name.eq_ignore_ascii_case("todo") {
        ToolCardKind::Todo
    } else {
        ToolCardKind::Generic
    }
}

fn argument_string<'a>(arguments: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(serde_json::Value::as_str))
        .filter(|value| !value.is_empty())
}

fn tool_card_title(card: &ToolCard, kind: ToolCardKind) -> String {
    match kind {
        ToolCardKind::Read => "Read".into(),
        ToolCardKind::Edit => "Edit".into(),
        ToolCardKind::Write => "Write".into(),
        ToolCardKind::Download => match card.state {
            ToolCardState::Pending => "Downloading".into(),
            ToolCardState::Success => "Downloaded".into(),
            ToolCardState::Error => "Download failed".into(),
        },
        ToolCardKind::Shell => "Shell".into(),
        ToolCardKind::Fetch => "Fetch".into(),
        ToolCardKind::List => "List".into(),
        ToolCardKind::Find => "Find".into(),
        ToolCardKind::Delegate => "Delegate tasks".into(),
        ToolCardKind::Todo => "Todo".into(),
        ToolCardKind::Generic => {
            let mut words = card.name.split('_');
            let first = words.next().unwrap_or("Tool");
            let mut title = String::with_capacity(card.name.len());
            let mut chars = first.chars();
            if let Some(initial) = chars.next() {
                title.extend(initial.to_uppercase());
                title.extend(chars);
            }
            for word in words {
                title.push(' ');
                title.push_str(word);
            }
            title
        }
    }
}

fn tool_card_detail(kind: ToolCardKind, arguments: Option<&serde_json::Value>) -> String {
    let Some(arguments) = arguments else {
        return String::new();
    };
    let value = match kind {
        ToolCardKind::Read => argument_string(arguments, &["path", "file_path"]).map(str::to_owned),
        ToolCardKind::Edit | ToolCardKind::Write => {
            argument_string(arguments, &["path", "file_path"]).map(str::to_owned)
        }
        ToolCardKind::Download => {
            argument_string(arguments, &["output", "path", "url"]).map(str::to_owned)
        }
        ToolCardKind::Shell => argument_string(arguments, &["command"]).map(str::to_owned),
        ToolCardKind::Fetch => argument_string(arguments, &["url", "path"]).map(str::to_owned),
        ToolCardKind::List => argument_string(arguments, &["path"]).map(str::to_owned),
        ToolCardKind::Find => {
            argument_string(arguments, &["name", "pattern", "content", "path"]).map(str::to_owned)
        }
        ToolCardKind::Delegate => {
            argument_string(arguments, &["intent", "task"]).map(str::to_owned)
        }
        ToolCardKind::Todo => {
            let action = argument_string(arguments, &["action"]).unwrap_or("manage");
            Some(
                if let Some(title) = argument_string(arguments, &["title"]) {
                    format!("{action} {title}")
                } else if let Some(id) = argument_string(arguments, &["id"]) {
                    let status = argument_string(arguments, &["status"]).unwrap_or("");
                    if status.is_empty() {
                        format!("{action} #{id}")
                    } else {
                        format!("{action} #{id} ({status})")
                    }
                } else if let Some(items) =
                    arguments.get("items").and_then(serde_json::Value::as_array)
                {
                    format!("{action} {} tasks", items.len())
                } else {
                    action.to_string()
                },
            )
        }
        ToolCardKind::Generic => {
            argument_string(arguments, &["intent", "path", "url", "command", "query"])
                .map(str::to_owned)
        }
    };
    clean(&value.unwrap_or_default())
}

fn pretty_tool_arguments(arguments: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| clean(arguments))
}

fn tool_card_body(
    card: &ToolCard,
    kind: ToolCardKind,
    expanded: bool,
    show_arguments: bool,
) -> Vec<String> {
    let arguments = pretty_tool_arguments(&card.arguments);
    let body = if card.state == ToolCardState::Pending {
        if !card.progress.is_empty() {
            card.progress.as_str()
        } else if !show_arguments
            && matches!(
                kind,
                ToolCardKind::Shell
                    | ToolCardKind::Read
                    | ToolCardKind::Download
                    | ToolCardKind::Fetch
                    | ToolCardKind::List
                    | ToolCardKind::Find
            )
        {
            ""
        } else {
            arguments.as_str()
        }
    } else {
        card.output.as_str()
    };
    let mut lines = Vec::new();
    if expanded
        && card.state != ToolCardState::Pending
        && !arguments.trim().is_empty()
        && arguments.trim() != "{}"
        && (show_arguments
            || matches!(
                kind,
                ToolCardKind::Edit
                    | ToolCardKind::Write
                    | ToolCardKind::Generic
                    | ToolCardKind::Delegate
            ))
    {
        lines.push("Arguments".into());
        lines.extend(arguments.lines().map(str::to_owned));
        if !body.is_empty() {
            lines.push("Result".into());
        }
    }
    lines.extend(clean(body).lines().map(str::to_owned));
    lines
}

fn tool_body_style(card: &ToolCard, kind: ToolCardKind, line: &str, bg: Color) -> Style {
    let foreground = if card.state == ToolCardState::Error {
        ERROR
    } else if kind == ToolCardKind::Edit && line.starts_with('+') && !line.starts_with("+++") {
        SUCCESS
    } else if kind == ToolCardKind::Edit && line.starts_with('-') && !line.starts_with("---") {
        ERROR
    } else if kind == ToolCardKind::Edit && line.starts_with("@@") {
        CODE
    } else if kind == ToolCardKind::Shell {
        FG
    } else if line == "Arguments" || line == "Result" {
        CODE
    } else if kind == ToolCardKind::Todo && line.contains("[x]") {
        SUCCESS
    } else if kind == ToolCardKind::Todo && line.contains("[>]") {
        ACCENT
    } else if kind == ToolCardKind::Todo && line.contains("[!]") {
        ERROR
    } else {
        MUTED
    };
    Style::default().fg(foreground).bg(bg)
}

fn extract_task_progress_msg<'a>(
    line: &'a str,
    task_num: usize,
    total: usize,
    name: &str,
) -> Option<&'a str> {
    let tag = format!("[{task_num}/{total}]");
    let tag_num = format!("[{task_num}/");
    let trimmed = line.trim();
    if trimmed.starts_with(&tag) || trimmed.starts_with(&tag_num) {
        let after_tag = if let Some(rest) = trimmed.strip_prefix(&tag) {
            rest.trim()
        } else if let Some(idx) = trimmed.find(']') {
            trimmed[idx + 1..].trim()
        } else {
            trimmed
        };
        let after_name = if let Some(rest) = after_tag.strip_prefix(name) {
            rest.trim()
        } else {
            after_tag
        };
        let msg = after_name.strip_prefix(':').unwrap_or(after_name).trim();
        return Some(if msg.is_empty() { "started" } else { msg });
    }
    if let Some(rest) = trimmed.strip_prefix(name) {
        let msg = rest.strip_prefix(':').unwrap_or(rest).trim();
        return Some(if msg.is_empty() { "started" } else { msg });
    }
    if trimmed.contains(name) {
        return Some(trimmed);
    }
    None
}

fn render_delegate_tasks_cards(
    card: &ToolCard,
    expanded: bool,
    width: u16,
) -> Option<Vec<Line<'static>>> {
    let parsed_args: serde_json::Value = serde_json::from_str(&card.arguments).ok()?;
    let tasks = parsed_args.get("tasks").and_then(|v| v.as_array())?;
    if tasks.is_empty() {
        return None;
    }
    let total = tasks.len();

    let parsed_output: Option<serde_json::Value> = serde_json::from_str(&card.output).ok();
    let results = parsed_output
        .as_ref()
        .and_then(|v| v.get("results"))
        .and_then(|v| v.as_array());

    let progress_lines: Vec<&str> = card
        .progress
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    let mut all_lines = Vec::new();

    for (i, task_val) in tasks.iter().enumerate() {
        let task_num = i + 1;
        let task_name = task_val
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("subagent");
        let task_prompt = task_val.get("task").and_then(|v| v.as_str()).unwrap_or("");
        let system_prompt = task_val
            .get("system_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let tools = task_val.get("tools").and_then(|v| v.as_array());

        let task_result = results.and_then(|arr| {
            // 结果按 tasks 数组 index 排序返回：index 精确匹配，name 仅作回退
            //（同名任务按 name 匹配会串行）。
            arr.get(i).or_else(|| {
                arr.iter()
                    .find(|r| r.get("name").and_then(|v| v.as_str()) == Some(task_name))
            })
        });

        let mut task_progress_msgs = Vec::new();
        for line in &progress_lines {
            if let Some(msg) = extract_task_progress_msg(line, task_num, total, task_name) {
                task_progress_msgs.push(msg);
            }
        }

        let (task_state, is_warning) = if let Some(r) = task_result {
            let status_str = r.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status_str == "completed" {
                (ToolCardState::Success, false)
            } else if status_str == "step_limit_reached" {
                (ToolCardState::Success, true)
            } else {
                (ToolCardState::Error, false)
            }
        } else if card.state == ToolCardState::Pending {
            if let Some(last_msg) = task_progress_msgs.last() {
                if last_msg.contains("completed") {
                    (ToolCardState::Success, false)
                } else if last_msg.contains("step_limit_reached") {
                    (ToolCardState::Success, true)
                } else if last_msg.contains("error")
                    || last_msg.contains("timed_out")
                    || last_msg.contains("failed")
                    || last_msg.contains("loop_detected")
                {
                    (ToolCardState::Error, false)
                } else {
                    (ToolCardState::Pending, false)
                }
            } else {
                (ToolCardState::Pending, false)
            }
        } else if card.state == ToolCardState::Error {
            (ToolCardState::Error, false)
        } else {
            (ToolCardState::Success, false)
        };

        let (icon, color, bg, default_status) = if is_warning {
            ("⚠", WARNING, PENDING_BG, "step_limit_reached")
        } else {
            match task_state {
                ToolCardState::Pending => ("◇", ACCENT, PENDING_BG, "running"),
                ToolCardState::Success => ("✓", SUCCESS, SUCCESS_BG, "done"),
                ToolCardState::Error => ("✗", ERROR, ERROR_BG, "failed"),
            }
        };

        let title = format!("Subagent [{task_num}/{total}]");
        let detail = task_name;
        let title_cells = Line::raw(&title).width();
        let detail_budget = usize::from(width)
            .saturating_sub(title_cells)
            .saturating_sub(10);
        let detail = clip_cells(&single_line(detail), detail_budget);

        all_lines.push(Line::default());
        all_lines.push(
            Line::from(clipped_spans(
                vec![
                    Span::styled("╭─ ", Style::default().fg(color).bg(bg)),
                    Span::styled(
                        format!("{icon} {title}"),
                        Style::default()
                            .fg(color)
                            .bg(bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if detail.is_empty() {
                            String::new()
                        } else {
                            format!("  {detail}")
                        },
                        Style::default().fg(FG).bg(bg),
                    ),
                ],
                usize::from(width),
            ))
            .style(Style::default().bg(bg)),
        );

        let mut body = Vec::new();
        if !task_prompt.trim().is_empty() {
            if expanded {
                body.push(format!("Task: {}", task_prompt.trim()));
            } else {
                body.push(format!("Task: {}", single_line(task_prompt.trim())));
            }
        }
        if expanded {
            if !system_prompt.trim().is_empty() {
                body.push(format!("System: {}", single_line(system_prompt.trim())));
            }
            if let Some(tool_arr) = tools {
                let t_names: Vec<&str> = tool_arr.iter().filter_map(|v| v.as_str()).collect();
                if !t_names.is_empty() {
                    body.push(format!("Tools: {}", t_names.join(", ")));
                }
            }
        }

        if let Some(r) = task_result {
            let status_str = r.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status_str == "completed" {
                if let Some(output) = r.get("output").and_then(|v| v.as_str()) {
                    let trimmed = output.trim();
                    if !trimmed.is_empty() {
                        if expanded {
                            body.push("Result:".into());
                            body.extend(trimmed.lines().map(str::to_owned));
                        } else {
                            let first_line = trimmed
                                .lines()
                                .find(|l| !l.trim().is_empty())
                                .unwrap_or(trimmed);
                            body.push(format!("Result: {first_line}"));
                        }
                    } else {
                        body.push("Result: completed".into());
                    }
                } else {
                    body.push("Result: completed".into());
                }
            } else if status_str == "step_limit_reached" {
                if let Some(output) = r.get("output").and_then(|v| v.as_str()) {
                    let trimmed = output.trim();
                    if !trimmed.is_empty() {
                        if expanded {
                            body.push("Result (step limit reached):".into());
                            body.extend(trimmed.lines().map(str::to_owned));
                        } else {
                            let first_line = trimmed
                                .lines()
                                .find(|l| !l.trim().is_empty())
                                .unwrap_or(trimmed);
                            body.push(format!("Result (step limit reached): {first_line}"));
                        }
                    } else {
                        body.push("Result: step limit reached".into());
                    }
                } else {
                    let msg = r
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("step limit reached");
                    body.push(format!("Warning: {msg}"));
                }
            } else {
                let err_msg = r
                    .get("error")
                    .and_then(|v| v.as_str())
                    .or_else(|| r.get("output").and_then(|v| v.as_str()))
                    .unwrap_or(status_str);
                body.push(format!("Error: {err_msg}"));
            }
        } else if card.state == ToolCardState::Pending {
            if task_progress_msgs.is_empty() {
                body.push("Progress: queued".into());
            } else if expanded {
                for p in task_progress_msgs.iter().rev().take(4).rev() {
                    body.push(format!("Progress: {p}"));
                }
            } else {
                let latest = task_progress_msgs.last().unwrap();
                body.push(format!("Progress: {latest}"));
            }
        } else if let Some(last_msg) = task_progress_msgs.last() {
            body.push(format!("Result: {last_msg}"));
        }

        let collapsed_limit = 3;
        let shown = body.len().min(if expanded {
            body.len()
        } else {
            collapsed_limit
        });
        let mut clipped_body = false;
        let body_budget = usize::from(width).saturating_sub(3).max(1);
        for line in body.iter().take(shown) {
            let content = if expanded {
                line.clone()
            } else {
                let clipped = clip_cells(line, body_budget);
                clipped_body |= clipped != *line;
                clipped
            };
            let fg_color = if task_state == ToolCardState::Error
                && (line.starts_with("Error:") || line.starts_with("failed"))
            {
                ERROR
            } else if is_warning
                && (line.starts_with("Warning:")
                    || line.starts_with("Result (step limit reached):"))
            {
                WARNING
            } else if line.starts_with("Task:")
                || line.starts_with("Result:")
                || line.starts_with("Progress:")
            {
                CODE
            } else {
                FG
            };
            all_lines.push(
                Line::from(vec![
                    Span::styled("│  ", Style::default().fg(color).bg(bg)),
                    Span::styled(content, Style::default().fg(fg_color).bg(bg)),
                ])
                .style(Style::default().bg(bg)),
            );
        }

        let hidden = body.len().saturating_sub(shown);
        let footer = if hidden > 0 {
            format!("Ctrl+O details · {hidden} more lines")
        } else if clipped_body {
            "Ctrl+O details".into()
        } else {
            default_status.into()
        };
        all_lines.push(
            Line::from(clipped_spans(
                vec![
                    Span::styled("╰─ ", Style::default().fg(color).bg(bg)),
                    Span::styled(footer, Style::default().fg(DIM).bg(bg)),
                ],
                usize::from(width),
            ))
            .style(Style::default().bg(bg)),
        );
    }

    Some(all_lines)
}

fn render_tool_card(
    card: &ToolCard,
    expanded: bool,
    width: u16,
    show_arguments: bool,
) -> Vec<Line<'static>> {
    let kind = tool_card_kind(&card.name);
    if kind == ToolCardKind::Delegate {
        if let Some(lines) = render_delegate_tasks_cards(card, expanded, width) {
            return lines;
        }
    }
    let parsed_arguments = serde_json::from_str::<serde_json::Value>(&card.arguments).ok();
    let title = tool_card_title(card, kind);
    let mut detail = tool_card_detail(kind, parsed_arguments.as_ref());
    // 子代理转录的参数常被截断（200 字符）导致 JSON 解析失败：标题兜底显示原文。
    if detail.is_empty()
        && show_arguments
        && !card.arguments.trim().is_empty()
        && card.arguments.trim() != "{}"
    {
        detail = single_line(&card.arguments);
    }
    let (icon, color, bg, status) = match card.state {
        ToolCardState::Pending => ("◇", ACCENT, PENDING_BG, "running"),
        ToolCardState::Success => ("✓", SUCCESS, SUCCESS_BG, "done"),
        ToolCardState::Error => ("✗", ERROR, ERROR_BG, "failed"),
    };
    let title_cells = Line::raw(&title).width();
    let detail_budget = usize::from(width)
        .saturating_sub(title_cells)
        .saturating_sub(10);
    let detail = clip_cells(&single_line(&detail), detail_budget);
    let mut lines = vec![
        Line::default(),
        Line::from(clipped_spans(
            vec![
                Span::styled("╭─ ", Style::default().fg(color).bg(bg)),
                Span::styled(
                    format!("{icon} {title}"),
                    Style::default()
                        .fg(color)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!("  {detail}")
                    },
                    Style::default().fg(FG).bg(bg),
                ),
            ],
            usize::from(width),
        ))
        .style(Style::default().bg(bg)),
    ];
    let body = tool_card_body(card, kind, expanded, show_arguments);
    let collapsed_limit = match kind {
        ToolCardKind::Edit => 5,
        ToolCardKind::Download => 2,
        ToolCardKind::Todo => 4,
        _ => 3,
    };
    let shown = body.len().min(if expanded {
        body.len()
    } else {
        collapsed_limit
    });
    let start = if !expanded && kind == ToolCardKind::Shell {
        body.len().saturating_sub(shown)
    } else {
        0
    };
    let mut clipped_body = false;
    // 卡片行前缀 `│  ` 占 3 列：折叠态正文严格限制在 `width - 3` 内，避免视口折行
    // 产生无边框前缀的续行（`│` 被挤掉）。展开态保留全文（由视口折行完整展示）。
    let body_budget = usize::from(width).saturating_sub(3).max(1);
    for line in body.iter().skip(start).take(shown) {
        let content = if expanded {
            line.clone()
        } else {
            let clipped = clip_cells(line, body_budget);
            clipped_body |= clipped != *line;
            clipped
        };
        lines.push(
            Line::from(vec![
                Span::styled("│  ", Style::default().fg(color).bg(bg)),
                Span::styled(content, tool_body_style(card, kind, line, bg)),
            ])
            .style(Style::default().bg(bg)),
        );
    }
    let hidden = body.len().saturating_sub(shown);
    let footer = if hidden > 0 {
        format!("Ctrl+O details · {hidden} more lines")
    } else if clipped_body {
        "Ctrl+O details".into()
    } else {
        status.into()
    };
    lines.push(
        Line::from(clipped_spans(
            vec![
                Span::styled("╰─ ", Style::default().fg(color).bg(bg)),
                Span::styled(footer, Style::default().fg(DIM).bg(bg)),
            ],
            usize::from(width),
        ))
        .style(Style::default().bg(bg)),
    );
    lines
}

fn effort_color(effort: ThinkingIntensity) -> Color {
    match effort.resolve(false) {
        ThinkingIntensity::Low => Color::Rgb(23, 143, 185),
        ThinkingIntensity::Middle => LINK,
        ThinkingIntensity::High => Color::Rgb(178, 129, 214),
        ThinkingIntensity::Max => CODE,
        ThinkingIntensity::Auto => DIM,
    }
}

fn draw_todo_table(frame: &mut Frame, area: Rect, items: &[cyber_core::TodoItem], view: TodoView) {
    if area.height < 2 || area.width < 10 {
        return;
    }
    let total = items.len();
    let completed = items
        .iter()
        .filter(|i| i.status == cyber_core::TodoStatus::Completed)
        .count();
    let in_progress = items
        .iter()
        .filter(|i| i.status == cyber_core::TodoStatus::InProgress)
        .count();

    let border_color = if in_progress > 0 {
        ACCENT
    } else if completed == total && total > 0 {
        SUCCESS
    } else {
        DIM
    };

    let title = match view {
        TodoView::Full => {
            format!(" 📋 任务清单 [{completed}/{total}] · 全部 {total} 项 · [Alt+↓] 收起 ")
        }
        // Collapsed 不画表格（此处按 Summary 处理以满足穷尽匹配，不会实际走到）。
        _ => format!(" 📋 任务清单 [{completed}/{total}] · 输入 /todo close 收起 · [Alt+↑] 全展 "),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let max_lines = inner.height as usize;
    if max_lines == 0 {
        return;
    }

    let window = todo_visible_window(items, max_lines);

    let max_row_w = (inner.width as usize).saturating_sub(1);
    let mut lines = Vec::with_capacity(max_lines);
    for item in &items[window.start..window.start + window.len] {
        let (symbol, color) = match item.status {
            cyber_core::TodoStatus::Pending => ("[ ]", MUTED),
            cyber_core::TodoStatus::InProgress => ("[>]", ACCENT),
            cyber_core::TodoStatus::Completed => ("[x]", SUCCESS),
            cyber_core::TodoStatus::Failed => ("[!]", ERROR),
        };
        let sym_text = format!(" {symbol} ");
        let id_text = format!("#{} ", item.id);
        use unicode_width::UnicodeWidthStr;
        let sym_w = UnicodeWidthStr::width(sym_text.as_str());
        let id_w = UnicodeWidthStr::width(id_text.as_str());
        let prefix_w = sym_w + id_w;
        let content_budget = max_row_w.saturating_sub(prefix_w);

        let mut spans = Vec::new();
        if max_row_w < sym_w {
            spans.push(Span::styled(
                clip_cells(&sym_text, max_row_w),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                sym_text,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
            let rem_id = max_row_w.saturating_sub(sym_w);
            if rem_id < id_w {
                spans.push(Span::styled(
                    clip_cells(&id_text, rem_id),
                    Style::default().fg(MUTED),
                ));
            } else {
                spans.push(Span::styled(id_text, Style::default().fg(MUTED)));
            }
        }

        if content_budget > 0 {
            let title_clean = clean(&item.title);
            let title_w = UnicodeWidthStr::width(title_clean.as_str());

            if let Some(notes) = &item.notes.as_ref().filter(|n| !n.trim().is_empty()) {
                let notes_clean = clean(notes);
                let notes_full = format!(" (备注: {notes_clean})");
                let notes_full_w = UnicodeWidthStr::width(notes_full.as_str());
                if title_w + notes_full_w <= content_budget {
                    spans.push(Span::styled(title_clean, Style::default().fg(FG)));
                    spans.push(Span::styled(notes_full, Style::default().fg(MUTED)));
                } else if title_w < content_budget {
                    spans.push(Span::styled(title_clean, Style::default().fg(FG)));
                    let rem = content_budget.saturating_sub(title_w);
                    if rem >= 7 {
                        let note_budget = rem.saturating_sub(6);
                        let clipped_note = clip_cells_ellipsis(&notes_clean, note_budget);
                        spans.push(Span::styled(
                            format!(" (备注: {clipped_note})"),
                            Style::default().fg(MUTED),
                        ));
                    }
                } else {
                    let clipped_title = clip_cells_ellipsis(&title_clean, content_budget);
                    spans.push(Span::styled(clipped_title, Style::default().fg(FG)));
                }
            } else {
                let clipped_title = clip_cells_ellipsis(&title_clean, content_budget);
                spans.push(Span::styled(clipped_title, Style::default().fg(FG)));
            }
        }
        lines.push(Line::from(spans));
    }

    if window.hidden_above > 0 || window.hidden_below > 0 {
        let trunc_msg = match (window.hidden_above, window.hidden_below) {
            (0, below) => format!("   ... 还有 {below} 项任务（输入 /todo list 查看全部）"),
            (above, 0) => format!("   ↑ 上方还有 {above} 项任务（输入 /todo list 查看全部）"),
            (above, below) => {
                format!("   ↑ 上方还有 {above} 项 · 下方 {below} 项（输入 /todo list 查看全部）")
            }
        };
        lines.push(Line::from(vec![Span::styled(
            clip_cells_ellipsis(&trunc_msg, max_row_w),
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        )]));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

/// 完整收起态的 1 行进度条：`  📋 任务清单 [c/t] (▶ 当前任务) · [Alt+↑] 展开 `。
///
/// 无边框、无窗口化；整行由 `clip_cells` 截断防越界（宽度不足时截掉的是尾部提示前缀）。
fn draw_todo_strip(frame: &mut Frame, area: Rect, items: &[cyber_core::TodoItem]) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let total = items.len();
    let completed = items
        .iter()
        .filter(|i| i.status == cyber_core::TodoStatus::Completed)
        .count();
    let in_progress = items
        .iter()
        .filter(|i| i.status == cyber_core::TodoStatus::InProgress)
        .count();
    let color = if in_progress > 0 {
        ACCENT
    } else if completed == total && total > 0 {
        SUCCESS
    } else {
        DIM
    };
    const HINT: &str = " · [Alt+↑] 展开 ";
    use unicode_width::UnicodeWidthStr;
    let budget = (area.width as usize).saturating_sub(UnicodeWidthStr::width(HINT));
    let prefix = format!(
        "  📋 任务清单 [{completed}/{total}]{}",
        clean(&todo_active_desc(items))
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                clip_cells(&prefix, budget),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(HINT, Style::default().fg(DIM)),
        ])),
        area,
    );
}

/// 居中面板几何（与 Skill 详情弹窗一致）：宽/高取 `area` 的 85%，宽最小 70、高最小 20，
/// 并夹紧到 `area`。过小的区域返回 None（面板不渲染）。
fn panel_geometry(area: Rect) -> Option<Rect> {
    if area.width < 20 || area.height < 6 {
        return None;
    }
    let width = (area.width as usize * 85 / 100)
        .max(70)
        .min(area.width as usize) as u16;
    let height = (area.height as usize * 85 / 100)
        .max(20)
        .min(area.height as usize) as u16;
    Some(Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    ))
}

fn subagent_badge(status: SubagentStatus) -> (&'static str, Color) {
    match status {
        SubagentStatus::Running => ("⏱ 运行中", ACCENT),
        SubagentStatus::Completed => ("✓ 完成", SUCCESS),
        SubagentStatus::Error => ("✗ 错误", ERROR),
        SubagentStatus::TimedOut => ("⏱ 超时", ACCENT),
        SubagentStatus::Killed => ("⊘ 已终止", MUTED),
        SubagentStatus::StepLimitReached => ("⚠ 步数耗尽", WARNING),
        SubagentStatus::LoopDetected => ("✗ 死循环", ERROR),
    }
}

fn job_badge(status: &JobStatus) -> (String, Color) {
    match status {
        JobStatus::Running => ("▶ 运行中".to_string(), ACCENT),
        JobStatus::Finished(code) => (format!("✓ 完成({code})"), SUCCESS),
        JobStatus::Killed => ("⊘ 已终止".to_string(), MUTED),
        JobStatus::Failed(_) => ("✗ 失败".to_string(), ERROR),
    }
}

/// 按字符截断（字符边界安全；超 `max` 补 `…`）。
fn truncate_chars(value: &str, max: usize) -> String {
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

/// 滚动窗口：`scroll` 语义为「自底部上滚行数」；`follow=true` 时恒贴底。
/// 返回 (起始行, 可见行)。
fn scroll_window(
    lines: &[Line<'static>],
    height: usize,
    scroll: usize,
    follow: bool,
) -> Vec<Line<'static>> {
    let max_scroll = lines.len().saturating_sub(height);
    let offset = if follow { 0 } else { scroll.min(max_scroll) };
    let start = max_scroll - offset;
    lines.iter().skip(start).take(height).cloned().collect()
}

/// 子代理面板：纯列表（多列：标记/#id/状态/名称/最新活动）。数据来自
/// `SubagentArchive` 快照；Enter 进入覆盖对话视图。
fn draw_subagent_panel(
    frame: &mut Frame,
    area: Rect,
    state: &mut SubagentPanelState,
    snapshot: &[SubagentRun],
) {
    let Some(popup_area) = panel_geometry(area) else {
        return;
    };
    blank_frame_surround(frame, area, popup_area);
    frame.render_widget(Clear, popup_area);
    if state.selected >= snapshot.len() && !snapshot.is_empty() {
        state.selected = snapshot.len() - 1;
    }

    let title_budget = (popup_area.width as usize).saturating_sub(2);
    let title = if snapshot.is_empty() {
        Line::from(vec![Span::styled(
            " ◉ 子代理 (Subagents) ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )])
    } else {
        Line::from(clipped_spans(
            vec![
                Span::styled(
                    " ◉ 子代理 (Subagents) ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("· {} 次运行 ", snapshot.len()),
                    Style::default().fg(DIM),
                ),
            ],
            title_budget,
        ))
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM))
        .title(title);
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);
    let inner_width = inner.width as usize;
    let content_height = inner.height.saturating_sub(1) as usize;
    if content_height == 0 {
        return;
    }

    // 列宽：前缀 2 + #id 5 + 状态 8 + 名称 16，其余给最新活动（预留 1 列安全余量防挤压边框）。
    const PREFIX_W: usize = 2;
    const ID_W: usize = 5;
    const BADGE_W: usize = 8;
    const NAME_W: usize = 16;
    let last_budget = inner_width
        .saturating_sub(PREFIX_W + ID_W + BADGE_W + NAME_W + 1)
        .max(1);

    let mut body: Vec<Line<'static>> = Vec::new();
    // 表头 + 分隔线
    body.push(Line::from(clipped_spans(
        vec![
            Span::styled("  ", Style::default().fg(DIM)),
            Span::styled(pad_cells("#", 4) + " ", Style::default().fg(DIM)),
            Span::styled(pad_cells("状态", BADGE_W), Style::default().fg(DIM)),
            Span::styled(pad_cells("名称", NAME_W), Style::default().fg(DIM)),
            Span::styled("最新活动", Style::default().fg(DIM)),
        ],
        inner_width.saturating_sub(1),
    )));
    body.push(Line::styled(
        "─".repeat(inner_width.saturating_sub(1)),
        Style::default().fg(DIM),
    ));
    for (index, run) in snapshot.iter().enumerate() {
        let (badge, badge_color) = subagent_badge(run.status);
        let last = run
            .lines
            .iter()
            .rev()
            .map(|line| single_line(line))
            .find(|s| !s.is_empty() && s != "thinking:")
            .unwrap_or_default();
        let selected = index == state.selected;
        let name_style = if selected {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(FG)
        };
        let line = Line::from(vec![
            Span::styled(
                if selected { "▶ " } else { "  " },
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("#{:<3} ", run.id), Style::default().fg(MUTED)),
            Span::styled(pad_cells(badge, BADGE_W), Style::default().fg(badge_color)),
            Span::styled(
                pad_cells(&clip_cells(&single_line(&run.name), NAME_W), NAME_W),
                name_style,
            ),
            Span::styled(clip_cells(&last, last_budget), Style::default().fg(MUTED)),
        ]);
        if selected {
            body.push(line.style(Style::default().bg(PENDING_BG)));
        } else {
            body.push(line);
        }
    }
    if snapshot.is_empty() {
        body.push(Line::default());
        let max_w = inner_width.saturating_sub(1);
        body.push(Line::styled(
            clip_cells("   暂无子代理运行记录", max_w),
            Style::default().fg(MUTED),
        ));
        body.push(Line::styled(
            clip_cells(
                "   （由 delegate_tasks 或 /bg run 产生；Ctrl+B 查看后台任务）",
                max_w,
            ),
            Style::default().fg(MUTED),
        ));
    }

    let visible = body
        .iter()
        .take(content_height)
        .cloned()
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(visible).style(Style::default().fg(FG)),
        Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(1),
        ),
    );
    let footer_hint = " 操作: ↑/↓ 选择 · Enter 覆盖对话查看 · Esc 关闭 · PgUp/PgDn 翻页 ";
    frame.render_widget(
        Paragraph::new(Line::styled(
            clip_cells(footer_hint, inner_width.saturating_sub(1)),
            Style::default().fg(DIM),
        )),
        Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        ),
    );
}

/// 转录行内容指纹（FNV-1a 64）：内容不变则相等，任何追加/丢旧都会变化。
/// 视图重建以此判定，而非行数（行数到 MAX_LINES 上限后不再变化）。
fn subagent_lines_fingerprint(lines: &[String]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for line in lines {
        for byte in line.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= 0xff;
    }
    hash
}

/// 子代理转录 → 对话式渲染：
/// - 连续 `thinking: …` 行合并为一个 Thinking 块（流式推理逐行写入，逐行渲染
///   会产生「每个单词一个 Thinking 头」）；
/// - `tool … args: …` 与后续 `{name} => …` 结果行合并成对话同款工具卡；结果行
///   向后按名搜索（并行工具调用时结果行交错，不保证紧跟）；
/// - 连续普通行合并为一个 markdown 文档渲染（跨行围栏/列表/标题/粗体才生效）。
fn build_subagent_view_lines(
    run: &SubagentRun,
    tools_expanded: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let thinking_theme = Theme {
        fg: MUTED,
        title: MUTED,
        ..CLI_THEME
    };
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut consumed = vec![false; run.lines.len()];
    let mut i = 0;
    while i < run.lines.len() {
        if consumed[i] {
            i += 1;
            continue;
        }
        let line = &run.lines[i];
        if let Some(text) = line.strip_prefix("thinking: ") {
            // 连续 thinking 行合并。转录已按推理流缓冲聚合（行内空格原样保留，
            // 事件边界可能切在词/数字中间），因此这里纯拼接：补空格会产生
            // "118 42" / "node _modules" 这类错位。
            let mut merged = String::from(text);
            let mut end = i + 1;
            while let Some(more) = run
                .lines
                .get(end)
                .and_then(|l| l.strip_prefix("thinking: "))
            {
                merged.push_str(more);
                end += 1;
            }
            if !merged.trim().is_empty() {
                if !out.is_empty() {
                    out.push(Line::default());
                }
                out.push(Line::styled(
                    "Thinking",
                    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                ));
                let mut rendered = crate::markdown::render(&merged, &thinking_theme);
                for rendered_line in &mut rendered {
                    for span in &mut rendered_line.spans {
                        span.style = span.style.fg(MUTED).add_modifier(Modifier::ITALIC);
                    }
                }
                out.extend(rendered);
            }
            i = end;
        } else if let Some(rest) = line.strip_prefix("tool ") {
            let Some(args_idx) = rest.find(" args: ") else {
                // 非标准工具行：并入普通文本组
                i = push_plain_block(run, &consumed, i, &mut out);
                continue;
            };
            let name = &rest[..args_idx];
            let arguments = rest[args_idx + " args: ".len()..].to_string();
            let result_prefix = format!("{name} => ");
            let mut result: Option<String> = None;
            for (k, cand) in run.lines.iter().enumerate().skip(i + 1) {
                if let Some(output) = cand.strip_prefix(&result_prefix) {
                    consumed[k] = true;
                    result = Some(output.to_string());
                    break;
                }
            }
            let (output, state) = match result {
                Some(output) => (output, ToolCardState::Success),
                None => (String::new(), ToolCardState::Pending),
            };
            let card = ToolCard {
                id: format!("sv-{i}-{name}"),
                name: name.to_string(),
                arguments,
                progress: String::new(),
                output,
                state,
                start: 0,
                end: 0,
            };
            out.extend(render_tool_card(&card, tools_expanded, width, true));
            i += 1;
        } else {
            i = push_plain_block(run, &consumed, i, &mut out);
        }
    }
    if out.is_empty() {
        out.push(Line::styled(
            "（该子代理暂无转录行）",
            Style::default().fg(MUTED),
        ));
    }
    out
}

/// 从 `start` 起收集连续普通行（非 thinking / 非标准工具行），合并为一个
/// markdown 文档渲染并追加到 `out`；返回下一个未处理索引。
fn push_plain_block(
    run: &SubagentRun,
    consumed: &[bool],
    start: usize,
    out: &mut Vec<Line<'static>>,
) -> usize {
    let mut block = String::new();
    let mut end = start;
    while end < run.lines.len() && !consumed[end] {
        let line = &run.lines[end];
        if line.starts_with("thinking: ") || (line.starts_with("tool ") && line.contains(" args: "))
        {
            break;
        }
        if !block.is_empty() {
            block.push('\n');
        }
        block.push_str(line);
        end += 1;
    }
    if !block.is_empty() {
        if !out.is_empty() {
            out.push(Line::default());
        }
        out.extend(crate::markdown::render(&block, &CLI_THEME));
    }
    end
}

/// 后台任务面板：列表 ⇄ 详情。Subagent 详情优先展示 `SubagentArchive` 转录。
fn draw_jobs_panel(
    frame: &mut Frame,
    area: Rect,
    state: &mut JobsPanelState,
    jobs: &[BackgroundJob],
    subagents: &SubagentArchive,
) {
    let Some(popup_area) = panel_geometry(area) else {
        return;
    };
    blank_frame_surround(frame, area, popup_area);
    frame.render_widget(Clear, popup_area);
    if state.selected >= jobs.len() && !jobs.is_empty() {
        state.selected = jobs.len() - 1;
    }

    let title = match state
        .detail
        .and_then(|id| jobs.iter().find(|job| job.id == id))
    {
        Some(job) => {
            let kind = match job.kind {
                JobKind::Shell => "Shell",
                JobKind::Subagent => "Subagent",
            };
            let (badge, color) = job_badge(&job.status);
            // 标题绘制在顶边框上：留出左右圆角，整体裁剪防越界。
            let budget = (popup_area.width as usize).saturating_sub(2);
            let spans = vec![
                Span::styled(
                    " ⚡ 后台任务 ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("#{} · {kind} · {}", job.id, single_line(&job.name)),
                    Style::default().fg(FG).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" · {badge}"), Style::default().fg(color)),
            ];
            Line::from(clipped_spans(spans, budget))
        }
        None => Line::from(vec![Span::styled(
            " ⚡ 后台任务 (Background Jobs) ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )]),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM))
        .title(title);
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);
    let inner_width = inner.width as usize;
    let content_height = inner.height.saturating_sub(1) as usize;
    if content_height == 0 {
        return;
    }

    let mut body: Vec<Line<'static>> = Vec::new();
    let mut footer_hint = " 操作: ↑/↓ 选择 · Enter 看日志 · k 终止 · r 清理 · Esc 关闭";
    if let Some(detail_id) = state.detail {
        let job = jobs.iter().find(|job| job.id == detail_id);
        if let Some(job) = job {
            footer_hint = " 操作: ↑/↓ · PgUp/PgDn 滚动 · End 回底 · Esc 返回列表";
            // Subagent 任务优先展示其归档转录；无转录回退 registry lines。
            let transcript = job.archive_run_id.and_then(|run_id| {
                subagents
                    .snapshot()
                    .into_iter()
                    .find(|run| run.id == run_id)
            });
            let mut shown = 0usize;
            if let Some(run) = transcript {
                let now = std::time::Instant::now();
                for line in &run.lines {
                    let elapsed = now.duration_since(run.started).as_secs_f64();
                    body.push(Line::styled(
                        clip_cells(
                            &format!("{elapsed:>5.1}s  {line}"),
                            inner_width.saturating_sub(1),
                        ),
                        Style::default().fg(FG),
                    ));
                    shown += 1;
                }
            } else {
                for line in &job.lines {
                    body.push(Line::styled(
                        clip_cells(line, inner_width.saturating_sub(1)),
                        Style::default().fg(FG),
                    ));
                    shown += 1;
                }
            }
            if shown == 0 {
                body.push(Line::styled("（暂无输出）", Style::default().fg(MUTED)));
            }
            body.push(Line::default());
            if state.follow_bottom {
                body.push(Line::styled(" ↓ 跟随底部", Style::default().fg(DIM)));
            }
        } else {
            state.detail = None;
            state.detail_scroll = 0;
            state.follow_bottom = true;
        }
    }
    if state.detail.is_none() {
        body.push(Line::styled(
            clip_cells(
                "   #  类型      状态          名称                    最新输出",
                inner_width.saturating_sub(1),
            ),
            Style::default().fg(DIM),
        ));
        for (index, job) in jobs.iter().enumerate() {
            let kind = match job.kind {
                JobKind::Shell => "Shell",
                JobKind::Subagent => "Subagent",
            };
            let (badge, _) = job_badge(&job.status);
            let last = job
                .lines
                .last()
                .map(|line| single_line(line))
                .unwrap_or_default();
            let selected = index == state.selected;
            let marker = if selected { "▶" } else { " " };
            let text = clip_cells(
                &format!(
                    " {marker} {} [{kind}] {} {badge}  {last}",
                    job.id,
                    single_line(&job.name)
                ),
                inner_width.saturating_sub(1),
            );
            let style = if selected {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(FG)
            };
            body.push(Line::styled(text, style));
        }
        if jobs.is_empty() {
            body.push(Line::default());
            body.push(Line::styled(
                clip_cells(
                    "   暂无后台任务（/bg shell <cmd> 或 /bg run <prompt> 启动）",
                    inner_width.saturating_sub(1),
                ),
                Style::default().fg(MUTED),
            ));
        }
    }
    let window = scroll_window(
        &body,
        content_height,
        state.detail_scroll,
        state.follow_bottom,
    );
    frame.render_widget(
        Paragraph::new(window).style(Style::default().fg(FG)),
        Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(1),
        ),
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            clip_cells(footer_hint, inner_width.saturating_sub(1)),
            Style::default().fg(DIM),
        )),
        Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        ),
    );
}

/// `/bg list` 纯文本输出（与面板列表行同格式）。
fn jobs_list_text(registry: &BackgroundRegistry) -> String {
    let jobs = registry.snapshot();
    if jobs.is_empty() {
        return "无后台任务（/bg shell <cmd> 或 /bg run <prompt> 启动）".into();
    }
    jobs.iter()
        .map(|job| {
            let kind = match job.kind {
                JobKind::Shell => "Shell",
                JobKind::Subagent => "Subagent",
            };
            let (badge, _) = job_badge(&job.status);
            let last = job
                .lines
                .last()
                .map(|line| single_line(line))
                .unwrap_or_default();
            format!("#{} [{kind}] {} {badge}  {last}", job.id, job.name)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn draw_composer(
    frame: &mut Frame,
    area: Rect,
    input: &TextArea<'_>,
    title: &str,
    border: Color,
    secret: bool,
    thinking: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = Style::default().fg(border);
    let gutter = 3.min(area.width.saturating_sub(1) / 2);
    let content_width = area.width.saturating_sub(gutter * 2);
    let content_y = area.y + u16::from(area.height > 1);
    let editor = Rect::new(
        area.x + gutter,
        content_y,
        content_width,
        area.height.saturating_sub(u16::from(area.height > 1)),
    );
    if area.height > 1 {
        let chrome = if area.width >= 6 { 6 } else { 2 };
        let label = clip_cells(title, area.width.saturating_sub(chrome) as usize);
        let fill = area
            .width
            .saturating_sub(chrome)
            .saturating_sub(Line::raw(&label).width() as u16);
        let header_line = if thinking {
            Line::from(vec![
                Span::styled(if chrome == 6 { "╭──" } else { "╭" }, style),
                Span::styled(
                    label,
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{}──╮", "─".repeat(fill as usize)), style),
            ])
        } else {
            Line::from(vec![
                Span::styled(if chrome == 6 { "╭──" } else { "╭" }, style),
                Span::styled(label, style),
                Span::styled(format!("{}──╮", "─".repeat(fill as usize)), style),
            ])
        };
        frame.render_widget(
            Paragraph::new(header_line),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }
    for y in content_y..area.bottom() {
        let last = y + 1 == area.bottom();
        let left = match (last, gutter) {
            (_, 0) => "",
            (true, 1) => "╰",
            (true, 2) => "╰─",
            (true, _) => "╰─ ",
            (false, 1) => "│",
            (false, 2) => "│ ",
            (false, _) => "│  ",
        };
        let right = match (last, gutter) {
            (_, 0) => "",
            (true, 1) => "╯",
            (true, 2) => "─╯",
            (true, _) => " ─╯",
            (false, 1) => "│",
            (false, 2) => " │",
            (false, _) => "  │",
        };
        frame.render_widget(
            Paragraph::new(left).style(style),
            Rect::new(area.x, y, gutter, 1),
        );
        frame.render_widget(
            Paragraph::new(right).style(style),
            Rect::new(area.right().saturating_sub(gutter), y, gutter, 1),
        );
    }
    // TextArea retains its own cursor, Unicode handling and horizontal/vertical viewport.
    if secret {
        frame.render_widget(
            Paragraph::new(
                "*".repeat(
                    input
                        .lines()
                        .join("\n")
                        .chars()
                        .count()
                        .min(content_width as usize),
                ),
            )
            .style(Style::default().fg(FG)),
            editor,
        );
    } else {
        frame.render_widget(input, editor);
    }
}

fn token_count(tokens: u128) -> String {
    if tokens >= 1000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        tokens.to_string()
    }
}
fn effort_label(effort: ThinkingIntensity) -> &'static str {
    match effort.resolve(false) {
        ThinkingIntensity::Middle => "medium",
        ThinkingIntensity::Max => "xhigh",
        other => other.as_str(),
    }
}

fn popup(frame: &mut Frame, bounds: Rect, title: &str, text: &str) {
    if bounds.width < 4 || bounds.height < 3 {
        return;
    }
    let width = (bounds.width as usize * 85 / 100)
        .max(40)
        .min(bounds.width as usize) as u16;
    let paragraph = Paragraph::new(
        text.lines()
            .map(|line| {
                Line::styled(
                    line.to_owned(),
                    if line.starts_with('>') {
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(MUTED)
                    },
                )
            })
            .collect::<Vec<_>>(),
    )
    .wrap(Wrap { trim: false });
    let height = paragraph
        .line_count(width.saturating_sub(2))
        .saturating_add(2)
        .min(bounds.height as usize) as u16;
    let area = Rect::new(
        bounds.x + (bounds.width - width) / 2,
        bounds.y + (bounds.height - height) / 2,
        width,
        height,
    );
    blank_frame_surround(frame, bounds, area);
    frame.render_widget(Clear, area);
    frame.render_widget(
        paragraph.block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(Line::styled(
                    clip_cells_ellipsis(title, (width as usize).saturating_sub(3)),
                    Style::default().fg(ACCENT),
                ))
                .border_style(Style::default().fg(ACCENT)),
        ),
        area,
    );
}

/// 面板外圈留白：把弹窗外扩 1 格的环形区域擦成空白（不越出 `bounds`）。
///
/// 与 Skill 详情弹窗一致——框体四周有留白，对话/列表文字不会紧贴边框，
/// 避免“文字吃掉边框”的观感（标题被角点切断后与右侧正文连成一片）。
fn blank_frame_surround(frame: &mut Frame, bounds: Rect, area: Rect) {
    let ring = Rect {
        x: area.x.saturating_sub(1).max(bounds.x),
        y: area.y.saturating_sub(1).max(bounds.y),
        width: area.width.saturating_add(2).min(
            bounds
                .right()
                .saturating_sub(area.x.saturating_sub(1).max(bounds.x)),
        ),
        height: area.height.saturating_add(2).min(
            bounds
                .bottom()
                .saturating_sub(area.y.saturating_sub(1).max(bounds.y)),
        ),
    };
    if ring.width == 0 || ring.height == 0 {
        return;
    }
    frame.render_widget(Clear, ring);
}

/// 居中弹窗几何（与 Skill 详情一致）：宽/高取 `bounds` 的 85%，宽最小 70、高最小 20，
/// 并夹紧到 `bounds`；区域过小返回 `None`（不渲染弹层）。
fn dialog_area(bounds: Rect) -> Option<Rect> {
    if bounds.width < 40 || bounds.height < 8 {
        return None;
    }
    let width = (bounds.width as usize * 85 / 100)
        .max(70)
        .min(bounds.width as usize) as u16;
    let height = (bounds.height as usize * 85 / 100)
        .max(20)
        .min(bounds.height as usize) as u16;
    Some(Rect::new(
        bounds.x + (bounds.width - width) / 2,
        bounds.y + (bounds.height - height) / 2,
        width,
        height,
    ))
}

/// 表单字段中文标签与是否必填；未列出的字段回退英文原名并视为可选。
fn form_field_label(name: &str) -> (&str, bool) {
    match name {
        "key" => ("变量名称 (key)", true),
        "value" => ("变量内容 (value)", false),
        "sensitive" => ("脱敏保护 (sensitive)", false),
        "enabled" => ("启用状态 (enabled)", false),
        "scope" => ("作用域 (scope)", false),
        "prompt" => ("规则提示 (prompt)", true),
        "target" => ("扫描目标 (本地路径或提示词，留空=自动探测本机工具)", false),
        "provider" => ("扫描服务商 (←/→ 切换)", false),
        "model" => ("扫描模型 (Enter 打开模型列表，可直接输入)", false),
        "preview" => ("仅预览不写盘 (preview)", false),
        other => (other, false),
    }
}

/// 表单弹层：居中模态框（视觉基准 `draw_skill_detail_modal`）。正文窗口化并跟随
/// 当前字段滚动；值一律取自实时输入缓冲 `form.inputs`，不读取尚未 `capture()` 的
/// `form.form.fields[i].value`。
fn draw_form_dialog(frame: &mut Frame, bounds: Rect, form: &FormState, status: &str) {
    let Some(area) = dialog_area(bounds) else {
        return;
    };
    blank_frame_surround(frame, bounds, area);
    frame.render_widget(Clear, area);

    let title_str = match &form.form.kind {
        FormKind::EnvVar { index: Some(_), .. } => {
            format!(" 🔑 编辑环境变量 / {} ", clean(&form.form.title))
        }
        FormKind::EnvVar { index: None, .. } => {
            " 🔑 新增环境变量 / Add Environment Variable ".to_string()
        }
        FormKind::MemoryRule { index: Some(_) } => {
            " 🧠 编辑记忆规则 / Edit Memory Rule ".to_string()
        }
        FormKind::MemoryRule { index: None } => " 🧠 新增记忆规则 / Add Memory Rule ".to_string(),
        FormKind::ToolboxScan => " 🤖 AI 智能扫描本地安全工具 / Toolbox Scan ".to_string(),
        // Provider 表单由 `draw_fullscreen_provider_form` 全屏分支拦截，此处仅作兜底。
        _ => format!(" {} ", clean(&form.form.title)),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .title(Line::styled(
            title_str,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);

    let count = form.form.fields.len();
    let selected = form.selected;
    let sel_bg = Color::Rgb(38, 79, 120);
    let current_label = form
        .form
        .fields
        .get(selected)
        .map(|field| form_field_label(&field.name).0)
        .unwrap_or("");

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut focused_start = 0usize;
    let mut focused_end = 0usize;

    lines.push(Line::from(vec![
        Span::styled("  当前字段: ", Style::default().fg(MUTED)),
        Span::styled(
            current_label.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  · 共 {count} 个字段"), Style::default().fg(DIM)),
    ]));
    lines.push(Line::styled(
        "  Tab/Shift+Tab 切换字段 · ↑/↓ 穿梭 · ←/→ 或空格 切换可选项 · Enter 下一项/保存 · Ctrl+S 立即保存 · Esc 取消",
        Style::default().fg(DIM),
    ));
    lines.push(Line::raw(""));
    let div_w = (chunks[0].width as usize).saturating_sub(4).max(10);
    lines.push(Line::styled(
        format!("  {}", "─".repeat(div_w)),
        Style::default().fg(DIM),
    ));
    lines.push(Line::styled(
        "  ✏️ 字段配置:",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::raw(""));

    for (i, field) in form.form.fields.iter().enumerate() {
        let start_line = lines.len();
        let is_sel = i == selected;
        let (cn_label, required) = form_field_label(&field.name);

        let ptr_style = if is_sel {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        };
        let label_style = if is_sel {
            Style::default().fg(FG).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        };
        let req_badge = if required {
            Span::styled(" [必填]", Style::default().fg(Color::Rgb(255, 120, 120)))
        } else {
            Span::styled(" [可选]", Style::default().fg(DIM))
        };
        lines.push(Line::from(vec![
            Span::styled(if is_sel { "▸ " } else { "  " }, ptr_style),
            Span::styled(cn_label.to_string(), label_style),
            req_badge,
        ]));

        // 值取自实时输入缓冲；`single_line` 保证多行输入不把换行注入单元格。
        let raw_val = form
            .inputs
            .get(i)
            .map(|input| input.lines().join("\n"))
            .unwrap_or_default();
        let val = single_line(&raw_val);
        let val_key = val.trim().to_ascii_lowercase();
        let is_true = matches!(val_key.as_str(), "true" | "1" | "yes");
        let toggle_hint = || Span::styled("   (←/→/空格 切换)", Style::default().fg(ACCENT));

        let mut spans = vec![Span::raw("    ")];
        match field.name.as_str() {
            "sensitive" => {
                if is_true {
                    spans.push(Span::styled("🔒 [脱敏保护]", Style::default().fg(MUTED)));
                } else {
                    spans.push(Span::styled("🔓 [明文公开]", Style::default().fg(DIM)));
                }
                if is_sel {
                    spans.push(toggle_hint());
                }
            }
            "enabled" => {
                if is_true {
                    spans.push(Span::styled(
                        "[ ● 开启 ]",
                        Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
                    ));
                } else {
                    spans.push(Span::styled("[ ○ 关闭 ]", Style::default().fg(DIM)));
                }
                if is_sel {
                    spans.push(toggle_hint());
                }
            }
            "scope" => {
                let (text, style) = match val_key.as_str() {
                    "both" => ("[ 全局与项目 (both) ]", Style::default().fg(CODE)),
                    "project" => ("[ 项目级 (project) ]", Style::default().fg(SUCCESS)),
                    _ => ("[ 全局 (global) ]", Style::default().fg(MUTED)),
                };
                spans.push(Span::styled(text, style));
                if is_sel {
                    spans.push(toggle_hint());
                }
            }
            _ if field.secret => {
                if raw_val.is_empty() {
                    spans.push(Span::styled("（留空）", Style::default().fg(DIM)));
                } else {
                    spans.push(Span::styled(
                        "*".repeat(raw_val.chars().count().min(32)),
                        Style::default().fg(CODE),
                    ));
                }
            }
            _ => {
                if val.is_empty() {
                    spans.push(Span::styled("（未填写）", Style::default().fg(DIM)));
                } else {
                    spans.push(Span::styled(val, Style::default().fg(FG)));
                }
            }
        }
        lines.push(Line::from(spans).style(if is_sel {
            Style::default().bg(sel_bg)
        } else {
            Style::default()
        }));

        if is_sel {
            focused_start = start_line;
            focused_end = start_line + 1;
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(
        match &form.form.kind {
            FormKind::ToolboxScan => {
                "  模型行按 Enter 打开模型列表（↑/↓ 选择 · Enter 确认 · m/f 手输模型名 · r 重新拉取）；保存后立即开始扫描"
            }
            _ => "  [必填] 该项不可为空   [←/→/空格] 就地切换布尔与作用域",
        },
        Style::default().fg(DIM),
    ));

    render_scrollable_content(frame, chunks[0], lines, focused_start, focused_end);
    frame.render_widget(
        Paragraph::new(Line::styled(
            clean(status),
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        )),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            " [Tab/⇧Tab] 切换字段 · [↑/↓] 穿梭 · [Enter] 下一项/保存 · [Ctrl+S] 保存 · [Esc] 取消 ",
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        )),
        chunks[2],
    );

    // 工具库扫描表单的模型列表浮层（Provider 表单由全屏渲染函数负责）。
    if let Some(p) = form.model_picker.as_ref().filter(|p| !p.manual) {
        draw_form_model_picker(frame, bounds, p);
    }
}

/// 选择器弹层：居中模态框，正文为选项列表（选中项高亮 + 详情右对齐）。
fn draw_picker_dialog(
    frame: &mut Frame,
    bounds: Rect,
    picker: &CommandPicker,
    selected: usize,
    delete_pending: bool,
    status: &str,
) {
    use unicode_width::UnicodeWidthStr;

    let Some(area) = dialog_area(bounds) else {
        return;
    };
    blank_frame_surround(frame, bounds, area);
    frame.render_widget(Clear, area);

    let icon = match picker.kind {
        PickerKind::Models => "🤖",
        PickerKind::Sessions => "💬",
        PickerKind::General => "📋",
    };
    let title_str = format!(
        " {icon} {} [{}/{}] ",
        clean(&picker.title),
        if picker.items.is_empty() {
            0
        } else {
            selected + 1
        },
        picker.items.len()
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .title(Line::styled(
            title_str,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);

    let current = picker
        .items
        .get(selected)
        .map(|item| item.label.clone())
        .unwrap_or_else(|| "（无可用选项）".to_string());

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut focused_start = 0usize;
    let mut focused_end = 0usize;

    lines.push(Line::from(vec![
        Span::styled("  当前选中: ", Style::default().fg(MUTED)),
        Span::styled(
            current.clone(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  · 共 {} 个选项", picker.items.len()),
            Style::default().fg(DIM),
        ),
    ]));
    if let Some(item) = picker.items.get(selected) {
        if !item.detail.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("  说明: ", Style::default().fg(MUTED)),
                Span::styled(item.detail.clone(), Style::default().fg(FG)),
            ]));
        }
    }
    lines.push(Line::raw(""));
    let div_w = (chunks[0].width as usize).saturating_sub(4).max(10);
    lines.push(Line::styled(
        format!("  {}", "─".repeat(div_w)),
        Style::default().fg(DIM),
    ));
    lines.push(Line::styled(
        "  📋 选项列表 (↑/↓ 选择 · Enter 确认):",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::raw(""));

    if picker.items.is_empty() {
        lines.push(Line::styled("  （无可用选项）", Style::default().fg(DIM)));
        focused_start = 0;
        focused_end = 0;
    } else {
        for (i, item) in picker.items.iter().enumerate() {
            let row_line = lines.len();
            let is_sel = i == selected;
            let pointer = if is_sel { "▸ " } else { "  " };
            let ptr_style = if is_sel {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(MUTED)
            };
            let label_style = if is_sel {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(FG)
            };

            let mut spans = vec![
                Span::styled(pointer, ptr_style),
                Span::styled(item.label.clone(), label_style),
            ];
            let mut used = pointer.width() + item.label.width();

            if matches!(picker.kind, PickerKind::Models)
                && (item.label.contains("当前使用") || item.label.contains("✓ 当前"))
            {
                const CURRENT_BADGE: &str = " [ 当前启用 ] ";
                spans.push(Span::styled(
                    CURRENT_BADGE,
                    Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
                ));
                used += CURRENT_BADGE.width();
            }
            if delete_pending && is_sel {
                const DELETE_BADGE: &str = "  [待删除!]";
                spans.push(Span::styled(
                    DELETE_BADGE,
                    Style::default().fg(ERROR).add_modifier(Modifier::BOLD),
                ));
                used += DELETE_BADGE.width();
            }

            if !item.detail.is_empty() {
                let pad = (chunks[0].width as usize).saturating_sub(used + item.detail.width() + 2);
                // 剩余宽度不足时整列省略 detail，绝不挤压右边框。
                if pad > 0 {
                    spans.push(Span::raw(" ".repeat(pad)));
                    spans.push(Span::styled(
                        item.detail.clone(),
                        Style::default().fg(MUTED),
                    ));
                }
            }

            lines.push(Line::from(spans));
            if is_sel {
                focused_start = row_line;
                focused_end = row_line;
            }
        }
    }

    render_scrollable_content(frame, chunks[0], lines, focused_start, focused_end);
    frame.render_widget(
        Paragraph::new(Line::styled(
            clean(status),
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        )),
        chunks[1],
    );
    let legend = if delete_pending {
        " [d] 再按一次确认删除 · [↑/↓] 切换目标 · [Esc] 取消 "
    } else if matches!(picker.kind, PickerKind::Sessions) {
        " [↑/↓] 选择 · [Enter] 打开会话 · [n] 新建会话 · [d] 删除会话 · [Esc] 关闭 "
    } else {
        " [↑/↓] 选择 · [Enter] 确认 · [Esc] 关闭 "
    };
    frame.render_widget(
        Paragraph::new(Line::styled(
            legend,
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        )),
        chunks[2],
    );
}

const SETTINGS_THEMES: &[&str] = &[
    "catppuccin",
    "cyberpunk",
    "dracula",
    "gruvbox",
    "nord",
    "tokyo-night",
];
const SETTINGS_MODES: &[&str] = &["chat", "workflow", "dashboard"];
const SETTINGS_LOG_LEVELS: &[&str] = &["error", "warn", "info", "debug", "trace"];
const SETTINGS_THINKING_LEVELS: &[ThinkingIntensity] = &[
    ThinkingIntensity::Low,
    ThinkingIntensity::Middle,
    ThinkingIntensity::High,
    ThinkingIntensity::Max,
    ThinkingIntensity::Auto,
];
const SETTINGS_PERMISSION_MODES: &[PermissionMode] = &[
    PermissionMode::Auto,
    PermissionMode::Manual,
    PermissionMode::Unlimited,
];

/// 焦点行指针：编辑态 `✎ `，只读态 `▶ `，未聚焦 `  `。
fn row_pointer(is_sel: bool, editing: bool) -> &'static str {
    match (is_sel, editing) {
        (true, true) => "✎ ",
        (true, false) => "▶ ",
        _ => "  ",
    }
}

fn render_setting_row(
    selected: bool,
    editing: bool,
    label: &'static str,
    value: String,
    hint: String,
    width: u16,
) -> Line<'static> {
    let pointer = row_pointer(selected, editing);
    let ptr_style = if selected {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(DIM)
    };
    let label_style = if selected {
        Style::default().fg(FG).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(FG)
    };
    let val_style = if selected {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Rgb(180, 220, 255))
    };

    let max_row_w = (width as usize).saturating_sub(1);
    if max_row_w < 51 {
        let label_w = max_row_w.saturating_sub(2).min(26);
        let val_w = max_row_w.saturating_sub(2 + label_w).min(22);
        return Line::from(vec![
            Span::styled(pointer, ptr_style),
            Span::styled(clip_cells(label, label_w), label_style),
            Span::styled(clip_cells(&value, val_w), val_style),
        ]);
    }

    // 指针(2) + 标签(26) + 数值(22) + 空格(1) = 51 列
    const PREFIX_W: usize = 2 + 26 + 22 + 1;
    let label_padded = pad_cells(label, 26);
    let val_padded = pad_cells(&clip_cells(&value, 22), 22);

    let mut spans = vec![
        Span::styled(pointer, ptr_style),
        Span::styled(label_padded, label_style),
        Span::styled(val_padded, val_style),
    ];

    let hint_budget = max_row_w.saturating_sub(PREFIX_W);
    if hint_budget > 3 && !hint.is_empty() {
        spans.push(Span::raw(" "));
        let clipped_hint = clip_cells_ellipsis(&hint, hint_budget);
        spans.push(Span::styled(clipped_hint, Style::default().fg(DIM)));
    }

    Line::from(spans)
}

fn render_scrollable_content<'a>(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'a>>,
    focused_start: usize,
    focused_end: usize,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let visible_rows = area.height as usize;
    let total = lines.len();

    if total <= visible_rows {
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }

    let max_scroll = total.saturating_sub(visible_rows);
    let mut scroll = if focused_end >= visible_rows {
        (focused_end + 1)
            .saturating_sub(visible_rows)
            .min(max_scroll)
    } else {
        0
    };
    if focused_start < scroll {
        scroll = focused_start;
    }
    scroll = scroll.min(max_scroll);

    frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), area);
}

fn draw_tab_agent_model(
    frame: &mut Frame,
    area: Rect,
    settings: &CliSettingsState,
    _screen: &CliScreen,
) {
    let prov = &settings.config_draft.agent.default_provider;
    let model = settings
        .providers_draft
        .providers
        .get(prov)
        .map(|p| p.model.clone())
        .unwrap_or_else(|| "未配置".into());
    let perm_str = settings
        .config_draft
        .agent
        .permission_mode
        .as_deref()
        .unwrap_or("auto");
    let perm_mode = PermissionMode::parse(perm_str).unwrap_or(PermissionMode::Auto);
    let prov_count = settings.providers_draft.providers.len();

    let lines = vec![
        render_setting_row(
            settings.selected_row == 0,
            settings.editing && settings.selected_row == 0,
            "默认服务商 (Provider)",
            format!("◄ [ {} ] ►", prov),
            format!("◄/► 切换 (已配置 {} 个 Provider)", prov_count),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 1,
            settings.editing && settings.selected_row == 1,
            "对应模型 (Model)",
            model,
            "由所选 Provider 决定 (/model 细调)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 2,
            settings.editing && settings.selected_row == 2,
            "思考强度 (Thinking)",
            format!(
                "◄ [ {} ] ►",
                settings.config_draft.agent.thinking_intensity.as_str()
            ),
            settings
                .config_draft
                .agent
                .thinking_intensity
                .label()
                .into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 3,
            settings.editing && settings.selected_row == 3,
            "工具审批模式 (Permission)",
            format!("◄ [ {} ] ►", perm_mode.label()),
            match perm_mode {
                PermissionMode::Auto => "常规放行，高危拦截",
                PermissionMode::Manual => "每次调用需确认",
                PermissionMode::Unlimited => "始终自动放行",
            }
            .into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 4,
            settings.editing && settings.selected_row == 4,
            "自动工具调用 (Auto Tools)",
            if settings.config_draft.agent.auto_tool_call {
                "[ ● 开启 ]".into()
            } else {
                "[ ○ 关闭 ]".into()
            },
            "允许模型自主规划并执行终端/文件工具".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 5,
            settings.editing && settings.selected_row == 5,
            "工具执行步数上限 (Max Steps)",
            format!("◄ [ {} 步 ] ►", settings.config_draft.agent.max_steps),
            "◄/► 调整 (范围: 1-1000 步)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 6,
            settings.editing && settings.selected_row == 6,
            "联网搜索与抓取 (Web Search)",
            if settings.config_draft.tools.web_search {
                "[ ● 开启 ]".into()
            } else {
                "[ ○ 关闭 ]".into()
            },
            "启用/禁用 web_fetch 外部网络查询工具".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 7,
            settings.editing && settings.selected_row == 7,
            "自适应识图引擎 (Vision Engine)",
            if settings.config_draft.agent.vision.enabled {
                "[ ● 开启 ]".into()
            } else {
                "[ ○ 关闭 ]".into()
            },
            "遇图像输入时自适应探测模型能力或图生文解析".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 8,
            settings.editing && settings.selected_row == 8,
            "识图专用服务商 (Vision Provider)",
            format!(
                "◄ [ {} ] ►",
                if settings.config_draft.agent.vision.provider.is_empty() {
                    "自动选择"
                } else {
                    &settings.config_draft.agent.vision.provider
                }
            ),
            "◄/► 切换用于图生文分析的专属服务商".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 9,
            settings.editing && settings.selected_row == 9,
            "识图专用模型 (Vision Model)",
            if settings.config_draft.agent.vision.model.is_empty() {
                "跟随服务商默认模型".into()
            } else {
                settings.config_draft.agent.vision.model.clone()
            },
            "图生文降级专用视觉模型（/vision model 细调）".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 10,
            settings.editing && settings.selected_row == 10,
            "异常重试次数 (Retry Attempts)",
            format!("◄ [ {} 次 ] ►", settings.config_draft.agent.retry_attempts),
            "◄/► 调整 (范围: 0-20 次，0 为不重试)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 11,
            settings.editing && settings.selected_row == 11,
            "重试时间间隔 (Retry Interval)",
            format!(
                "◄ [ {} 秒 ] ►",
                settings.config_draft.agent.retry_delay_secs
            ),
            "◄/► 调整 (范围: 1-60 秒)".into(),
            area.width,
        ),
    ];

    render_scrollable_content(
        frame,
        area,
        lines,
        settings.selected_row,
        settings.selected_row,
    );
}

fn draw_tab_ui_workflow(
    frame: &mut Frame,
    area: Rect,
    settings: &CliSettingsState,
    _screen: &CliScreen,
) {
    let lines = vec![
        render_setting_row(
            settings.selected_row == 0,
            settings.editing && settings.selected_row == 0,
            "主题配色 (Theme)",
            format!("◄ [ {} ] ►", settings.config_draft.ui.theme),
            "即时生效 (支持 cyberpunk/dracula/nord 等)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 1,
            settings.editing && settings.selected_row == 1,
            "鼠标捕获 (Mouse Capture)",
            if settings.config_draft.ui.mouse {
                "[ ● 开启 ]".into()
            } else {
                "[ ○ 关闭 ]".into()
            },
            "即时生效 (开启滚轮滚动，关闭划词复制)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 2,
            settings.editing && settings.selected_row == 2,
            "默认启动模式 (Default Mode)",
            format!("◄ [ {} ] ►", settings.config_draft.ui.default_mode),
            "重启生效 (chat / workflow / dashboard)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 3,
            settings.editing && settings.selected_row == 3,
            "界面动效渲染 (Animations)",
            if settings.config_draft.ui.animations {
                "[ ● 开启 ]".into()
            } else {
                "[ ○ 关闭 ]".into()
            },
            "启用流式字符光标渐变与平滑过渡".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 4,
            settings.editing && settings.selected_row == 4,
            "工作流最大并行节点",
            format!(
                "◄ [ {} 节点 ] ►",
                settings.config_draft.workflow.max_parallel_nodes
            ),
            "◄/► 调整 (Workflow 编排最大并行度: 1-64)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 5,
            settings.editing && settings.selected_row == 5,
            "工作流执行超时 (Timeout)",
            format!(
                "◄ [ {} 秒 ] ►",
                settings.config_draft.workflow.default_timeout_secs
            ),
            "单个工作流最大允许运行时间 (10-86400秒)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 6,
            settings.editing && settings.selected_row == 6,
            "断点续跑检查点 (Checkpoint)",
            if settings.config_draft.workflow.checkpoint {
                "[ ● 开启 ]".into()
            } else {
                "[ ○ 关闭 ]".into()
            },
            "允许从失败或中断的工作流节点继续恢复".into(),
            area.width,
        ),
    ];

    render_scrollable_content(
        frame,
        area,
        lines,
        settings.selected_row,
        settings.selected_row,
    );
}

fn draw_tab_subagents(frame: &mut Frame, area: Rect, settings: &CliSettingsState) {
    let sub = &settings.config_draft.agent.subagents;
    let lines = vec![
        render_setting_row(
            settings.selected_row == 0,
            settings.editing && settings.selected_row == 0,
            "子代理系统 (Subagents)",
            if sub.enabled {
                "[ ● 开启 ]".into()
            } else {
                "[ ○ 关闭 ]".into()
            },
            "暴露 delegate_tasks 批量任务拆解工具".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 1,
            settings.editing && settings.selected_row == 1,
            "单轮最大任务数 (Max Tasks)",
            format!("◄ [ {} 个 ] ►", sub.max_tasks),
            "一次最多拆分派发的子任务上限 (1-64)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 2,
            settings.editing && settings.selected_row == 2,
            "最大并行执行数 (Parallel)",
            format!("◄ [ {} 并发 ] ►", sub.max_parallel),
            "后台同时运行的独立子代理线程上限 (1-16)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 3,
            settings.editing && settings.selected_row == 3,
            "单任务超时时限 (Timeout)",
            format!("◄ [ {} 秒 ] ►", sub.timeout_secs),
            "单个子任务最大执行时限 (10-3600秒)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 4,
            settings.editing && settings.selected_row == 4,
            "子任务最大步数 (Sub Steps)",
            format!("◄ [ {} 步 ] ►", sub.max_steps),
            "单个子任务工具调用最大循环次数 (5-100)".into(),
            area.width,
        ),
    ];

    render_scrollable_content(
        frame,
        area,
        lines,
        settings.selected_row,
        settings.selected_row,
    );
}

fn draw_tab_tools_mcp(
    frame: &mut Frame,
    area: Rect,
    settings: &CliSettingsState,
    screen: &CliScreen,
) {
    let extra_path_str = if settings.config_draft.tools.extra_path.is_empty() {
        "(空)".to_string()
    } else {
        settings.config_draft.tools.extra_path.join(";")
    };

    let mut lines = Vec::new();
    let mut focused_start = 0;
    let mut focused_end = 0;

    let row0_start = lines.len();
    lines.push(render_setting_row(
        settings.selected_row == 0,
        settings.editing && settings.selected_row == 0,
        "优先容器执行 (Docker)",
        if settings.config_draft.tools.prefer_docker {
            "[ ● 开启 ]".into()
        } else {
            "[ ○ 关闭 ]".into()
        },
        "若环境安装 Docker，则优先容器隔离执行".into(),
        area.width,
    ));
    if settings.selected_row == 0 {
        focused_start = row0_start;
        focused_end = lines.len().saturating_sub(1);
    }

    let row1_start = lines.len();
    lines.push(render_setting_row(
        settings.selected_row == 1,
        settings.editing && settings.selected_row == 1,
        "CTF 渗透答题模式 (CTF Mode)",
        if screen.ctf_enabled {
            "[ ● 开启 ]".into()
        } else {
            "[ ○ 关闭 ]".into()
        },
        "启用 Writeup 自动生成与专属解题工具".into(),
        area.width,
    ));
    if settings.selected_row == 1 {
        focused_start = row1_start;
        focused_end = lines.len().saturating_sub(1);
    }

    let row2_start = lines.len();
    lines.push(render_setting_row(
        settings.selected_row == 2,
        false,
        "MCP 全屏管理控制台 (/mcp)",
        "[ 按 Enter 打开 ]".into(),
        "打开独立全屏 MCP 控制台，实时调试工具 Schema、测活与增删服务".into(),
        area.width,
    ));
    if settings.selected_row == 2 {
        focused_start = row2_start;
        focused_end = lines.len().saturating_sub(1);
    }

    lines.push(render_setting_row(
        false,
        false,
        "额外环境变量 PATH",
        extra_path_str,
        "附加注入到 Shell 子进程的 PATH 变量".into(),
        area.width,
    ));
    lines.push(Line::raw(""));

    let total_mcp_tools: usize = settings.mcp_servers.iter().map(|s| s.tool_count).sum();
    lines.push(Line::from(vec![Span::styled(
        format!(
            "  [已配置 MCP 服务集群概要 (总计: {} 个服务 · {} 个工具)]",
            settings.mcp_servers.len(),
            total_mcp_tools
        ),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )]));
    if settings.mcp_servers.is_empty() {
        lines.push(Line::styled(
            "    • 暂无配置 MCP 服务（可通过 mcp.json 或 /mcp 添加）",
            Style::default().fg(DIM),
        ));
    } else {
        for s in settings.mcp_servers.iter().take(3) {
            let status_badge = if s.connected {
                if s.tool_count == 0 {
                    Span::styled(" [ ● 已连接 (0 工具) ] ", Style::default().fg(ACCENT))
                } else {
                    Span::styled(
                        format!(" [ ● 已连接 ({} 工具) ] ", s.tool_count),
                        Style::default().fg(SUCCESS),
                    )
                }
            } else {
                Span::styled(" [ ○ 未连接 ] ", Style::default().fg(DIM))
            };
            lines.push(Line::from(vec![
                Span::raw("    • "),
                Span::styled(
                    format!("{:<14}", s.name),
                    Style::default().fg(FG).add_modifier(Modifier::BOLD),
                ),
                status_badge,
                Span::raw(" "),
                Span::styled(&s.detail, Style::default().fg(DIM)),
            ]));
            if s.connected && s.tool_count == 0 {
                lines.push(Line::styled(
                    "      提示: 0 个工具。若该服务为 SSE 端点，请将 transport 改为 sse",
                    Style::default().fg(ACCENT),
                ));
            }
        }
    }

    lines.push(Line::raw(""));

    let skills_header = format!(
        "  [已加载 Skills 技能 ({} 个)] (↑/↓ 移动焦点 · Enter 查看详情与 SKILL.md 说明)",
        settings.skills.len()
    );
    lines.push(Line::from(vec![Span::styled(
        skills_header,
        Style::default().fg(CODE).add_modifier(Modifier::BOLD),
    )]));

    if settings.skills.is_empty() {
        lines.push(Line::styled(
            "    • 暂无已加载 Skill",
            Style::default().fg(DIM),
        ));
    } else {
        for (i, s) in settings.skills.iter().enumerate() {
            let is_sel = settings.selected_row == 3 + i;
            let start_line = lines.len();

            let pointer = if is_sel { "▶ " } else { "  " };
            let ptr_style = if is_sel {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM)
            };
            let source_badge = match s.source.as_str() {
                "全局" => Span::styled(" [全局] ", Style::default().fg(CODE)),
                _ => Span::styled(" [项目级] ", Style::default().fg(SUCCESS)),
            };
            let manual_tag = if s.disable_model_invocation {
                Span::styled(" [仅显式] ", Style::default().fg(DIM))
            } else {
                Span::raw("")
            };

            let enter_hint = if is_sel {
                Span::styled(
                    "  [按 Enter 查看详情]",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("")
            };

            let desc_preview = if s.description.is_empty() {
                String::new()
            } else {
                format!(" {}", s.description)
            };

            lines.push(Line::from(vec![
                Span::styled(pointer, ptr_style),
                Span::styled(
                    format!("{:<30}", s.name),
                    if is_sel {
                        Style::default().fg(FG).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(FG)
                    },
                ),
                source_badge,
                manual_tag,
                Span::styled(desc_preview, Style::default().fg(MUTED)),
                enter_hint,
            ]));

            let end_line = lines.len().saturating_sub(1);
            if is_sel {
                focused_start = start_line;
                focused_end = end_line;
            }
        }
    }

    render_scrollable_content(frame, area, lines, focused_start, focused_end);
}

fn draw_tab_providers(frame: &mut Frame, area: Rect, settings: &CliSettingsState) {
    let names = settings.providers_draft.sorted_names();
    let default_prov = &settings.config_draft.agent.default_provider;
    let total_provs = names.len();

    let header_text = if total_provs > 1 {
        format!(
            "  已配置服务商列表 [{}/{} 项 · ↑/↓ 切换焦点] (按 Enter 设为默认 · A 预设/协议添加 · T 连通测活 · M 模型 · E 编辑 · D 删除):",
            (settings.selected_row + 1).min(total_provs),
            total_provs
        )
    } else {
        "  已配置服务商列表 (按 Enter 设为默认 · A 预设/协议添加 · T 连通测活 · M 模型 · E 编辑 · D 删除):".to_string()
    };

    let mut lines = vec![
        Line::styled(header_text, Style::default().fg(MUTED)),
        Line::raw(""),
    ];

    let mut focused_start = 0;
    let mut focused_end = 0;

    if names.is_empty() {
        lines.push(Line::styled(
            "    (暂无配置服务商，按 A 添加)",
            Style::default().fg(DIM),
        ));
    } else {
        for (i, name) in names.iter().enumerate() {
            let is_sel = i == settings.selected_row;
            let is_def = name == default_prov;

            let start_line = lines.len();

            let pointer = if is_sel { "▶ " } else { "  " };
            let ptr_style = if is_sel {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM)
            };
            let star = if is_def { "★ " } else { "☆ " };
            let star_style = if is_def {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM)
            };
            let def_tag = if is_def { " (默认服务商)" } else { "" };
            let name_style = if is_sel {
                Style::default().fg(FG).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(FG)
            };

            lines.push(Line::from(vec![
                Span::styled(pointer, ptr_style),
                Span::styled(star, star_style),
                Span::styled(format!("[ {} ]", name), name_style),
                Span::styled(def_tag, Style::default().fg(ACCENT)),
            ]));

            if let Some(p) = settings.providers_draft.providers.get(name) {
                let detail = format!(
                    "      类型: {} · 模型: {} · BaseURL: {}",
                    p.kind, p.model, p.base_url
                );
                lines.push(Line::styled(detail, Style::default().fg(DIM)));
            }
            if let Some((is_ok, msg, rtt)) = settings.provider_test_results.get(name) {
                let (icon, status_label, color) = if *is_ok {
                    ("🟢", "连通正常", SUCCESS)
                } else {
                    ("🔴", "连通失败", ERROR)
                };
                let test_line =
                    format!("      测速: {icon} {status_label} (延迟: {rtt}ms · {msg})");
                lines.push(Line::styled(test_line, Style::default().fg(color)));
            }
            lines.push(Line::raw(""));

            let end_line = lines.len();
            if is_sel {
                focused_start = start_line;
                focused_end = end_line;
            }
        }
    }

    render_scrollable_content(frame, area, lines, focused_start, focused_end);
}

/// 「8. 工具库」页显示行 → 自定义工具下标。
///
/// 行 `0` 固定是 `🤖 AI 智能扫描本地安全工具` 入口，其后 `1..=N` 依次对应
/// `custom_tools[0..N]`；扫描行与越界行返回 `None`。
fn toolbox_tool_row(settings: &CliSettingsState) -> Option<usize> {
    settings
        .selected_row
        .checked_sub(1)
        .filter(|idx| *idx < settings.custom_tools.len())
}

/// 「8. 工具库」页：AI 扫描入口行 + 自定义工具列表。
fn draw_tab_toolbox(
    frame: &mut Frame,
    area: Rect,
    settings: &CliSettingsState,
    _screen: &CliScreen,
) {
    let total = settings.custom_tools.len();
    let mut lines = vec![
        Line::styled(
            format!(
                "  自定义安全工具 (Custom Tools) [{} 项] · 手动录入 / 删除 / AI 智能扫描",
                total
            ),
            Style::default().fg(MUTED),
        ),
        Line::raw(""),
    ];

    let mut focused_start = 0;
    let mut focused_end = 0;

    // 首行固定为 AI 智能扫描入口（Enter 打开扫描表单），自定义工具列表在其后。
    let scan_sel = settings.selected_row == 0;
    let scan_start = lines.len();
    let scan_pointer = if scan_sel { "▶ " } else { "  " };
    let scan_style = if scan_sel {
        Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED)
    };
    lines.push(Line::from(crate::views::clipped_spans(
        vec![
            Span::styled(
                scan_pointer,
                if scan_sel {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(DIM)
                },
            ),
            Span::styled("🤖 AI 智能扫描本地安全工具", scan_style),
            Span::styled(
                " (Enter 打开扫描表单：可输入路径/提示词并选择模型)",
                Style::default().fg(DIM),
            ),
        ],
        area.width as usize,
    )));
    if scan_sel {
        focused_start = scan_start;
        focused_end = lines.len();
    }
    lines.push(Line::raw(""));

    if total == 0 {
        lines.push(Line::styled(
            "    （暂无自定义工具：按 A 手动录入）",
            Style::default().fg(DIM),
        ));
    }

    for (i, tool) in settings.custom_tools.iter().enumerate() {
        let is_sel = toolbox_tool_row(settings) == Some(i);
        let start_line = lines.len();
        let pointer = if is_sel { "▶ " } else { "  " };
        let ptr_style = if is_sel {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };
        let name_style = if is_sel {
            Style::default().fg(FG).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(FG)
        };
        let delete_tag = if settings.tools_pending_delete == Some(i) {
            Span::styled(
                "  [待删除!]",
                Style::default().fg(ERROR).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("")
        };

        let spans = crate::views::clipped_spans(
            vec![
                Span::styled(pointer, ptr_style),
                Span::styled(format!("[{}] ", tool.name), name_style),
                // 描述可能来自模型生成，含换行/控制字符时必须在单行内归一化，
                // 否则整行会被后续字段挤掉。
                Span::styled(single_line(&tool.description), Style::default().fg(MUTED)),
                Span::styled(
                    format!("  · {}", single_line(&tool.command)),
                    Style::default().fg(DIM),
                ),
                delete_tag,
            ],
            area.width as usize,
        );
        lines.push(Line::from(spans));
        lines.push(Line::raw(""));

        if is_sel {
            focused_start = start_line;
            focused_end = lines.len();
        }
    }

    render_scrollable_content(frame, area, lines, focused_start, focused_end);
}

fn draw_tab_env_memory(frame: &mut Frame, area: Rect, settings: &CliSettingsState) {
    let mut lines = Vec::new();
    let env_count = settings.config_draft.env.vars.len();
    let mem_count = settings.config_draft.memory.rules.len();
    let env_slots = env_count.max(1);
    let mem_slots = mem_count.max(1);
    let is_env_focused = settings.selected_row < env_slots;
    let is_mem_focused =
        settings.selected_row >= env_slots && settings.selected_row < env_slots + mem_slots;
    let mem_list_row_base = env_slots + mem_slots;
    let is_memlist_focused =
        settings.total_memories() > 0 && settings.selected_row >= mem_list_row_base;

    let mut focused_start = 0;
    let mut focused_end = 0;

    // 1. 环境变量分区标题
    let env_header = if is_env_focused {
        format!(
            "  ⚙ 自定义环境变量 (注入 Shell/Agent 子进程) [{} 项 · 当前聚焦] (A 添加 · Enter 编辑 · 空格 脱敏 · E 表单 · D 删除):",
            env_count
        )
    } else {
        format!(
            "  ⚙ 自定义环境变量 (注入 Shell/Agent 子进程) [{} 项] (A 添加 · Enter 编辑 · 空格 脱敏 · E 表单 · D 删除):",
            env_count
        )
    };
    lines.push(Line::styled(
        env_header,
        if is_env_focused {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        },
    ));

    if env_count == 0 {
        let is_sel = is_env_focused && settings.selected_row == 0;
        let start_line = lines.len();
        let pointer = row_pointer(is_sel, settings.editing && is_sel);
        let ptr_style = if is_sel {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };
        lines.push(Line::from(vec![
            Span::styled(pointer, ptr_style),
            Span::styled(
                "(暂无环境变量，按 A 添加)",
                if is_sel {
                    Style::default().fg(FG).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(DIM)
                },
            ),
        ]));
        if is_sel {
            focused_start = start_line;
            focused_end = start_line;
        }
    } else {
        for (i, var) in settings.config_draft.env.vars.iter().enumerate() {
            let is_sel = is_env_focused && i == settings.selected_row;
            let start_line = lines.len();
            let pointer = row_pointer(is_sel, settings.editing && is_sel);
            let ptr_style = if is_sel {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM)
            };
            let idx_badge = Span::styled(
                format!("[ {:02} ] ", i + 1),
                Style::default().fg(if is_sel { ACCENT } else { MUTED }),
            );
            let key_span = Span::styled(
                format!("{:<20} ", var.key),
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            );
            let sensitive_badge = if var.sensitive {
                Span::styled("🔒 [脱敏保护]  ", Style::default().fg(MUTED))
            } else {
                Span::styled("🔓 [明文公开]  ", Style::default().fg(DIM))
            };
            let val_display = if var.sensitive {
                "sk-************************"
            } else {
                &var.value
            };
            let val_span = Span::styled(
                val_display,
                Style::default().fg(if var.sensitive { MUTED } else { FG }),
            );
            let hint_span = if is_sel {
                Span::styled(
                    "  [Enter 编辑 · 空格 脱敏 · E 表单 · D 删除]",
                    Style::default().fg(ACCENT),
                )
            } else {
                Span::raw("")
            };
            lines.push(Line::from(vec![
                Span::styled(pointer, ptr_style),
                idx_badge,
                key_span,
                sensitive_badge,
                val_span,
                hint_span,
            ]));
            if is_sel {
                focused_start = start_line;
                focused_end = start_line;
            }
        }
    }

    lines.push(Line::raw(""));

    // 2. 长期记忆规则分区标题
    let mem_header = if is_mem_focused {
        format!(
            "  🧠 长期用户记忆约定规则 (Memory Rules) [{} 项 · 当前聚焦] (A 添加 · Enter 编辑 · 空格 启停 · ←/→ 作用域 · E 表单 · D 删除):",
            mem_count
        )
    } else {
        format!(
            "  🧠 长期用户记忆约定规则 (Memory Rules) [{} 项] (A 添加 · Enter 编辑 · 空格 启停 · ←/→ 作用域 · E 表单 · D 删除):",
            mem_count
        )
    };
    lines.push(Line::styled(
        mem_header,
        if is_mem_focused {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        },
    ));

    if mem_count == 0 {
        let is_sel = is_mem_focused && (settings.selected_row >= env_slots);
        let start_line = lines.len();
        let pointer = row_pointer(is_sel, settings.editing && is_sel);
        let ptr_style = if is_sel {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };
        lines.push(Line::from(vec![
            Span::styled(pointer, ptr_style),
            Span::styled(
                "(暂无记忆规则，按 A 添加)",
                if is_sel {
                    Style::default().fg(FG).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(DIM)
                },
            ),
        ]));
        if is_sel {
            focused_start = start_line;
            focused_end = start_line;
        }
    } else {
        for (i, rule) in settings.config_draft.memory.rules.iter().enumerate() {
            let row_idx = env_slots + i;
            let is_sel = is_mem_focused && row_idx == settings.selected_row;
            let start_line = lines.len();
            let pointer = row_pointer(is_sel, settings.editing && is_sel);
            let ptr_style = if is_sel {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM)
            };
            let idx_badge = Span::styled(
                format!("[ #{:02} ] ", i + 1),
                Style::default().fg(if is_sel { ACCENT } else { MUTED }),
            );
            let status_badge = if rule.enabled {
                Span::styled(
                    "[ ● 开启 ] ",
                    Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled("[ ○ 关闭 ] ", Style::default().fg(DIM))
            };
            let scope_badge = match rule.scope.as_str() {
                "both" => Span::styled("[ 全局与项目 (both) ] ", Style::default().fg(CODE)),
                "project" => Span::styled("[ 项目级 (project) ]    ", Style::default().fg(SUCCESS)),
                _ => Span::styled("[ 全局 (global) ]       ", Style::default().fg(MUTED)),
            };
            let scope_hint = if is_sel {
                Span::styled("(←/→) ", Style::default().fg(DIM))
            } else {
                Span::raw("")
            };
            let prompt_span = Span::styled(
                &rule.prompt,
                Style::default().fg(if rule.enabled { FG } else { DIM }),
            );
            let hint_span = if is_sel {
                Span::styled(
                    "  [Enter 编辑 · E 表单 · D 删除]",
                    Style::default().fg(ACCENT),
                )
            } else {
                Span::raw("")
            };
            lines.push(Line::from(vec![
                Span::styled(pointer, ptr_style),
                idx_badge,
                status_badge,
                scope_badge,
                scope_hint,
                prompt_span,
                hint_span,
            ]));
            if is_sel {
                focused_start = start_line;
                focused_end = start_line;
            }
        }
    }

    lines.push(Line::raw(""));

    // 3. 用户记忆列表分区（全局 + 项目级）
    let total_mem = settings.total_memories();
    let memlist_header = if is_memlist_focused {
        format!(
            "  🧾 用户记忆列表 (Memory List) [{} 条 · 当前聚焦] (↑/↓ 选择 · Enter 查看详情):",
            total_mem
        )
    } else {
        format!(
            "  🧾 用户记忆列表 (Memory List) [{} 条] (↑/↓ 选择 · Enter 查看详情):",
            total_mem
        )
    };
    lines.push(Line::styled(
        memlist_header,
        if is_memlist_focused {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        },
    ));

    let groups: [(&str, &str, &[MemoryEntry], usize); 2] = [
        (
            "全局记忆 (Global)",
            &settings.memory_global_path,
            &settings.memories_global,
            0,
        ),
        (
            "项目级记忆 (Project)",
            &settings.memory_project_path,
            &settings.memories_project,
            settings.memories_global.len(),
        ),
    ];
    for (label, file_hint, entries, flat_base) in groups {
        lines.push(Line::styled(
            format!("  ▸ {} [{} 条] — {}", label, entries.len(), file_hint),
            Style::default().fg(CODE).add_modifier(Modifier::BOLD),
        ));
        if entries.is_empty() {
            lines.push(Line::styled("      (暂无)", Style::default().fg(DIM)));
            continue;
        }
        for (i, entry) in entries.iter().enumerate() {
            let flat = flat_base + i;
            let is_sel = is_memlist_focused && settings.selected_row == mem_list_row_base + flat;
            let start_line = lines.len();
            let pointer = if is_sel { "▶ " } else { "  " };
            let ptr_style = if is_sel {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM)
            };
            let idx_badge = Span::styled(
                format!("[ #{:02} ] ", entry.index),
                Style::default().fg(if is_sel { ACCENT } else { MUTED }),
            );
            let content_span = Span::styled(&entry.content, Style::default().fg(FG));
            let hint_span = if is_sel {
                Span::styled("  [Enter 查看详情]", Style::default().fg(ACCENT))
            } else {
                Span::raw("")
            };
            lines.push(Line::from(vec![
                Span::styled(pointer, ptr_style),
                idx_badge,
                content_span,
                hint_span,
            ]));
            if is_sel {
                focused_start = start_line;
                focused_end = start_line;
            }
        }
    }

    render_scrollable_content(frame, area, lines, focused_start, focused_end);
}

fn draw_tab_storage_system(frame: &mut Frame, area: Rect, settings: &CliSettingsState) {
    let lines = vec![
        render_setting_row(
            settings.selected_row == 0,
            settings.editing && settings.selected_row == 0,
            "会话历史保留天数",
            format!(
                "◄ [ {} 天 ] ►",
                settings.config_draft.storage.history_retention_days
            ),
            "◄/► 调整 (超过天数的会话自动清理)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 1,
            settings.editing && settings.selected_row == 1,
            "日志记录级别 (Log Level)",
            format!("◄ [ {} ] ►", settings.config_draft.storage.log_level),
            "trace / debug / info / warn / error".into(),
            area.width,
        ),
        Line::raw(""),
        Line::styled(
            "  [本地存储路径与状态]",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Line::from(vec![
            Span::raw("    • "),
            Span::styled("配置文件路径 (Config):     ", Style::default().fg(MUTED)),
            Span::styled(&settings.config_path, Style::default().fg(FG)),
            Span::styled(" (已就绪)", Style::default().fg(SUCCESS)),
        ]),
        Line::from(vec![
            Span::raw("    • "),
            Span::styled("服务商密钥文件 (Providers): ", Style::default().fg(MUTED)),
            Span::styled(&settings.providers_path, Style::default().fg(FG)),
            Span::styled(" (已就绪)", Style::default().fg(SUCCESS)),
        ]),
        Line::from(vec![
            Span::raw("    • "),
            Span::styled("会话历史目录 (Sessions):   ", Style::default().fg(MUTED)),
            Span::styled(&settings.sessions_dir, Style::default().fg(FG)),
            Span::styled(
                format!(" (已保存 {} 个会话)", settings.sessions_count),
                Style::default().fg(MUTED),
            ),
        ]),
        Line::from(vec![
            Span::raw("    • "),
            Span::styled("项目级配置覆盖 (Project):  ", Style::default().fg(MUTED)),
            if settings.has_project_config {
                Span::styled("已启用项目级配置覆盖", Style::default().fg(SUCCESS))
            } else {
                Span::styled(
                    "未启用项目级配置 (以全局配置为准)",
                    Style::default().fg(DIM),
                )
            },
        ]),
    ];

    render_scrollable_content(
        frame,
        area,
        lines,
        settings.selected_row,
        settings.selected_row,
    );
}

/// 「关于」页内容行（单一来源）：设置中心「9. 关于」页签与全屏 `Panel::About` 共用。
fn about_lines(info: &AboutInfo) -> Vec<Line<'static>> {
    let section = |title: &str| {
        Line::styled(
            format!("  [{title}]"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    };
    let kv = |key: &str, value: String| {
        Line::from(vec![
            Span::raw("    "),
            Span::styled(format!("{key:<12}"), Style::default().fg(MUTED)),
            Span::styled(value, Style::default().fg(FG)),
        ])
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::styled(
        format!("Cyber Master V{}  ·  网络安全智能体终端", info.version),
        Style::default().fg(FG).add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::styled(
        "对话式安全智能体终端：多 LLM 流式对话、统一工具表（内置 + MCP + Skill）、Todo 任务编排、子 Agent 并发与 CTF 协作面板。",
        Style::default().fg(MUTED),
    ));
    lines.push(Line::raw(""));
    lines.push(section("版本与更新"));
    lines.push(kv("当前版本", format!("V{}", info.version)));
    lines.push(match &info.latest {
        Some(latest) => kv(
            "可更新版本",
            format!("V{latest}（按 U 或输入 /update 检查并升级）"),
        ),
        None => kv("可更新版本", "未检测到新版本（按 U 检查更新）".into()),
    });
    lines.push(kv(
        "可执行文件",
        info.binary.clone().unwrap_or_else(|| "(未知)".into()),
    ));
    lines.push(kv("升级方式", "cyber update（或本页按 U 检查更新）".into()));
    lines.push(Line::raw(""));
    lines.push(section("项目信息"));
    lines.push(kv(
        "简介",
        "网络安全智能体终端：代码交互、任务编排与 CTF 协作".into(),
    ));
    lines.push(kv(
        "仓库",
        format!("https://github.com/{}", cyber_core::update::GITHUB_REPO),
    ));
    lines.push(kv(
        "镜像",
        format!("https://cnb.cool/{}", cyber_core::update::CNB_REPO),
    ));
    lines.push(kv("协议", env!("CARGO_PKG_LICENSE").to_string()));
    lines.push(kv("作者", "chuzouX".into()));
    lines.push(Line::raw(""));
    lines.push(section("运行环境"));
    lines.push(kv(
        "服务商 / 模型",
        format!(
            "{} / {} · {} effort",
            info.provider, info.model, info.effort
        ),
    ));
    lines.push(kv("工作目录", info.cwd.clone()));
    lines.push(kv("会话数", info.sessions_count.to_string()));
    lines.push(kv(
        "项目级配置",
        if info.has_project_config {
            "已启用（.cyber.md / .cyber/）".into()
        } else {
            "未启用（以全局配置为准）".into()
        },
    ));
    lines.push(kv("日志级别", info.log_level.clone()));
    lines.push(kv(
        "内置工具 / Skills",
        format!("{} 个工具 · {} 个 Skill", info.tool_count, info.skill_count),
    ));
    lines.push(kv(
        "自定义工具",
        format!("{} 个（{}）", info.custom_tool_count, info.tools_dir),
    ));
    lines.push(kv("配置文件", info.config_path.clone()));
    lines.push(kv("服务商文件", info.providers_path.clone()));
    lines.push(kv("会话目录", info.sessions_dir.clone()));
    lines.push(Line::raw(""));
    lines.push(section("核心能力"));
    for (name, desc) in [
        (
            "流式对话与思考链",
            "多 Provider（OpenAI / Anthropic / Ollama / Responses）× 思考强度 × 上下文自动压缩",
        ),
        (
            "统一工具表",
            "内置 Shell / 文件 / 搜索工具 + MCP（stdio / HTTP / SSE）+ Skill 渐进式披露",
        ),
        (
            "任务编排",
            "Todo 结构化清单三态视图 + 子 Agent 并发委派 + 后台任务面板",
        ),
        (
            "安全作业",
            "CTF 题目协作与 writeup 归档、自定义安全工具库与 AI 智能扫描",
        ),
        (
            "设置中心",
            "F3 打开：模型 / 界面 / 并发 / 工具 MCP / 服务商 / 环境记忆 / 系统存储 / 工具库 / 关于",
        ),
    ] {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(format!("▸ {name}  "), Style::default().fg(ACCENT)),
            Span::styled(desc.to_string(), Style::default().fg(MUTED)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(section("快捷键说明书"));
    lines.push(Line::styled(
        "    对话页输入框为空时直接生效；面板内按键见各行说明。",
        Style::default().fg(MUTED),
    ));
    for (key, desc) in SHORTCUT_KEYS {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(format!("{key:<16}"), Style::default().fg(CODE)),
            Span::styled((*desc).to_string(), Style::default().fg(FG)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(section("斜杠命令速查"));
    for spec in cli_commands::commands() {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(format!("{:<34}", spec.usage), Style::default().fg(CODE)),
            Span::styled(spec.desc.to_string(), Style::default().fg(MUTED)),
        ]));
    }
    lines
}

/// 设置中心「9. 关于」页：只读长页；`scroll` 为行偏移，按内容高度自动收敛。
fn draw_tab_about(frame: &mut Frame, area: Rect, info: Option<&AboutInfo>, scroll: usize) {
    let Some(info) = info else {
        return;
    };
    let lines = about_lines(info);
    let max = lines.len().saturating_sub(area.height as usize);
    let offset = scroll.min(max).min(u16::MAX as usize) as u16;
    frame.render_widget(Paragraph::new(lines).scroll((offset, 0)), area);
}

/// 全屏「关于 / About」面板：与设置中心「9. 关于」页共用 `about_lines`。
fn draw_about_panel(frame: &mut Frame, area: Rect, info: &AboutInfo, scroll: usize) {
    if area.width < 40 || area.height < 6 {
        return;
    }
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_style(Style::default().fg(CODE))
        .title(Line::from(Span::styled(
            format!(" 关于 / About · Cyber Master V{} ", info.version),
            Style::default().fg(CODE).add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(inner);
    let lines = about_lines(info);
    let max = lines.len().saturating_sub(chunks[0].height as usize);
    let offset = scroll.min(max).min(u16::MAX as usize) as u16;
    frame.render_widget(Paragraph::new(lines).scroll((offset, 0)), chunks[0]);
    frame.render_widget(
        Paragraph::new(Line::styled(
            "U/Enter 检查更新 · ↑/↓ · PgUp/PgDn 滚动 · ? 或 Esc 关闭",
            Style::default().fg(DIM),
        )),
        chunks[1],
    );
}

fn draw_discard_modal(frame: &mut Frame, parent: Rect) {
    let width = 54.min(parent.width);
    let height = 11.min(parent.height);
    let area = Rect::new(
        parent.x + (parent.width.saturating_sub(width)) / 2,
        parent.y + (parent.height.saturating_sub(height)) / 2,
        width,
        height,
    );

    frame.render_widget(Clear, area);

    let block = Block::bordered()
        .border_style(Style::default().fg(AMBER).add_modifier(Modifier::BOLD))
        .title(Line::styled(
            " ⚠️ 未保存的设置修改 ",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::styled(
            "检测到设置已被修改但尚未保存！",
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::styled("是否在关闭前保存这些更改？", Style::default().fg(MUTED)),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                " [ Enter / Ctrl+S 保存生效 ] ",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(" [ Esc 确认放弃并退出 ] ", Style::default().fg(ERROR)),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                " [ 方向键 ←/→ 取消并留在此处 ] ",
                Style::default().fg(MUTED),
            ),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}

/// 清空弹窗区域，并消除底层宽字符在弹窗竖边框处的残影，之后才可安全绘制弹窗。
///
/// ratatui 的缓冲 diff 会跳过宽字符（CJK/emoji）的尾随单元格：若底层文本的宽字符正好
/// 占据弹窗竖边框左侧的单元格，其尾随单元格就是边框本身，边框那一格永远不会下发到终端，
/// 视觉上就是「边框被文字挤掉」。因此把左侧那半格宽字符先重置掉，边框即可完整刷新。
fn clear_popup_under(frame: &mut Frame, area: Rect) {
    use unicode_width::UnicodeWidthStr;

    frame.render_widget(Clear, area);
    if area.x == 0 {
        return;
    }
    let margin_x = area.x - 1;
    let buf = frame.buffer_mut();
    if margin_x >= buf.area.width {
        return;
    }
    let bottom = area.bottom().min(buf.area.height);
    for y in area.y..bottom {
        if UnicodeWidthStr::width(buf[(margin_x, y)].symbol()) > 1 {
            buf[(margin_x, y)].reset();
        }
    }
}

fn draw_skill_detail_modal(
    frame: &mut Frame,
    parent: Rect,
    settings: &CliSettingsState,
    detail: &SkillDetailModal,
) {
    if parent.width < 20 || parent.height < 10 {
        return;
    }

    let skill = match settings.skills.get(detail.skill_index) {
        Some(s) => s,
        None => return,
    };

    let width = (parent.width * 85 / 100).max(70).min(parent.width);
    let height = (parent.height * 85 / 100).max(20).min(parent.height);
    let area = Rect::new(
        parent.x + (parent.width.saturating_sub(width)) / 2,
        parent.y + (parent.height.saturating_sub(height)) / 2,
        width,
        height,
    );

    clear_popup_under(frame, area);

    let title_str = format!(
        " 📖 Skill 详情 [{}/{}]: {} ",
        detail.skill_index + 1,
        settings.skills.len(),
        skill.name
    );
    let title_str = clip_cells_ellipsis(&title_str, (area.width as usize).saturating_sub(3));
    let block = Block::bordered()
        .border_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .title(Line::styled(
            title_str,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

    let mut content_lines = Vec::new();

    let source_badge = match skill.source.as_str() {
        "全局" => Span::styled(" [全局] ", Style::default().fg(CODE)),
        _ => Span::styled(" [项目级] ", Style::default().fg(SUCCESS)),
    };
    let manual_tag = if skill.disable_model_invocation {
        Span::styled(" [仅显式调用] ", Style::default().fg(DIM))
    } else {
        Span::raw("")
    };
    content_lines.push(Line::from(vec![
        Span::styled("  名称: ", Style::default().fg(MUTED)),
        Span::styled(
            &skill.name,
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled("    来源: ", Style::default().fg(MUTED)),
        source_badge,
        manual_tag,
    ]));

    content_lines.push(Line::from(vec![
        Span::styled("  文件: ", Style::default().fg(MUTED)),
        Span::styled(&skill.path, Style::default().fg(DIM)),
    ]));

    let desc_str = if skill.description.is_empty() {
        "(无描述)"
    } else {
        &skill.description
    };
    content_lines.push(Line::from(vec![
        Span::styled("  简介: ", Style::default().fg(MUTED)),
        Span::styled(desc_str, Style::default().fg(FG)),
    ]));

    if !skill.triggers.is_empty() {
        content_lines.push(Line::from(vec![
            Span::styled("  触发词: ", Style::default().fg(MUTED)),
            Span::styled(skill.triggers.join(", "), Style::default().fg(CODE)),
        ]));
    }

    if !skill.tools.is_empty() {
        content_lines.push(Line::from(vec![
            Span::styled("  依赖工具: ", Style::default().fg(MUTED)),
            Span::styled(skill.tools.join(", "), Style::default().fg(ACCENT)),
        ]));
    }

    if !skill.allowed_tools.is_empty() {
        content_lines.push(Line::from(vec![
            Span::styled("  预批准工具: ", Style::default().fg(MUTED)),
            Span::styled(skill.allowed_tools.join(", "), Style::default().fg(SUCCESS)),
        ]));
    }

    content_lines.push(Line::raw(""));
    let div_w = (chunks[0].width as usize).saturating_sub(4).max(10);
    content_lines.push(Line::styled(
        format!("  {}", "─".repeat(div_w)),
        Style::default().fg(DIM),
    ));
    content_lines.push(Line::from(vec![Span::styled(
        "  📄 SKILL.md 说明与执行指令正文:",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )]));
    content_lines.push(Line::raw(""));

    if skill.body.trim().is_empty() {
        content_lines.push(Line::styled(
            "    (该 Skill 暂无 Markdown 正文内容)",
            Style::default().fg(DIM),
        ));
    } else {
        let mut in_code_block = false;
        for line in skill.body.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("```") {
                in_code_block = !in_code_block;
                content_lines.push(Line::styled(
                    format!("    {}", line),
                    Style::default().fg(CODE).add_modifier(Modifier::BOLD),
                ));
            } else if in_code_block {
                content_lines.push(Line::styled(
                    format!("    {}", line),
                    Style::default().fg(CODE),
                ));
            } else if trimmed.starts_with('#') {
                content_lines.push(Line::styled(
                    format!("    {}", line),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ));
            } else if trimmed.starts_with('-')
                || trimmed.starts_with('*')
                || trimmed.starts_with('>')
            {
                content_lines.push(Line::styled(
                    format!("    {}", line),
                    Style::default().fg(FG),
                ));
            } else if trimmed.is_empty() {
                content_lines.push(Line::raw(""));
            } else {
                content_lines.push(Line::styled(
                    format!("    {}", line),
                    Style::default().fg(FG),
                ));
            }
        }
    }

    let visible_rows = chunks[0].height as usize;
    let max_scroll = content_lines.len().saturating_sub(visible_rows);
    let scroll = detail.scroll.min(max_scroll);

    frame.render_widget(
        Paragraph::new(content_lines).scroll((scroll as u16, 0)),
        chunks[0],
    );

    let hint_line = Line::styled(
        " [↑/↓/PgUp/PgDn] 滚屏 · [←/→] 切换上/下一技能 · [Esc/q/Enter] 关闭返回",
        Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
    );
    frame.render_widget(Paragraph::new(hint_line), chunks[1]);
}

fn draw_memory_detail_modal(
    frame: &mut Frame,
    parent: Rect,
    settings: &CliSettingsState,
    detail: &MemoryDetailModal,
) {
    if parent.width < 20 || parent.height < 10 {
        return;
    }

    let total = settings.total_memories();
    let (label, path, entries) = match detail.group {
        MemoryGroup::Global => (
            "全局记忆",
            &settings.memory_global_path,
            &settings.memories_global,
        ),
        MemoryGroup::Project => (
            "项目级记忆",
            &settings.memory_project_path,
            &settings.memories_project,
        ),
    };
    let Some(entry) = entries.get(detail.entry) else {
        return;
    };
    let flat = match detail.group {
        MemoryGroup::Global => detail.entry,
        MemoryGroup::Project => settings.memories_global.len() + detail.entry,
    };

    let width = (parent.width * 85 / 100).max(70).min(parent.width);
    let height = (parent.height * 85 / 100).max(20).min(parent.height);
    let area = Rect::new(
        parent.x + (parent.width.saturating_sub(width)) / 2,
        parent.y + (parent.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    clear_popup_under(frame, area);

    let title_str = format!(
        " 📖 记忆详情 [{}/{}] · {} #{} ",
        flat + 1,
        total,
        label,
        entry.index
    );
    let block = Block::bordered()
        .border_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .title(Line::styled(
            title_str,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

    let mut content_lines = Vec::new();
    content_lines.push(Line::from(vec![
        Span::styled("  作用域: ", Style::default().fg(MUTED)),
        Span::styled(
            label,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
    ]));
    content_lines.push(Line::from(vec![
        Span::styled("  文件: ", Style::default().fg(MUTED)),
        Span::styled(path, Style::default().fg(DIM)),
    ]));
    content_lines.push(Line::from(vec![
        Span::styled("  编号: ", Style::default().fg(MUTED)),
        Span::styled(format!("#{}", entry.index), Style::default().fg(CODE)),
    ]));
    content_lines.push(Line::raw(""));
    let div_w = (chunks[0].width as usize).saturating_sub(4).max(10);
    content_lines.push(Line::styled(
        format!("  {}", "─".repeat(div_w)),
        Style::default().fg(DIM),
    ));
    content_lines.push(Line::from(vec![Span::styled(
        "  📝 记忆正文:",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )]));
    content_lines.push(Line::raw(""));
    if entry.content.trim().is_empty() {
        content_lines.push(Line::styled("    (该记忆为空)", Style::default().fg(DIM)));
    } else {
        for line in entry.content.lines() {
            content_lines.push(Line::styled(format!("    {line}"), Style::default().fg(FG)));
        }
    }

    let visible_rows = chunks[0].height as usize;
    let max_scroll = content_lines.len().saturating_sub(visible_rows);
    let scroll = detail.scroll.min(max_scroll);
    frame.render_widget(
        Paragraph::new(content_lines).scroll((scroll as u16, 0)),
        chunks[0],
    );

    let hint_line = Line::styled(
        " [↑/↓/PgUp/PgDn] 滚屏 · [←/→] 切换上/下一条记忆 · [Esc/q/Enter] 关闭返回",
        Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
    );
    frame.render_widget(Paragraph::new(hint_line), chunks[1]);
}

/// CLI 快捷键说明书条目 `(按键, 说明)`：`Panel::Shortcuts` 浮层与「关于」页共用。
/// 渲染列宽 16。
const SHORTCUT_KEYS: &[(&str, &str)] = &[
    ("Enter", "Send / select completion"),
    ("Alt/Shift+Enter", "New line"),
    ("Tab · Up/Down", "Complete / choose command"),
    ("F1", "Open About page"),
    ("F2 / Ctrl+P", "Cycle mode (manual/auto/unlimited)"),
    ("F3 / Ctrl+,", "Open Settings center panel"),
    ("Ctrl+G", "Toggle subagent panel (live transcripts)"),
    ("Ctrl+B", "Toggle background jobs panel"),
    ("Up / Down", "History prompts / scroll line"),
    ("Mouse Wheel", "Scroll chat / approval arguments"),
    ("Ctrl+O", "Toggle tool details"),
    ("Ctrl+T", "Toggle CTF challenges panel (when CTF enabled)"),
    ("Ctrl+C", "Cancel task"),
    ("Alt+↑ / Alt+↓", "Todo list: expand / collapse (3 states)"),
    ("PgUp / PgDn", "Scroll conversation"),
    ("?", "Toggle shortcuts (empty input)"),
];

fn render_tab_bar(area_width: u16, current_tab: SettingsTab) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;

    let all = SettingsTab::all();
    let active_idx = all.iter().position(|&t| t == current_tab).unwrap_or(0);

    let full_w: usize = all
        .iter()
        .map(|t| format!(" {} ", t.title()).width() + 1)
        .sum();
    let short_w: usize = all
        .iter()
        .map(|t| format!(" {} ", t.short_title()).width() + 1)
        .sum();

    let title_fn: fn(SettingsTab) -> &'static str = if (area_width as usize) >= full_w {
        |t| t.title()
    } else if (area_width as usize) >= short_w {
        |t| t.short_title()
    } else {
        |t| t.compact_title()
    };

    let titles: Vec<String> = all.iter().map(|&t| format!(" {} ", title_fn(t))).collect();
    let widths: Vec<usize> = titles.iter().map(|s| s.width()).collect();
    let total_w: usize = widths.iter().sum::<usize>() + (all.len().saturating_sub(1));

    if total_w <= area_width as usize {
        let mut spans = Vec::new();
        for (i, title) in titles.into_iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" "));
            }
            if i == active_idx {
                spans.push(Span::styled(
                    title,
                    Style::default()
                        .fg(Color::Rgb(18, 18, 22))
                        .bg(ACCENT)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(title, Style::default().fg(MUTED)));
            }
        }
        return Line::from(spans);
    }

    let mut start_idx = active_idx;
    let mut end_idx = active_idx;

    loop {
        let left_expandable = start_idx > 0;
        let right_expandable = end_idx + 1 < all.len();
        if !left_expandable && !right_expandable {
            break;
        }

        let mut changed = false;

        if right_expandable {
            let next_end = end_idx + 1;
            let arrow_w = (if start_idx > 0 { 2 } else { 0 })
                + (if next_end + 1 < all.len() { 2 } else { 0 });
            let span_w: usize =
                widths[start_idx..=next_end].iter().sum::<usize>() + (next_end - start_idx);
            if arrow_w + span_w <= area_width as usize {
                end_idx = next_end;
                changed = true;
            }
        }

        if left_expandable {
            let next_start = start_idx - 1;
            let arrow_w = (if next_start > 0 { 2 } else { 0 })
                + (if end_idx + 1 < all.len() { 2 } else { 0 });
            let span_w: usize =
                widths[next_start..=end_idx].iter().sum::<usize>() + (end_idx - next_start);
            if arrow_w + span_w <= area_width as usize {
                start_idx = next_start;
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }

    let mut spans = Vec::new();
    if start_idx > 0 {
        spans.push(Span::styled(
            "◄ ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    }

    for (rel_i, i) in (start_idx..=end_idx).enumerate() {
        if rel_i > 0 {
            spans.push(Span::raw(" "));
        }
        let title = &titles[i];
        if i == active_idx {
            spans.push(Span::styled(
                title.clone(),
                Style::default()
                    .fg(Color::Rgb(18, 18, 22))
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(title.clone(), Style::default().fg(MUTED)));
        }
    }

    if end_idx + 1 < all.len() {
        spans.push(Span::styled(
            " ►",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    }

    Line::from(spans)
}

fn draw_fullscreen_provider_picker(
    frame: &mut Frame,
    area: Rect,
    picker: &CommandPicker,
    selected: usize,
) {
    if area.width < 40 || area.height < 8 {
        return;
    }

    frame.render_widget(Clear, area);

    let is_protocol_picker = picker.title.contains("Protocol");
    let main_title = if is_protocol_picker {
        " 🔌 选择服务商协议类型 / Select Protocol Kind "
    } else {
        " 🚀 服务商预设库与自定义接入 / Add Provider from Preset or Custom "
    };

    let outer_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(
            Line::from(main_title).style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        );
    let inner = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::vertical([
        Constraint::Length(2), // 顶部提示与状态摘要
        Constraint::Min(0),    // 双栏主体 (左侧列表 + 右侧规格详情)
        Constraint::Length(1), // 底部操作快捷键
    ])
    .split(inner);

    let header_area = chunks[0];
    let body_area = chunks[1];
    let footer_area = chunks[2];

    // 1. 顶部导读栏
    let subtitle = if is_protocol_picker {
        "选择您自建或中转服务支持的接口协议标准。不同协议对应不同的请求格式、端点规范与鉴权 Header。"
    } else {
        "内置主流大模型厂商开箱即用预设；亦可接入任意 OpenAI 兼容网关 (vLLM/OneAPI)、Claude 或本地 Ollama。"
    };
    let count_info = format!(
        "共 {} 个选项 · 当前已选第 {} 项",
        picker.items.len(),
        if picker.items.is_empty() {
            0
        } else {
            selected + 1
        }
    );
    let header_p = Paragraph::new(vec![
        Line::from(Span::styled(subtitle, Style::default().fg(MUTED))),
        Line::from(Span::styled(count_info, Style::default().fg(DIM))),
    ]);
    frame.render_widget(header_p, header_area);

    // 2. 双栏主体
    let panes = Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(body_area);
    let list_area = panes[0];
    let detail_area = panes[1];

    // 左栏：列表
    let list_title = if is_protocol_picker {
        " 协议规范列表 / Protocols "
    } else {
        " 服务商选项 / Providers "
    };
    let list_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(
            Line::from(list_title).style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        );
    let list_inner = list_block.inner(list_area);
    frame.render_widget(list_block, list_area);

    let total_items = picker.items.len();
    let visible_rows = list_inner.height as usize;
    let offset = if visible_rows == 0 {
        0
    } else if selected >= visible_rows {
        selected - visible_rows + 1
    } else {
        0
    };

    let mut list_lines: Vec<Line<'static>> = Vec::new();
    if total_items == 0 {
        list_lines.push(Line::from("（无可用选项）").style(Style::default().fg(DIM)));
    } else {
        for (i, item) in picker
            .items
            .iter()
            .enumerate()
            .skip(offset)
            .take(visible_rows)
        {
            let is_sel = i == selected;
            let marker = if is_sel { "▸ " } else { "  " };
            let sel_bg = Color::Rgb(38, 79, 120);
            let row_style = if is_sel {
                Style::default().bg(sel_bg)
            } else {
                Style::default()
            };

            let is_custom = item.label.contains("自定义") || item.command.contains("add-custom");
            let badge_span = if is_custom {
                Span::styled(
                    "[ 自定义 ] ",
                    Style::default().fg(CODE).add_modifier(Modifier::BOLD),
                )
            } else if is_protocol_picker {
                Span::styled(
                    "[ 协议 ] ",
                    Style::default()
                        .fg(Color::Rgb(160, 210, 255))
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    "[ 预设 ] ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )
            };

            let label_color = if is_sel {
                FG
            } else if is_custom {
                CODE
            } else {
                FG
            };

            list_lines.push(
                Line::from(vec![
                    Span::styled(
                        marker,
                        if is_sel {
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(MUTED)
                        },
                    ),
                    badge_span,
                    Span::styled(
                        item.label.clone(),
                        Style::default().fg(label_color).add_modifier(if is_sel {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                    ),
                ])
                .style(row_style),
            );
        }
    }
    frame.render_widget(Paragraph::new(list_lines), list_inner);

    // 右栏：详情卡片
    let detail_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(CODE))
        .title(
            Line::from(" 详细规格与配置指南 / Specifications & Guide ")
                .style(Style::default().fg(CODE).add_modifier(Modifier::BOLD)),
        );
    let detail_inner = detail_block.inner(detail_area);
    frame.render_widget(detail_block, detail_area);

    let mut detail_lines: Vec<Line> = Vec::new();
    if let Some(curr_item) = picker.items.get(selected) {
        let preset_opt = if curr_item.command.starts_with("/provider add-preset ") {
            let id = curr_item
                .command
                .trim_start_matches("/provider add-preset ")
                .trim();
            cyber_core::PROVIDER_PRESETS
                .iter()
                .find(|p| p.id.eq_ignore_ascii_case(id))
        } else {
            None
        };

        if let Some(preset) = preset_opt {
            detail_lines.push(Line::from(vec![
                Span::styled(
                    "厂商全称: ",
                    Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    preset.name,
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled("协议类型: ", Style::default().fg(MUTED)),
                Span::styled(
                    format!("[{}]", preset.kind),
                    Style::default().fg(CODE).add_modifier(Modifier::BOLD),
                ),
            ]));
            detail_lines.push(Line::from(""));

            let base_url_str = if preset.base_url.is_empty() {
                "（根据实际自建端点自行填写）"
            } else {
                preset.base_url
            };
            detail_lines.push(Line::from(vec![
                Span::styled(
                    "默认端点 Base URL: ",
                    Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
                ),
                Span::styled(base_url_str, Style::default().fg(FG)),
            ]));

            let def_model_str = if preset.default_model.is_empty() {
                "（自定义模型标识）"
            } else {
                preset.default_model
            };
            detail_lines.push(Line::from(vec![
                Span::styled(
                    "推荐默认模型:      ",
                    Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    def_model_str,
                    Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
                ),
            ]));

            if !preset.suggested_models.is_empty() {
                detail_lines.push(Line::from(vec![
                    Span::styled("常见可选模型:      ", Style::default().fg(MUTED)),
                    Span::styled(preset.suggested_models.join(", "), Style::default().fg(DIM)),
                ]));
            }

            if !preset.env_var_suggestion.is_empty() {
                detail_lines.push(Line::from(vec![
                    Span::styled("推荐环境变量:      ", Style::default().fg(MUTED)),
                    Span::styled(
                        format!("${{{}}}", preset.env_var_suggestion),
                        Style::default().fg(Color::Rgb(255, 170, 80)),
                    ),
                ]));
            }

            detail_lines.push(Line::from(""));
            detail_lines.push(Line::from(vec![Span::styled(
                "服务商说明:",
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            )]));
            detail_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(preset.description, Style::default().fg(FG)),
            ]));

            detail_lines.push(Line::from(""));
            detail_lines.push(Line::from(vec![Span::styled(
                "💡 快速接入操作指引:",
                Style::default().fg(CODE).add_modifier(Modifier::BOLD),
            )]));
            detail_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "按 Enter 选中后，将自动预填端点 URL 与默认模型。进入表单后，您只需输入 API Key (可使用 ${ENV} 语法) 即可完成接入。",
                    Style::default().fg(DIM),
                ),
            ]));
        } else if curr_item.command.starts_with("/provider add-custom") {
            detail_lines.push(Line::from(vec![
                Span::styled(
                    "接入模式: ",
                    Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "🛠️ 自定义服务商接入 (Custom Provider)",
                    Style::default().fg(CODE).add_modifier(Modifier::BOLD),
                ),
            ]));
            detail_lines.push(Line::from(""));
            detail_lines.push(Line::from(vec![Span::styled(
                "支持的协议规范:",
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            )]));
            detail_lines.push(Line::from(vec![
                Span::raw("  • "),
                Span::styled(
                    "openai-compatible",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " : 通用兼容层（vLLM / OneAPI / NewAPI / FastChat / 本地代理）",
                    Style::default().fg(FG),
                ),
            ]));
            detail_lines.push(Line::from(vec![
                Span::raw("  • "),
                Span::styled(
                    "anthropic",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "         : Anthropic 原生 Claude Messages API 协议",
                    Style::default().fg(FG),
                ),
            ]));
            detail_lines.push(Line::from(vec![
                Span::raw("  • "),
                Span::styled(
                    "ollama",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "            : Ollama 本地 / 内网实例私有运行服务",
                    Style::default().fg(FG),
                ),
            ]));
            detail_lines.push(Line::from(vec![
                Span::raw("  • "),
                Span::styled(
                    "openai",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "            : OpenAI 官方原生 API 规范与端点",
                    Style::default().fg(FG),
                ),
            ]));
            detail_lines.push(Line::from(""));
            detail_lines.push(Line::from(vec![Span::styled(
                "💡 操作流程:",
                Style::default().fg(CODE).add_modifier(Modifier::BOLD),
            )]));
            detail_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "按 Enter 后将进入协议选择界面，确认协议后进入全屏表单，自主设置服务商名称、端点 Base URL、秘钥及模型。",
                    Style::default().fg(DIM),
                ),
            ]));
        } else if is_protocol_picker {
            let (proto_name, proto_desc, default_endpoint, auth_type) = if curr_item
                .command
                .contains("openai-compatible")
            {
                (
                    "openai-compatible (通用兼容协议)",
                    "业界事实标准协议规范。适用于 OneAPI、NewAPI、vLLM、FastChat、LocalAI 以及任何自建的反向代理网关。完全兼容 OpenAI /v1/chat/completions 路由。",
                    "http://localhost:8000/v1 (或自建服务端口)",
                    "HTTP Bearer Token 头部鉴权",
                )
            } else if curr_item.command.contains("anthropic") {
                (
                    "anthropic (Claude Messages 协议)",
                    "Anthropic 官方原生 API 规范，支持 Claude 3.5 / 3.7 Sonnet 系列多模态及推理能力。采用特定的 Content Blocks 消息格式。",
                    "https://api.anthropic.com/v1",
                    "x-api-key 自定义 HTTP 头部鉴权",
                )
            } else if curr_item.command.contains("ollama") {
                (
                    "ollama (本地私有模型协议)",
                    "Ollama 本地或局域网开源模型运行服务（如 Qwen2.5, DeepSeek-R1, Llama 3.3）。数据不出内网，天生支持高安全审计。",
                    "http://127.0.0.1:11434",
                    "免密钥鉴权（若配置了反向代理亦支持自定义 Key）",
                )
            } else {
                (
                    "openai (OpenAI 原生规范)",
                    "OpenAI 官方 API 服务标准，支持 GPT-4o、o1/o3-mini 等旗舰通用与推理模型。",
                    "https://api.openai.com/v1",
                    "Bearer <OPENAI_API_KEY>",
                )
            };

            detail_lines.push(Line::from(vec![
                Span::styled(
                    "选定协议: ",
                    Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    proto_name,
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
            ]));
            detail_lines.push(Line::from(""));
            detail_lines.push(Line::from(vec![Span::styled(
                "协议简介:",
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            )]));
            detail_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(proto_desc, Style::default().fg(FG)),
            ]));
            detail_lines.push(Line::from(""));
            detail_lines.push(Line::from(vec![
                Span::styled("推荐默认端点: ", Style::default().fg(MUTED)),
                Span::styled(default_endpoint, Style::default().fg(CODE)),
            ]));
            detail_lines.push(Line::from(vec![
                Span::styled("鉴权认证方式: ", Style::default().fg(MUTED)),
                Span::styled(auth_type, Style::default().fg(DIM)),
            ]));
            detail_lines.push(Line::from(""));
            detail_lines.push(Line::from(vec![Span::styled(
                "💡 下一步指引:",
                Style::default().fg(CODE).add_modifier(Modifier::BOLD),
            )]));
            detail_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "按 Enter 选定此协议，将自动创建该协议类型的表单，您可以进一步填写名称、Base URL、密钥和模型名称。",
                    Style::default().fg(DIM),
                ),
            ]));
        } else {
            detail_lines.push(Line::from(vec![
                Span::styled("选项: ", Style::default().fg(MUTED)),
                Span::styled(&curr_item.label, Style::default().fg(FG)),
            ]));
            if !curr_item.detail.is_empty() {
                detail_lines.push(Line::from(vec![
                    Span::styled("详情: ", Style::default().fg(MUTED)),
                    Span::styled(&curr_item.detail, Style::default().fg(DIM)),
                ]));
            }
        }
    }
    let detail_p = Paragraph::new(detail_lines).wrap(Wrap { trim: false });
    frame.render_widget(detail_p, detail_inner);

    // 3. 底部快捷键提示
    let hint_line = Line::from(vec![
        Span::styled(" [ ↑/↓ 上下选择 ] ", Style::default().fg(MUTED)),
        Span::raw("  "),
        Span::styled(
            " [ Enter 确认并进入配置 ] ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(" [ Esc 返回设置中心 ] ", Style::default().fg(MUTED)),
    ])
    .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(Paragraph::new(vec![hint_line]), footer_area);
}

fn draw_fullscreen_provider_form(
    frame: &mut Frame,
    area: Rect,
    form: &mut FormState,
    status: &str,
) {
    if area.width < 40 || area.height < 8 {
        return;
    }

    frame.render_widget(Clear, area);

    let is_edit =
        form.form.title.to_lowercase().contains("edit") || form.form.title.contains("编辑");
    let main_title = if is_edit {
        " ⚙️ 服务商参数配置 · 编辑模式 / Edit Provider Configuration "
    } else {
        " ➕ 服务商接入配置 · 新增模式 / Add Provider Configuration "
    };

    let outer_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(CODE))
        .title(
            Line::from(main_title).style(Style::default().fg(CODE).add_modifier(Modifier::BOLD)),
        );
    let inner = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::vertical([
        Constraint::Length(2), // 顶部提示与目标服务商摘要
        Constraint::Min(0),    // 双栏主体 (左侧字段输入 + 右侧参数向导)
        Constraint::Length(2), // 底部状态与操作快捷键
    ])
    .split(inner);

    let header_area = chunks[0];
    let body_area = chunks[1];
    let footer_area = chunks[2];

    // 提取当前表单中已输入的 name 和 kind
    let cur_name = form
        .form
        .fields
        .iter()
        .position(|f| f.name == "name")
        .and_then(|idx| form.inputs.get(idx))
        .map(|inp| inp.lines().join("").trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "（未命名）".into());

    let cur_kind = form
        .form
        .fields
        .iter()
        .position(|f| f.name == "kind")
        .and_then(|idx| form.inputs.get(idx))
        .map(|inp| inp.lines().join("").trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "openai".into());

    // 1. 顶部导读
    let header_p = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("服务商标识: ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{cur_name} "),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("· 协议规范: ", Style::default().fg(MUTED)),
            Span::styled(
                format!("[{cur_kind}] "),
                Style::default().fg(CODE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("· 共 {} 个配置字段", form.form.fields.len()),
                Style::default().fg(DIM),
            ),
        ]),
        Line::from(Span::styled(
            "请完成以下字段配置。使用 Tab 或 ↑/↓ 切换字段，←/→ 或空格切换协议类型，Ctrl+S 保存。",
            Style::default().fg(DIM),
        )),
    ]);
    frame.render_widget(header_p, header_area);

    // 2. 双栏主体
    let panes = Layout::horizontal([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(body_area);
    let fields_area = panes[0];
    let guide_area = panes[1];

    // 左栏：字段列表
    let fields_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(
            Line::from(" 配置字段输入 / Field Inputs ")
                .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        );
    let fields_inner = fields_block.inner(fields_area);
    frame.render_widget(fields_block, fields_area);

    let count = form.form.fields.len();
    let sel_idx = form.selected;

    // 计算滚动的起始位置以确保当前选中字段可见
    let field_box_height = 2usize;
    let total_field_lines = count * field_box_height + 2;
    let visible_field_lines = fields_inner.height as usize;
    let max_offset = total_field_lines.saturating_sub(visible_field_lines);
    let desired_line = sel_idx * field_box_height + if sel_idx >= 8 { 2 } else { 0 };
    let field_scroll_offset = desired_line
        .saturating_sub(visible_field_lines.saturating_sub(field_box_height))
        .min(max_offset);

    let mut field_lines: Vec<Line> = Vec::new();
    for (i, field) in form.form.fields.iter().enumerate() {
        let is_sel = i == sel_idx;
        let marker = if is_sel { "▸ " } else { "  " };

        let (cn_label, is_required) = match field.name.as_str() {
            "name" => ("服务商标识 (name)", true),
            "kind" => ("协议类型 (kind)", true),
            "endpoint" => ("端点地址 (endpoint)", true),
            "apikey" => ("访问密钥 (apikey)", false),
            "model" => ("默认模型 (model)（Enter 打开模型列表）", true),
            "maxtokens" => ("最大 Token (maxtokens)", true),
            "temperature" => ("采样温度 (temperature)", true),
            "context_length" => ("上下文窗口 (context_length)", false),
            "chat_endpoint" => (
                "自定义对话端点 chat_endpoint（留空默认 {base_url}/chat/completions）",
                false,
            ),
            "models_endpoint" => (
                "自定义模型列表端点 models_endpoint（留空默认 {base_url}/models）",
                false,
            ),
            "thinking_type" => (
                "思考模式 thinking.type（未设置 / enabled / disabled；未设置=不下发）",
                false,
            ),
            "thinking_effort" => (
                "思考强度 thinking.effort（未设置 / low / medium / high；未设置=不下发）",
                false,
            ),
            "tool_name" => ("工具标识 (name，仅字母数字下划线连字符)", true),
            "tool_description" => ("工具说明 (description)", false),
            "tool_command" => ("执行命令 (command，用 {param} 引用参数)", true),
            "tool_tags" => ("标签 (tags，逗号分隔)", false),
            "tool_params" => ("参数 (params：名称|r或o|说明|默认值，分号分隔多个)", false),
            _ => (field.name.as_str(), false),
        };

        // 在高级选项前插入分隔线与标题
        if field.name == "chat_endpoint" {
            field_lines.push(Line::raw(""));
            field_lines.push(Line::styled(
                "  ── 高级设置 高级选项 ──",
                Style::default().fg(CODE).add_modifier(Modifier::BOLD),
            ));
        }

        let req_badge = if is_required {
            Span::styled(" [必填]", Style::default().fg(Color::Rgb(255, 120, 120)))
        } else {
            Span::styled(" [可选]", Style::default().fg(DIM))
        };

        let label_style = if is_sel {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        };

        // 行 1: 字段标签
        field_lines.push(Line::from(vec![
            Span::styled(
                marker,
                if is_sel {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                },
            ),
            Span::styled(cn_label, label_style),
            req_badge,
        ]));

        // 行 2: 字段值展示
        let sel_bg = Color::Rgb(38, 79, 120);
        let val_str = if let Some(inp) = form.inputs.get(i) {
            inp.lines().join("\n")
        } else {
            String::new()
        };

        if field.name == "kind" {
            let cur_k = val_str.trim().to_ascii_lowercase();
            let mut pill_spans = vec![Span::raw("    ")];
            for k in cyber_core::PROVIDER_KINDS {
                let is_cur = *k == cur_k;
                let (pill_text, pill_style) = if is_cur {
                    (
                        format!(" [{k}] "),
                        Style::default()
                            .fg(Color::Rgb(20, 20, 20))
                            .bg(CODE)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    (format!(" {k} "), Style::default().fg(DIM))
                };
                pill_spans.push(Span::styled(pill_text, pill_style));
                pill_spans.push(Span::raw(" "));
            }
            if is_sel {
                pill_spans.push(Span::styled(
                    " (←/→ 或空格切换)",
                    Style::default().fg(ACCENT),
                ));
            }
            let line_style = if is_sel {
                Style::default().bg(sel_bg)
            } else {
                Style::default()
            };
            field_lines.push(Line::from(pill_spans).style(line_style));
        } else if field.name == "context_length" {
            let cur = val_str.trim().to_string();
            let is_custom = !cur.is_empty()
                && !cyber_core::CONTEXT_LENGTH_PRESETS
                    .iter()
                    .any(|(_, v)| !v.is_empty() && cur == *v);
            let mut pill_spans = vec![Span::raw("    ")];
            for (label, value) in cyber_core::CONTEXT_LENGTH_PRESETS {
                let is_cur = if value.is_empty() {
                    cur.is_empty()
                } else {
                    cur == *value
                };
                let (pill_text, pill_style) = if is_cur {
                    (
                        format!(" [{label}] "),
                        Style::default()
                            .fg(Color::Rgb(20, 20, 20))
                            .bg(CODE)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    (format!(" {label} "), Style::default().fg(DIM))
                };
                pill_spans.push(Span::styled(pill_text, pill_style));
                pill_spans.push(Span::raw(" "));
            }
            let custom_text = if is_custom {
                format!("[自定义: {cur}]")
            } else {
                "自定义".to_string()
            };
            pill_spans.push(Span::styled(
                format!(" {custom_text} "),
                if is_custom {
                    Style::default()
                        .fg(Color::Rgb(20, 20, 20))
                        .bg(CODE)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(DIM)
                },
            ));
            if is_sel {
                pill_spans.push(Span::styled(
                    " (←/→ 或空格切换预设；直接输入数字为自定义)",
                    Style::default().fg(ACCENT),
                ));
            }
            let line_style = if is_sel {
                Style::default().bg(sel_bg)
            } else {
                Style::default()
            };
            field_lines.push(Line::from(pill_spans).style(line_style));
        } else if field.secret {
            let display_val = if val_str.is_empty() {
                "（留空，若已配置环境变量或免密）".to_string()
            } else {
                "*".repeat(val_str.chars().count().min(32))
            };

            let row_style = if is_sel {
                Style::default().bg(sel_bg).fg(FG)
            } else {
                Style::default().fg(if val_str.is_empty() { DIM } else { CODE })
            };
            field_lines.push(
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled(display_val, row_style),
                ])
                .style(if is_sel {
                    Style::default().bg(sel_bg)
                } else {
                    Style::default()
                }),
            );
        } else {
            let display_val = if val_str.is_empty() {
                let placeholder = match field.name.as_str() {
                    "name" => "如 deepseek, my_vllm, custom_gateway",
                    "endpoint" => "如 https://api.deepseek.com",
                    "model" => "如 deepseek-chat, gpt-4o",
                    "maxtokens" => "384000",
                    "temperature" => "0.7",
                    "chat_endpoint" => "留空默认 {base_url}/chat/completions",
                    "models_endpoint" => "留空默认 {base_url}/models",
                    "thinking_type" => "未设置 / enabled / disabled",
                    "thinking_effort" => "未设置 / low / medium / high",
                    _ => "",
                };
                Span::styled(format!("({placeholder})"), Style::default().fg(DIM))
            } else {
                Span::styled(
                    val_str.clone(),
                    if is_sel {
                        Style::default().fg(FG).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(FG)
                    },
                )
            };

            let line_style = if is_sel {
                Style::default().bg(sel_bg)
            } else {
                Style::default()
            };
            let mut spans = vec![Span::raw("    "), display_val];
            if field.name == "model" && form.model_picker.as_ref().is_some_and(|p| p.manual) {
                spans.push(Span::styled("  [✎ 手输模式]", Style::default().fg(ACCENT)));
            }
            field_lines.push(Line::from(spans).style(line_style));
        }
    }

    let visible_slice = field_lines
        .into_iter()
        .skip(field_scroll_offset)
        .take(visible_field_lines)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible_slice), fields_inner);

    // 右栏：实时动态参数向导
    let guide_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(CODE))
        .title(
            Line::from(" 参数向导与协议规范 / Guidance & Tips ")
                .style(Style::default().fg(CODE).add_modifier(Modifier::BOLD)),
        );
    let guide_inner = guide_block.inner(guide_area);
    frame.render_widget(guide_block, guide_area);

    let sel_field_name = form
        .form
        .fields
        .get(sel_idx)
        .map(|f| f.name.as_str())
        .unwrap_or("");
    let mut guide_lines: Vec<Line<'static>> = Vec::new();

    // 顶部通用协议状态
    guide_lines.push(Line::from(vec![
        Span::styled(
            "当前协议: ",
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("[{cur_kind}]"),
            Style::default().fg(CODE).add_modifier(Modifier::BOLD),
        ),
    ]));
    match cur_kind.as_str() {
        "openai-compatible" => {
            guide_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "兼容 OpenAI REST 规范的网关。适合 OneAPI、NewAPI、vLLM、FastChat、本地反代。",
                    Style::default().fg(DIM),
                ),
            ]));
        }
        "anthropic" => {
            guide_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "Anthropic Claude 原生 Messages API。端点通常为 https://api.anthropic.com/v1。",
                    Style::default().fg(DIM),
                ),
            ]));
        }
        "ollama" => {
            guide_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "本地/内网 Ollama 实例。端点默认为 http://127.0.0.1:11434，通常无需填写密钥。",
                    Style::default().fg(DIM),
                ),
            ]));
        }
        _ => {
            guide_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "标准 OpenAI 官方接口规范。端点默认为 https://api.openai.com/v1。",
                    Style::default().fg(DIM),
                ),
            ]));
        }
    }
    guide_lines.push(Line::from(""));

    // 字段深度指引
    guide_lines.push(Line::from(vec![Span::styled(
        "当前字段配置指南:",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )]));

    match sel_field_name {
        "name" => {
            guide_lines.push(Line::from(vec![Span::styled(
                "▸ 服务商标识 (name):",
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )]));
            guide_lines.push(Line::from(
                "  • 作为系统内部唯一索引键，不可与已有服务商重复。",
            ));
            guide_lines.push(Line::from(
                "  • 仅允许字母、数字、下划线及减号，不可包含空格或斜杠。",
            ));
            guide_lines.push(Line::from(
                "  • 示例: deepseek_custom, vllm_local, my_gateway",
            ));
        }
        "kind" => {
            guide_lines.push(Line::from(vec![Span::styled(
                "▸ 协议类型 (kind):",
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )]));
            guide_lines.push(Line::from(
                "  • 决定网络请求的消息格式、流式响应解析协议与鉴权头。",
            ));
            guide_lines.push(Line::from(
                "  • 切换方式: 在本行按键盘 ← 或 → 箭头键，或按空格键循环切换。",
            ));
            guide_lines.push(Line::from(
                "  • 可选协议: openai-compatible, anthropic, ollama, openai",
            ));
        }
        "endpoint" => {
            guide_lines.push(Line::from(vec![Span::styled(
                "▸ 接口地址 (endpoint / Base URL):",
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )]));
            guide_lines.push(Line::from(
                "  • API 请求的基础 URL，必须包含 http:// 或 https:// 前缀。",
            ));
            guide_lines.push(Line::from("  • 常见端点参考:"));
            guide_lines.push(Line::from("    - DeepSeek 官方 : https://api.deepseek.com"));
            guide_lines.push(Line::from(
                "    - SiliconFlow  : https://api.siliconflow.cn/v1",
            ));
            guide_lines.push(Line::from("    - 本地 vLLM    : http://localhost:8000/v1"));
            guide_lines.push(Line::from("    - 本地 Ollama  : http://localhost:11434"));
        }
        "apikey" => {
            guide_lines.push(Line::from(vec![Span::styled(
                "▸ 访问密钥 (apikey):",
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )]));
            guide_lines.push(Line::from("  • 服务商颁发的 API Key / Token。"));
            guide_lines.push(Line::from(
                "  • 环境变量支持: 支持以 ${ENV_VAR} 语法动态读取系统环境变量。",
            ));
            guide_lines.push(Line::from(
                "    例如: ${DEEPSEEK_API_KEY} 或 ${OPENAI_API_KEY}",
            ));
            guide_lines.push(Line::from("  • Ollama 或免密内网网关可留空。"));
        }
        "model" => {
            guide_lines.push(Line::from(vec![Span::styled(
                "▸ 默认模型名称 (model):",
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )]));
            guide_lines.push(Line::from(
                "  • 请求时默认调用的模型 ID，必须与平台实际模型标识一致。",
            ));
            guide_lines.push(Line::from(
                "  • 在本行按 Enter（或空格）拉取 {base_url}/models 模型列表并在浮层中选择；列表内 m/f 手输模型名。",
            ));
            guide_lines.push(Line::from(
                "  • 浮层内 t 实测推理能力、v 实测视觉能力（只写能力缓存，不落盘 providers.toml）。",
            ));
            guide_lines.push(Line::from(
                "  • 列表拉取失败时自动进入手输兜底模式（model 字段恢复可编辑）；Enter 退出该模式。",
            ));
            guide_lines.push(Line::from(
                "  • 常见模型: deepseek-chat, gpt-4o, claude-3-7-sonnet-20250219, qwen2.5:32b",
            ));
        }
        "maxtokens" => {
            guide_lines.push(Line::from(vec![Span::styled(
                "▸ 最大输出 Token (maxtokens):",
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )]));
            guide_lines.push(Line::from(
                "  • 单次回复的最大生成 Token 数量限制，必须为正整数。",
            ));
            guide_lines.push(Line::from(
                "  • 默认 384000；实际发送值会按该模型 context_length 自动钳制，无需手动保守设置。",
            ));
        }
        "temperature" => {
            guide_lines.push(Line::from(vec![Span::styled(
                "▸ 采样温度 (temperature):",
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )]));
            guide_lines.push(Line::from("  • 浮点数，有效范围 0.0 ~ 2.0。"));
            guide_lines.push(Line::from(
                "  • 0.0 ~ 0.3: 高度严谨确定，适合代码审计与精准漏洞分析。",
            ));
            guide_lines.push(Line::from("  • 0.7: 默认推荐，兼顾逻辑严密与自然表达。"));
        }
        "context_length" => {
            guide_lines.push(Line::from(vec![Span::styled(
                "▸ 上下文窗口大小 (context_length):",
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )]));
            guide_lines.push(Line::from(
                "  • 可选配置。指定该模型的总上下文 Token 承载上限。",
            ));
            guide_lines.push(Line::from(
                "  • 按 ←/→ 或空格在常用预设（默认/128K/256K/512K/1M）间循环切换；直接输入数字则为自定义值（此时 ←/→ 不修改已输入的值）。",
            ));
            guide_lines.push(Line::from(
                "  • 留空 = 未设置（此时不参与 max_tokens 的自动钳制）。",
            ));
        }
        "chat_endpoint" => {
            guide_lines.push(Line::from(vec![Span::styled(
                "▸ 自定义对话端点 (chat_endpoint):",
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )]));
            guide_lines.push(Line::from(
                "  • 高级选项。自定义流式对话端点完整 URL 或路径。",
            ));
            guide_lines.push(Line::from(
                "  • 留空时默认使用: {base_url}/chat/completions（Ollama 默认为 {base_url}/api/chat）。",
            ));
            guide_lines.push(Line::from(
                "  • 适用于特殊自建网关、API 代理或版本路由（如 /v1/chat/completions）。",
            ));
        }
        "models_endpoint" => {
            guide_lines.push(Line::from(vec![Span::styled(
                "▸ 自定义模型列表端点 (models_endpoint):",
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )]));
            guide_lines.push(Line::from(
                "  • 高级选项。自定义模型列表拉取端点完整 URL 或路径。",
            ));
            guide_lines.push(Line::from(
                "  • 留空时默认使用: {base_url}/models（Ollama 默认为 {base_url}/api/tags）。",
            ));
            guide_lines.push(Line::from(
                "  • 适用于拉取端点与对话端点不在同一子路径的自建代理服务。",
            ));
        }
        "thinking_type" => {
            guide_lines.push(Line::from(vec![Span::styled(
                "▸ 思考模式 (thinking.type):",
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )]));
            guide_lines.push(Line::from(
                "  • 未设置 = 不下发任何思考参数（与旧行为完全一致）。",
            ));
            guide_lines.push(Line::from(
                "  • enabled = 按协议下发开启参数: openai/openai-compatible → thinking={\"type\":\"enabled\"}；",
            ));
            guide_lines.push(Line::from(
                "    anthropic → thinking={\"type\":\"enabled\",\"budget_tokens\":max_tokens/2} 且 temperature 固定为 1；",
            ));
            guide_lines.push(Line::from(
                "    ollama → think=true；responses → reasoning={\"effort\":\"medium\"}。",
            ));
            guide_lines.push(Line::from(
                "  • disabled = openai 家族与 ollama 下发关闭值；anthropic 无对应字段故不下发。",
            ));
            guide_lines.push(Line::from(
                "  • 仅对支持思考的模型有意义；可用模型列表浮层内的 t 键实测。",
            ));
        }
        "thinking_effort" => {
            guide_lines.push(Line::from(vec![Span::styled(
                "▸ 思考强度 (thinking.effort):",
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )]));
            guide_lines.push(Line::from(
                "  • 未设置 = 不下发 reasoning_effort / reasoning.effort。",
            ));
            guide_lines.push(Line::from(
                "  • openai / openai-compatible → 请求体顶层 reasoning_effort = low|medium|high。",
            ));
            guide_lines.push(Line::from(
                "  • responses → reasoning = {\"effort\": low|medium|high}（未设 effort 但 type=enabled 时用 medium）。",
            ));
            guide_lines.push(Line::from(
                "  • anthropic / ollama 无对应参数，设置后被忽略（不会报错）。",
            ));
        }
        _ => {
            guide_lines.push(Line::from("  • 填写对应字段内容后，按 Tab 切换至下一项。"));
        }
    }

    guide_lines.push(Line::from(""));
    guide_lines.push(Line::from(vec![Span::styled(
        "💡 实测连通性提示:",
        Style::default().fg(CODE).add_modifier(Modifier::BOLD),
    )]));
    guide_lines.push(Line::from(
        "  保存后回到服务商列表，按下 T 键可直接向该端点发起网络与模型探针测速。",
    ));

    let guide_p = Paragraph::new(guide_lines).wrap(Wrap { trim: false });
    frame.render_widget(guide_p, guide_inner);

    // 3. 底部状态与操作栏
    let status_line = if !status.is_empty() {
        let is_err = status.to_lowercase().contains("error")
            || status.to_lowercase().contains("invalid")
            || status.contains("失败")
            || status.contains("错误")
            || status.contains("cannot");
        let st_style = if is_err {
            Style::default().fg(ERROR).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD)
        };
        Line::from(vec![
            Span::styled("提示: ", Style::default().fg(MUTED)),
            Span::styled(status.to_string(), st_style),
        ])
    } else {
        Line::from(Span::styled(
            "就绪 · 修改后可按 Ctrl+S 快速保存",
            Style::default().fg(DIM),
        ))
    };

    let mut hint_spans = vec![
        Span::styled(" [ Tab/Shift+Tab 切换字段 ] ", Style::default().fg(MUTED)),
        Span::styled(" [ ↑/↓ 上下行 ] ", Style::default().fg(MUTED)),
        Span::styled(
            " [ ←/→ 或 Space 切换协议/思考 ] ",
            Style::default().fg(CODE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " [ Enter 选择模型 ] ",
            Style::default().fg(CODE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " [ Ctrl+S 保存生效 ] ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" [ Esc 取消返回 ] ", Style::default().fg(MUTED)),
    ];
    if form.model_picker.as_ref().is_some_and(|p| p.manual) {
        hint_spans.push(Span::styled(
            " 手输模式: 直接编辑 model 字段，Enter 退出 ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    }
    let hint_line = Line::from(hint_spans).alignment(ratatui::layout::Alignment::Center);

    let footer_p = Paragraph::new(vec![status_line, hint_line]);
    frame.render_widget(footer_p, footer_area);

    // 4. 模型列表浮层（手输模式不画）。
    if let Some(p) = form.model_picker.as_ref().filter(|p| !p.manual) {
        draw_form_model_picker(frame, area, p);
    }
}

/// 绘制 /provider 表单的模型选择浮层（居中，带能力标签与粘性滚动）。
fn draw_form_model_picker(frame: &mut Frame, area: Rect, picker: &FormModelPicker) {
    let width = (area.width * 70 / 100).max(50).min(area.width);
    let height = (area.height * 60 / 100).max(8).min(area.height);
    let rect = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    if rect.width < 20 || rect.height < 4 {
        return;
    }
    frame.render_widget(Clear, rect);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(
            Line::from(" 🤖 选择默认模型 / Select Model ")
                .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        );
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let chunks = Layout::vertical([
        Constraint::Min(0),                                             // 列表
        Constraint::Length(if picker.error.is_some() { 2 } else { 1 }), // 提示 + 错误
    ])
    .split(inner);
    let list_area = chunks[0];
    let footer_area = chunks[1];

    let sel_bg = Color::Rgb(38, 79, 120);
    let mut lines: Vec<Line> = Vec::new();
    for (i, e) in picker.entries.iter().enumerate() {
        let selected = i == picker.selected;
        let marker = if selected { "▸ " } else { "  " };
        let row_style = if selected {
            Style::default()
                .bg(sel_bg)
                .fg(FG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(FG)
        };
        let mut spans = vec![Span::styled(format!("{marker}{}", e.id), row_style)];
        if e.vision.is_supported() {
            spans.push(Span::styled("  ◈ 视觉", Style::default().fg(Color::Cyan)));
        }
        if e.reasoning.is_supported() {
            spans.push(Span::styled(
                "  ◈ 推理",
                Style::default().fg(Color::LightGreen),
            ));
        }
        lines.push(Line::from(clipped_spans(spans, list_area.width as usize)));
    }

    // 粘性滚动：选中项溢出视口时自动调整。
    let visible_h = list_area.height as usize;
    let total = lines.len();
    let scroll = if total <= visible_h || picker.selected < visible_h {
        0
    } else {
        (picker.selected + 1 - visible_h).min(total.saturating_sub(visible_h))
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(FG))
            .scroll((scroll as u16, 0)),
        list_area,
    );

    let mut footer: Vec<Line> = vec![Line::from(Span::styled(
        " ↑/↓ 选择 · Enter 确认 · r 重新拉取 · m/f 手输模型名 · t 实测推理 · v 实测视觉 · Esc 关闭 ",
        Style::default().fg(DIM),
    ))];
    if let Some(err) = &picker.error {
        footer.push(Line::from(Span::styled(
            format!(" ⚠ {err}"),
            Style::default().fg(ERROR),
        )));
    }
    frame.render_widget(Paragraph::new(footer), footer_area);
}

fn draw_settings_panel(
    frame: &mut Frame,
    bounds: Rect,
    settings: &CliSettingsState,
    screen: &CliScreen,
) {
    if bounds.width < 40 || bounds.height < 6 {
        return;
    }

    let popup_area = bounds;

    frame.render_widget(Clear, popup_area);

    let border_color = if settings.dirty { ACCENT } else { DIM };
    let (dirty_badge, badge_style) = if settings.dirty {
        (
            " [● 已修改未保存] ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            " [✔ 配置已实时保存] ",
            Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
        )
    };
    let mut title_spans = vec![
        Span::styled(
            " ⚙ Cyber Master 全局设置中心 (Settings) ",
            Style::default().fg(CODE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(dirty_badge, badge_style),
    ];
    if screen.setup_mode {
        title_spans.push(Span::styled(
            " · Setup ",
            Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
        ));
    }
    let title = Line::from(title_spans);
    let block = Block::bordered()
        .border_style(Style::default().fg(border_color))
        .title(title);
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let chunks = Layout::vertical([
        Constraint::Length(1), // Tabs
        Constraint::Length(1), // Divider
        Constraint::Min(3),    // Content
        Constraint::Length(1), // Tip
        Constraint::Length(1), // Buttons
        Constraint::Length(1), // Key hints
    ])
    .split(inner);

    let tab_line = render_tab_bar(chunks[0].width, settings.tab);
    frame.render_widget(Paragraph::new(tab_line), chunks[0]);

    let div_str = "─".repeat(chunks[1].width as usize);
    frame.render_widget(
        Paragraph::new(Line::styled(div_str, Style::default().fg(DIM))),
        chunks[1],
    );

    match settings.tab {
        SettingsTab::AgentModel => draw_tab_agent_model(frame, chunks[2], settings, screen),
        SettingsTab::UiWorkflow => draw_tab_ui_workflow(frame, chunks[2], settings, screen),
        SettingsTab::Subagents => draw_tab_subagents(frame, chunks[2], settings),
        SettingsTab::ToolsMcp => draw_tab_tools_mcp(frame, chunks[2], settings, screen),
        SettingsTab::Providers => draw_tab_providers(frame, chunks[2], settings),
        SettingsTab::EnvMemory => draw_tab_env_memory(frame, chunks[2], settings),
        SettingsTab::StorageSystem => draw_tab_storage_system(frame, chunks[2], settings),
        SettingsTab::Toolbox => draw_tab_toolbox(frame, chunks[2], settings, screen),
        SettingsTab::About => {
            draw_tab_about(frame, chunks[2], screen.about.as_ref(), screen.about_scroll)
        }
    }

    let provider_tip;
    let tip_text = match settings.tab {
        SettingsTab::AgentModel => "💡 提示: 思考强度用于控制 DeepSeek reasoning / Claude thinking 预算。按 Ctrl+S 立即保存生效。",
        SettingsTab::UiWorkflow => "💡 提示: 若需使用终端原生划词复制功能，可在此处将鼠标捕获关闭。主题与鼠标即时生效。",
        SettingsTab::Subagents => "💡 提示: 子任务并发数受本地 CPU 与服务商 API 频率限制，推荐配置为 2~6 个并发。",
        SettingsTab::ToolsMcp => {
            if settings.selected_row >= 3 {
                "💡 提示: 按 Enter 打开查看该 Skill 的详细配置与 SKILL.md Markdown 指令正文；弹窗内可按 ←/→ 切换技能。"
            } else {
                "💡 提示: MCP 服务器配置存储于 mcp.json，可输入 /mcp 调试；向下滚动即可逐项浏览并查阅已加载 Skills。"
            }
        }
        SettingsTab::Providers => {
            let names = settings.providers_draft.sorted_names();
            if let Some(name) = names.get(settings.selected_row) {
                if let Some((is_ok, msg, rtt)) = settings.provider_test_results.get(name) {
                    let icon = if *is_ok { "🟢" } else { "🔴" };
                    provider_tip = format!("💡 测速结果 [{name}]: {icon} 延迟 {rtt}ms · {msg}");
                    &provider_tip
                } else {
                    "💡 提示: 按 Enter 即可快速将高亮服务商切换为全局默认 Provider；按 A 键可打开预设/自定义向导添加服务商，T 键测试连通性，M 键浏览模型。"
                }
            } else {
                "💡 提示: 按 Enter 即可快速将高亮服务商切换为全局默认 Provider；按 A 键可打开预设/自定义向导添加服务商，T 键测试连通性，M 键浏览模型。"
            }
        }
        SettingsTab::EnvMemory => {
            let env_count = settings.config_draft.env.vars.len();
            let mem_count = settings.config_draft.memory.rules.len();
            let env_slots = env_count.max(1);
            let mem_slots = mem_count.max(1);
            if settings.selected_row < env_slots {
                "💡 环境变量提示: 注入到工具执行子进程 (Bash/Shell/Agent)；标记为脱敏保护的变量在终端与日志中均会打码遮蔽。"
            } else if settings.selected_row < env_slots + mem_slots {
                "💡 记忆规则提示: 每次交互时自动注入 Agent 顶层 Prompt；按 Enter 进入编辑后再用 Space 开关启用、←/→ 轮换作用域 (全局/项目)；E 打开编辑表单。"
            } else {
                "💡 记忆列表提示: 以下为 memory.md 中已写入的长期记忆条目 (全局/项目级)；按 Enter 查看完整正文，弹窗内 ←/→ 切换上/下一条。"
            }
        }
        SettingsTab::StorageSystem => "💡 提示: 日志级别调整后将在后台日志中实时生效；如需排查详细工具执行流，建议调至 debug 级别。",
        SettingsTab::Toolbox => "💡 提示: 首行 Enter 打开 AI 扫描表单（可输入路径/提示词并选择模型）· Enter/E 编辑工具 · A 添加 · D 删除；扫描结果输出到对话区。",
        SettingsTab::About => "💡 提示: 只读页 · ↑/↓ 或滚轮滚动 · U/Enter 检查更新；底部为快捷键与斜杠命令速查（等价于按 ? 打开 Shortcuts 浮层）。",
    };
    frame.render_widget(
        Paragraph::new(Line::styled(tip_text, Style::default().fg(MUTED))),
        chunks[3],
    );

    let buttons = match settings.tab {
        SettingsTab::Providers if chunks[4].width < 100 => Line::from(vec![
            Span::styled(
                " [ A 添加 ] ",
                Style::default().fg(CODE).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(" [ T 测活 ] ", Style::default().fg(ACCENT)),
            Span::raw(" "),
            Span::styled(" [ E 编辑 ] ", Style::default().fg(CODE)),
            Span::raw(" "),
            Span::styled(" [ D 删除 ] ", Style::default().fg(ERROR)),
            Span::raw(" "),
            Span::styled(" [ Esc 关闭 ] ", Style::default().fg(MUTED)),
        ]),
        SettingsTab::Providers => Line::from(vec![
            Span::styled(
                " [ A 预设/自定义添加 ] ",
                Style::default().fg(CODE).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(" [ T 连通测速 ] ", Style::default().fg(ACCENT)),
            Span::raw(" "),
            Span::styled(" [ M 查看模型 ] ", Style::default().fg(CODE)),
            Span::raw(" "),
            Span::styled(" [ E 编辑 ] ", Style::default().fg(CODE)),
            Span::raw(" "),
            Span::styled(" [ D 删除 ] ", Style::default().fg(ERROR)),
            Span::raw(" "),
            Span::styled(
                " [ S 保存 (Ctrl+S) ] ",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(" [ Esc 关闭 ] ", Style::default().fg(MUTED)),
        ]),
        SettingsTab::EnvMemory if chunks[4].width < 100 => Line::from(vec![
            Span::styled(
                " [ A 添加 ] ",
                Style::default().fg(CODE).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                " [ E 编辑 ] ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                " [ Space 切换 ] ",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                " [ D 删除 ] ",
                Style::default().fg(ERROR).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(" [ Esc 关闭 ] ", Style::default().fg(MUTED)),
        ]),
        SettingsTab::EnvMemory => Line::from(vec![
            Span::styled(
                " [ A 添加 ] ",
                Style::default().fg(CODE).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                " [ E 编辑 ] ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                " [ Space 切换状态 ] ",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                " [ ←/→ 切换作用域 ] ",
                Style::default().fg(CODE).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                " [ D 删除 ] ",
                Style::default().fg(ERROR).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                " [ S 保存 (Ctrl+S) ] ",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(" [ Esc 关闭 ] ", Style::default().fg(MUTED)),
        ]),
        SettingsTab::About => Line::from(vec![
            Span::styled(
                " [ U 检查更新 ] ",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(" [ ↑/↓ 滚动 ] ", Style::default().fg(CODE)),
            Span::raw("  "),
            Span::styled(" [ Esc 关闭 ] ", Style::default().fg(MUTED)),
        ]),
        _ => Line::from(vec![
            Span::styled(
                " [ S 保存生效 (Ctrl+S) ] ",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(" [ Esc 关闭 ] ", Style::default().fg(MUTED)),
            Span::raw("  "),
            Span::styled(" [ R 恢复默认 ] ", Style::default().fg(CODE)),
        ]),
    };
    frame.render_widget(Paragraph::new(buttons), chunks[4]);

    let nav_hint = if screen.setup_mode {
        " 首次配置向导：↑/↓ 选择 · Enter 编辑 · ←/→ 切分类 · 选服务商(A) → 填 API Key → Ctrl+S 保存 · Tab/1-9 切换分类 · Esc 完成并退出 "
    } else if settings.editing {
        " 编辑中：←/→ 调整数值 · Enter/Esc 完成 · ↑/↓ 移动焦点 "
    } else {
        match settings.tab {
            SettingsTab::Providers if chunks[5].width < 100 => {
                "操作: ↑/↓ · Enter 设默认 · A 添加 · T 测活 · M 模型 · E 编辑 · D 删除 · ←/→ 切分类 · Esc 关闭"
            }
            SettingsTab::Providers => {
                "操作: ↑/↓ 选择 · Enter 设为默认 · A 添加 (预设/自定义协议) · T 测活 · M 模型 · E 编辑 · D 删除 · ←/→ 切分类 · Esc 关闭"
            }
            SettingsTab::EnvMemory if chunks[5].width < 90 => {
                "操作: ↑/↓ · Enter 编辑 (E 打开表单) · A 添加 · D 删除 · ←/→ 切分类 · Esc 关闭"
            }
            SettingsTab::EnvMemory => {
                "操作: ↑/↓ 选择项目 · Enter 编辑 (记忆列表 Enter 查看详情 · E 打开表单) · A 添加 · D 删除 · ←/→ 切分类 · Esc 关闭"
            }
            SettingsTab::ToolsMcp if chunks[5].width < 90 => {
                "操作: ↑/↓ 移动 · Enter 编辑/打开 · Tab/←/→ 轮换 · Esc 关闭"
            }
            SettingsTab::ToolsMcp => {
                if settings.selected_row >= 3 {
                    "操作: ↑/↓ 移动焦点 · Enter/空格/o 查看 Skill 详情 · Tab/←/→ 轮换标签 · 1-9 直达分类 · Esc 关闭"
                } else {
                    "操作: ↑/↓ 选择项目 · Enter 编辑 · ←/→ 轮换标签 · 1-9 直达分类 · Esc 关闭"
                }
            }
            SettingsTab::Toolbox => {
                "操作: ↑/↓ 选择行 (首行＝AI 扫描入口) · Enter/E 编辑工具 · A 添加 · D 删除 (连按两次) · ←/→ 轮换 · 1-9 直达 · Esc 关闭"
            }
            SettingsTab::About => "操作: ↑/↓ · PgUp/PgDn 滚动 · U 检查更新 · ←/→ 或 Tab/1-9 切换分类 · Esc 关闭",
            _ if chunks[5].width < 80 => {
                "操作: ↑/↓ 选择 · Enter 编辑 · ←/→ 切分类 · Tab 轮换 · 1-9 直达 · Esc 关闭"
            }
            _ => "操作: ↑/↓ 选择项目 · Enter 进入编辑 (再按 Enter/Esc 退出) · ←/→ 切分类 · Tab 轮换标签 · 1-9 直达分类 · Esc 关闭",
        }
    };
    frame.render_widget(
        Paragraph::new(Line::styled(nav_hint, Style::default().fg(DIM))),
        chunks[5],
    );

    if let Some(detail) = &settings.skill_detail {
        draw_skill_detail_modal(frame, popup_area, settings, detail);
    }

    if let Some(detail) = &settings.memory_detail {
        draw_memory_detail_modal(frame, popup_area, settings, detail);
    }

    if settings.pending_discard_confirm {
        draw_discard_modal(frame, popup_area);
    }
}

/// 打开 Provider 预设/自定义向导（设置中心 Providers 页 `A` 键与首次配置向导共用）。
///
/// 只接收 `CliScreen` 中与 `settings` 不重叠的字段，便于在持有 `settings` 可变借用
/// 的调用点直接调用。
fn open_provider_wizard(
    runner: &mut Option<SessionRunner>,
    settings_return_tab: &mut Option<SettingsTab>,
    panel: &mut Option<Panel>,
    picker: &mut Option<CommandPicker>,
    picker_selected: &mut usize,
) {
    let Some(owner) = runner.as_mut() else {
        return;
    };
    if let Ok(CliAction::Picker(next)) = cli_commands::execute(owner, "/provider wizard") {
        *settings_return_tab = Some(SettingsTab::Providers);
        *panel = None;
        *picker = Some(next);
        *picker_selected = 0;
    }
}

/// 向导模式的退出判定：配置可用才允许离开，否则留在面板并给出提示。
fn setup_finish(screen: &mut CliScreen, runner: &Option<SessionRunner>) -> bool {
    let usable = runner.as_ref().is_some_and(|owner| {
        cyber_core::setup::configured(&owner.ctx.config, &owner.ctx.providers)
    });
    if usable {
        return true;
    }
    screen.status = "尚未完成配置：请先选择服务商并填写 API Key（Ctrl+S 保存）".into();
    false
}

/// 落盘设置草稿并立即生效。
///
/// setup 模式走 `cyber_core::setup::commit`（两阶段原子提交 + 完成标记，写盘失败必须上报）；
/// 普通模式沿用 `save_config` / `save_providers`（providers 写盘失败与现状一致被忽略）。
fn persist_settings(
    screen: &CliScreen,
    runner: &mut Option<SessionRunner>,
) -> std::result::Result<(), String> {
    let Some(settings) = screen.settings.as_ref() else {
        return Ok(());
    };
    let (config_draft, providers_draft) = (
        settings.config_draft.clone(),
        settings.providers_draft.clone(),
    );
    let Some(owner) = runner.as_mut() else {
        return Ok(());
    };
    if screen.setup_mode {
        cyber_core::setup::commit(&owner.ctx.paths, &config_draft, &providers_draft)
            .map_err(|error| error.to_string())?;
    } else {
        cyber_core::save_config(&config_draft, &owner.ctx.paths.config_file)
            .map_err(|error| error.to_string())?;
        let _ = cyber_core::save_providers(&providers_draft, &owner.ctx.paths.providers_file);
    }
    owner.ctx.providers = providers_draft;
    owner.ctx.config = config_draft;
    Ok(())
}

fn save_settings_state(
    screen: &mut CliScreen,
    runner: &mut Option<SessionRunner>,
    permissions: &Arc<PermissionBroker>,
    exit: bool,
) {
    let Some(settings) = screen.settings.as_ref() else {
        if exit && !screen.setup_mode {
            screen.panel = None;
        }
        return;
    };
    // 回合进行中 runner 已被 take 走：草稿只能保留，落盘必须等回合结束（不得假成功）。
    if runner.is_none() {
        screen.status =
            "回合进行中：设置面板可查看/编辑，保存将在回合结束后生效（请稍后再按 Ctrl+S）".into();
        return;
    }
    let new_mouse = settings.config_draft.ui.mouse;
    let mouse_changed = runner
        .as_ref()
        .is_some_and(|owner| owner.ctx.config.ui.mouse != new_mouse);

    if let Err(error) = persist_settings(screen, runner) {
        screen.status = format!("保存配置失败: {error}");
        if exit && !screen.setup_mode {
            screen.panel = None;
            screen.settings = None;
        }
        return;
    }

    if let Some(owner) = runner.as_mut() {
        if let Some(mode_str) = &owner.ctx.config.agent.permission_mode {
            if let Some(mode) = PermissionMode::parse(mode_str) {
                screen.permission_mode = mode;
                permissions.set_mode(mode);
            }
        }
        screen.effort = owner.ctx.config.agent.thinking_intensity;

        if mouse_changed {
            if new_mouse {
                let _ = execute!(io::stdout(), EnableMouseCapture);
            } else {
                let _ = execute!(io::stdout(), DisableMouseCapture);
            }
        }

        screen.sync(owner);
    }
    if let Some(settings) = screen.settings.as_mut() {
        settings.dirty = false;
        settings.pending_discard_confirm = false;
    }
    screen.status = "✔ 设置已保存并立即生效".into();

    // 向导模式必须由 Esc 显式完成退出，保存本身不关面板。
    if exit && !screen.setup_mode {
        screen.panel = None;
        screen.settings = None;
    }
}

/// 回合进行中（runner 已被 `take()` 走）需要 runner 的设置操作一律拦下并提示，
/// 不做静默失效。纯草稿编辑（方向键、开关、枚举切换）不受此限制。
fn defer_settings_action(status: &mut String, runner: &Option<SessionRunner>) -> bool {
    if runner.is_none() {
        *status = "回合进行中：该操作需等回合结束后再执行".into();
        true
    } else {
        false
    }
}

fn handle_settings_key(
    screen: &mut CliScreen,
    runner: &mut Option<SessionRunner>,
    permissions: &Arc<PermissionBroker>,
    key: KeyEvent,
) -> color_eyre::Result<bool> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // F3 or Ctrl+, toggles settings close (with discard protection)
    if key.code == KeyCode::F(3) || (control && key.code == KeyCode::Char(',')) {
        if screen.setup_mode {
            if screen.settings.as_ref().is_some_and(|s| s.dirty) {
                screen.status = "请先 Ctrl+S 保存配置".into();
                return Ok(false);
            }
            if defer_settings_action(&mut screen.status, runner) {
                return Ok(false);
            }
            return Ok(setup_finish(screen, runner));
        }
        if let Some(settings) = screen.settings.as_mut() {
            if settings.dirty {
                settings.pending_discard_confirm = true;
                return Ok(false);
            }
        }
        screen.panel = None;
        screen.settings = None;
        return Ok(false);
    }

    if let Some(settings) = screen.settings.as_mut() {
        if settings.pending_discard_confirm {
            match key.code {
                KeyCode::Esc => {
                    screen.panel = None;
                    screen.settings = None;
                    screen.status = "已放弃未保存修改".into();
                    return Ok(false);
                }
                KeyCode::Enter | KeyCode::Char('s') | KeyCode::Char('S') => {
                    save_settings_state(screen, runner, permissions, true);
                    return Ok(false);
                }
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Char('c')
                | KeyCode::Char('C') => {
                    settings.pending_discard_confirm = false;
                    return Ok(false);
                }
                _ => return Ok(false),
            }
        }
    }

    if let Some(settings) = screen.settings.as_mut() {
        if let Some(detail) = settings.skill_detail.as_mut() {
            match key.code {
                // 退出弹窗返回列表
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                    settings.skill_detail = None;
                    return Ok(false);
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    settings.skill_detail = None;
                    return Ok(false);
                }
                // 滚屏操作
                KeyCode::Up | KeyCode::Char('k') => {
                    detail.scroll = detail.scroll.saturating_sub(1);
                    return Ok(false);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    detail.scroll = detail.scroll.saturating_add(1);
                    return Ok(false);
                }
                KeyCode::PageUp => {
                    detail.scroll = detail.scroll.saturating_sub(10);
                    return Ok(false);
                }
                KeyCode::PageDown => {
                    detail.scroll = detail.scroll.saturating_add(10);
                    return Ok(false);
                }
                KeyCode::Home => {
                    detail.scroll = 0;
                    return Ok(false);
                }
                KeyCode::End => {
                    detail.scroll = usize::MAX / 2;
                    return Ok(false);
                }
                // 左右键无缝切换上一个/下一个技能
                KeyCode::Left | KeyCode::Char('h') => {
                    if detail.skill_index > 0 {
                        detail.skill_index -= 1;
                        detail.scroll = 0;
                        settings.selected_row = 3 + detail.skill_index; // 同步外层光标！
                    }
                    return Ok(false);
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    if detail.skill_index + 1 < settings.skills.len() {
                        detail.skill_index += 1;
                        detail.scroll = 0;
                        settings.selected_row = 3 + detail.skill_index; // 同步外层光标！
                    }
                    return Ok(false);
                }
                _ => return Ok(false),
            }
        }
    }

    if let Some(settings) = screen.settings.as_mut() {
        let env_base = settings.config_draft.env.vars.len().max(1)
            + settings.config_draft.memory.rules.len().max(1);
        let g = settings.memories_global.len();
        let total = g + settings.memories_project.len();
        if let Some(detail) = settings.memory_detail.as_mut() {
            match key.code {
                // 退出弹窗返回列表
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                    settings.memory_detail = None;
                    return Ok(false);
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    settings.memory_detail = None;
                    return Ok(false);
                }
                // 滚屏操作
                KeyCode::Up | KeyCode::Char('k') => {
                    detail.scroll = detail.scroll.saturating_sub(1);
                    return Ok(false);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    detail.scroll = detail.scroll.saturating_add(1);
                    return Ok(false);
                }
                KeyCode::PageUp => {
                    detail.scroll = detail.scroll.saturating_sub(10);
                    return Ok(false);
                }
                KeyCode::PageDown => {
                    detail.scroll = detail.scroll.saturating_add(10);
                    return Ok(false);
                }
                KeyCode::Home => {
                    detail.scroll = 0;
                    return Ok(false);
                }
                KeyCode::End => {
                    detail.scroll = usize::MAX / 2;
                    return Ok(false);
                }
                // 左右键无缝切换上一条/下一条记忆（跨分组）
                KeyCode::Left | KeyCode::Char('h') if total > 0 => {
                    let cur = match detail.group {
                        MemoryGroup::Global => detail.entry,
                        MemoryGroup::Project => g + detail.entry,
                    };
                    if cur > 0 {
                        let new = cur - 1;
                        let (grp, ent) = if new < g {
                            (MemoryGroup::Global, new)
                        } else {
                            (MemoryGroup::Project, new - g)
                        };
                        detail.group = grp;
                        detail.entry = ent;
                        detail.scroll = 0;
                        settings.selected_row = env_base + new; // 同步外层光标！
                    }
                    return Ok(false);
                }
                KeyCode::Right | KeyCode::Char('l') if total > 0 => {
                    let cur = match detail.group {
                        MemoryGroup::Global => detail.entry,
                        MemoryGroup::Project => g + detail.entry,
                    };
                    if cur + 1 < total {
                        let new = cur + 1;
                        let (grp, ent) = if new < g {
                            (MemoryGroup::Global, new)
                        } else {
                            (MemoryGroup::Project, new - g)
                        };
                        detail.group = grp;
                        detail.entry = ent;
                        detail.scroll = 0;
                        settings.selected_row = env_base + new; // 同步外层光标！
                    }
                    return Ok(false);
                }
                _ => return Ok(false),
            }
        }
    }

    if control && (key.code == KeyCode::Char('s') || key.code == KeyCode::Char('S')) {
        save_settings_state(screen, runner, permissions, false);
        return Ok(false);
    }

    // Ctrl+D 已不再是退出快捷键：设置中心各段的单字母 `d` 是「删除 provider / 环境变量 /
    // 记忆规则」，必须显式吞掉，避免「移除退出键」变成「误删数据键」。
    if control && key.code == KeyCode::Char('d') {
        return Ok(false);
    }

    // ── 设置项编辑态 / 只读态 ─────────────────────────────────────────────
    // 只读态：←/→（h/l）切换标签页；Enter 对「可编辑值行」进入编辑态；值行上的空格被吞掉。
    // 编辑态：←/→/h/l/空格 落到下方 per-tab 分支改值；Enter/Esc 退出编辑；
    //         ↑/↓/Home/End/PgUp/PgDn/k/j 退出编辑并继续走下方移动逻辑。
    {
        let editing = screen.settings.as_ref().is_some_and(|s| s.editing);
        let is_value_row = screen
            .settings
            .as_ref()
            .is_some_and(CliSettingsState::focused_row_is_value);
        if editing {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    if let Some(settings) = screen.settings.as_mut() {
                        settings.editing = false;
                    }
                    return Ok(false);
                }
                KeyCode::Up
                | KeyCode::Down
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Char('k')
                | KeyCode::Char('j') => {
                    if let Some(settings) = screen.settings.as_mut() {
                        settings.editing = false;
                    }
                    // 不 return：继续走下方既有移动逻辑
                }
                _ => {}
            }
        } else if matches!(key.code, KeyCode::Left | KeyCode::Char('h')) {
            if let Some(settings) = screen.settings.as_mut() {
                settings.prev_tab();
            }
            screen.sync_about_if_active();
            return Ok(false);
        } else if matches!(key.code, KeyCode::Right | KeyCode::Char('l')) {
            if let Some(settings) = screen.settings.as_mut() {
                settings.next_tab();
            }
            screen.sync_about_if_active();
            return Ok(false);
        } else if key.code == KeyCode::Enter && is_value_row {
            if let Some(settings) = screen.settings.as_mut() {
                settings.editing = true;
            }
            return Ok(false);
        } else if key.code == KeyCode::Char(' ') && is_value_row {
            screen.status = "按 Enter 进入编辑后再调整该项".into();
            return Ok(false);
        }
    }

    if key.code == KeyCode::Esc {
        if screen.setup_mode {
            if screen.settings.as_ref().is_some_and(|s| s.dirty) {
                screen.status = "请先 Ctrl+S 保存配置".into();
                return Ok(false);
            }
            if defer_settings_action(&mut screen.status, runner) {
                return Ok(false);
            }
            return Ok(setup_finish(screen, runner));
        }
        if let Some(settings) = screen.settings.as_mut() {
            if settings.dirty {
                settings.pending_discard_confirm = true;
                return Ok(false);
            }
        }
        screen.panel = None;
        screen.settings = None;
        return Ok(false);
    }

    if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
        if let Some(settings) = screen.settings.as_mut() {
            if shift || key.code == KeyCode::BackTab {
                settings.prev_tab();
            } else {
                settings.next_tab();
            }
        }
        screen.sync_about_if_active();
        return Ok(false);
    }

    if let KeyCode::Char(ch @ '1'..='9') = key.code {
        if let Some(settings) = screen.settings.as_mut() {
            let tabs = SettingsTab::all();
            let idx = (ch as usize) - ('1' as usize);
            if let Some(&tab) = tabs.get(idx) {
                settings.set_tab(tab);
            }
        }
        screen.sync_about_if_active();
        return Ok(false);
    }

    if key.code == KeyCode::Char('r') || key.code == KeyCode::Char('R') {
        if let Some(settings) = screen.settings.as_mut() {
            settings.reset_current_tab();
            // 「关于」页只读、无默认值可恢复（`reset_current_tab` 提前返回），不得报假成功。
            if settings.tab != SettingsTab::About {
                screen.status = format!("已将 {} 恢复为默认配置", settings.tab.title());
            }
        }
        return Ok(false);
    }

    if key.code == KeyCode::Char('s') || key.code == KeyCode::Char('S') {
        save_settings_state(screen, runner, permissions, false);
        return Ok(false);
    }

    // 「关于」页：只读长页。↑/↓/PgUp/PgDn/Home/End 滚动内容，U/Enter 走既有更新检查；
    // Esc/F3/Tab/数字键/Ctrl+S/←→ 已在上面处理，仍然可用。
    if screen
        .settings
        .as_ref()
        .is_some_and(|s| s.tab == SettingsTab::About)
    {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                screen.about_scroll = screen.about_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                screen.about_scroll = screen.about_scroll.saturating_add(1);
            }
            KeyCode::PageUp => {
                screen.about_scroll = screen.about_scroll.saturating_sub(5);
            }
            KeyCode::PageDown => {
                screen.about_scroll = screen.about_scroll.saturating_add(5);
            }
            KeyCode::Home => screen.about_scroll = 0,
            KeyCode::End => screen.about_scroll = usize::MAX, // 绘制时按内容高度收敛
            KeyCode::Char('u') | KeyCode::Char('U') | KeyCode::Enter => {
                screen.begin_update_check(cli_commands::CliUpdate::Check);
            }
            _ => {}
        }
        return Ok(false);
    }

    let Some(settings) = screen.settings.as_mut() else {
        return Ok(false);
    };

    let max_row = settings.tab.max_row(settings);

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            settings.selected_row = settings.selected_row.saturating_sub(1);
            return Ok(false);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            settings.selected_row = (settings.selected_row + 1).min(max_row);
            return Ok(false);
        }
        KeyCode::Home => {
            settings.selected_row = 0;
            return Ok(false);
        }
        KeyCode::End => {
            settings.selected_row = max_row;
            return Ok(false);
        }
        KeyCode::PageUp => {
            settings.selected_row = settings.selected_row.saturating_sub(5);
            return Ok(false);
        }
        KeyCode::PageDown => {
            settings.selected_row = (settings.selected_row + 5).min(max_row);
            return Ok(false);
        }
        _ => {}
    }

    match settings.tab {
        SettingsTab::AgentModel => match settings.selected_row {
            0 => {
                let names = settings.providers_draft.sorted_names();
                if !names.is_empty() {
                    let cur_idx = names
                        .iter()
                        .position(|n| n == &settings.config_draft.agent.default_provider)
                        .unwrap_or(0);
                    let next_idx = match key.code {
                        KeyCode::Left | KeyCode::Char('h') => {
                            (cur_idx + names.len() - 1) % names.len()
                        }
                        KeyCode::Right
                        | KeyCode::Char('l')
                        | KeyCode::Enter
                        | KeyCode::Char(' ') => (cur_idx + 1) % names.len(),
                        _ => cur_idx,
                    };
                    if next_idx != cur_idx {
                        settings.config_draft.agent.default_provider = names[next_idx].clone();
                        settings.dirty = true;
                    }
                }
            }
            1 => {}
            2 => {
                let cur = settings.config_draft.agent.thinking_intensity;
                let idx = SETTINGS_THINKING_LEVELS
                    .iter()
                    .position(|&t| t == cur)
                    .unwrap_or(1);
                let next_idx = match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        (idx + SETTINGS_THINKING_LEVELS.len() - 1) % SETTINGS_THINKING_LEVELS.len()
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter | KeyCode::Char(' ') => {
                        (idx + 1) % SETTINGS_THINKING_LEVELS.len()
                    }
                    _ => idx,
                };
                if next_idx != idx {
                    settings.config_draft.agent.thinking_intensity =
                        SETTINGS_THINKING_LEVELS[next_idx];
                    settings.dirty = true;
                }
            }
            3 => {
                let cur_str = settings
                    .config_draft
                    .agent
                    .permission_mode
                    .as_deref()
                    .unwrap_or("auto");
                let cur_mode = PermissionMode::parse(cur_str).unwrap_or(PermissionMode::Auto);
                let idx = SETTINGS_PERMISSION_MODES
                    .iter()
                    .position(|&m| m == cur_mode)
                    .unwrap_or(0);
                let next_idx = match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        (idx + SETTINGS_PERMISSION_MODES.len() - 1)
                            % SETTINGS_PERMISSION_MODES.len()
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter | KeyCode::Char(' ') => {
                        (idx + 1) % SETTINGS_PERMISSION_MODES.len()
                    }
                    _ => idx,
                };
                if next_idx != idx {
                    settings.config_draft.agent.permission_mode =
                        Some(SETTINGS_PERMISSION_MODES[next_idx].code().into());
                    settings.dirty = true;
                }
            }
            4 => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter
                ) {
                    settings.config_draft.agent.auto_tool_call =
                        !settings.config_draft.agent.auto_tool_call;
                    settings.dirty = true;
                }
            }
            5 => {
                let step = if shift { 50 } else { 10 };
                let cur = settings.config_draft.agent.max_steps;
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        settings.config_draft.agent.max_steps = cur.saturating_sub(step).max(1);
                        settings.dirty = true;
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        settings.config_draft.agent.max_steps = (cur + step).min(1000);
                        settings.dirty = true;
                    }
                    _ => {}
                }
            }
            6 => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter
                ) {
                    settings.config_draft.tools.web_search =
                        !settings.config_draft.tools.web_search;
                    settings.dirty = true;
                }
            }
            7 => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter
                ) {
                    settings.config_draft.agent.vision.enabled =
                        !settings.config_draft.agent.vision.enabled;
                    settings.dirty = true;
                }
            }
            8 => {
                let mut names = vec!["".to_string()];
                names.extend(settings.providers_draft.sorted_names());
                let cur_idx = names
                    .iter()
                    .position(|n| n == &settings.config_draft.agent.vision.provider)
                    .unwrap_or(0);
                let next_idx = match key.code {
                    KeyCode::Left | KeyCode::Char('h') => (cur_idx + names.len() - 1) % names.len(),
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter | KeyCode::Char(' ') => {
                        (cur_idx + 1) % names.len()
                    }
                    _ => cur_idx,
                };
                if next_idx != cur_idx {
                    settings.config_draft.agent.vision.provider = names[next_idx].clone();
                    settings.dirty = true;
                }
            }
            9 => {}
            10 => {
                let step = if shift { 5 } else { 1 };
                let cur = settings.config_draft.agent.retry_attempts;
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        settings.config_draft.agent.retry_attempts = cur.saturating_sub(step);
                        settings.dirty = true;
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        settings.config_draft.agent.retry_attempts = (cur + step).min(20);
                        settings.dirty = true;
                    }
                    _ => {}
                }
            }
            11 => {
                let step = if shift { 5 } else { 1 };
                let cur = settings.config_draft.agent.retry_delay_secs;
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        settings.config_draft.agent.retry_delay_secs =
                            cur.saturating_sub(step).max(1);
                        settings.dirty = true;
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        settings.config_draft.agent.retry_delay_secs = (cur + step).min(60);
                        settings.dirty = true;
                    }
                    _ => {}
                }
            }
            _ => {}
        },
        SettingsTab::UiWorkflow => match settings.selected_row {
            0 => {
                let cur = settings.config_draft.ui.theme.as_str();
                let idx = SETTINGS_THEMES.iter().position(|&t| t == cur).unwrap_or(1);
                let next_idx = match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        (idx + SETTINGS_THEMES.len() - 1) % SETTINGS_THEMES.len()
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter | KeyCode::Char(' ') => {
                        (idx + 1) % SETTINGS_THEMES.len()
                    }
                    _ => idx,
                };
                if next_idx != idx {
                    settings.config_draft.ui.theme = SETTINGS_THEMES[next_idx].into();
                    settings.dirty = true;
                }
            }
            1 => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter
                ) {
                    settings.config_draft.ui.mouse = !settings.config_draft.ui.mouse;
                    settings.dirty = true;
                }
            }
            2 => {
                let cur = settings.config_draft.ui.default_mode.as_str();
                let idx = SETTINGS_MODES.iter().position(|&m| m == cur).unwrap_or(0);
                let next_idx = match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        (idx + SETTINGS_MODES.len() - 1) % SETTINGS_MODES.len()
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter | KeyCode::Char(' ') => {
                        (idx + 1) % SETTINGS_MODES.len()
                    }
                    _ => idx,
                };
                if next_idx != idx {
                    settings.config_draft.ui.default_mode = SETTINGS_MODES[next_idx].into();
                    settings.dirty = true;
                }
            }
            3 => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter
                ) {
                    settings.config_draft.ui.animations = !settings.config_draft.ui.animations;
                    settings.dirty = true;
                }
            }
            4 => {
                let cur = settings.config_draft.workflow.max_parallel_nodes;
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        settings.config_draft.workflow.max_parallel_nodes =
                            cur.saturating_sub(1).max(1);
                        settings.dirty = true;
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        settings.config_draft.workflow.max_parallel_nodes = (cur + 1).min(64);
                        settings.dirty = true;
                    }
                    _ => {}
                }
            }
            5 => {
                let cur = settings.config_draft.workflow.default_timeout_secs;
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        settings.config_draft.workflow.default_timeout_secs =
                            cur.saturating_sub(60).max(10);
                        settings.dirty = true;
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        settings.config_draft.workflow.default_timeout_secs = (cur + 60).min(86400);
                        settings.dirty = true;
                    }
                    _ => {}
                }
            }
            6 => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter
                ) {
                    settings.config_draft.workflow.checkpoint =
                        !settings.config_draft.workflow.checkpoint;
                    settings.dirty = true;
                }
            }
            _ => {}
        },
        SettingsTab::Subagents => match settings.selected_row {
            0 => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter
                ) {
                    settings.config_draft.agent.subagents.enabled =
                        !settings.config_draft.agent.subagents.enabled;
                    settings.dirty = true;
                }
            }
            1 => {
                let cur = settings.config_draft.agent.subagents.max_tasks;
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        settings.config_draft.agent.subagents.max_tasks =
                            cur.saturating_sub(1).max(1);
                        settings.dirty = true;
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        settings.config_draft.agent.subagents.max_tasks = (cur + 1).min(64);
                        settings.dirty = true;
                    }
                    _ => {}
                }
            }
            2 => {
                let cur = settings.config_draft.agent.subagents.max_parallel;
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        settings.config_draft.agent.subagents.max_parallel =
                            cur.saturating_sub(1).max(1);
                        settings.dirty = true;
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        settings.config_draft.agent.subagents.max_parallel = (cur + 1).min(16);
                        settings.dirty = true;
                    }
                    _ => {}
                }
            }
            3 => {
                let cur = settings.config_draft.agent.subagents.timeout_secs;
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        settings.config_draft.agent.subagents.timeout_secs =
                            cur.saturating_sub(30).max(10);
                        settings.dirty = true;
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        settings.config_draft.agent.subagents.timeout_secs = (cur + 30).min(3600);
                        settings.dirty = true;
                    }
                    _ => {}
                }
            }
            4 => {
                let cur = settings.config_draft.agent.subagents.max_steps;
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        settings.config_draft.agent.subagents.max_steps =
                            cur.saturating_sub(5).max(5);
                        settings.dirty = true;
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        settings.config_draft.agent.subagents.max_steps = (cur + 5).min(100);
                        settings.dirty = true;
                    }
                    _ => {}
                }
            }
            _ => {}
        },
        SettingsTab::ToolsMcp => match settings.selected_row {
            0 => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter
                ) {
                    settings.config_draft.tools.prefer_docker =
                        !settings.config_draft.tools.prefer_docker;
                    settings.dirty = true;
                }
            }
            1 => {
                if defer_settings_action(&mut screen.status, runner) {
                    return Ok(false);
                }
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter
                ) {
                    screen.ctf_enabled = !screen.ctf_enabled;
                    if let Some(r) = runner.as_mut() {
                        r.ctf_enabled = screen.ctf_enabled;
                    }
                    settings.dirty = true;
                }
            }
            2 if key.code == KeyCode::Enter || key.code == KeyCode::Char(' ') => {
                let (config, path) = match runner.as_ref() {
                    Some(owner) => (
                        cyber_mcp::McpServersConfig::load(&owner.ctx.paths.mcp_servers_file)
                            .unwrap_or_default(),
                        owner.ctx.paths.mcp_servers_file.clone(),
                    ),
                    None => (
                        cyber_mcp::McpServersConfig::load(
                            &screen.view_snapshot.paths.mcp_servers_file,
                        )
                        .unwrap_or_default(),
                        screen.view_snapshot.paths.mcp_servers_file.clone(),
                    ),
                };
                let mcp = match runner.as_ref() {
                    Some(owner) => owner.registries.mcp.clone(),
                    None => screen.view_snapshot.mcp.clone(),
                };
                screen.mcp_panel = Some(crate::views::mcp_panel::McpPanelState::new(
                    config,
                    mcp.as_deref(),
                    path,
                ));
                screen.panel = Some(Panel::Mcp);
                return Ok(false);
            }
            idx if idx >= 3 => {
                let skill_idx = idx - 3;
                if skill_idx < settings.skills.len()
                    && matches!(
                        key.code,
                        KeyCode::Enter
                            | KeyCode::Char('o')
                            | KeyCode::Char('O')
                            | KeyCode::Char(' ')
                    )
                {
                    settings.skill_detail = Some(SkillDetailModal {
                        skill_index: skill_idx,
                        scroll: 0,
                    });
                }
            }
            _ => {}
        },
        SettingsTab::Toolbox => {
            // A 添加自定义工具：与当前行无关（空清单或停在扫描行上同样可用）。
            if matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A')) {
                if defer_settings_action(&mut screen.status, runner) {
                    return Ok(false);
                }
                if let Some(owner) = runner.as_mut() {
                    if let Ok(CliAction::Form(form)) = cli_commands::execute(owner, "/toolbox add")
                    {
                        settings.tools_pending_delete = None;
                        screen.settings_return_tab = Some(SettingsTab::Toolbox);
                        screen.form = Some(FormState::new(form));
                        screen.panel = None;
                    }
                }
                return Ok(false);
            }
            if settings.selected_row == 0 {
                // 首行：打开 AI 智能扫描表单（目标路径/提示词 + 服务商/模型 + 仅预览）。
                if key.code == KeyCode::Enter {
                    if screen.setup_mode {
                        screen.status =
                            "请先 Ctrl+S 保存服务商配置；Esc 完成首次配置后即可运行 AI 扫描".into();
                        return Ok(false);
                    }
                    if defer_settings_action(&mut screen.status, runner) {
                        return Ok(false);
                    }
                    settings.tools_pending_delete = None;
                    let (provider, model) = scan_form_defaults(runner.as_ref());
                    screen.settings_return_tab = Some(SettingsTab::Toolbox);
                    screen.form = Some(FormState::new(cli_commands::toolbox_scan_form(
                        provider, model,
                    )));
                    screen.panel = None;
                    return Ok(false);
                }
            } else if let Some(tool_idx) = toolbox_tool_row(settings) {
                let name = settings.custom_tools[tool_idx].name.clone();
                match key.code {
                    KeyCode::Enter | KeyCode::Char('e') | KeyCode::Char('E') => {
                        if defer_settings_action(&mut screen.status, runner) {
                            return Ok(false);
                        }
                        if let Some(owner) = runner.as_mut() {
                            if let Ok(CliAction::Form(form)) =
                                cli_commands::execute(owner, &format!("/toolbox edit {name}"))
                            {
                                settings.tools_pending_delete = None;
                                screen.settings_return_tab = Some(SettingsTab::Toolbox);
                                screen.form = Some(FormState::new(form));
                                screen.panel = None;
                            }
                        }
                    }
                    KeyCode::Char('d') | KeyCode::Char('D') => {
                        if defer_settings_action(&mut screen.status, runner) {
                            return Ok(false);
                        }
                        if settings.tools_pending_delete == Some(tool_idx) {
                            if let Some(owner) = runner.as_mut() {
                                let _ = cli_commands::execute(
                                    owner,
                                    &format!("/toolbox remove {name}"),
                                );
                            }
                            screen.status =
                                format!("已删除自定义工具 {name}（重启后从工具表移除）");
                            let tools: Vec<cyber_core::CustomToolConfig> = runner
                                .as_ref()
                                .map(|owner| {
                                    cyber_core::load_custom_tools(&owner.ctx.paths.tools_dir)
                                        .0
                                        .into_iter()
                                        .map(|tool| tool.config)
                                        .collect()
                                })
                                .unwrap_or_default();
                            settings.custom_tools = tools;
                            settings.tools_pending_delete = None;
                            settings.selected_row =
                                settings.selected_row.min(settings.custom_tools.len());
                        } else {
                            settings.tools_pending_delete = Some(tool_idx);
                            screen.status = format!("再次按 D 确认删除自定义工具 {name}");
                        }
                    }
                    _ => {}
                }
            }
        }
        SettingsTab::Providers => {
            let names = settings.providers_draft.sorted_names();
            if let Some(name) = names.get(settings.selected_row) {
                match key.code {
                    KeyCode::Enter => {
                        settings.config_draft.agent.default_provider = name.clone();
                        settings.dirty = true;
                    }
                    KeyCode::Char('a') | KeyCode::Char('A') => {
                        if defer_settings_action(&mut screen.status, runner) {
                            return Ok(false);
                        }
                        open_provider_wizard(
                            runner,
                            &mut screen.settings_return_tab,
                            &mut screen.panel,
                            &mut screen.picker,
                            &mut screen.picker_selected,
                        );
                    }
                    KeyCode::Char('t') | KeyCode::Char('T') => {
                        if let Some(cfg) = settings.providers_draft.providers.get(name).cloned() {
                            let target_name = name.clone();
                            screen.status = format!("正在测试服务商 [{name}] 连通性与网络延迟...");
                            let start = std::time::Instant::now();
                            let res = tokio::task::block_in_place(|| {
                                tokio::runtime::Handle::current()
                                    .block_on(cyber_agent::fetch_models(&cfg))
                            });
                            let rtt = start.elapsed().as_millis();
                            match res {
                                Ok(models) => {
                                    let msg = format!("发现 {} 款可用模型", models.len());
                                    settings
                                        .provider_test_results
                                        .insert(target_name.clone(), (true, msg.clone(), rtt));
                                    screen.message(
                                        "Ping",
                                        &format!(
                                            "🟢 [{target_name}] 连通正常 (延迟: {rtt}ms，{msg})"
                                        ),
                                        SUCCESS,
                                    );
                                    screen.status =
                                        format!("服务商 [{target_name}] 连通正常 ({rtt}ms)");
                                }
                                Err(e) => {
                                    let err_msg = e.to_string();
                                    settings
                                        .provider_test_results
                                        .insert(target_name.clone(), (false, err_msg.clone(), rtt));
                                    screen.message(
                                        "Ping",
                                        &format!(
                                            "🔴 [{target_name}] 连通失败 ({rtt}ms): {err_msg}"
                                        ),
                                        ERROR,
                                    );
                                    screen.status = format!("服务商 [{target_name}] 连通失败");
                                }
                            }
                        }
                    }
                    KeyCode::Char('m') | KeyCode::Char('M') => {
                        let mut items = Vec::new();
                        if let Some(p) = settings.providers_draft.providers.get(name) {
                            let mut models: Vec<_> = p.models.keys().cloned().collect();
                            if !p.model.is_empty() && !models.contains(&p.model) {
                                models.push(p.model.clone());
                            }
                            sort_model_ids(&mut models);
                            for m in models {
                                let is_curr = m == p.model;
                                let label = if is_curr {
                                    format!("{m} (当前使用)")
                                } else {
                                    m.clone()
                                };
                                items.push(cli_commands::PickerItem {
                                    label,
                                    detail: format!("Provider: {name}"),
                                    command: format!("/model {name} {m}"),
                                });
                            }
                        }
                        if items.is_empty() {
                            items.push(cli_commands::PickerItem {
                                label: "(无独立模型配置)".into(),
                                detail: name.clone(),
                                command: String::new(),
                            });
                        }
                        screen.settings_return_tab = Some(SettingsTab::Providers);
                        screen.panel = None;
                        screen.picker = Some(cli_commands::CommandPicker {
                            title: format!("Models ({name})"),
                            items,
                            kind: cli_commands::PickerKind::Models,
                        });
                        screen.picker_selected = 0;
                    }
                    KeyCode::Char('e') | KeyCode::Char('E') => {
                        if defer_settings_action(&mut screen.status, runner) {
                            return Ok(false);
                        }
                        if let Some(r) = runner.as_mut() {
                            if let Ok(CliAction::Form(form)) =
                                cli_commands::execute(r, &format!("/provider edit {name}"))
                            {
                                screen.settings_return_tab = Some(SettingsTab::Providers);
                                screen.form = Some(FormState::new(form));
                                screen.panel = None;
                            }
                        }
                    }
                    KeyCode::Char('d') | KeyCode::Char('D') if names.len() > 1 => {
                        let deleted_name = name.clone();
                        settings.providers_draft.providers.remove(&deleted_name);
                        if settings.config_draft.agent.default_provider == deleted_name {
                            if let Some(first) = settings.providers_draft.sorted_names().first() {
                                settings.config_draft.agent.default_provider = first.clone();
                            }
                        }
                        settings.dirty = true;
                        if settings.selected_row >= settings.providers_draft.providers.len() {
                            settings.selected_row =
                                settings.providers_draft.providers.len().saturating_sub(1);
                        }
                    }
                    _ => {}
                }
            } else if key.code == KeyCode::Char('a') || key.code == KeyCode::Char('A') {
                if defer_settings_action(&mut screen.status, runner) {
                    return Ok(false);
                }
                open_provider_wizard(
                    runner,
                    &mut screen.settings_return_tab,
                    &mut screen.panel,
                    &mut screen.picker,
                    &mut screen.picker_selected,
                );
            }
        }
        SettingsTab::EnvMemory => {
            let env_count = settings.config_draft.env.vars.len();
            let mem_count = settings.config_draft.memory.rules.len();
            let env_slots = env_count.max(1);
            let mem_slots = mem_count.max(1);

            if settings.selected_row < env_slots {
                match key.code {
                    KeyCode::Char('a') | KeyCode::Char('A') => {
                        if defer_settings_action(&mut screen.status, runner) {
                            return Ok(false);
                        }
                        if let Some(r) = runner.as_mut() {
                            if settings.dirty {
                                r.ctx.config = settings.config_draft.clone();
                                let _ = cyber_core::save_config(
                                    &r.ctx.config,
                                    &r.ctx.paths.config_file,
                                );
                            }
                            if let Ok(CliAction::Form(form)) = cli_commands::execute(r, "/env add")
                            {
                                screen.settings_return_tab = Some(SettingsTab::EnvMemory);
                                screen.form = Some(FormState::new(form));
                                screen.panel = None;
                            }
                        }
                    }
                    // 只读态 Enter 已被路由层拦截为「进入编辑态」；打开编辑表单保留给 E。
                    KeyCode::Char('e') | KeyCode::Char('E') if env_count > 0 => {
                        let var_key = settings.config_draft.env.vars[settings.selected_row]
                            .key
                            .clone();
                        if defer_settings_action(&mut screen.status, runner) {
                            return Ok(false);
                        }
                        if let Some(r) = runner.as_mut() {
                            if settings.dirty {
                                r.ctx.config = settings.config_draft.clone();
                                let _ = cyber_core::save_config(
                                    &r.ctx.config,
                                    &r.ctx.paths.config_file,
                                );
                            }
                            if let Ok(CliAction::Form(form)) =
                                cli_commands::execute(r, &format!("/env edit {var_key}"))
                            {
                                screen.settings_return_tab = Some(SettingsTab::EnvMemory);
                                screen.form = Some(FormState::new(form));
                                screen.panel = None;
                            }
                        }
                    }
                    KeyCode::Char(' ') if env_count > 0 => {
                        if let Some(var) = settings
                            .config_draft
                            .env
                            .vars
                            .get_mut(settings.selected_row)
                        {
                            var.sensitive = !var.sensitive;
                            settings.dirty = true;
                            let status_str = if var.sensitive {
                                "已开启脱敏保护 (打码遮蔽)"
                            } else {
                                "已切换为明文公开"
                            };
                            screen.status = format!("环境变量 '{}': {}", var.key, status_str);
                        }
                    }
                    KeyCode::Char('d') | KeyCode::Char('D') if env_count > 0 => {
                        let removed = settings.config_draft.env.vars.remove(settings.selected_row);
                        settings.dirty = true;
                        screen.status = format!("已删除环境变量 '{}'", removed.key);
                        let max_r = settings.tab.max_row(settings);
                        settings.selected_row = settings.selected_row.min(max_r);
                    }
                    _ => {}
                }
            } else if settings.selected_row < env_slots + mem_slots {
                let rule_idx = settings.selected_row - env_slots;
                match key.code {
                    KeyCode::Char('a') | KeyCode::Char('A') => {
                        if defer_settings_action(&mut screen.status, runner) {
                            return Ok(false);
                        }
                        if let Some(r) = runner.as_mut() {
                            if settings.dirty {
                                r.ctx.config = settings.config_draft.clone();
                                let _ = cyber_core::save_config(
                                    &r.ctx.config,
                                    &r.ctx.paths.config_file,
                                );
                            }
                            if let Ok(CliAction::Form(form)) =
                                cli_commands::execute(r, "/memory rule add")
                            {
                                screen.settings_return_tab = Some(SettingsTab::EnvMemory);
                                screen.form = Some(FormState::new(form));
                                screen.panel = None;
                            }
                        }
                    }
                    // 只读态 Enter 已被路由层拦截为「进入编辑态」；打开编辑表单保留给 E。
                    KeyCode::Char('e') | KeyCode::Char('E') if mem_count > 0 => {
                        if defer_settings_action(&mut screen.status, runner) {
                            return Ok(false);
                        }
                        if let Some(r) = runner.as_mut() {
                            if settings.dirty {
                                r.ctx.config = settings.config_draft.clone();
                                let _ = cyber_core::save_config(
                                    &r.ctx.config,
                                    &r.ctx.paths.config_file,
                                );
                            }
                            if let Ok(CliAction::Form(form)) = cli_commands::execute(
                                r,
                                &format!("/memory rule edit {}", rule_idx + 1),
                            ) {
                                screen.settings_return_tab = Some(SettingsTab::EnvMemory);
                                screen.form = Some(FormState::new(form));
                                screen.panel = None;
                            }
                        }
                    }
                    KeyCode::Char(' ') if mem_count > 0 => {
                        if let Some(rule) = settings.config_draft.memory.rules.get_mut(rule_idx) {
                            rule.enabled = !rule.enabled;
                            settings.dirty = true;
                            let status_str = if rule.enabled {
                                "已开启"
                            } else {
                                "已关闭"
                            };
                            screen.status = format!("记忆规则 #{}: {}", rule_idx + 1, status_str);
                        }
                    }
                    KeyCode::Left | KeyCode::Right if mem_count > 0 => {
                        if let Some(rule) = settings.config_draft.memory.rules.get_mut(rule_idx) {
                            let scopes = ["both", "project", "global"];
                            let cur_idx = scopes.iter().position(|s| *s == rule.scope).unwrap_or(0);
                            let next_idx = if key.code == KeyCode::Left {
                                (cur_idx + scopes.len() - 1) % scopes.len()
                            } else {
                                (cur_idx + 1) % scopes.len()
                            };
                            rule.scope = scopes[next_idx].to_string();
                            settings.dirty = true;
                            let scope_label = match rule.scope.as_str() {
                                "both" => "全局与项目",
                                "project" => "项目级",
                                _ => "全局",
                            };
                            screen.status = format!(
                                "记忆规则 #{}: 作用域切换为 [{}]",
                                rule_idx + 1,
                                scope_label
                            );
                        }
                    }
                    KeyCode::Char('d') | KeyCode::Char('D')
                        if mem_count > 0 && rule_idx < settings.config_draft.memory.rules.len() =>
                    {
                        settings.config_draft.memory.rules.remove(rule_idx);
                        settings.dirty = true;
                        screen.status = format!("已删除记忆规则 #{}", rule_idx + 1);
                        let max_r = settings.tab.max_row(settings);
                        settings.selected_row = settings.selected_row.min(max_r);
                    }
                    _ => {}
                }
            } else {
                let mem_idx = settings.selected_row - env_slots - mem_slots;
                if matches!(
                    key.code,
                    KeyCode::Enter | KeyCode::Char('o') | KeyCode::Char('O') | KeyCode::Char(' ')
                ) {
                    if let Some((group, entry)) = settings.memory_at(mem_idx) {
                        settings.memory_detail = Some(MemoryDetailModal {
                            group,
                            entry,
                            scroll: 0,
                        });
                    }
                }
            }
        }
        SettingsTab::StorageSystem => match settings.selected_row {
            0 => {
                let cur = settings.config_draft.storage.history_retention_days;
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        settings.config_draft.storage.history_retention_days =
                            cur.saturating_sub(5).max(1);
                        settings.dirty = true;
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        settings.config_draft.storage.history_retention_days = (cur + 5).min(3650);
                        settings.dirty = true;
                    }
                    _ => {}
                }
            }
            1 => {
                let cur = settings.config_draft.storage.log_level.as_str();
                let idx = SETTINGS_LOG_LEVELS
                    .iter()
                    .position(|&l| l == cur)
                    .unwrap_or(2);
                let next_idx = match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        (idx + SETTINGS_LOG_LEVELS.len() - 1) % SETTINGS_LOG_LEVELS.len()
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter | KeyCode::Char(' ') => {
                        (idx + 1) % SETTINGS_LOG_LEVELS.len()
                    }
                    _ => idx,
                };
                if next_idx != idx {
                    settings.config_draft.storage.log_level = SETTINGS_LOG_LEVELS[next_idx].into();
                    settings.dirty = true;
                }
            }
            _ => {}
        },
        // 「关于」页只读，无 per-tab 动作（上游提前 return，实际不可达）。
        SettingsTab::About => {}
    }

    Ok(false)
}

fn handle_mcp_key(
    screen: &mut CliScreen,
    _runner: &mut Option<SessionRunner>,
    _permissions: &Arc<PermissionBroker>,
    key: KeyEvent,
) -> color_eyre::Result<bool> {
    let Some(mcp) = screen.mcp_panel.as_mut() else {
        screen.panel = None;
        return Ok(false);
    };

    let action = mcp.handle_key(key);
    match action {
        crate::views::mcp_panel::McpPanelAction::None => Ok(false),
        crate::views::mcp_panel::McpPanelAction::Close => {
            screen.panel = None;
            screen.mcp_panel = None;
            Ok(false)
        }
        crate::views::mcp_panel::McpPanelAction::Save => {
            if let Err(e) = mcp.save_to_disk() {
                screen.message("MCP", &format!("保存失败: {e}"), ERROR);
            } else {
                screen.message("MCP", "MCP 服务器配置已保存至 servers.toml", SUCCESS);
            }
            Ok(false)
        }
        crate::views::mcp_panel::McpPanelAction::TestConnection(server_name) => {
            let Some(item) = mcp.servers.iter().find(|s| s.spec.name == server_name) else {
                return Ok(false);
            };
            let spec = item.spec.clone();
            mcp.set_status(format!("正在测试 [{server_name}] 连接并探测工具清单..."));
            let start = std::time::Instant::now();
            let res = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    match cyber_mcp::McpConnection::connect(&spec).await {
                        Ok((conn, handle)) => {
                            let tools = conn.tools().to_vec();
                            conn.shutdown();
                            let _ =
                                tokio::time::timeout(std::time::Duration::from_millis(500), handle)
                                    .await;
                            Ok(tools)
                        }
                        Err(e) => Err(e.to_string()),
                    }
                })
            });
            let elapsed = start.elapsed();
            match res {
                Ok(tools) => {
                    let tool_count = tools.len();
                    if let Some(s) = mcp.servers.iter_mut().find(|s| s.spec.name == server_name) {
                        s.connected = true;
                        s.tool_count = tool_count;
                        s.latency = Some(elapsed);
                        s.error = None;
                        s.tools = tools;
                    }
                    mcp.set_status(format!(
                        "🟢 [{server_name}] 连接成功 (握手延迟: {}ms, 导出 {tool_count} 个工具)",
                        elapsed.as_millis()
                    ));
                }
                Err(e) => {
                    if let Some(s) = mcp.servers.iter_mut().find(|s| s.spec.name == server_name) {
                        s.connected = false;
                        s.error = Some(e.clone());
                        s.latency = None;
                    }
                    mcp.set_status(format!("🔴 [{server_name}] 连接失败: {e}"));
                }
            }
            Ok(false)
        }
    }
}

/// 模型面板状态条文案（`None` = 不显示）：拉取中 / 拉取失败 / 尚未按 Enter 拉取。
///
/// 与 `draw_model_picker` 的脚注高度计算同源：存在状态条时脚注占 2 行。
fn model_picker_fetch_status(state: &CliModelPickerState) -> Option<(String, Color)> {
    match (&state.fetching, &state.fetch_error) {
        (Some(provider), _) => Some((format!("⟳ 正在从接口拉取 [{provider}] 的模型列表…"), ACCENT)),
        (None, Some(err)) => Some((format!("⚠ 模型列表拉取失败：{err}（显示本地配置）"), ERROR)),
        (None, None) if !state.fetched => Some((
            "当前显示 providers.toml 的本地模型 · 按 Enter 选定 provider 并从接口拉取".to_string(),
            MUTED,
        )),
        (None, None) => None,
    }
}

fn draw_model_picker(
    frame: &mut Frame,
    bounds: Rect,
    state: &mut CliModelPickerState,
    active_provider: &str,
) {
    if bounds.width < 40 || bounds.height < 6 {
        return;
    }

    let popup_area = bounds;

    frame.render_widget(Clear, popup_area);

    let outer_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(
            Line::from(" 模型选择 / Model Picker ")
                .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        );
    let inner = outer_block.inner(popup_area);
    frame.render_widget(outer_block, popup_area);

    // 拉取状态条（拉取中/失败/尚未拉取时占一行）：接口结果的可见进度与失败原因。
    let fetch_status = model_picker_fetch_status(state);

    let chunks = Layout::vertical([
        Constraint::Min(0),                                             // 双栏
        Constraint::Length(if fetch_status.is_some() { 2 } else { 1 }), // 状态条 + 快捷键条
    ])
    .split(inner);
    let body = chunks[0];
    let footer_area = chunks[1];

    let panes =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).split(body);
    let prov_area = panes[0];
    let model_area = panes[1];

    // 1. 左栏：Providers
    let prov_focused = !state.focus_models;
    let prov_border_color = if prov_focused { ACCENT } else { DIM };
    let prov_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(prov_border_color))
        .title(
            Line::from(" 服务商 / Providers ").style(
                Style::default()
                    .fg(if prov_focused { ACCENT } else { MUTED })
                    .add_modifier(Modifier::BOLD),
            ),
        );
    let prov_inner = prov_block.inner(prov_area);
    frame.render_widget(prov_block, prov_area);

    let names = state.providers.sorted_names();
    let mut prov_lines: Vec<Line<'static>> = Vec::new();
    if names.is_empty() {
        prov_lines
            .push(Line::from("（无可用服务商）").alignment(ratatui::layout::Alignment::Center));
    } else {
        for (i, name) in names.iter().enumerate() {
            let selected = i == state.provider_selected;
            let is_default = name == &state.default_provider;
            let is_active_session = active_provider == *name;
            let marker = if selected { "▸ " } else { "  " };
            let cfg = state.providers.providers.get(name);
            let kind = cfg.map(|c| c.kind.as_str()).unwrap_or("");
            let model = cfg.map(|c| c.model.as_str()).unwrap_or("");

            let sel_bg = Color::Rgb(38, 79, 120);
            let row_style = if selected && prov_focused {
                Style::default().bg(sel_bg)
            } else if selected {
                Style::default().bg(Color::Rgb(30, 36, 48))
            } else {
                Style::default()
            };

            let max_prov_w = (prov_inner.width as usize).saturating_sub(1);
            let mut badges_w = 2usize;
            if is_default {
                badges_w += 8;
            }
            if is_active_session {
                badges_w += 2;
            }
            let name_budget = max_prov_w.saturating_sub(badges_w);
            let clipped_name = clip_cells_ellipsis(name, name_budget);

            let mut title_spans = vec![
                Span::styled(
                    marker,
                    if selected {
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(MUTED)
                    },
                ),
                Span::styled(
                    clipped_name,
                    Style::default().fg(FG).add_modifier(Modifier::BOLD),
                ),
            ];
            if is_default {
                title_spans.push(Span::styled(
                    "  ★ 默认",
                    Style::default()
                        .fg(Color::Rgb(254, 188, 56))
                        .add_modifier(Modifier::BOLD),
                ));
            }
            if is_active_session {
                title_spans.push(Span::styled(
                    " ●",
                    Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
                ));
            }

            let sub_text = format!("    [{kind}] {model}");
            let clipped_sub = clip_cells_ellipsis(&sub_text, max_prov_w);

            prov_lines.push(Line::from(title_spans).style(row_style));
            prov_lines.push(
                Line::from(vec![Span::styled(clipped_sub, Style::default().fg(MUTED))])
                    .style(row_style),
            );
        }
    }

    // 粘性滚动计算（左栏每项 2 行）
    let prov_h = prov_inner.height as usize;
    let prov_total = prov_lines.len();
    let sel_start = state.provider_selected * 2;
    let sel_end = (sel_start + 2).min(prov_total);
    let mut prov_scroll = state.provider_scroll;
    if prov_total <= prov_h {
        prov_scroll = 0;
    } else if sel_start < prov_scroll {
        prov_scroll = sel_start;
    } else if sel_end > prov_scroll + prov_h {
        prov_scroll = sel_end
            .saturating_sub(prov_h)
            .min(prov_total.saturating_sub(prov_h));
    }
    state.provider_scroll = prov_scroll;

    frame.render_widget(
        Paragraph::new(prov_lines).scroll((prov_scroll as u16, 0)),
        prov_inner,
    );

    // 2. 右栏：Models
    let model_focused = state.focus_models;
    let model_border_color = if model_focused { ACCENT } else { DIM };
    let current_prov_name = names
        .get(state.provider_selected)
        .cloned()
        .unwrap_or_default();
    let model_title = if current_prov_name.is_empty() {
        " 模型 / Models ".to_string()
    } else {
        format!(" 模型 / Models ({current_prov_name}) ")
    };
    // 标题绘制在顶边框上：裁剪防止长 provider 名覆盖右上角边框。
    let model_title =
        clip_cells_ellipsis(&model_title, (model_area.width as usize).saturating_sub(3));
    let model_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(model_border_color))
        .title(
            Line::from(model_title).style(
                Style::default()
                    .fg(if model_focused { ACCENT } else { MUTED })
                    .add_modifier(Modifier::BOLD),
            ),
        );
    let model_inner = model_block.inner(model_area);
    frame.render_widget(model_block, model_area);

    let mut model_lines: Vec<Line<'static>> = Vec::new();
    let cur_prov_cfg = state.providers.providers.get(&current_prov_name);
    if state.models.is_empty() {
        model_lines.push(
            Line::from(format!(
                "（无可用模型{})",
                if state.fetched {
                    ""
                } else {
                    " · 按 Enter 从接口获取"
                }
            ))
            .style(Style::default().fg(MUTED))
            .alignment(ratatui::layout::Alignment::Center),
        );
    } else {
        // 先算粘性滚动（只依赖选中项/总数/视口高度），再只构建可见窗口内的行：
        // 长模型列表（数百～数千条）下逐行构建 + 逐行能力查询会让每帧成本线性增长
        // （旧实现在这里每行调一次 `CapabilityStore::load()`）。
        let visible_h = model_inner.height as usize;
        let total = state.models.len();
        let sel = state.model_selected;
        let prev = state.model_scroll.min(total.saturating_sub(visible_h));
        let model_scroll = if total <= visible_h {
            0
        } else if sel < prev {
            sel
        } else if sel >= prev + visible_h {
            (sel + 1)
                .saturating_sub(visible_h)
                .min(total.saturating_sub(visible_h))
        } else {
            prev
        };
        state.model_scroll = model_scroll;
        // 能力缓存每帧只读一次（显式 models 配置 → 实测缓存 → 名称规则表）。
        let store = cyber_core::CapabilityStore::load();
        let cur_prov_models = cur_prov_cfg.map(|p| &p.models);
        for i in model_scroll..(model_scroll + visible_h).min(total) {
            let m = &state.models[i];
            let selected = i == state.model_selected;
            let marker = if selected { "▸ " } else { "  " };
            let is_probing = state.probing_model.as_deref() == Some(m.as_str());
            let is_active = cur_prov_cfg.map(|p| p.model == *m).unwrap_or(false);

            let label = if let Some(alias) = cur_prov_cfg
                .and_then(|p| p.models.get(m))
                .and_then(|mc| mc.alias.as_ref())
                .filter(|a| !a.is_empty())
            {
                format!("{alias} → {m}")
            } else {
                m.clone()
            };

            let sel_bg = Color::Rgb(38, 79, 120);
            let row_style = if selected && model_focused {
                Style::default().bg(sel_bg)
            } else if selected {
                Style::default().bg(Color::Rgb(30, 36, 48))
            } else {
                Style::default()
            };

            let max_model_w = (model_inner.width as usize).saturating_sub(1);
            let has_vision = if cur_prov_cfg.is_some() {
                cyber_core::resolve_vision_capability(
                    cur_prov_models,
                    &current_prov_name,
                    m,
                    &store,
                )
                .is_supported()
            } else {
                false
            };
            let mut badges_w = 2usize; // marker
            if is_probing || has_vision {
                badges_w += 8;
            }
            if is_active {
                badges_w += 8;
            }
            let label_budget = max_model_w.saturating_sub(badges_w);
            let clipped_label = clip_cells_ellipsis(&label, label_budget);

            let mut spans = vec![
                Span::styled(
                    marker,
                    if selected {
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(MUTED)
                    },
                ),
                Span::styled(
                    clipped_label,
                    Style::default().fg(FG).add_modifier(Modifier::BOLD),
                ),
            ];

            if is_probing {
                spans.push(Span::styled(
                    "  ⟳ 探测中",
                    Style::default()
                        .fg(Color::Rgb(254, 188, 56))
                        .add_modifier(Modifier::BOLD),
                ));
            } else if has_vision {
                spans.push(Span::styled(
                    "  ◈ 视觉",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
            }

            if is_active {
                spans.push(Span::styled(
                    "  ✓ 当前",
                    Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
                ));
            }

            model_lines.push(Line::from(spans).style(row_style));
        }
    }

    frame.render_widget(Paragraph::new(model_lines), model_inner);

    // 3. 底部状态与快捷键条
    let (status_area, hint_area) = if fetch_status.is_some() {
        (
            Some(Rect {
                height: 1,
                ..footer_area
            }),
            Rect {
                y: footer_area.y + 1,
                height: 1,
                ..footer_area
            },
        )
    } else {
        (None, footer_area)
    };
    if let (Some(status_area), Some((text, color))) = (status_area, fetch_status) {
        let status_line = Line::styled(
            clip_cells_ellipsis(&text, status_area.width as usize),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
        .alignment(ratatui::layout::Alignment::Center);
        frame.render_widget(Paragraph::new(status_line), status_area);
    }
    let hint_line = Line::from(vec![
        Span::styled(
            " Tab/←/→",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" 切栏 · ", Style::default().fg(MUTED)),
        Span::styled(
            "↑/↓",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" 移动 · ", Style::default().fg(MUTED)),
        Span::styled(
            "Enter",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" 确认选择 · ", Style::default().fg(MUTED)),
        Span::styled(
            "r",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" 重新拉取 · ", Style::default().fg(MUTED)),
        Span::styled(
            "t",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" 探测识图 · ", Style::default().fg(MUTED)),
        Span::styled(
            "Esc",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" 关闭 ", Style::default().fg(MUTED)),
    ])
    .alignment(ratatui::layout::Alignment::Center);

    frame.render_widget(Paragraph::new(hint_line), hint_area);
}

fn handle_model_picker_key(
    screen: &mut CliScreen,
    runner: &mut Option<SessionRunner>,
    key: KeyEvent,
) -> color_eyre::Result<bool> {
    let Some(state) = screen.model_picker.as_mut() else {
        screen.panel = None;
        return Ok(false);
    };

    // 左栏移动只换本地清单；左栏 Enter（选定）/ `r` 才联网拉取。
    // 两者都在借用结束后统一处理（此处的 state 借用尚未释放）。
    let mut provider_changed = false;
    let mut refetch_from_api = false;

    match key.code {
        KeyCode::Esc => {
            screen.panel = None;
            screen.model_picker = None;
            return Ok(false);
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            screen.panel = None;
            screen.model_picker = None;
            return Ok(false);
        }
        KeyCode::Tab => {
            state.focus_models = !state.focus_models;
        }
        KeyCode::Left | KeyCode::Char('h') => {
            state.focus_models = false;
        }
        KeyCode::Right | KeyCode::Char('l') => {
            state.focus_models = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if state.focus_models {
                if state.model_selected > 0 {
                    state.model_selected -= 1;
                } else if !state.models.is_empty() {
                    state.model_selected = state.models.len() - 1;
                }
            } else {
                let total = state.providers.sorted_names().len();
                if total > 0 {
                    if state.provider_selected > 0 {
                        state.provider_selected -= 1;
                    } else {
                        state.provider_selected = total - 1;
                    }
                    provider_changed = true;
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if state.focus_models {
                if !state.models.is_empty() {
                    if state.model_selected + 1 < state.models.len() {
                        state.model_selected += 1;
                    } else {
                        state.model_selected = 0;
                    }
                }
            } else {
                let total = state.providers.sorted_names().len();
                if total > 0 {
                    if state.provider_selected + 1 < total {
                        state.provider_selected += 1;
                    } else {
                        state.provider_selected = 0;
                    }
                    provider_changed = true;
                }
            }
        }
        KeyCode::Enter => {
            if !state.focus_models {
                state.focus_models = true;
                // 选定该 provider → 从接口拉取其模型列表（已在拉取中则不重复发起；
                // 失败/陈旧列表由此获得重试）。
                if state.fetching.is_none() {
                    refetch_from_api = true;
                }
            } else {
                let selected_model = state.models.get(state.model_selected).cloned();
                let selected_provider = state
                    .providers
                    .sorted_names()
                    .get(state.provider_selected)
                    .cloned();

                if let (Some(prov), Some(model)) = (selected_provider, selected_model) {
                    // runner 已被 take 走（回合进行中）：不得显示未落盘的假成功。
                    let Some(r) = runner.as_mut() else {
                        screen.status =
                            "回合进行中：模型切换需等回合结束后再执行（面板保持打开）".into();
                        return Ok(false);
                    };
                    if let Err(err) = r.select_model_persisted(&prov, Some(&model)) {
                        screen.message("Error", &format!("保存模型失败: {err}"), ERROR);
                        return Ok(false);
                    }
                    screen.provider = prov.clone();
                    screen.model = model.clone();
                    screen.status = format!("已切换模型为 {prov} · {model}");
                    screen.message(
                        "Model",
                        &format!("✔ 已切换模型为 {prov} · {model}"),
                        SUCCESS,
                    );
                    screen.panel = None;
                    screen.model_picker = None;
                }
            }
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            // 手动重新从接口拉取当前 provider 的模型列表。
            if state.fetching.is_none() {
                refetch_from_api = true;
            }
        }
        KeyCode::Char('t') | KeyCode::Char('T') if state.focus_models => {
            let selected_model = state.models.get(state.model_selected).cloned();
            let selected_provider = state
                .providers
                .sorted_names()
                .get(state.provider_selected)
                .cloned();

            if let (Some(prov), Some(model)) = (selected_provider, selected_model) {
                if state.probing_model.is_none() {
                    if let Some(cfg_snapshot) = state.providers.providers.get(&prov).cloned() {
                        state.probing_model = Some(model.clone());
                        screen.message(
                            "Probe",
                            &format!("正在实测模型 [{model}] 的视觉识图能力..."),
                            ACCENT,
                        );
                        let prov_name = prov.clone();
                        let model_name = model.clone();
                        tokio::spawn(async move {
                            let res =
                                cyber_agent::probe_model_vision(&cfg_snapshot, &model_name).await;
                            if let Ok(cap) = res {
                                let _ = cyber_core::save_model_vision_capability(
                                    &prov_name,
                                    &model_name,
                                    cap,
                                    None,
                                );
                            }
                        });
                    }
                }
            }
        }
        _ => {}
    }

    if provider_changed {
        // 切换 provider：只换本地配置清单，不联网。
        screen.refresh_model_picker_local();
    }
    if refetch_from_api {
        // 左栏 Enter 选定 / r 手动重取：联网拉取。
        screen.fetch_model_picker_models();
    }

    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn handle_ctf_key(
    screen: &mut CliScreen,
    runner: &mut Option<SessionRunner>,
    permissions: &Arc<PermissionBroker>,
    events: &mpsc::UnboundedSender<AgentEvent>,
    active: &mut Option<ActiveTurn>,
    cancel: &mut Option<oneshot::Sender<()>>,
    key: KeyEvent,
) -> color_eyre::Result<bool> {
    if let Some(form) = screen.ctf_edit_form.as_mut() {
        let action = form.handle_key(key);
        match action {
            crate::views::ctf_edit_form::CtfEditFormAction::None => {}
            crate::views::ctf_edit_form::CtfEditFormAction::Cancel => {
                screen.ctf_edit_form = None;
            }
            crate::views::ctf_edit_form::CtfEditFormAction::Save => {
                if let Some(form) = screen.ctf_edit_form.take() {
                    let challenge_id = form.challenge_id.clone();
                    let challenge = screen
                        .ctf_challenges
                        .lock()
                        .ok()
                        .and_then(|list| list.iter().find(|c| c.id == challenge_id).cloned());
                    if let Some(c) = challenge {
                        let applied = form.apply_to(c);
                        if let Some(name) = screen.ctf_update_selected(applied) {
                            if let Some(r) = runner.as_mut() {
                                let _ = r.save();
                            }
                            screen.message("CTF", &format!("「{name}」已保存"), ACCENT);
                        }
                    }
                }
            }
        }
        return Ok(false);
    }

    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    if control && (key.code == KeyCode::Char('t') || key.code == KeyCode::Char('T')) {
        screen.panel = None;
        screen.ctf_detail_view = false;
        screen.ctf_detail_scroll = 0;
        return Ok(false);
    }

    // Ctrl+D 已不再是退出快捷键；CTF 面板的单字母 `d` 是删除题目，必须显式吞掉。
    if control && key.code == KeyCode::Char('d') {
        return Ok(false);
    }

    if key.code == KeyCode::Esc {
        if key.modifiers.contains(KeyModifiers::SHIFT) || !screen.ctf_detail_view {
            screen.panel = None;
            screen.ctf_detail_view = false;
            screen.ctf_detail_scroll = 0;
        } else {
            screen.ctf_detail_view = false;
            screen.ctf_detail_scroll = 0;
        }
        return Ok(false);
    }

    let len = screen.ctf_challenges_count();

    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => {
            if screen.ctf_detail_view {
                screen.ctf_detail_view = false;
                screen.ctf_detail_scroll = 0;
            } else {
                screen.panel = None;
                screen.ctf_detail_view = false;
                screen.ctf_detail_scroll = 0;
            }
        }
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') | KeyCode::Char('8') => {
            if screen.ctf_detail_view {
                screen.ctf_detail_scroll = screen.ctf_detail_scroll.saturating_sub(1);
            } else if screen.ctf_selected > 0 {
                screen.ctf_selected -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') | KeyCode::Char('2') => {
            if screen.ctf_detail_view {
                screen.ctf_detail_scroll = screen.ctf_detail_scroll.saturating_add(1);
            } else if len > 0 && screen.ctf_selected + 1 < len {
                screen.ctf_selected += 1;
            }
        }
        KeyCode::PageUp => {
            if screen.ctf_detail_view {
                screen.ctf_detail_scroll = screen.ctf_detail_scroll.saturating_sub(10);
            } else {
                screen.ctf_selected = screen.ctf_selected.saturating_sub(5);
            }
        }
        KeyCode::PageDown => {
            if screen.ctf_detail_view {
                screen.ctf_detail_scroll = screen.ctf_detail_scroll.saturating_add(10);
            } else if len > 0 {
                screen.ctf_selected = (screen.ctf_selected + 5).min(len.saturating_sub(1));
            }
        }
        KeyCode::Home => {
            if screen.ctf_detail_view {
                screen.ctf_detail_scroll = 0;
            } else {
                screen.ctf_selected = 0;
            }
        }
        KeyCode::End => {
            if screen.ctf_detail_view {
                screen.ctf_detail_scroll = usize::MAX / 2;
            } else if len > 0 {
                screen.ctf_selected = len.saturating_sub(1);
            }
        }
        KeyCode::Enter => {
            if !screen.ctf_detail_view && len > 0 && screen.ctf_selected < len {
                screen.ctf_detail_view = true;
                screen.ctf_detail_scroll = 0;
            }
        }
        KeyCode::Char('d') | KeyCode::Char('D') => {
            if !screen.ctf_detail_view {
                if let Some(name) = screen.ctf_remove_selected() {
                    if let Some(r) = runner.as_mut() {
                        let _ = r.save();
                    }
                    screen.message("CTF", &format!("已删除「{name}」"), ACCENT);
                }
            }
        }
        KeyCode::Char('s') | KeyCode::Char('S') => {
            if !screen.ctf_detail_view {
                if let Some((name, solved)) = screen.ctf_toggle_status_selected() {
                    if let Some(r) = runner.as_mut() {
                        let _ = r.save();
                    }
                    let status_str = if solved { "已完成" } else { "进行中" };
                    screen.message("CTF", &format!("「{name}」→ {status_str}"), ACCENT);
                }
            }
        }
        KeyCode::Char('g') => {
            if !screen.ctf_detail_view {
                if let Some((name, is_global)) = screen.ctf_toggle_global_selected() {
                    if let Some(r) = runner.as_mut() {
                        let _ = r.save();
                    }
                    let scope_str = if is_global {
                        "全局 ★"
                    } else {
                        "仅本 session"
                    };
                    screen.message("CTF", &format!("「{name}」→ {scope_str}"), ACCENT);
                }
            }
        }
        KeyCode::Char('G') => {
            if !screen.ctf_detail_view {
                let count = screen.ctf_set_all_global();
                if count > 0 {
                    if let Some(r) = runner.as_mut() {
                        let _ = r.save();
                    }
                    screen.message("CTF", &format!("已将 {count} 道题目设为全局"), ACCENT);
                } else {
                    screen.message("CTF", "当前 session 无需转换的题目", MUTED);
                }
            }
        }
        KeyCode::Char('e') | KeyCode::Char('E') => {
            if !screen.ctf_detail_view {
                if let Some(challenge) = screen.ctf_challenge_at(screen.ctf_selected) {
                    screen.ctf_edit_form = Some(
                        crate::views::ctf_edit_form::CtfEditFormState::from_challenge(&challenge),
                    );
                }
            }
        }
        KeyCode::Char('w') | KeyCode::Char('W') if screen.ctf_detail_view => {
            if let Some(challenge) = screen.ctf_challenge_at(screen.ctf_selected) {
                if challenge.is_solved() {
                    screen.panel = None;
                    screen.ctf_detail_view = false;
                    screen.ctf_detail_scroll = 0;
                    return apply_action(
                        screen,
                        CliAction::Task(cli_commands::CliTask::Writeup {
                            challenge: Box::new(challenge),
                        }),
                        runner,
                        permissions,
                        events,
                        active,
                        cancel,
                    );
                } else {
                    screen.message("CTF", "题目尚未解决，需解题完成后才能生成 Writeup", MUTED);
                }
            }
        }
        _ => {}
    }

    Ok(false)
}

#[derive(Clone, Debug, Default)]
struct WrappedViewport {
    width: u16,
    padding: u16,
    source: Vec<Line<'static>>,
    ends: Vec<usize>,
    rows: Vec<Line<'static>>,
    row_ranges: Vec<(usize, usize)>,
}

impl WrappedViewport {
    fn update(&mut self, lines: &[Line<'static>], width: u16) {
        use unicode_width::UnicodeWidthChar;

        // Retain the unchanged prefix; streaming only rebuilds the changed tail.
        let unchanged = if self.width == width {
            self.source
                .iter()
                .zip(lines)
                .take_while(|(a, b)| a == b)
                .count()
        } else {
            0
        };
        self.width = width;
        self.source.truncate(unchanged);
        self.ends.truncate(unchanged);
        let valid_rows = self.ends.last().copied().unwrap_or(0);
        self.rows.truncate(valid_rows);
        self.row_ranges.truncate(valid_rows);
        let full_width = usize::from(width).max(1);
        let padding = usize::from(self.padding).min(full_width.saturating_sub(1) / 2);
        let width = full_width.saturating_sub(padding * 2).max(1);
        for line in &lines[unchanged..] {
            let mut row = Line::default().style(line.style);
            if padding > 0 {
                row.spans.push(Span::raw(" ".repeat(padding)));
            }
            let mut used = 0;
            let mut current_char = 0;
            let mut start_char = 0;
            for span in &line.spans {
                let mut text = String::new();
                for ch in span.content.chars() {
                    let columns = if ch == '\t' {
                        4
                    } else {
                        ch.width().unwrap_or(0)
                    };
                    if used + columns > width && used != 0 {
                        row.spans
                            .push(Span::styled(std::mem::take(&mut text), span.style));
                        if line.style.bg.is_some() {
                            row.spans.push(Span::raw(
                                " ".repeat(full_width.saturating_sub(used + padding)),
                            ));
                        }
                        self.rows.push(row);
                        self.row_ranges.push((start_char, current_char));
                        start_char = current_char;
                        row = Line::default().style(line.style);
                        if padding > 0 {
                            row.spans.push(Span::raw(" ".repeat(padding)));
                        }
                        used = 0;
                    }
                    if ch == '\t' {
                        text.push_str("    ");
                    } else {
                        text.push(ch);
                    }
                    used += columns;
                    current_char += 1;
                }
                if !text.is_empty() {
                    row.spans.push(Span::styled(text, span.style));
                }
            }
            if line.style.bg.is_some() {
                row.spans.push(Span::raw(
                    " ".repeat(full_width.saturating_sub(used + padding)),
                ));
            }
            self.rows.push(row);
            self.row_ranges.push((start_char, current_char));
            self.source.push(line.clone());
            self.ends.push(self.rows.len());
        }
    }

    #[allow(dead_code)]
    fn coord_from_viewport(&self, viewport_row: usize, visual_col: u16) -> Option<ContentCoord> {
        use unicode_width::UnicodeWidthChar;

        if viewport_row >= self.rows.len() || self.source.is_empty() {
            return None;
        }
        let line_idx = self.ends.partition_point(|&end| end <= viewport_row);
        if line_idx >= self.source.len() {
            return None;
        }
        let (start_char, end_char) = self.row_ranges.get(viewport_row).copied()?;
        if start_char >= end_char {
            return Some(ContentCoord::new(line_idx, start_char));
        }

        let padding = usize::from(self.padding);
        let col = usize::from(visual_col);
        if col <= padding {
            return Some(ContentCoord::new(line_idx, start_char));
        }
        let target_col = col - padding;

        let line = &self.source[line_idx];
        let mut cur_col = 0;
        let mut current_idx = 0;

        for span in &line.spans {
            for ch in span.content.chars() {
                if current_idx >= start_char && current_idx < end_char {
                    let w = if ch == '\t' {
                        4
                    } else {
                        ch.width().unwrap_or(0)
                    };
                    if cur_col + w > target_col {
                        return Some(ContentCoord::new(line_idx, current_idx));
                    }
                    cur_col += w;
                }
                current_idx += 1;
                if current_idx >= end_char {
                    break;
                }
            }
            if current_idx >= end_char {
                break;
            }
        }

        Some(ContentCoord::new(line_idx, end_char))
    }

    fn window(&self, start: usize, height: u16) -> Vec<Line<'static>> {
        self.rows
            .iter()
            .skip(start)
            .take(height as usize)
            .cloned()
            .collect()
    }

    #[allow(dead_code)]
    fn window_with_selection(
        &self,
        start: usize,
        height: u16,
        sel: Option<&TextSelection>,
        sel_style: Style,
    ) -> Vec<Line<'static>> {
        let count = height as usize;
        let mut result = Vec::with_capacity(count);

        for row_idx in start..(start + count).min(self.rows.len()) {
            let row = &self.rows[row_idx];
            if let Some(selection) = sel {
                if !selection.is_empty() {
                    let line_idx = self.ends.partition_point(|&end| end <= row_idx);
                    if let Some(&(char_start, char_end)) = self.row_ranges.get(row_idx) {
                        let highlighted = crate::selection::apply_selection_to_row_full(
                            row,
                            char_start,
                            char_end,
                            line_idx,
                            selection,
                            sel_style,
                            usize::from(self.padding),
                            self.source.get(line_idx),
                        );
                        result.push(highlighted);
                        continue;
                    }
                }
            }
            result.push(row.clone());
        }

        result
    }
}

type ActiveTurn = tokio::task::JoinHandle<(SessionRunner, HeadlessOutcome)>;

/// 全屏 CLI 的启动模式：聊天（`cyber`）或首次配置向导（`cyber setup`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliStart {
    Chat,
    Setup,
}

pub async fn run_cli(cwd: &Path, mock: bool) -> color_eyre::Result<()> {
    match run_cli_inner(cwd, mock, CliStart::Chat).await {
        Ok(_) => Ok(()),
        Err(error) => {
            eprintln!("[error] {error}");
            Err(error)
        }
    }
}

/// 全屏设置向导入口（`cyber setup`）。返回 `true` = 退出时配置可用。
pub async fn run_setup(cwd: &Path, mock: bool) -> color_eyre::Result<bool> {
    run_cli_inner(cwd, mock, CliStart::Setup).await
}

/// `run_setup` 的同步包装：启动闸门在同步上下文中拉起向导。
pub fn run_setup_blocking(cwd: &Path, mock: bool) -> color_eyre::Result<bool> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(run_setup(cwd, mock))),
        Err(_) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(run_setup(cwd, mock))
        }
    }
}

async fn run_cli_inner(cwd: &Path, mock: bool, start: CliStart) -> color_eyre::Result<bool> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        color_eyre::eyre::bail!(
            "Interactive CLI requires a terminal; use `cyber run` for scripts."
        );
    }
    let mut runner = Some(SessionRunner::new(cwd, mock).await?);
    let owner = runner.as_ref().expect("runner just constructed");
    let mut screen = CliScreen::new(owner);
    let mut question_rx = owner
        .registries
        .question_rx
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if start == CliStart::Setup {
        // 首次配置向导：直接落在设置中心的 Providers 页，配置缺失时自动打开预设向导。
        screen.setup_mode = true;
        let mut settings = CliSettingsState::from_view(&owner.view());
        settings.tab = SettingsTab::Providers;
        screen.settings = Some(settings);
        screen.panel = Some(Panel::Settings);
        let usable = cyber_core::setup::configured(&owner.ctx.config, &owner.ctx.providers);
        if !usable {
            open_provider_wizard(
                &mut runner,
                &mut screen.settings_return_tab,
                &mut screen.panel,
                &mut screen.picker,
                &mut screen.picker_selected,
            );
        }
    }
    let (broker, mut requests) = PermissionBroker::interactive();
    broker.set_mode(screen.permission_mode);
    let permissions = Arc::new(broker);
    let (event_tx, mut agent_events) = mpsc::unbounded_channel();
    let mut active: Option<ActiveTurn> = None;
    let mut cancel: Option<oneshot::Sender<()>> = None;
    let _session = TerminalSession::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut events = EventStream::new();
    let mut paste = PasteDetector::new();
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    if let Some(newer) = screen.new_version.as_deref() {
        screen.message(
            "Notice",
            &format!("💡 检测到新版本 V{newer}，可运行 /update 更新（或退出后 cyber update）"),
            AMBER,
        );
    }
    let (update_tx, mut update_rx) = mpsc::unbounded_channel::<String>();
    let (models_tx, mut models_rx) = mpsc::unbounded_channel::<ModelFetchResult>();
    screen.model_fetch_tx = Some(models_tx);
    // `/update` 联网检查通道：`--mock` 下不注入，`/update` 退化为只读本地缓存。
    let (update_check_tx, mut update_check_rx) = mpsc::unbounded_channel::<UpdateCheckResult>();
    if !mock {
        screen.update_check_tx = Some(update_check_tx);
    }
    if !mock {
        tokio::spawn(async move {
            if let Some(info) = cyber_core::update::check_for_updates(false).await {
                if cyber_core::update::is_newer(cyber_core::update::CURRENT_VERSION, &info.version)
                {
                    let _ = update_tx.send(info.version);
                }
            }
        });
    }
    let result = async {
        #[cfg(windows)]
        let mut was_ctrl_v_down = false;
        loop {
            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    if let Some(cancel_tx) = cancel.take() {
                        let _ = cancel_tx.send(());
                        screen.queued_prompts.clear();
                        screen.status = "Cancelling (再按一次 Ctrl+C 强制退出)".into();
                    } else {
                        break;
                    }
                    if let Some(q) = screen.question_state.take() {
                        let _ = q.request.reply.send(cyber_agent::QuestionResponse {
                            answers: Vec::new(),
                            cancelled: true,
                        });
                    }
                }
                Some(new_ver) = update_rx.recv() => {
                    if screen.new_version.as_deref() != Some(&new_ver) {
                        screen.new_version = Some(new_ver.clone());
                        screen.message(
                            "Notice",
                            &format!("💡 检测到新版本 V{new_ver}，可运行 /update 更新（或退出后 cyber update）"),
                            AMBER,
                        );
                    }
                }
                Some(fetch) = models_rx.recv() => {
                    if let Some(state) = screen.model_picker.as_mut() {
                        state.deliver_fetch(fetch);
                    }
                }
                Some(result) = update_check_rx.recv() => screen.deliver_update(result),
                _ = tick.tick() => {
                    if !TERMINAL_ACTIVE.load(Ordering::SeqCst) {
                        color_eyre::eyre::bail!("A task panicked; the terminal was restored. See the diagnostic above.");
                    }
                    #[cfg(windows)]
                    if crate::win_paste::check_ctrl_v_rising_edge(&mut was_ctrl_v_down)
                        && crate::win_paste::is_terminal_foreground()
                        && screen.approval.is_none()
                        && screen.form.is_none()
                        && screen.question_state.is_none()
                        && screen.handle_clipboard_image_only()
                    {
                        screen.update_completions(runner.as_ref());
                    }
                    if let Some(text) = paste.flush_if_stale() {
                        screen.insert_text(&text);
                        if screen.question_state.is_none() {
                            screen.completion_closed = false;
                            screen.completion_accepted = false;
                            screen.update_completions(runner.as_ref());
                        }
                    }
                    if screen.needs_clear {
                        terminal.clear()?;
                        screen.needs_clear = false;
                    }
                    terminal.draw(|frame| screen.draw(frame))?;
                    drain_background_completions(
                        &mut screen,
                        &mut runner,
                        &permissions,
                        &event_tx,
                        &mut active,
                        &mut cancel,
                    )?;
                }
                event = agent_events.recv() => { if let Some(event) = event { screen.event(event); } }
                request = requests.recv(), if screen.approval.is_none() => {
                    if let Some(request) = request {
                        if !screen.busy || screen.status == "Cancelling" || request.reply.is_closed() {
                            let _ = request.reply.send(PermissionDecision::Deny);
                            continue;
                        }
                        if let Some(text) = paste.flush() { screen.insert_text(&text); }
                        screen.panel = None;
                        screen.approval_choice = None;
                        screen.approval_input = composer();
                        screen.approval_input.set_placeholder_text("Press 1, 2 or 3 to select an action");
                        screen.approval = Some(request);
                    }
                }
                q_req = async {
                    match question_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                }, if screen.question_state.is_none() => {
                    if let Some(req) = q_req {
                        if !screen.busy || screen.status == "Cancelling" || req.reply.is_closed() {
                            let _ = req.reply.send(cyber_agent::QuestionResponse {
                                answers: Vec::new(),
                                cancelled: true,
                            });
                            continue;
                        }
                        if let Some(text) = paste.flush() { screen.insert_text(&text); }
                        screen.panel = None;
                        screen.question_state = Some(crate::question_ui::QuestionUiState::new(req));
                    }
                }
                finished = async { active.as_mut().unwrap().await }, if active.is_some() => {
                    // The handle has been consumed, even when the task panicked.
                    // Remove it before propagating errors so cleanup cannot repoll it.
                    active.take();
                    let (mut restored, outcome) = finished?;
                    while let Ok(event) = agent_events.try_recv() { screen.event(event); }
                    // 回合中完成的后台子代理结果：flush 进会话并持久化。
                    let pending_count = screen.pending_job_results.len();
                    while let Some(summary) = screen.pending_job_results.pop_front() {
                        restored.entries.push(ChatEntry::System(summary));
                    }
                    if pending_count > 0 {
                        let _ = restored.save();
                    }
                    screen.sync(&restored);
                    if let Some(error) = outcome.error {
                        let summary = if matches!(restored.entries.last(), Some(ChatEntry::TurnSummary { .. })) {
                            screen.messages.split_off(screen.messages.len().saturating_sub(2))
                        } else { Vec::new() };
                        screen.message("Error", &error, ERROR);
                        screen.messages.extend(summary);
                    }
                    runner = Some(restored);
                    active = None;
                    cancel = None;
                    screen.reply(PermissionDecision::Deny);
                    screen.busy = false;
                    screen.status.clear();
                    screen.active_steering_tx = None;
                    if drain_queued_inputs(
                        &mut screen,
                        &mut runner,
                        &permissions,
                        &event_tx,
                        &mut active,
                        &mut cancel,
                    )? {
                        break;
                    }
                }
                event = events.next() => {
                    match event {
                        Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                            match paste.observe(key) {
                                KeyDisposition::Buffer => continue,
                                KeyDisposition::FlushThenProcess => {
                                    if let Some(text) = paste.flush() {
                                        screen.insert_text(&text);
                                        if screen.question_state.is_none() {
                                            screen.completion_closed = false;
                                            screen.completion_accepted = false;
                                            screen.update_completions(runner.as_ref());
                                        }
                                    }
                                }
                                KeyDisposition::Process => {}
                            }
                            if handle_key(&mut screen, key, &mut runner, &permissions, &event_tx, &mut active, &mut cancel)? { break; }
                        }
                        Some(Ok(Event::Paste(text))) => {
                            if let Some(buffered) = paste.flush() {
                                screen.insert_text(&buffered);
                            }
                            if screen
                                .last_paste_instant
                                .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(300))
                            {
                                continue;
                            }
                            screen.last_paste_instant = Some(std::time::Instant::now());
                            if screen.question_state.is_none() {
                                if text.trim().is_empty() {
                                    if screen.handle_clipboard_image_only() {
                                        screen.completion_closed = false;
                                        screen.completion_accepted = false;
                                        screen.update_completions(runner.as_ref());
                                        continue;
                                    }
                                } else if screen.handle_text_image_paste(&text) {
                                    screen.completion_closed = false;
                                    screen.completion_accepted = false;
                                    screen.update_completions(runner.as_ref());
                                    continue;
                                }
                            }
                            screen.insert_text(&text);
                            if screen.question_state.is_none() {
                                screen.completion_closed = false;
                                screen.completion_accepted = false;
                                screen.update_completions(runner.as_ref());
                            }
                        }
                        Some(Ok(Event::Mouse(mouse))) => {
                            match mouse.kind {
                                MouseEventKind::ScrollUp => {
                                    if screen.panel == Some(Panel::Settings) {
                                        screen.settings_scroll(true);
                                    } else if screen.panel == Some(Panel::Mcp) {
                                        if let Some(mcp) = screen.mcp_panel.as_mut() {
                                            mcp.scroll_up(3);
                                        }
                                    } else if screen.panel == Some(Panel::ModelPicker) {
                                        let mut provider_changed = false;
                                        if let Some(state) = screen.model_picker.as_mut() {
                                            if state.focus_models {
                                                if state.model_selected > 0 {
                                                    state.model_selected -= 1;
                                                }
                                            } else if state.provider_selected > 0 {
                                                state.provider_selected -= 1;
                                                provider_changed = true;
                                            }
                                        }
                                        if provider_changed {
                                            screen.refresh_model_picker_local();
                                        }
                                    } else if screen.panel == Some(Panel::Ctf) {
                                        if screen.ctf_detail_view {
                                            screen.ctf_detail_scroll = screen.ctf_detail_scroll.saturating_sub(3);
                                        } else if screen.ctf_selected > 0 {
                                            screen.ctf_selected = screen.ctf_selected.saturating_sub(1);
                                        }
                                    } else if screen.panel == Some(Panel::About) {
                                        screen.about_scroll = screen.about_scroll.saturating_sub(3);
                                    } else if screen.approval.is_some() {
                                        screen.approval_scroll = screen.approval_scroll.saturating_sub(3);
                                    } else if let Some(view) = screen.subagent_view.as_mut() {
                                        view.follow_bottom = false;
                                        view.scroll = if view.max_scroll > 0 {
                                            view.scroll.saturating_add(3).min(view.max_scroll)
                                        } else {
                                            view.scroll.saturating_add(3)
                                        };
                                    } else {
                                        screen.auto_scroll = false;
                                        screen.scroll = screen.scroll.saturating_add(3).min(screen.max_scroll);
                                    }
                                }
                                MouseEventKind::ScrollDown => {
                                    if screen.panel == Some(Panel::Settings) {
                                        screen.settings_scroll(false);
                                    } else if screen.panel == Some(Panel::Mcp) {
                                        if let Some(mcp) = screen.mcp_panel.as_mut() {
                                            mcp.scroll_down(3);
                                        }
                                    } else if screen.panel == Some(Panel::ModelPicker) {
                                        let mut provider_changed = false;
                                        if let Some(state) = screen.model_picker.as_mut() {
                                            if state.focus_models {
                                                if state.model_selected + 1 < state.models.len() {
                                                    state.model_selected += 1;
                                                }
                                            } else {
                                                let total = state.providers.sorted_names().len();
                                                if state.provider_selected + 1 < total {
                                                    state.provider_selected += 1;
                                                    provider_changed = true;
                                                }
                                            }
                                        }
                                        if provider_changed {
                                            screen.refresh_model_picker_local();
                                        }
                                    } else if screen.panel == Some(Panel::Ctf) {
                                        if screen.ctf_detail_view {
                                            screen.ctf_detail_scroll = screen.ctf_detail_scroll.saturating_add(3);
                                        } else {
                                            let len = screen.ctf_challenges_count();
                                            if len > 0 && screen.ctf_selected + 1 < len {
                                                screen.ctf_selected += 1;
                                            }
                                        }
                                    } else if screen.panel == Some(Panel::About) {
                                        screen.about_scroll = screen.about_scroll.saturating_add(3);
                                    } else if screen.approval.is_some() {
                                        screen.approval_scroll = screen
                                            .approval_scroll
                                            .saturating_add(3)
                                            .min(screen.approval_max_scroll);
                                    } else if let Some(view) = screen.subagent_view.as_mut() {
                                        if !view.follow_bottom {
                                            view.scroll = view.scroll.saturating_sub(3);
                                            if view.scroll == 0 {
                                                view.follow_bottom = true;
                                            }
                                        }
                                    } else {
                                        screen.scroll = screen.scroll.saturating_sub(3);
                                        if screen.scroll == 0 {
                                            screen.auto_scroll = true;
                                        }
                                    }
                                }
                                _ => {
                                    screen.handle_mouse(mouse);
                                }
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(error)) => return Err(error.into()),
                        None => break,
                    }
                }
            }
        }
        Ok::<(), color_eyre::Report>(())
    }.await;
    // Cancellation is cooperative with the shared runner so history is saved
    // even if rendering/input fails. Never detach an executing agent on exit.
    screen.reply(PermissionDecision::Deny);
    if let Some(q) = screen.question_state.take() {
        let _ = q.request.reply.send(cyber_agent::QuestionResponse {
            answers: Vec::new(),
            cancelled: true,
        });
    }
    if let Some(cancel) = cancel {
        let _ = cancel.send(());
    }
    if let Some(mut active) = active {
        match tokio::time::timeout(Duration::from_millis(1000), &mut active).await {
            Ok(Ok((restored, _))) => {
                runner = Some(restored);
            }
            _ => {
                active.abort();
            }
        }
    }
    if let Some(owner) = &runner {
        if let Some(mcp) = &owner.registries.mcp {
            mcp.shutdown_all().await;
        }
    }
    // 后台任务 detached 持有 registry clone：进程退出前显式终止运行中任务。
    screen.background.kill_all();
    restore_terminal();
    result?;
    let configured_at_exit = match runner.as_ref() {
        Some(owner) => cyber_core::setup::configured(&owner.ctx.config, &owner.ctx.providers),
        None => {
            let paths = cyber_core::Paths::detect()?;
            let (config, providers) = cyber_core::setup::effective_config(&paths, cwd)?;
            cyber_core::setup::configured(&config, &providers)
        }
    };
    Ok(configured_at_exit)
}

/// 把后台任务（子代理或命令）的完成状态格式化为：
/// 1. `summary`（在界面上作为 System 消息展示，支持 Markdown 渲染）
/// 2. `prompt`（作为提示词注入给模型，开启或引导思考轮次）
fn format_job_completion(
    job: &BackgroundJob,
    subagents: &SubagentArchive,
) -> Option<(String, String)> {
    match job.kind {
        JobKind::Subagent => {
            let result = job.archive_run_id.and_then(|run_id| {
                subagents
                    .snapshot()
                    .into_iter()
                    .find(|run| run.id == run_id)
                    .and_then(|run| run.result)
            });
            match &job.status {
                JobStatus::Finished(_) => {
                    const MAX_RESULT_CHARS: usize = 16_000;
                    let body = match result {
                        Some(text) if text.chars().count() > MAX_RESULT_CHARS => {
                            let mut truncated = truncate_chars(&text, MAX_RESULT_CHARS);
                            truncated.push_str(
                                "\n\n（输出过长已截断，完整转录可在 Ctrl+G 子代理面板查看）",
                            );
                            truncated
                        }
                        Some(text) => text,
                        None => "（子代理已完成，未产生文本输出）".into(),
                    };
                    let body = body.trim();
                    let summary = if body.starts_with('#')
                        || body.starts_with('|')
                        || body.starts_with('-')
                        || body.starts_with("```")
                    {
                        format!(
                            "⚡ 后台子代理 #{}（{}）完成：\n\n{}",
                            job.id, job.name, body
                        )
                    } else {
                        format!("⚡ 后台子代理 #{}（{}）完成：{}", job.id, job.name, body)
                    };
                    let prompt = format!(
                        "[后台子代理任务完成通知]\n子代理名称：{}\n任务ID：#{}\n状态：执行成功\n输出内容：\n{}\n\n请结合上述后台子代理的执行结果继续分析并回答。",
                        job.name, job.id, body
                    );
                    Some((summary, prompt))
                }
                JobStatus::Failed(error) => {
                    const MAX_ERROR_CHARS: usize = 4_000;
                    let body = if error.chars().count() > MAX_ERROR_CHARS {
                        truncate_chars(error, MAX_ERROR_CHARS)
                    } else {
                        error.clone()
                    };
                    let summary = format!(
                        "⚡ 后台子代理 #{}（{}）失败：\n\n{}",
                        job.id,
                        job.name,
                        body.trim()
                    );
                    let prompt = format!(
                        "[后台子代理任务失败通知]\n子代理名称：{}\n任务ID：#{}\n状态：执行失败\n错误信息：\n{}\n\n子代理执行出错，请根据上述错误信息判断原因并决定下一步操作。",
                        job.name, job.id, body.trim()
                    );
                    Some((summary, prompt))
                }
                JobStatus::Running | JobStatus::Killed => None,
            }
        }
        JobKind::Shell => match &job.status {
            JobStatus::Finished(code) => {
                const MAX_SHELL_OUTPUT_CHARS: usize = 8_000;
                let raw_output = job.lines.join("\n");
                let body = if raw_output.chars().count() > MAX_SHELL_OUTPUT_CHARS {
                    let mut truncated = truncate_chars(&raw_output, MAX_SHELL_OUTPUT_CHARS);
                    truncated
                        .push_str("\n\n（输出过长已截断，完整日志可在 Ctrl+B 后台任务面板查看）");
                    truncated
                } else if raw_output.trim().is_empty() {
                    "（无输出）".to_string()
                } else {
                    raw_output
                };
                let body = body.trim();
                let summary = format!(
                    "⚡ 后台任务 #{}（{}）完成（退出码 {}）：\n\n```\n{}\n```",
                    job.id, job.name, code, body
                );
                let prompt = format!(
                    "[后台命令执行完成通知]\n执行命令：{}\n任务ID：#{}\n退出码：{}\n输出内容：\n```\n{}\n```\n\n请根据该后台命令的输出结果继续分析或执行下一步操作。",
                    job.name, job.id, code, body
                );
                Some((summary, prompt))
            }
            JobStatus::Failed(error) => {
                const MAX_SHELL_ERROR_CHARS: usize = 4_000;
                let body = if error.chars().count() > MAX_SHELL_ERROR_CHARS {
                    truncate_chars(error, MAX_SHELL_ERROR_CHARS)
                } else {
                    error.clone()
                };
                let summary = format!(
                    "⚡ 后台任务 #{}（{}）失败：\n\n{}",
                    job.id,
                    job.name,
                    body.trim()
                );
                let prompt = format!(
                    "[后台命令执行失败通知]\n执行命令：{}\n任务ID：#{}\n错误信息：\n{}\n\n后台命令执行失败，请根据错误信息进行排查并决定下一步操作。",
                    job.name, job.id, body.trim()
                );
                Some((summary, prompt))
            }
            JobStatus::Running | JobStatus::Killed => None,
        },
    }
}

/// 把已结束的后台任务（Shell 或 Subagent）结果注入当前会话。
///
/// - 若当前处于空闲状态（不在思考状态下）：注入 System 摘要，将提示词注入为一轮新任务并开启思考状态（`spawn_turn`）。
/// - 若当前已在思考状态下（busy）：暂存 System 摘要待回合收尾时持久化，同时通过 `steering` 动态注入正在执行的思考流中，并入队保底。
fn drain_background_completions(
    screen: &mut CliScreen,
    runner: &mut Option<SessionRunner>,
    permissions: &Arc<PermissionBroker>,
    events: &mpsc::UnboundedSender<AgentEvent>,
    active: &mut Option<ActiveTurn>,
    cancel: &mut Option<oneshot::Sender<()>>,
) -> color_eyre::Result<()> {
    for job in screen.background.take_unreported_completions() {
        let Some((summary, prompt)) = format_job_completion(&job, &screen.subagents) else {
            continue;
        };

        if let Some(owner) = runner.as_mut() {
            // 当前处于空闲状态（不在思考状态下）：
            // 1. 注入会话记录
            owner.entries.push(ChatEntry::System(summary.clone()));
            owner.save()?;
            screen.message("System", &summary, MUTED);

            // 2. 注入提示词并开启思考状态！
            spawn_turn(
                screen,
                runner,
                prompt,
                true,
                permissions,
                events,
                active,
                cancel,
            );
        } else {
            // 当前已经在思考状态下（busy）：
            // 1. 结果暂存，待当前回合收尾时持久化进会话
            screen.pending_job_results.push_back(summary);
            // 2. 将提示词注入正在运行的推理流中（steering），并加入队列保底
            if let Some(tx) = &screen.active_steering_tx {
                let _ = tx.send(prompt.clone());
            }
            screen.queued_prompts.push_back(QueuedPrompt {
                text: prompt,
                displayed: true,
                kind: QueuedKind::Prompt,
            });
        }
    }
    Ok(())
}

/// busy（thinking/流式/工具执行）期间以 `/` 开头的输入的处置。
enum BusySlash {
    /// 立即执行（交 apply_action）
    Action(CliAction),
    /// 已在 screen 侧执行完毕（/paste、/bg run 等）
    Handled,
    /// 需要 runner：排队到回合结束后按序执行
    Defer,
    /// 退出（/quit、/exit）
    Quit,
}

/// busy 期间以 `/` 开头输入的分类：先交 `readonly_action`（与空闲同源的只读分派，立即执行），
/// 其余不触碰 runner 的本地指令就地执行，需要 runner 的按顺序排队。
fn classify_busy_slash(screen: &mut CliScreen, text: &str) -> BusySlash {
    // 只读指令（含面板与列表）解析为动作后立即执行；需要 runner 的指令返回 None 继续走
    // 下面的排队/就地分支。与空闲路径共用 `readonly_action`，输出逐字节一致。
    let readonly = cli_commands::readonly_action(&screen.view(), text);
    match readonly {
        Ok(Some(action)) => return BusySlash::Action(action),
        Err(error) => return BusySlash::Action(cli_commands::error_action(&error)),
        Ok(None) => {}
    }
    // `/update` 不在 `slash::COMMANDS` 中（TUI 专属入口），由上方 `readonly_action`
    // 以 `update_action` 分派后立即执行，不再落到 `Unknown` 分支。
    match crate::slash::parse(text) {
        crate::slash::SlashCommand::Quit => BusySlash::Quit,
        crate::slash::SlashCommand::Cancel => BusySlash::Action(CliAction::Cancel),
        crate::slash::SlashCommand::Todo(args) => {
            match cli_commands::todo_action(&screen.todos, &args) {
                Ok((action, _)) => BusySlash::Action(action),
                Err(error) => BusySlash::Action(cli_commands::error_action(&error)),
            }
        }
        crate::slash::SlashCommand::Image(args) => {
            let arg = args.trim();
            if arg.is_empty() || arg.eq_ignore_ascii_case("paste") {
                if !screen.handle_clipboard_image_or_text() {
                    screen.message(
                        "Error",
                        "剪贴板中未检测到图片。\n提示：\n1. Windows Terminal 会拦截 Ctrl+V，请按 Alt+V 粘贴图片！\n2. 或使用 /image <路径|URL> 指定图片路径。",
                        ERROR,
                    );
                }
                BusySlash::Handled
            } else {
                BusySlash::Defer
            }
        }
        crate::slash::SlashCommand::Bg(args) => match cli_commands::parse_bg(&args) {
            Ok(cli_commands::CliJobs::Run { .. }) => {
                screen.message(
                    "后台任务",
                    "AI 正在运行，请先 /cancel 或等待完成后再启动后台子代理",
                    MUTED,
                );
                BusySlash::Handled
            }
            Ok(jobs) => BusySlash::Action(CliAction::Jobs(jobs)),
            Err(error) => {
                screen.message("Error", &error.to_string(), ERROR);
                BusySlash::Handled
            }
        },
        crate::slash::SlashCommand::Subagents(args) => {
            let lower = args.trim().to_ascii_lowercase();
            if lower == "stop" || lower.starts_with("stop ") {
                let target = lower.strip_prefix("stop").unwrap_or("").trim();
                let message = match cli_commands::stop_subagents(
                    &screen.background,
                    &screen.subagents,
                    target,
                ) {
                    Ok(msg) => msg,
                    Err(err) => err.to_string(),
                };
                screen.message("Subagents", &message, ACCENT);
                BusySlash::Handled
            } else {
                BusySlash::Defer
            }
        }
        crate::slash::SlashCommand::Unknown(_) => {
            screen.message("Error", "Unknown command; use /help", ERROR);
            BusySlash::Handled
        }
        _ => BusySlash::Defer,
    }
}

/// busy 期间需要 runner 的指令入队：回执排队条数，回合收尾由 `drain_queued_inputs`
/// 依次执行。输入框提交与面板内选中（如会话选择器 Enter）共用，避免静默丢弃。
fn queue_busy_command(screen: &mut CliScreen, text: &str) {
    screen.queued_prompts.push_back(QueuedPrompt {
        text: text.to_owned(),
        displayed: true,
        kind: QueuedKind::Command,
    });
    let pending = screen
        .queued_prompts
        .iter()
        .filter(|q| q.kind == QueuedKind::Command)
        .count();
    screen.message(
        "已排队",
        &format!("回合结束后执行：{text}（排队 {pending} 条指令）"),
        MUTED,
    );
}

#[allow(clippy::too_many_arguments)]
fn handle_key(
    screen: &mut CliScreen,
    key: KeyEvent,
    runner: &mut Option<SessionRunner>,
    permissions: &Arc<PermissionBroker>,
    events: &mpsc::UnboundedSender<AgentEvent>,
    active: &mut Option<ActiveTurn>,
    cancel: &mut Option<oneshot::Sender<()>>,
) -> color_eyre::Result<bool> {
    if let Some(q_state) = screen.question_state.as_mut() {
        match q_state.handle_key(key) {
            crate::question_ui::QuestionUiResult::Continue => return Ok(false),
            crate::question_ui::QuestionUiResult::Submit(resp) => {
                if let Some(q) = screen.question_state.take() {
                    let _ = q.request.reply.send(resp);
                }
                return Ok(false);
            }
            crate::question_ui::QuestionUiResult::Cancel => {
                if let Some(q) = screen.question_state.take() {
                    let _ = q.request.reply.send(cyber_agent::QuestionResponse {
                        answers: Vec::new(),
                        cancelled: true,
                    });
                }
                return Ok(false);
            }
        }
    }
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    if key.code != KeyCode::Char('d') || !key.modifiers.is_empty() {
        screen.delete_pending = None;
    }
    if key.code == KeyCode::Esc && screen.has_selection() {
        screen.clear_selection();
        return Ok(false);
    }

    let is_ctrl_c = control && (key.code == KeyCode::Char('c') || key.code == KeyCode::Char('C'));
    if is_ctrl_c {
        if screen.has_selection() {
            screen.copy_selection_to_clipboard();
            screen.clear_selection();
            return Ok(false);
        }
        if screen.approval.is_some() {
            screen.reply(PermissionDecision::Deny);
        }
        if let Some(cancel_tx) = cancel.take() {
            let _ = cancel_tx.send(());
            if let Some(q) = screen.question_state.take() {
                let _ = q.request.reply.send(cyber_agent::QuestionResponse {
                    answers: Vec::new(),
                    cancelled: true,
                });
            }
            screen.queued_prompts.clear();
            screen.status = "Cancelling (再按一次 Ctrl+C 强制退出)".into();
            screen.last_ctrl_c = Some(std::time::Instant::now());
            return Ok(false);
        } else if screen.busy || screen.status.contains("Cancelling") {
            return Ok(true);
        } else if !screen.busy {
            if screen.panel.is_some()
                || screen.settings.is_some()
                || screen.form.is_some()
                || screen.picker.is_some()
                || screen.subagent_view.is_some()
            {
                screen.panel = None;
                screen.settings = None;
                screen.form = None;
                screen.picker = None;
                screen.subagent_panel = None;
                screen.jobs_panel = None;
                screen.subagent_view = None;
                screen.status = "已关闭面板 (再按一次 Ctrl+C 退出程序)".into();
                screen.last_ctrl_c = Some(std::time::Instant::now());
                return Ok(false);
            }
            if screen.input.is_empty() {
                return Ok(true);
            }
            if screen
                .last_ctrl_c
                .is_some_and(|t| t.elapsed() < Duration::from_millis(1500))
            {
                return Ok(true);
            }
            screen.last_ctrl_c = Some(std::time::Instant::now());
            screen.input = composer();
            screen.completions.clear();
            screen.status = "已清空输入 (再按一次 Ctrl+C 退出程序)".into();
            return Ok(false);
        }
        return Ok(true);
    }
    if screen.approval.is_some() && key.code == KeyCode::Esc {
        screen.reply(PermissionDecision::Deny);
        return Ok(false);
    }
    if control && key.code == KeyCode::Char('l') {
        screen.needs_clear = true;
        return Ok(false);
    }
    let is_v = key.code == KeyCode::Char('v') || key.code == KeyCode::Char('V');
    if is_v
        && (key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::ALT))
    {
        if screen
            .last_paste_instant
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(300))
        {
            return Ok(false);
        }
        if screen.handle_clipboard_image_or_text() {
            screen.update_completions(runner.as_ref());
            return Ok(false);
        }
    }
    if screen.panel == Some(Panel::Settings) {
        return handle_settings_key(screen, runner, permissions, key);
    }
    if screen.panel == Some(Panel::ModelPicker) {
        return handle_model_picker_key(screen, runner, key);
    }
    if screen.panel == Some(Panel::Mcp) {
        return handle_mcp_key(screen, runner, permissions, key);
    }
    if screen.panel == Some(Panel::Ctf) || screen.ctf_edit_form.is_some() {
        return handle_ctf_key(screen, runner, permissions, events, active, cancel, key);
    }
    // provider 表单里的模型列表面板打开时，Esc 只关面板、不关表单（见下方表单分支的
    // 面板按键拦截）。这里必须先放行，否则 form 会被整体 take 掉，用户看到的是
    // 「Esc 关面板的同时把 Provider 编辑界面也关了」。
    let form_model_picker_open = screen
        .form
        .as_ref()
        .is_some_and(|form| form.model_picker.is_some());
    if screen.approval.is_none() && key.code == KeyCode::Esc && !form_model_picker_open {
        if screen.form.take().is_some() || screen.picker.take().is_some() {
            screen.delete_pending = None;
            if let Some(tab) = screen.settings_return_tab.take() {
                screen.panel = Some(Panel::Settings);
                if let Some(settings) = screen.settings.as_mut() {
                    settings.tab = tab;
                    settings.editing = false;
                    if let Some(owner) = runner.as_ref() {
                        settings.providers_draft = owner.ctx.providers.clone();
                        settings.config_draft = owner.ctx.config.clone();
                    }
                } else {
                    let mut s = settings_state(runner, screen);
                    s.tab = tab;
                    screen.settings = Some(s);
                }
            }
            return Ok(false);
        }
        if (!screen.completion_closed && !screen.completions.is_empty())
            || screen.input.lines()[0].starts_with('/')
        {
            screen.completion_closed = true;
            return Ok(false);
        }
    }
    if screen.panel.is_some() && key.code == KeyCode::Esc {
        match screen.panel {
            Some(Panel::Ctf) if screen.ctf_detail_view => {
                screen.ctf_detail_view = false;
                screen.ctf_detail_scroll = 0;
            }
            Some(Panel::Subagents) => {
                screen.panel = None;
                screen.subagent_panel = None;
            }
            Some(Panel::Jobs) => {
                let in_detail = screen
                    .jobs_panel
                    .as_ref()
                    .is_some_and(|state| state.detail.is_some());
                if in_detail {
                    if let Some(state) = screen.jobs_panel.as_mut() {
                        state.detail = None;
                        state.detail_scroll = 0;
                        state.follow_bottom = true;
                    }
                } else {
                    screen.panel = None;
                    screen.jobs_panel = None;
                }
            }
            _ => {
                screen.panel = None;
                screen.ctf_detail_view = false;
                screen.ctf_detail_scroll = 0;
            }
        }
        return Ok(false);
    }
    // 全屏「关于」面板：只读；↑/↓/PgUp/PgDn 滚动，U/Enter 检查更新，? 关闭（Esc 已在上面处理）。
    if screen.panel == Some(Panel::About) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                screen.about_scroll = screen.about_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                screen.about_scroll = screen.about_scroll.saturating_add(1);
            }
            KeyCode::PageUp => {
                screen.about_scroll = screen.about_scroll.saturating_sub(5);
            }
            KeyCode::PageDown => {
                screen.about_scroll = screen.about_scroll.saturating_add(5);
            }
            KeyCode::Home => screen.about_scroll = 0,
            KeyCode::End => screen.about_scroll = usize::MAX,
            KeyCode::Char('u') | KeyCode::Char('U') | KeyCode::Enter => {
                screen.begin_update_check(cli_commands::CliUpdate::Check);
            }
            KeyCode::Char('?') => screen.panel = None,
            _ => {}
        }
        return Ok(false);
    }
    if control && key.code == KeyCode::Char('t') {
        if screen.panel == Some(Panel::Ctf) {
            screen.panel = None;
            screen.ctf_detail_view = false;
            screen.ctf_detail_scroll = 0;
        } else if screen.ctf_enabled {
            if let Some(challenges) = runner
                .as_ref()
                .and_then(|r| r.registries.ctf_challenges.as_ref())
            {
                screen.ctf_challenges = Arc::clone(challenges);
            }
            screen.ctf_selected = 0;
            screen.ctf_detail_view = false;
            screen.ctf_detail_scroll = 0;
            screen.ctf_list_scroll.set(0);
            screen.panel = Some(Panel::Ctf);
        } else {
            screen.message("CTF", "CTF 模式未开启（可输入 /ctf enable 开启）", MUTED);
        }
        return Ok(false);
    }
    if key.code == KeyCode::F(1)
        && screen.approval.is_none()
        && screen.form.is_none()
        && screen.picker.is_none()
    {
        screen.about_scroll = 0;
        let info = AboutInfo::collect(screen);
        screen.about = Some(info);
        screen.panel = Some(Panel::About);
        return Ok(false);
    }
    if (control && key.code == KeyCode::Char(',')) || key.code == KeyCode::F(3) {
        if screen.panel == Some(Panel::Settings) {
            if let Some(settings) = screen.settings.as_mut() {
                if settings.dirty {
                    settings.pending_discard_confirm = true;
                    return Ok(false);
                }
            }
            screen.panel = None;
            screen.settings = None;
        } else {
            let state = settings_state(runner, screen);
            screen.settings = Some(state);
            screen.panel = Some(Panel::Settings);
        }
        return Ok(false);
    }
    if control && key.code == KeyCode::Char('g') {
        if screen.panel == Some(Panel::Subagents) {
            screen.panel = None;
            screen.subagent_panel = None;
        } else {
            screen.panel = Some(Panel::Subagents);
            screen.settings = None;
            screen.ctf_detail_view = false;
            screen.jobs_panel = None;
            screen.subagent_panel = Some(SubagentPanelState::default());
        }
        return Ok(false);
    }
    if control && key.code == KeyCode::Char('b') {
        if screen.panel == Some(Panel::Jobs) {
            screen.panel = None;
            screen.jobs_panel = None;
        } else {
            screen.panel = Some(Panel::Jobs);
            screen.settings = None;
            screen.ctf_detail_view = false;
            screen.subagent_panel = None;
            screen.jobs_panel = Some(JobsPanelState::default());
        }
        return Ok(false);
    }
    if screen.panel == Some(Panel::Subagents) {
        if let Some(state) = screen.subagent_panel.as_mut() {
            let runs = screen.subagents.snapshot();
            if state.selected >= runs.len() && !runs.is_empty() {
                state.selected = runs.len() - 1;
            }
            let len = runs.len();
            match key.code {
                KeyCode::Up if len > 0 => state.selected = state.selected.saturating_sub(1),
                KeyCode::Down if len > 0 => {
                    state.selected = (state.selected + 1).min(len - 1);
                }
                KeyCode::Home => state.selected = 0,
                KeyCode::End if len > 0 => state.selected = len - 1,
                KeyCode::PageUp => state.selected = state.selected.saturating_sub(5),
                KeyCode::PageDown if len > 0 => {
                    state.selected = (state.selected + 5).min(len - 1);
                }
                KeyCode::Enter if len > 0 => {
                    let run_id = runs[state.selected].id;
                    screen.panel = None;
                    screen.subagent_panel = None;
                    screen.subagent_view = Some(SubagentViewState {
                        run_id,
                        follow_bottom: true,
                        scroll: 0,
                        tools_expanded: false,
                        built: None,
                        viewport: WrappedViewport {
                            padding: 1,
                            ..WrappedViewport::default()
                        },
                        max_scroll: 0,
                    });
                }
                _ => {}
            }
        }
        return Ok(false);
    }
    if screen.panel == Some(Panel::Jobs) {
        if let Some(state) = screen.jobs_panel.as_mut() {
            let jobs = screen.background.snapshot();
            if state.selected >= jobs.len() && !jobs.is_empty() {
                state.selected = jobs.len() - 1;
            }
            match state.detail {
                Some(id) => {
                    let still_exists = jobs.iter().any(|job| job.id == id);
                    if !still_exists {
                        state.detail = None;
                        state.detail_scroll = 0;
                        state.follow_bottom = true;
                    } else {
                        match key.code {
                            KeyCode::Up => {
                                state.follow_bottom = false;
                                state.detail_scroll = state.detail_scroll.saturating_add(1);
                            }
                            KeyCode::Down => {
                                if !state.follow_bottom {
                                    state.detail_scroll = state.detail_scroll.saturating_sub(1);
                                }
                            }
                            KeyCode::PageUp => {
                                state.follow_bottom = false;
                                state.detail_scroll = state.detail_scroll.saturating_add(10);
                            }
                            KeyCode::PageDown => {
                                if !state.follow_bottom {
                                    state.detail_scroll = state.detail_scroll.saturating_sub(10);
                                }
                            }
                            KeyCode::End => {
                                state.detail_scroll = 0;
                                state.follow_bottom = true;
                            }
                            _ => {}
                        }
                    }
                }
                None => {
                    let len = jobs.len();
                    match key.code {
                        KeyCode::Up if len > 0 => state.selected = state.selected.saturating_sub(1),
                        KeyCode::Down if len > 0 => {
                            state.selected = (state.selected + 1).min(len - 1);
                        }
                        KeyCode::Home => state.selected = 0,
                        KeyCode::End if len > 0 => state.selected = len - 1,
                        KeyCode::PageUp => state.selected = state.selected.saturating_sub(5),
                        KeyCode::PageDown if len > 0 => {
                            state.selected = (state.selected + 5).min(len - 1);
                        }
                        KeyCode::Enter if len > 0 => {
                            state.detail = Some(jobs[state.selected].id);
                            state.detail_scroll = 0;
                            state.follow_bottom = true;
                        }
                        // k 终止：仅 Running 生效；r 清理：仅已结束生效。
                        KeyCode::Char('k') if len > 0 => {
                            screen.background.kill(jobs[state.selected].id);
                        }
                        KeyCode::Char('r') if len > 0 => {
                            let removed = screen.background.remove(jobs[state.selected].id);
                            if removed && state.selected > 0 {
                                state.selected -= 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        return Ok(false);
    }
    // 子代理覆盖视图：仅拦截滚动/退出键；字符与 Enter 放行到输入框
    //（输入框仍绑定主对话）。busy 中同样可操作（不依赖 runner）。
    if screen.subagent_view.is_some() && screen.panel.is_none() && screen.approval.is_none() {
        let toggle_expanded = control && key.code == KeyCode::Char('o');
        let mut close_view = false;
        let mut intercepted = false;
        if let Some(view) = screen.subagent_view.as_mut() {
            match key.code {
                KeyCode::Up => {
                    view.follow_bottom = false;
                    view.scroll = if view.max_scroll > 0 {
                        view.scroll.saturating_add(1).min(view.max_scroll)
                    } else {
                        view.scroll.saturating_add(1)
                    };
                    intercepted = true;
                }
                KeyCode::Down => {
                    if !view.follow_bottom {
                        view.scroll = view.scroll.saturating_sub(1);
                        if view.scroll == 0 {
                            view.follow_bottom = true;
                        }
                    }
                    intercepted = true;
                }
                KeyCode::PageUp => {
                    view.follow_bottom = false;
                    view.scroll = if view.max_scroll > 0 {
                        view.scroll.saturating_add(10).min(view.max_scroll)
                    } else {
                        view.scroll.saturating_add(10)
                    };
                    intercepted = true;
                }
                KeyCode::PageDown => {
                    if !view.follow_bottom {
                        view.scroll = view.scroll.saturating_sub(10);
                        if view.scroll == 0 {
                            view.follow_bottom = true;
                        }
                    }
                    intercepted = true;
                }
                KeyCode::Home => {
                    view.follow_bottom = false;
                    view.scroll = view.max_scroll;
                    intercepted = true;
                }
                KeyCode::End => {
                    view.scroll = 0;
                    view.follow_bottom = true;
                    intercepted = true;
                }
                KeyCode::Esc => {
                    close_view = true;
                    intercepted = true;
                }
                _ => {}
            }
            if toggle_expanded {
                view.tools_expanded = !view.tools_expanded;
            }
        }
        if close_view {
            screen.subagent_view = None;
        }
        if intercepted || toggle_expanded {
            return Ok(false);
        }
    }
    let is_esc = key.code == KeyCode::Esc;
    // 表单里的模型列表面板打开时，Esc 只关面板：整段 Esc 处理都跳过，
    // 交给下方表单分支的面板按键拦截。
    if is_esc && !form_model_picker_open {
        if screen.approval.is_none() && (screen.form.is_some() || screen.picker.is_some()) {
            screen.form = None;
            screen.picker = None;
            screen.delete_pending = None;
            if let Some(tab) = screen.settings_return_tab.take() {
                screen.panel = Some(Panel::Settings);
                if let Some(settings) = screen.settings.as_mut() {
                    settings.tab = tab;
                    settings.editing = false;
                    if let Some(owner) = runner.as_ref() {
                        settings.providers_draft = owner.ctx.providers.clone();
                        settings.config_draft = owner.ctx.config.clone();
                    }
                } else {
                    let mut s = settings_state(runner, screen);
                    s.tab = tab;
                    screen.settings = Some(s);
                }
            }
            return Ok(false);
        }
        screen.reply(PermissionDecision::Deny);
        if let Some(cancel) = cancel.take() {
            let _ = cancel.send(());
            screen.status = "Cancelling".into();
        }
        return Ok(false);
    }
    if let Some(panel) = screen.panel {
        if key.code == KeyCode::Char('?') && panel == Panel::Shortcuts {
            screen.panel = None;
        }
        return Ok(false);
    }
    if key.code == KeyCode::F(2) || (control && key.code == KeyCode::Char('p')) {
        let next_mode = screen.permission_mode.next();
        screen.permission_mode = next_mode;
        permissions.set_mode(next_mode);
        screen.message(
            "Mode",
            &format!("已切换审批模式为：{}", next_mode.label()),
            ACCENT,
        );
        return Ok(false);
    }
    // todo 清单三态：Alt+↑ 逐级展开（收起→精简→全量），Alt+↓ 逐级收起（全量→精简→收起）。
    // 裸 ↑/↓ 已被补全菜单/输入历史/滚动占用；小键盘方向键在本 harness 下与主键盘方向键同码
    // （crossterm 0.28 的 `KeyEventState::KEYPAD` 仅 Unix CSI-u/kitty 路径产生，本项目未启用
    // 键盘增强标志），故用 Alt 组合键。到顶/到底时按键被消费但状态与提示不变。
    if (key.code == KeyCode::Up || key.code == KeyCode::Down)
        && key.modifiers.contains(KeyModifiers::ALT)
        && screen.approval.is_none()
        && screen.form.is_none()
        && screen.picker.is_none()
        && screen.subagent_view.is_none()
    {
        let expand = key.code == KeyCode::Up;
        let next = match (expand, screen.todo_view) {
            (true, TodoView::Collapsed) => TodoView::Summary,
            (true, TodoView::Summary) => TodoView::Full,
            (true, TodoView::Full) => TodoView::Full,
            (false, TodoView::Full) => TodoView::Summary,
            (false, TodoView::Summary) => TodoView::Collapsed,
            (false, TodoView::Collapsed) => TodoView::Collapsed,
        };
        if next != screen.todo_view {
            screen.todo_view = next;
            screen.status = match next {
                TodoView::Collapsed => "任务清单已收起（/todo open 或 Alt+↑ 展开）".into(),
                TodoView::Summary => "任务清单已展开（Alt+↑ 全展 · Alt+↓ 收起）".into(),
                TodoView::Full => "任务清单已全量展开（Alt+↓ 收起）".into(),
            };
        }
        return Ok(false);
    }
    // `/update` 确认态：y = 用安装脚本更新（可能需要退出），n/其它键取消；面板与表单态不接管。
    if let Some(pending) = screen
        .update_pending
        .as_ref()
        .map(|info| info.version.clone())
    {
        if screen.approval.is_none()
            && screen.form.is_none()
            && screen.picker.is_none()
            && screen.subagent_view.is_none()
        {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') if key.modifiers.is_empty() => {
                    return Ok(screen.start_update(&pending));
                }
                KeyCode::Char('n') | KeyCode::Char('N') if key.modifiers.is_empty() => {
                    screen.update_pending = None;
                    screen.status = "已取消更新（可按 /update 重新检查）".into();
                    screen.message("更新", "已取消更新", MUTED);
                    return Ok(false);
                }
                _ => {
                    screen.update_pending = None;
                    screen.status = "已取消更新（可按 /update 重新检查）".into();
                }
            }
        }
    }
    if !screen.completion_closed
        && !screen.completions.is_empty()
        && screen.approval.is_none()
        && screen.form.is_none()
        && screen.picker.is_none()
    {
        let is_up = key.code == KeyCode::Up
            || (!control
                && !key.modifiers.contains(KeyModifiers::ALT)
                && key.code == KeyCode::Char('8')
                && !screen.input.lines()[0].contains(' '));
        let is_down = key.code == KeyCode::Down
            || (!control
                && !key.modifiers.contains(KeyModifiers::ALT)
                && key.code == KeyCode::Char('2')
                && !screen.input.lines()[0].contains(' '));

        if is_up {
            screen.completion_accepted = false;
            screen.completion_selected = (screen.completion_selected + screen.completions.len()
                - 1)
                % screen.completions.len();
            return Ok(false);
        }
        if is_down {
            screen.completion_accepted = false;
            screen.completion_selected =
                (screen.completion_selected + 1) % screen.completions.len();
            return Ok(false);
        }
        if key.code == KeyCode::Tab {
            screen.complete();
            screen.completion_accepted = true;
            screen.update_completions(runner.as_ref());
            return Ok(false);
        }
        if key.code == KeyCode::BackTab {
            screen.completion_accepted = false;
            screen.completion_selected = (screen.completion_selected + screen.completions.len()
                - 1)
                % screen.completions.len();
            return Ok(false);
        }
        if key.code == KeyCode::Enter && !screen.completion_accepted && screen.complete() {
            screen.completion_accepted = true;
            screen.update_completions(runner.as_ref());
            return Ok(false);
        }
    }
    if screen.scroll > 0
        && screen.approval.is_none()
        && screen.form.is_none()
        && screen.picker.is_none()
    {
        if key.code == KeyCode::Up {
            screen.auto_scroll = false;
            screen.scroll = screen.scroll.saturating_add(1).min(screen.max_scroll);
            return Ok(false);
        }
        if key.code == KeyCode::Down {
            screen.scroll = screen.scroll.saturating_sub(1);
            if screen.scroll == 0 {
                screen.auto_scroll = true;
            }
            return Ok(false);
        }
        if key.code == KeyCode::Home {
            screen.auto_scroll = false;
            screen.scroll = screen.max_scroll;
            return Ok(false);
        }
        if key.code == KeyCode::End {
            screen.scroll = 0;
            screen.auto_scroll = true;
            return Ok(false);
        }
    } else if screen.approval.is_none()
        && screen.form.is_none()
        && screen.picker.is_none()
        && screen.panel.is_none()
        && (screen.completion_closed || screen.completions.is_empty())
        && key.modifiers.is_empty()
    {
        if key.code == KeyCode::Up && screen.input.lines().len() <= 1 {
            if !screen.prompt_history.is_empty() {
                if screen.history_index.is_none() {
                    screen.saved_draft = screen.input.lines().join("\n");
                    screen.history_index = Some(screen.prompt_history.len() - 1);
                } else if let Some(idx) = screen.history_index {
                    if idx > 0 {
                        screen.history_index = Some(idx - 1);
                    }
                }
                if let Some(idx) = screen.history_index {
                    let text = screen.prompt_history[idx].clone();
                    screen.input = composer();
                    screen.input.insert_str(&text);
                }
                return Ok(false);
            }
        } else if key.code == KeyCode::Down && screen.input.lines().len() <= 1 {
            if let Some(idx) = screen.history_index {
                if idx + 1 < screen.prompt_history.len() {
                    screen.history_index = Some(idx + 1);
                    let text = screen.prompt_history[idx + 1].clone();
                    screen.input = composer();
                    screen.input.insert_str(&text);
                } else {
                    screen.history_index = None;
                    let draft = std::mem::take(&mut screen.saved_draft);
                    screen.input = composer();
                    screen.input.insert_str(&draft);
                }
                return Ok(false);
            }
        }
    }
    if key.code == KeyCode::PageUp {
        if screen.approval.is_some() {
            screen.approval_scroll = screen.approval_scroll.saturating_sub(10);
        } else {
            screen.auto_scroll = false;
            screen.scroll = screen.scroll.saturating_add(10).min(screen.max_scroll);
        }
        return Ok(false);
    }
    if key.code == KeyCode::PageDown {
        if screen.approval.is_some() {
            screen.approval_scroll = screen
                .approval_scroll
                .saturating_add(10)
                .min(screen.approval_max_scroll);
        } else {
            screen.scroll = screen.scroll.saturating_sub(10);
            if screen.scroll == 0 {
                screen.auto_scroll = true;
            }
        }
        return Ok(false);
    }
    if screen.approval.is_some() && key.code == KeyCode::Up {
        screen.approval_scroll = screen.approval_scroll.saturating_sub(1);
        return Ok(false);
    }
    if screen.approval.is_some() && key.code == KeyCode::Down {
        screen.approval_scroll = screen
            .approval_scroll
            .saturating_add(1)
            .min(screen.approval_max_scroll);
        return Ok(false);
    }
    // A clipped approval must never be confirmed, even with a valid nonce.
    if screen
        .approval
        .as_ref()
        .is_some_and(|request| !screen.approval_visible || screen.approval_nonce != request.nonce)
        && key.code == KeyCode::Enter
    {
        return Ok(false);
    }
    if screen.approval.is_some() {
        match (key.code, key.modifiers) {
            (KeyCode::Char('1'), KeyModifiers::NONE) => {
                screen.select_approval(ApprovalChoice::Once);
            }
            (KeyCode::Char('2'), KeyModifiers::NONE) => {
                screen.select_approval(ApprovalChoice::Session);
            }
            (KeyCode::Char('3'), KeyModifiers::NONE) => {
                screen.select_approval(ApprovalChoice::Deny);
            }
            (KeyCode::Left, KeyModifiers::NONE) => {
                let choice = match screen.approval_choice {
                    Some(ApprovalChoice::Deny) => ApprovalChoice::Session,
                    Some(ApprovalChoice::Session) => ApprovalChoice::Once,
                    _ => ApprovalChoice::Deny,
                };
                screen.select_approval(choice);
            }
            (KeyCode::Right, KeyModifiers::NONE) | (KeyCode::Tab, KeyModifiers::NONE) => {
                let choice = match screen.approval_choice {
                    Some(ApprovalChoice::Once) => ApprovalChoice::Session,
                    Some(ApprovalChoice::Session) => ApprovalChoice::Deny,
                    _ => ApprovalChoice::Once,
                };
                screen.select_approval(choice);
            }
            (KeyCode::BackTab, _) => {
                let choice = match screen.approval_choice {
                    Some(ApprovalChoice::Deny) => ApprovalChoice::Session,
                    Some(ApprovalChoice::Session) => ApprovalChoice::Once,
                    _ => ApprovalChoice::Deny,
                };
                screen.select_approval(choice);
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                if let Some(choice) = screen.approval_choice {
                    screen.reply(choice.decision());
                }
            }
            _ => {}
        }
        return Ok(false);
    }
    if control && key.code == KeyCode::Char('o') {
        screen.toggle_tools();
        return Ok(false);
    }
    if let Some(form) = &mut screen.form {
        // 1) 模型列表面板优先接管按键（手输模式除外）。
        if form.model_picker.as_ref().is_some_and(|p| !p.manual) {
            match key.code {
                KeyCode::Up => {
                    if let Some(p) = form.model_picker.as_mut() {
                        let n = p.entries.len();
                        if n > 0 {
                            p.selected = (p.selected + n - 1) % n;
                        }
                    }
                }
                KeyCode::Down => {
                    if let Some(p) = form.model_picker.as_mut() {
                        let n = p.entries.len();
                        if n > 0 {
                            p.selected = (p.selected + 1) % n;
                        }
                    }
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    let chosen = form
                        .model_picker
                        .as_ref()
                        .and_then(|p| p.entries.get(p.selected))
                        .map(|e| e.id.clone());
                    if let Some(m) = chosen {
                        form.model_picker = None;
                        form.set_model_input(&m);
                    }
                }
                KeyCode::Esc => form.model_picker = None,
                KeyCode::Char('m') | KeyCode::Char('f') => {
                    if let Some(p) = form.model_picker.as_mut() {
                        p.manual = true;
                    }
                }
                KeyCode::Char('r') => {
                    // 重新拉取：用表单当前值构造临时配置（同步阻塞，与 Settings 的 T 键同模式）。
                    let cfg = provider_config_for_form(form, runner.as_ref());
                    let res = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(cyber_agent::fetch_models(&cfg))
                    })
                    .map_err(|e| e.to_string());
                    let provider_name = form_provider_name(form);
                    let models_map = runner
                        .as_ref()
                        .and_then(|r| r.ctx.providers.providers.get(&provider_name))
                        .map(|p| &p.models);
                    form.open_model_picker(res, models_map, &provider_name);
                }
                KeyCode::Char('t') | KeyCode::Char('v') => {
                    // 实测能力：只写能力缓存，不写 providers.toml（表单尚未保存）。
                    let model = form
                        .model_picker
                        .as_ref()
                        .and_then(|p| p.entries.get(p.selected))
                        .map(|e| e.id.clone());
                    if let Some(model) = model {
                        let cfg = provider_config_for_form(form, runner.as_ref());
                        let is_vision = key.code == KeyCode::Char('v');
                        let cap = tokio::task::block_in_place(|| {
                            tokio::runtime::Handle::current().block_on(async {
                                if is_vision {
                                    cyber_agent::probe_model_vision(&cfg, &model)
                                        .await
                                        .map(ProbeKindLite::Vision)
                                } else {
                                    cyber_agent::probe_model_reasoning(&cfg, &model)
                                        .await
                                        .map(ProbeKindLite::Reasoning)
                                }
                            })
                        });
                        let provider_name = form_provider_name(form);
                        screen.status = match cap {
                            Ok(ProbeKindLite::Vision(c)) => {
                                let _ = cyber_core::save_model_vision_capability(
                                    &provider_name,
                                    &model,
                                    c,
                                    None,
                                );
                                format!(
                                    "模型 [{model}] 视觉实测完成：{}",
                                    if c.is_supported() {
                                        "◈ 视觉"
                                    } else {
                                        "未测出识图输出"
                                    }
                                )
                            }
                            Ok(ProbeKindLite::Reasoning(c)) => {
                                let _ = cyber_core::save_model_reasoning_capability(
                                    &provider_name,
                                    &model,
                                    c,
                                    None,
                                );
                                format!(
                                    "模型 [{model}] 推理实测完成：{}",
                                    if c.is_supported() {
                                        "◈ 推理"
                                    } else {
                                        "未测出思考输出"
                                    }
                                )
                            }
                            Err(e) => format!("模型 [{model}] 探针失败: {e}"),
                        };
                        // 重算能力标签（探针结果刚写入缓存）。
                        let ids: Vec<String> = form
                            .model_picker
                            .as_ref()
                            .map(|p| p.entries.iter().map(|e| e.id.clone()).collect())
                            .unwrap_or_default();
                        let models_map = runner
                            .as_ref()
                            .and_then(|r| r.ctx.providers.providers.get(&provider_name))
                            .map(|p| &p.models);
                        form.open_model_picker(Ok(ids), models_map, &provider_name);
                    }
                }
                _ => {}
            }
            return Ok(false);
        }
        // 2) 手输模式：Esc / Enter 退出浮层（保留已输入内容）。
        if form.model_picker.as_ref().is_some_and(|p| p.manual) {
            match key.code {
                KeyCode::Esc => {
                    form.model_picker = None;
                    return Ok(false);
                }
                KeyCode::Enter if !form.model_input().trim().is_empty() => {
                    form.model_picker = None;
                    return Ok(false);
                }
                _ => {}
            }
        }
        // 3) 光标停在 model 字段：Enter（扫描表单仅 Enter）→ 打开模型列表。
        //    Provider 表单按需联网拉取；扫描表单只列 `providers.toml` 的本地模型（不联网）。
        let scan_form = matches!(form.form.kind, FormKind::ToolboxScan);
        let model_open_key =
            key.code == KeyCode::Enter || (!scan_form && key.code == KeyCode::Char(' '));
        let on_model = form
            .form
            .fields
            .get(form.selected)
            .is_some_and(|f| f.name == "model");
        if on_model && form.model_picker.is_none() && model_open_key {
            let provider_name = form_provider_name(form);
            let models_map = runner
                .as_ref()
                .and_then(|r| r.ctx.providers.providers.get(&provider_name))
                .map(|p| &p.models);
            if scan_form {
                let mut local: Vec<String> = models_map
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default();
                sort_model_ids(&mut local);
                if local.is_empty() {
                    form.open_model_picker(
                        Err(format!(
                            "服务商 [{provider_name}] 未在 providers.toml 配置模型清单；请直接手输模型名"
                        )),
                        models_map,
                        &provider_name,
                    );
                } else {
                    form.open_model_picker(Ok(local), models_map, &provider_name);
                }
            } else if form.last_models.is_empty() {
                let cfg = provider_config_for_form(form, runner.as_ref());
                let res = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(cyber_agent::fetch_models(&cfg))
                })
                .map_err(|e| e.to_string());
                form.open_model_picker(res, models_map, &provider_name);
            } else {
                let list = form.last_models.clone();
                form.open_model_picker(Ok(list), models_map, &provider_name);
            }
            return Ok(false);
        }
        let count = form.inputs.len();
        let save = (control && key.code == KeyCode::Char('s'))
            || (key.code == KeyCode::Enter
                && key.modifiers.is_empty()
                && form.selected + 1 >= count);
        // 当前选中字段名 + 手输兜底是否生效（model 字段只在手输模式下可编辑）。
        let sel_field_name = form
            .form
            .fields
            .get(form.selected)
            .map(|f| f.name.clone())
            .unwrap_or_default();
        let manual_model = form.model_picker.as_ref().is_some_and(|p| p.manual);
        if save {
            form.capture();
            if let Some(owner) = runner.as_mut() {
                match cli_commands::submit_form(owner, &form.form) {
                    Ok(action) => {
                        screen.form = None;
                        return apply_action(
                            screen,
                            action,
                            runner,
                            permissions,
                            events,
                            active,
                            cancel,
                        );
                    }
                    Err(_) => {
                        screen.status = "Cannot save form; check fields and configuration".into()
                    }
                }
            }
        } else if count > 0 {
            match key.code {
                KeyCode::Up => form.selected = (form.selected + count - 1) % count,
                KeyCode::Down => form.selected = (form.selected + 1) % count,
                KeyCode::BackTab => form.selected = (form.selected + count - 1) % count,
                KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    form.selected = (form.selected + count - 1) % count
                }
                KeyCode::Tab => form.selected = (form.selected + 1) % count,
                KeyCode::Enter if key.modifiers.is_empty() => {
                    form.selected = (form.selected + 1) % count
                }
                _ if form
                    .form
                    .fields
                    .get(form.selected)
                    .is_some_and(|f| f.name == "provider")
                    && matches!(
                        key.code,
                        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                    ) =>
                {
                    let names: Vec<String> = runner
                        .as_ref()
                        .map(|r| r.ctx.providers.sorted_names())
                        .unwrap_or_default();
                    if !names.is_empty() {
                        let options: Vec<&str> = names.iter().map(String::as_str).collect();
                        cycle_field_value(form, &options, key.code == KeyCode::Left);
                    }
                }
                _ if form
                    .form
                    .fields
                    .get(form.selected)
                    .is_some_and(|f| f.name == "kind")
                    && matches!(
                        key.code,
                        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                    ) =>
                {
                    let cur_kind = form.inputs[form.selected]
                        .lines()
                        .join("")
                        .trim()
                        .to_ascii_lowercase();
                    let cur_idx = cyber_core::PROVIDER_KINDS
                        .iter()
                        .position(|k| *k == cur_kind)
                        .unwrap_or(0);
                    let next_idx = if key.code == KeyCode::Left {
                        (cur_idx + cyber_core::PROVIDER_KINDS.len() - 1)
                            % cyber_core::PROVIDER_KINDS.len()
                    } else {
                        (cur_idx + 1) % cyber_core::PROVIDER_KINDS.len()
                    };
                    let new_kind = cyber_core::PROVIDER_KINDS[next_idx];
                    form.inputs[form.selected] = composer();
                    form.inputs[form.selected].insert_str(new_kind);
                }
                _ if form.form.fields.get(form.selected).is_some_and(|f| {
                    matches!(f.name.as_str(), "sensitive" | "enabled" | "preview")
                }) && matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                ) =>
                {
                    let cur_val = form.inputs[form.selected]
                        .lines()
                        .join("")
                        .trim()
                        .to_ascii_lowercase();
                    let is_true = matches!(cur_val.as_str(), "true" | "1" | "yes");
                    let new_val = if is_true { "false" } else { "true" };
                    form.inputs[form.selected] = composer();
                    form.inputs[form.selected].insert_str(new_val);
                }
                _ if form
                    .form
                    .fields
                    .get(form.selected)
                    .is_some_and(|f| f.name == "scope")
                    && matches!(
                        key.code,
                        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                    ) =>
                {
                    let scopes = ["both", "project", "global"];
                    let cur_val = form.inputs[form.selected]
                        .lines()
                        .join("")
                        .trim()
                        .to_ascii_lowercase();
                    let cur_idx = scopes.iter().position(|s| *s == cur_val).unwrap_or(0);
                    let next_idx = if key.code == KeyCode::Left {
                        (cur_idx + scopes.len() - 1) % scopes.len()
                    } else {
                        (cur_idx + 1) % scopes.len()
                    };
                    form.inputs[form.selected] = composer();
                    form.inputs[form.selected].insert_str(scopes[next_idx]);
                }
                _ if form
                    .form
                    .fields
                    .get(form.selected)
                    .is_some_and(|f| f.name == "context_length")
                    && matches!(
                        key.code,
                        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                    ) =>
                {
                    let cur = form.inputs[form.selected]
                        .lines()
                        .join("")
                        .trim()
                        .to_string();
                    // 环 = 预设槽（空值 = 「默认(留空)」槽）；自定义值不属于环 → 保持不动，
                    // 避免 ←/→ 意外覆盖用户手输的数字。
                    let cur_idx = if cur.is_empty() {
                        Some(0)
                    } else {
                        cyber_core::CONTEXT_LENGTH_PRESETS
                            .iter()
                            .position(|(_, v)| !v.is_empty() && cur == *v)
                    };
                    if let Some(cur_idx) = cur_idx {
                        let total = cyber_core::CONTEXT_LENGTH_PRESETS.len();
                        let next_idx = if key.code == KeyCode::Left {
                            (cur_idx + total - 1) % total
                        } else {
                            (cur_idx + 1) % total
                        };
                        let value = cyber_core::CONTEXT_LENGTH_PRESETS[next_idx].1;
                        form.inputs[form.selected] = composer();
                        form.inputs[form.selected].insert_str(value);
                    }
                }
                _ if form
                    .form
                    .fields
                    .get(form.selected)
                    .is_some_and(|f| matches!(f.name.as_str(), "thinking_type"))
                    && matches!(
                        key.code,
                        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                    ) =>
                {
                    cycle_field_value(
                        form,
                        &["", "enabled", "disabled"],
                        key.code == KeyCode::Left,
                    );
                }
                _ if form
                    .form
                    .fields
                    .get(form.selected)
                    .is_some_and(|f| matches!(f.name.as_str(), "thinking_effort"))
                    && matches!(
                        key.code,
                        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                    ) =>
                {
                    cycle_field_value(
                        form,
                        &["", "low", "medium", "high"],
                        key.code == KeyCode::Left,
                    );
                }
                // model / thinking 三行的值不在输入框里自由编辑（model 仅手输模式开放；
                // 工具库扫描表单的 model 直接手输，Enter 打开列表）。
                _ if !(scan_form || (sel_field_name == "model" && manual_model))
                    && matches!(
                        sel_field_name.as_str(),
                        "model" | "thinking_type" | "thinking_effort"
                    )
                    && matches!(key.code, KeyCode::Char(_))
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) => {}
                _ => {
                    form.inputs[form.selected].input(key);
                }
            }
        }
        return Ok(false);
    }
    if let Some(picker) = &screen.picker {
        if !key.modifiers.is_empty() && key.code != KeyCode::BackTab {
            return Ok(false);
        }
        let count = picker.items.len();
        let command = match key.code {
            KeyCode::Up | KeyCode::BackTab if count > 0 => {
                screen.picker_selected = (screen.picker_selected + count - 1) % count;
                screen.delete_pending = None;
                None
            }
            KeyCode::Down | KeyCode::Tab if count > 0 => {
                screen.picker_selected = (screen.picker_selected + 1) % count;
                screen.delete_pending = None;
                None
            }
            KeyCode::Enter if count > 0 => {
                Some(picker.items[screen.picker_selected].command.clone())
            }
            KeyCode::Char('n') if matches!(picker.kind, PickerKind::Sessions) => {
                Some("/new".into())
            }
            KeyCode::Char('d') if matches!(picker.kind, PickerKind::Sessions) && count > 0 => {
                let words = picker.items[screen.picker_selected]
                    .command
                    .split_whitespace()
                    .collect::<Vec<_>>();
                if let ["/sessions", id] = words.as_slice() {
                    if screen.delete_pending.as_deref() == Some(*id) {
                        Some(format!("/sessions delete {id}"))
                    } else {
                        screen.delete_pending = Some((*id).into());
                        None
                    }
                } else {
                    None
                }
            }
            _ => {
                screen.delete_pending = None;
                None
            }
        };
        if let Some(command) = command {
            screen.picker = None;
            screen.delete_pending = None;
            if command.is_empty() {
                if let Some(tab) = screen.settings_return_tab.take() {
                    screen.panel = Some(Panel::Settings);
                    if let Some(settings) = screen.settings.as_mut() {
                        settings.tab = tab;
                        settings.editing = false;
                        if let Some(owner) = runner.as_ref() {
                            settings.providers_draft = owner.ctx.providers.clone();
                            settings.config_draft = owner.ctx.config.clone();
                        }
                    }
                }
                return Ok(false);
            }
            // 回合进行中（runner 已被 take 走）：与在输入框里敲同一指令等价
            // （只读立即执行、本地立即执行、需 runner 的入队），不得静默丢弃选中项。
            let Some(owner) = runner.as_mut() else {
                return match classify_busy_slash(screen, &command) {
                    BusySlash::Quit => Ok(true),
                    BusySlash::Handled => Ok(false),
                    BusySlash::Action(action) => {
                        apply_action(screen, action, runner, permissions, events, active, cancel)
                    }
                    BusySlash::Defer => {
                        queue_busy_command(screen, &command);
                        Ok(false)
                    }
                };
            };
            match cli_commands::execute(owner, &command) {
                Ok(action) => {
                    return apply_action(
                        screen,
                        action,
                        runner,
                        permissions,
                        events,
                        active,
                        cancel,
                    )
                }
                Err(error) => screen.message("Error", &error.to_string(), ERROR),
            }
        }
        return Ok(false);
    }
    if screen.input.is_empty() && key.code == KeyCode::Char('?') {
        screen.panel = Some(Panel::Shortcuts);
        return Ok(false);
    }
    let is_enter = key.code == KeyCode::Enter;
    let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let has_alt_or_ctrl = key
        .modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL);

    if !is_enter {
        screen.input.input(key);
        screen.completion_closed = false;
        screen.completion_accepted = false;
        screen.completion_selected = 0;
        screen.update_completions(runner.as_ref());
        return Ok(false);
    }

    if has_shift {
        screen.input.insert_newline();
        screen.completion_closed = false;
        screen.completion_accepted = false;
        screen.completion_selected = 0;
        screen.update_completions(runner.as_ref());
        return Ok(false);
    }

    let text = screen.input.lines().join("\n");
    let text = text.trim();
    if text.is_empty() {
        return Ok(false);
    }

    if screen.busy {
        let text_owned = text.to_owned();
        screen.input = composer();
        screen.completions.clear();
        if text.starts_with('/') {
            // 以 / 开头的输入一律交 classify_busy_slash：本地指令立即执行，
            // 需要 runner 的指令排队（回合收尾 drain_queued_inputs 执行），不再静默丢弃。
            return match classify_busy_slash(screen, &text_owned) {
                BusySlash::Quit => Ok(true),
                BusySlash::Handled => Ok(false),
                BusySlash::Action(action) => {
                    apply_action(screen, action, runner, permissions, events, active, cancel)
                }
                BusySlash::Defer => {
                    queue_busy_command(screen, &text_owned);
                    Ok(false)
                }
            };
        }

        if has_alt_or_ctrl {
            // Alt+Enter / Ctrl+Enter: 即时导向（立刻打断当前生成并读取新指示）
            screen.message("You (立刻打断)", text, ACCENT);
            screen.prompt_history.push(text.to_owned());
            screen.history_index = None;
            screen.saved_draft.clear();
            screen.scroll = 0;
            screen.auto_scroll = true;
            screen.status = "已立即打断并读取新指示…".into();
            screen.reply(PermissionDecision::Deny);
            if let Some(tx) = cancel.take() {
                let _ = tx.send(());
            }
            screen.queued_prompts.push_back(QueuedPrompt {
                text: text.to_owned(),
                displayed: true,
                kind: QueuedKind::Prompt,
            });
            return Ok(false);
        } else {
            // Enter 无修饰键：平滑追加（不打断）
            screen.input = composer();
            screen.completions.clear();
            screen.message("You (追加)", text, ACCENT);
            screen.prompt_history.push(text.to_owned());
            screen.history_index = None;
            screen.saved_draft.clear();
            screen.scroll = 0;
            screen.auto_scroll = true;
            screen.status = "已追加指示（下一步自动读取）".into();
            if let Some(tx) = &screen.active_steering_tx {
                let _ = tx.send(text.to_owned());
            }
            screen.queued_prompts.push_back(QueuedPrompt {
                text: text.to_owned(),
                displayed: true,
                kind: QueuedKind::Prompt,
            });
            return Ok(false);
        }
    }

    screen.input = composer();
    screen.completions.clear();
    if text.starts_with('/') {
        if text.eq_ignore_ascii_case("/paste")
            || text.eq_ignore_ascii_case("/image")
            || text.eq_ignore_ascii_case("/image paste")
        {
            if screen.handle_clipboard_image_or_text() {
                screen.update_completions(runner.as_ref());
                return Ok(false);
            } else {
                screen.message(
                    "Error",
                    "剪贴板中未检测到图片。\n提示：\n1. Windows Terminal 会拦截 Ctrl+V，请按 Alt+V 粘贴图片！\n2. 或使用 /image <路径|URL> 指定图片路径。",
                    ERROR,
                );
                return Ok(false);
            }
        }
        if let Some(owner) = runner.as_mut() {
            match cli_commands::execute(owner, text) {
                Ok(action) => {
                    return apply_action(
                        screen,
                        action,
                        runner,
                        permissions,
                        events,
                        active,
                        cancel,
                    )
                }
                Err(error) => screen.message("Error", &error.to_string(), ERROR),
            }
        }
        return Ok(false);
    }

    spawn_turn(
        screen,
        runner,
        text.to_string(),
        false,
        permissions,
        events,
        active,
        cancel,
    );
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn spawn_turn(
    screen: &mut CliScreen,
    runner: &mut Option<SessionRunner>,
    prompt: String,
    displayed: bool,
    permissions: &Arc<PermissionBroker>,
    events: &mpsc::UnboundedSender<AgentEvent>,
    active: &mut Option<ActiveTurn>,
    cancel: &mut Option<oneshot::Sender<()>>,
) -> bool {
    let Some(mut owned) = runner.take() else {
        return false;
    };
    // 回合期间 runner 被 take：快照 (cwd, env) 供 `/bg shell` busy 时启动后台任务。
    let background_env = (
        owned.cwd.clone(),
        owned
            .ctx
            .config
            .env
            .vars
            .iter()
            .map(|value| (value.key.clone(), value.value.clone()))
            .collect::<Vec<(String, String)>>(),
    );
    screen.background_env = Some(background_env);
    let expanded = screen.expand_attached_placeholders(&prompt);
    if !displayed {
        screen.message("You", &prompt, ACCENT);
        screen.prompt_history.push(prompt.clone());
        screen.history_index = None;
        screen.saved_draft.clear();
        screen.scroll = 0;
        screen.auto_scroll = true;
    }
    screen.has_run = true;
    screen.busy = true;
    screen.thinking_started = Some(std::time::Instant::now());
    screen.status = "Working".into();
    permissions.set_mode(screen.permission_mode);
    let permissions = permissions.clone();
    let events = events.clone();
    let (tx, rx) = oneshot::channel();
    *cancel = Some(tx);
    let (s_tx, s_rx) = cyber_agent::steering_channel();
    screen.active_steering_tx = Some(s_tx);
    let intensity = screen.effort;
    *active = Some(tokio::spawn(async move {
        let outcome = owned
            .run_turn_ui(expanded, intensity, permissions, events, rx, Some(s_rx))
            .await;
        (owned, outcome)
    }));
    true
}

/// 回合结束：按输入顺序消费排队输入——指令就地执行，遇到第一个排队提示词即启动新一轮并返回。
/// 返回 `Ok(true)` 表示某条排队指令要求退出（/quit）。
fn drain_queued_inputs(
    screen: &mut CliScreen,
    runner: &mut Option<SessionRunner>,
    permissions: &Arc<PermissionBroker>,
    events: &mpsc::UnboundedSender<AgentEvent>,
    active: &mut Option<ActiveTurn>,
    cancel: &mut Option<oneshot::Sender<()>>,
) -> color_eyre::Result<bool> {
    while let Some(next) = screen.queued_prompts.pop_front() {
        match next.kind {
            QueuedKind::Command => {
                let Some(owner) = runner.as_mut() else {
                    screen.message(
                        "已排队",
                        &format!("指令未执行（回合未收尾）：{}", next.text),
                        ERROR,
                    );
                    continue;
                };
                match cli_commands::execute(owner, &next.text) {
                    Ok(action) => {
                        if apply_action(
                            screen,
                            action,
                            runner,
                            permissions,
                            events,
                            active,
                            cancel,
                        )? {
                            return Ok(true);
                        }
                    }
                    Err(error) => screen.message("Error", &error.to_string(), ERROR),
                }
                if screen.busy {
                    // 该指令自身开启了新回合（如 /compact），剩余队列留待下次收尾
                    return Ok(false);
                }
            }
            QueuedKind::Prompt => {
                spawn_turn(
                    screen,
                    runner,
                    next.text,
                    next.displayed,
                    permissions,
                    events,
                    active,
                    cancel,
                );
                return Ok(false);
            }
        }
    }
    Ok(false)
}

/// 设置面板状态来源：空闲取 runner，回合进行中取 `CliScreen` 的只读快照
/// （runner 已被 `take()` 走；不得退化为 `Config::default()` 的空面板）。
fn settings_state(runner: &Option<SessionRunner>, screen: &CliScreen) -> CliSettingsState {
    match runner {
        Some(owner) => CliSettingsState::from_view(&owner.view()),
        None => CliSettingsState::from_view(&screen.view()),
    }
}

/// 模型面板状态来源（同 `settings_state`）。
fn model_picker_state(runner: &Option<SessionRunner>, screen: &CliScreen) -> CliModelPickerState {
    match runner {
        Some(owner) => CliModelPickerState::from_view(&owner.view()),
        None => CliModelPickerState::from_view(&screen.view()),
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_action(
    screen: &mut CliScreen,
    action: CliAction,
    runner: &mut Option<SessionRunner>,
    permissions: &Arc<PermissionBroker>,
    events: &mpsc::UnboundedSender<AgentEvent>,
    active: &mut Option<ActiveTurn>,
    cancel: &mut Option<oneshot::Sender<()>>,
) -> color_eyre::Result<bool> {
    match action {
        CliAction::Output { title, text } => {
            if title == "Todo" && text.contains("已添加任务") {
                screen.todo_view = TodoView::Summary;
            }
            screen.message(&title, &text, MUTED);
            if let Some(tab) = screen.settings_return_tab.take() {
                screen.panel = Some(Panel::Settings);
                if let Some(settings) = screen.settings.as_mut() {
                    settings.tab = tab;
                    settings.editing = false;
                    if let Some(owner) = runner.as_ref() {
                        settings.providers_draft = owner.ctx.providers.clone();
                        settings.config_draft = owner.ctx.config.clone();
                    }
                } else {
                    let mut s = settings_state(runner, screen);
                    s.tab = tab;
                    screen.settings = Some(s);
                }
            }
        }
        CliAction::TodoVisibility(open) => {
            // `/todo open` 语义 = 精简态（与 Alt+↑ 展开一档一致）；全量态只能由 Alt+↑ 第二档到达。
            screen.todo_view = if open {
                TodoView::Summary
            } else {
                TodoView::Collapsed
            };
            if open {
                screen.status = "任务清单已展开（Alt+↑ 全展 · Alt+↓ 收起）".into();
            } else {
                screen.status = "任务清单已收起（/todo open 或 Alt+↑ 展开）".into();
            }
        }
        CliAction::Refresh {
            message,
            reset_usage,
        } => {
            if let Some(owner) = runner.as_ref() {
                if reset_usage {
                    permissions.clear_session();
                    screen.reset_usage();
                    screen.used_tokens =
                        estimate_messages_tokens(&entries_to_messages(&owner.entries));
                }
                screen.sync(owner);
                if let Some(message) = message {
                    screen.status = clean(&message);
                }
            }
            if let Some(tab) = screen.settings_return_tab.take() {
                screen.panel = Some(Panel::Settings);
                if let Some(settings) = screen.settings.as_mut() {
                    settings.tab = tab;
                    settings.editing = false;
                    if let Some(owner) = runner.as_ref() {
                        settings.providers_draft = owner.ctx.providers.clone();
                        settings.config_draft = owner.ctx.config.clone();
                    }
                } else {
                    let mut s = settings_state(runner, screen);
                    s.tab = tab;
                    screen.settings = Some(s);
                }
            }
        }
        CliAction::Form(form) => {
            screen.status.clear();
            screen.form = Some(FormState::new(form));
        }
        CliAction::Panel(panel) => {
            if panel == Panel::ModelPicker {
                // 打开只显示本地配置清单；接口拉取由左栏 Enter（选定 provider）或 r 触发。
                let state = model_picker_state(runner, screen);
                screen.model_picker = Some(state);
                screen.refresh_model_picker_local();
            } else if panel == Panel::Mcp {
                let (config, path) = match runner.as_ref() {
                    Some(owner) => (
                        cyber_mcp::McpServersConfig::load(&owner.ctx.paths.mcp_servers_file)
                            .unwrap_or_default(),
                        owner.ctx.paths.mcp_servers_file.clone(),
                    ),
                    None => {
                        let view = screen.view();
                        (
                            cyber_mcp::McpServersConfig::load(&view.paths.mcp_servers_file)
                                .unwrap_or_default(),
                            view.paths.mcp_servers_file.clone(),
                        )
                    }
                };
                let mcp = match runner.as_ref() {
                    Some(owner) => owner.registries.mcp.clone(),
                    None => screen.view_snapshot.mcp.clone(),
                };
                screen.mcp_panel = Some(crate::views::mcp_panel::McpPanelState::new(
                    config,
                    mcp.as_deref(),
                    path,
                ));
            } else if panel == Panel::Ctf {
                screen.ctf_enabled = true;
                if let Some(r) = runner.as_mut() {
                    r.ctf_enabled = true;
                    if let Some(challenges) = r.registries.ctf_challenges.as_ref() {
                        screen.ctf_challenges = Arc::clone(challenges);
                    }
                }
                screen.ctf_selected = 0;
                screen.ctf_detail_view = false;
                screen.ctf_detail_scroll = 0;
                screen.ctf_list_scroll.set(0);
            } else if panel == Panel::About {
                screen.about_scroll = 0;
                let info = AboutInfo::collect(screen);
                screen.about = Some(info);
            }
            screen.panel = Some(panel);
        }
        CliAction::Picker(picker) => {
            if matches!(picker.kind, cli_commands::PickerKind::Models) {
                // 打开只显示本地配置清单；接口拉取由左栏 Enter（选定 provider）或 r 触发。
                let state = model_picker_state(runner, screen);
                screen.model_picker = Some(state);
                screen.panel = Some(Panel::ModelPicker);
                screen.picker = None;
                screen.refresh_model_picker_local();
                return Ok(false);
            }
            screen.picker = Some(picker);
            screen.picker_selected = 0;
            screen.delete_pending = None;
        }
        CliAction::Task(task) => {
            if screen.busy || active.is_some() {
                return Ok(false);
            }
            // 扫描任务接管整个视图（结果输出到对话区），不再回到设置中心。
            if matches!(task, cli_commands::CliTask::ToolboxScan { .. }) {
                screen.settings_return_tab = None;
            }
            if let Some(mut owned) = runner.take() {
                permissions.set_mode(screen.permission_mode);
                let permissions = permissions.clone();
                let events = events.clone();
                let (tx, rx) = oneshot::channel();
                *cancel = Some(tx);
                screen.busy = true;
                screen.has_run = true;
                screen.thinking_started = Some(std::time::Instant::now());
                screen.status = "Working".into();
                *active = Some(tokio::spawn(async move {
                    let outcome =
                        cli_commands::run_task(&mut owned, task, permissions, events, rx).await;
                    (owned, outcome)
                }));
            }
        }
        CliAction::Cancel => {
            screen.reply(PermissionDecision::Deny);
            if let Some(tx) = cancel.take() {
                let _ = tx.send(());
                screen.status = "Cancelling".into();
            }
        }
        CliAction::Update(kind) => screen.begin_update_check(kind),
        CliAction::Quit => return Ok(true),
        CliAction::Mode(mode) => {
            screen.permission_mode = mode;
            permissions.set_mode(mode);
            screen.message(
                "Mode",
                &format!("已切换审批模式为：{}", mode.label()),
                ACCENT,
            );
        }
        CliAction::Settings => {
            let state = settings_state(runner, screen);
            screen.settings = Some(state);
            screen.panel = Some(Panel::Settings);
        }
        CliAction::SettingsTab(tab) => {
            let mut state = settings_state(runner, screen);
            state.tab = tab;
            screen.settings = Some(state);
            screen.panel = Some(Panel::Settings);
            screen.sync_about_if_active();
        }
        CliAction::Jobs(jobs) => match jobs {
            cli_commands::CliJobs::Shell { command } => {
                let Some((cwd, env)) = screen.background_env.clone() else {
                    screen.message("后台任务", "后台任务不可用（缺少工作目录快照）", ERROR);
                    return Ok(false);
                };
                match cyber_agent::background::spawn_shell_job(
                    &screen.background,
                    cwd,
                    env,
                    command.clone(),
                ) {
                    Ok(id) => screen.message(
                        "后台任务",
                        &format!("后台任务 #{id} 已启动：{command}"),
                        ACCENT,
                    ),
                    Err(error) => screen.message("后台任务", &error, ERROR),
                }
            }
            cli_commands::CliJobs::Run { prompt } => {
                let Some(owner) = runner.as_ref() else {
                    screen.message(
                        "后台任务",
                        "AI 正在运行，请先 /cancel 或等待完成后再启动后台子代理",
                        MUTED,
                    );
                    return Ok(false);
                };
                let id = owner.start_background_subagent(prompt);
                screen.message("后台任务", &format!("已启动后台子代理 #{id}"), ACCENT);
            }
            cli_commands::CliJobs::List => {
                screen.message("后台任务", &jobs_list_text(&screen.background), MUTED);
            }
            cli_commands::CliJobs::Kill(id) => {
                let text = if screen.background.kill(id) {
                    format!("已请求终止 #{id}")
                } else {
                    "任务不存在或已结束".to_string()
                };
                screen.message("后台任务", &text, MUTED);
            }
            cli_commands::CliJobs::Tail(id) => {
                let snapshot = screen.background.snapshot();
                match snapshot.into_iter().find(|job| job.id == id) {
                    Some(job) => {
                        let tail = job
                            .lines
                            .iter()
                            .rev()
                            .take(20)
                            .rev()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("\n");
                        let text = if tail.is_empty() {
                            format!("#{id} 暂无输出")
                        } else {
                            tail
                        };
                        screen.message(&format!("后台任务 #{id}"), &text, MUTED);
                    }
                    None => screen.message("后台任务", "任务不存在", MUTED),
                }
            }
        },
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn test_todo_table_long_text_does_not_corrupt_borders() {
        let items = vec![
            cyber_core::TodoItem {
                id: "1".into(),
                title: "这是一段极其冗长的测试渗透攻击任务步骤说明文字，包含全角中文标点与长标题，应当被安全边界截断，不能侵入右侧边框导致右边框字符被覆盖擦除。".into(),
                status: cyber_core::TodoStatus::InProgress,
                notes: Some("备注信息：同时包含长备注，用于测试剩余预算不足时的优雅省略号处理。".into()),
            },
            cyber_core::TodoItem {
                id: "2".into(),
                title: "第二项较短任务".into(),
                status: cyber_core::TodoStatus::Pending,
                notes: None,
            },
        ];

        let width = 60u16;
        let height = 15u16;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                draw_todo_table(
                    frame,
                    Rect::new(0, 0, width, height),
                    &items,
                    TodoView::Summary,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        // 校验最右列 (x = 59) 从 y = 0 到 y = 14 的字符
        // 顶部角为 ╮，底部角为 ╯，中间整列为 │，绝对不能有汉字、空格覆写
        let top_right = buffer[(width - 1, 0)].symbol();
        let bottom_right = buffer[(width - 1, height - 1)].symbol();
        assert_eq!(top_right, "╮", "右上角边框未对齐或被覆写: {top_right}");
        assert_eq!(
            bottom_right, "╯",
            "右下角边框未对齐或被覆写: {bottom_right}"
        );
        for y in 1..height - 1 {
            let ch = buffer[(width - 1, y)].symbol();
            assert_eq!(ch, "│", "第 {y} 行右边框被内容挤压覆盖或损坏: '{ch}'");
        }
    }

    #[test]
    fn todo_table_scrolls_to_keep_in_progress_item_visible() {
        use cyber_core::{TodoItem, TodoStatus};
        // 8 条：1-4 Completed，第 5 条 InProgress，6-8 Pending
        let items: Vec<TodoItem> = (1..=8)
            .map(|i| {
                let (id, status) = if i <= 4 {
                    (i.to_string(), TodoStatus::Completed)
                } else if i == 5 {
                    (i.to_string(), TodoStatus::InProgress)
                } else {
                    (i.to_string(), TodoStatus::Pending)
                };
                TodoItem::new(id, format!("任务 {i}"), status)
            })
            .collect();

        let width = 60u16;
        let height = 7u16;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                draw_todo_table(
                    frame,
                    Rect::new(0, 0, width, height),
                    &items,
                    TodoView::Summary,
                );
            })
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        // 内区 5 行 → visible 4、focus 下标 4 → start 1、hidden_above 1、hidden_below 3
        assert!(content.contains("[>] #5"), "进行中任务必须可见: {content}");
        assert!(
            content.contains("[x] #2"),
            "折叠边界后首条应可见: {content}"
        );
        assert!(!content.contains("[x] #1"), "首条应已滚出视口: {content}");
        // 宽字符在缓冲区内占两格，第二格为空符号，直接 contains 会失败：先去掉所有空白再断言
        let compact: String = content.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            compact.contains("上方还有1项"),
            "应提示上方折叠数: {content}"
        );
        assert!(compact.contains("下方3项"), "应提示下方折叠数: {content}");
    }

    #[test]
    fn test_subagent_panel_row_width_within_bounds() {
        let run = SubagentRun {
            id: 1,
            name: "subagent-network-reconnaissance-and-port-enumeration".into(),
            status: cyber_agent::SubagentStatus::Running,
            lines: vec!["正在执行长命令扫描探测开放服务端口 80, 443, 8080, 8443...".into()],
            result: None,
            error: None,
            started: std::time::Instant::now(),
        };
        let inner_width = 70usize;
        const PREFIX_W: usize = 2;
        const ID_W: usize = 5;
        const BADGE_W: usize = 8;
        const NAME_W: usize = 16;
        let last_budget = inner_width
            .saturating_sub(PREFIX_W + ID_W + BADGE_W + NAME_W + 1)
            .max(1);

        let (badge, _) = subagent_badge(run.status);
        let last = single_line(run.lines.last().unwrap());

        let line = Line::from(vec![
            Span::styled("▶ ", Style::default()),
            Span::styled(format!("#{:<3} ", run.id), Style::default()),
            Span::styled(pad_cells(badge, BADGE_W), Style::default()),
            Span::styled(
                pad_cells(&clip_cells(&single_line(&run.name), NAME_W), NAME_W),
                Style::default(),
            ),
            Span::styled(clip_cells(&last, last_budget), Style::default()),
        ]);
        use unicode_width::UnicodeWidthStr;
        let total_w: usize = line
            .spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        assert!(
            total_w < inner_width,
            "子代理面板数据行宽度 {total_w} 必须 <= inner_width - 1 ({})",
            inner_width - 1
        );
    }

    /// 检查居中弹窗的右边界整列与右上/右下圆角是否被内容覆写。
    fn popup_border_violations(buffer: &ratatui::buffer::Buffer, area: Rect) -> Vec<String> {
        let Some(popup) = panel_geometry(area) else {
            return vec!["panel_geometry returned None".to_string()];
        };
        let mut bad = Vec::new();
        let right = popup.x + popup.width - 1;
        let top_right = buffer[(right, popup.y)].symbol();
        if top_right != "╮" {
            bad.push(format!("右上角被覆写: {top_right:?}"));
        }
        let bottom_right = buffer[(right, popup.y + popup.height - 1)].symbol();
        if bottom_right != "╯" {
            bad.push(format!("右下角被覆写: {bottom_right:?}"));
        }
        for y in popup.y + 1..popup.y + popup.height - 1 {
            let sym = buffer[(right, y)].symbol();
            if sym != "│" {
                bad.push(format!("右边界 y={y} 被覆写: {sym:?}"));
            }
        }
        let left = popup.x;
        for y in popup.y + 1..popup.y + popup.height - 1 {
            let sym = buffer[(left, y)].symbol();
            if sym != "│" {
                bad.push(format!("左边界 y={y} 被覆写: {sym:?}"));
            }
        }
        bad
    }

    fn hostile_commands_line() -> String {
        "tool Commands args: {\"action\":\"list\",\"pattern\":\"/provider add-preset <id>\"} => \
         /help  /model  /sessions  /todo  /bg  /mcp  /skills  /memory  /env  /update  /quit"
            .to_string()
    }

    #[test]
    fn test_jobs_panel_borders_survive_long_commands_text() {
        let jobs = vec![
            BackgroundJob {
                id: 7,
                kind: JobKind::Shell,
                name: "help".into(),
                status: JobStatus::Running,
                lines: vec![hostile_commands_line(), hostile_commands_line()],
                created: std::time::Instant::now(),
                reported: false,
                kill: None,
                archive_run_id: None,
            },
            BackgroundJob {
                id: 8,
                kind: JobKind::Subagent,
                name: "极长的后台任务名称用于测试标题裁剪与边框保护：搜索所有服务商预设与自定义接入方式并汇总".into(),
                status: JobStatus::Finished(0),
                lines: vec![hostile_commands_line()],
                created: std::time::Instant::now(),
                reported: false,
                kill: None,
                archive_run_id: Some(1),
            },
        ];
        let archive = cyber_agent::SubagentArchive::default();
        let run_id = archive.start("Commands-recon");
        for _ in 0..3 {
            archive.append_line(run_id, hostile_commands_line());
        }

        for (w, h) in [(120u16, 40u16), (70, 24), (62, 20)] {
            // 列表模式
            let mut state = JobsPanelState {
                selected: 1,
                detail: None,
                detail_scroll: 0,
                follow_bottom: true,
            };
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|f| draw_jobs_panel(f, f.area(), &mut state, &jobs, &archive))
                .unwrap();
            let bad = popup_border_violations(terminal.backend().buffer(), Rect::new(0, 0, w, h));
            assert!(bad.is_empty(), "jobs 列表 w={w} 边框损坏: {bad:#?}");

            // 详情模式（Subagent 归档转录）
            let mut state = JobsPanelState {
                selected: 1,
                detail: Some(8),
                detail_scroll: 0,
                follow_bottom: true,
            };
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|f| draw_jobs_panel(f, f.area(), &mut state, &jobs, &archive))
                .unwrap();
            let bad = popup_border_violations(terminal.backend().buffer(), Rect::new(0, 0, w, h));
            assert!(bad.is_empty(), "jobs 详情 w={w} 边框损坏: {bad:#?}");

            // 详情模式（Shell 原始输出行）
            let mut state = JobsPanelState {
                selected: 0,
                detail: Some(7),
                detail_scroll: 0,
                follow_bottom: true,
            };
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|f| draw_jobs_panel(f, f.area(), &mut state, &jobs, &archive))
                .unwrap();
            let bad = popup_border_violations(terminal.backend().buffer(), Rect::new(0, 0, w, h));
            assert!(bad.is_empty(), "jobs shell 详情 w={w} 边框损坏: {bad:#?}");
        }
    }

    #[test]
    fn test_subagent_panel_borders_survive_long_activity() {
        let snapshot = vec![
            SubagentRun {
                id: 1,
                name: "recon".into(),
                status: cyber_agent::SubagentStatus::Running,
                lines: vec![hostile_commands_line()],
                result: None,
                error: None,
                started: std::time::Instant::now(),
            },
            SubagentRun {
                id: 2,
                name: "极长子代理名称用于测试列宽裁剪与右边框保护：遍历所有服务商与模型组合".into(),
                status: cyber_agent::SubagentStatus::Completed,
                lines: vec![hostile_commands_line(); 4],
                result: Some(hostile_commands_line()),
                error: None,
                started: std::time::Instant::now(),
            },
        ];
        for (w, h) in [(120u16, 40u16), (70, 24), (62, 20)] {
            let mut state = SubagentPanelState { selected: 1 };
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|f| draw_subagent_panel(f, f.area(), &mut state, &snapshot))
                .unwrap();
            let bad = popup_border_violations(terminal.backend().buffer(), Rect::new(0, 0, w, h));
            assert!(bad.is_empty(), "subagents w={w} 边框损坏: {bad:#?}");

            let mut state = SubagentPanelState { selected: 1 };
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|f| draw_subagent_panel(f, f.area(), &mut state, &[]))
                .unwrap();
            let bad = popup_border_violations(terminal.backend().buffer(), Rect::new(0, 0, w, h));
            assert!(bad.is_empty(), "subagents 空态 w={w} 边框损坏: {bad:#?}");
        }
    }

    #[test]
    fn test_tool_cards_never_exceed_card_width_when_collapsed() {
        use unicode_width::UnicodeWidthChar;
        let card = ToolCard {
            id: "commands".into(),
            name: "Commands".into(),
            arguments: "{\"action\":\"list\"}".into(),
            progress: String::new(),
            output: format!(
                "{}\n{}\n{}",
                hostile_commands_line(),
                hostile_commands_line(),
                hostile_commands_line()
            ),
            state: ToolCardState::Success,
            start: 0,
            end: 0,
        };
        let width_of = |line: &Line<'static>| -> usize {
            line.spans
                .iter()
                .flat_map(|s| s.content.chars())
                .map(|c| if c == '\t' { 4 } else { c.width().unwrap_or(0) })
                .sum()
        };
        for width in [20u16, 24, 40, 60, 120] {
            for expanded in [false, true] {
                let lines = render_tool_card(&card, expanded, width, false);
                for (i, line) in lines.iter().enumerate() {
                    if expanded && line.spans.first().map(|s| s.content.as_ref()) == Some("│  ") {
                        // 展开态正文允许由视口折行完整展示，不参与宽度约束。
                        continue;
                    }
                    let w = width_of(line);
                    assert!(
                        w <= width as usize,
                        "卡片行超宽会触发折行挤掉边框: width={width} expanded={expanded} row={i} w={w}"
                    );
                }
            }

            // 子代理覆盖视图（面板 Enter 进入）折叠态：每行都不得触发视口折行，
            // 否则续行丢失 `│` 前缀，表现为卡片边框被文字顶掉。
            let run = SubagentRun {
                id: 1,
                name: "commands-recon".into(),
                status: cyber_agent::SubagentStatus::Running,
                lines: vec![
                    "tool commands args: {\"action\":\"list\"}".to_string(),
                    hostile_commands_line(),
                    hostile_commands_line(),
                ],
                result: None,
                error: None,
                started: std::time::Instant::now(),
            };
            let view_lines = build_subagent_view_lines(&run, false, width);
            for (i, line) in view_lines.iter().enumerate() {
                let w = width_of(line);
                assert!(
                    w <= width as usize,
                    "子代理覆盖视图行超宽: width={width} row={i} w={w}"
                );
            }
        }
    }

    #[test]
    fn test_model_picker_borders_survive_long_provider_and_model_names() {
        let mut providers = cyber_core::ProvidersConfig::default();
        let mut provider = cyber_core::ProviderConfig {
            kind: "openai-compatible".into(),
            model: "deepseek-ai/DeepSeek-V3.2-Exp-SuperLong-Model-Identifier-2026".into(),
            ..Default::default()
        };
        provider.models.insert(
            "deepseek-ai/DeepSeek-V3.2-Exp-SuperLong-Model-Identifier-2026".into(),
            cyber_core::ModelConfig {
                alias: Some("极长的模型别名用于测试右栏标题裁剪与边框保护".into()),
                ..Default::default()
            },
        );
        providers.providers.insert(
            "extremely-long-provider-name-for-border-testing".into(),
            provider,
        );
        let mut state = CliModelPickerState {
            focus_models: true,
            provider_selected: 0,
            model_selected: 0,
            providers,
            default_provider: "extremely-long-provider-name-for-border-testing".into(),
            models: vec!["deepseek-ai/DeepSeek-V3.2-Exp-SuperLong-Model-Identifier-2026".into()],
            provider_scroll: 0,
            model_scroll: 0,
            probing_model: None,
            fetching: None,
            fetch_error: None,
            fetched: false,
            fetch_id: 0,
        };

        for (w, h) in [(120u16, 40u16), (90, 26), (80, 22)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|f| {
                    draw_model_picker(
                        f,
                        f.area(),
                        &mut state,
                        "extremely-long-provider-name-for-border-testing",
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer();

            // 复算面板几何：外层 + 左右分栏，逐个校验边框完整性。
            let outer = Rect::new(0, 0, w, h);
            let inner = Block::bordered().inner(outer);
            let status_h = if model_picker_fetch_status(&state).is_some() {
                2
            } else {
                1
            };
            let chunks = Layout::vertical([
                ratatui::layout::Constraint::Min(0),
                ratatui::layout::Constraint::Length(status_h),
            ])
            .split(inner);
            let panes = Layout::horizontal([
                ratatui::layout::Constraint::Percentage(35),
                ratatui::layout::Constraint::Percentage(65),
            ])
            .split(chunks[0]);

            let mut bad = Vec::new();
            for (name, rect) in [
                ("outer", outer),
                ("providers", panes[0]),
                ("models", panes[1]),
            ] {
                let right = rect.x + rect.width - 1;
                let top_right = buffer[(right, rect.y)].symbol();
                if top_right != "╮" {
                    bad.push(format!("{name} 右上角被覆写: {top_right:?}"));
                }
                let bottom_right = buffer[(right, rect.y + rect.height - 1)].symbol();
                if bottom_right != "╯" {
                    bad.push(format!("{name} 右下角被覆写: {bottom_right:?}"));
                }
                for y in rect.y + 1..rect.y + rect.height - 1 {
                    let sym = buffer[(right, y)].symbol();
                    if sym != "│" {
                        bad.push(format!("{name} 右边界 y={y} 被覆写: {sym:?}"));
                    }
                    let left_sym = buffer[(rect.x, y)].symbol();
                    if left_sym != "│" {
                        bad.push(format!("{name} 左边界 y={y} 被覆写: {left_sym:?}"));
                    }
                }
            }
            assert!(bad.is_empty(), "model picker w={w} 边框损坏: {bad:#?}");
        }
    }

    #[tokio::test]
    async fn model_picker_window_bounds_long_list_and_nav() {
        // 长模型列表只渲染可见窗口：选中项必须可见、远处条目不得渲染、
        // 整帧耗时不随列表长度线性增长（旧实现每行调一次 `CapabilityStore::load()`）。
        const TOTAL: usize = 8000;
        let mut state = CliModelPickerState {
            focus_models: true,
            provider_selected: 0,
            model_selected: TOTAL - 3,
            providers: cyber_core::ProvidersConfig::default(),
            default_provider: String::new(),
            models: (0..TOTAL).map(|i| format!("model-{i:04}")).collect(),
            provider_scroll: 0,
            model_scroll: 0,
            probing_model: None,
            fetching: None,
            fetch_error: None,
            fetched: true,
            fetch_id: 0,
        };

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let started = std::time::Instant::now();
        terminal
            .draw(|f| {
                draw_model_picker(f, f.area(), &mut state, "");
            })
            .unwrap();
        let elapsed = started.elapsed();

        let buffer = terminal.backend().buffer();
        let text: String = (0..30u16)
            .map(|y| {
                (0..120u16)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("model-7997"),
            "选中项必须可见（窗口跟随选中项）: {text}"
        );
        assert!(!text.contains("model-0000"), "窗口外条目不得被渲染: {text}");
        assert!(state.model_scroll > 0, "选中项在末尾时滚动偏移必须跟随");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "8000 条模型的单帧渲染耗时 {elapsed:?} 过高（疑似逐行读盘/构建全量行）"
        );

        // 模型栏 ↑/↓ 在长列表下照常移动选中项（窗口随之滚动）
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.model_picker = Some(CliModelPickerState {
            model_selected: 10,
            models: (0..TOTAL).map(|i| format!("model-{i:04}")).collect(),
            focus_models: true,
            ..Default::default()
        });
        screen.panel = Some(Panel::ModelPicker);
        handle_model_picker_key(
            &mut screen,
            &mut None,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(
            screen.model_picker.as_ref().unwrap().model_selected,
            11,
            "模型栏 Down 必须移动选中项"
        );
        handle_model_picker_key(
            &mut screen,
            &mut None,
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(screen.model_picker.as_ref().unwrap().model_selected, 10);
    }

    #[test]
    fn test_settings_row_long_hint_clipped() {
        use unicode_width::UnicodeWidthStr;
        let width = 75u16;
        let row = render_setting_row(
            true,
            false,
            "测试设置标签项 (Test)",
            "当前设定值".into(),
            "这是一段长说明文字：用户在此可以配置核心功能，说明过长时会自动添加省略号，保证不会超出设定的面板宽度。".into(),
            width,
        );
        let row_w: usize = row
            .spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        assert!(
            row_w <= (width as usize).saturating_sub(1),
            "设置行视觉宽度 {row_w} 必须 <= width - 1 ({})",
            width - 1
        );
    }
    #[test]
    fn omp_composer_merges_last_row_and_preserves_unicode_cursor() {
        for (effort, expected) in [
            (ThinkingIntensity::Low, Color::Rgb(23, 143, 185)),
            (ThinkingIntensity::Middle, LINK),
            (ThinkingIntensity::High, Color::Rgb(178, 129, 214)),
            (ThinkingIntensity::Max, CODE),
            (ThinkingIntensity::Auto, LINK),
        ] {
            assert_eq!(effort_color(effort), expected);
        }
        let mut input = composer();
        input.insert_str("A你好\nsecond");
        let mut terminal = Terminal::new(TestBackend::new(40, 3)).unwrap();
        terminal
            .draw(|frame| {
                draw_composer(
                    frame,
                    frame.area(),
                    &input,
                    " model · high ",
                    effort_color(ThinkingIntensity::High),
                    false,
                    false,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "╭");
        assert_eq!(buffer[(38, 2)].symbol(), "─");
        assert_eq!(buffer[(39, 2)].symbol(), "╯");
        assert_eq!(buffer[(3, 1)].symbol(), "A");
        assert_eq!(buffer[(4, 1)].symbol(), "你");
        assert_eq!(buffer[(6, 1)].symbol(), "好");
        assert!(buffer[(9, 2)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buffer[(0, 2)].fg, Color::Rgb(178, 129, 214));
        let input = composer();
        let mut terminal = Terminal::new(TestBackend::new(40, 2)).unwrap();
        terminal
            .draw(|frame| {
                draw_composer(
                    frame,
                    frame.area(),
                    &input,
                    " model · low ",
                    effort_color(ThinkingIntensity::Low),
                    false,
                    false,
                )
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(0, 1)].symbol(), "╰");
        assert_eq!(terminal.backend().buffer()[(39, 1)].symbol(), "╯");
    }

    #[tokio::test]
    async fn omp_transcript_uses_warm_fill_flat_assistant_and_semantic_markdown() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.message("You", "Inspect boundaries", ACCENT);
        screen.response_text(true, "quiet thought");
        screen.response_text(false, "# Findings\n- check `guard` and [docs](https://example.com)\n```rust\nlet safe = true;\n```");
        screen.tool_call("read-1", "read_file", r#"{"path":"real.rs"}"#);
        screen.tool_result("read-1", "read_file", "actual result", false);
        screen.tool_call("shell-1", "shell", r#"{"command":"false"}"#);
        screen.tool_call("sh-err", "shell", r#"{"command":"exit 1"}"#);
        screen.tool_result("sh-err", "shell", "actual failure", true);
        screen.response_text(false, "Follow-up after tools");
        assert_eq!(
            history(&screen)
                .lines()
                .filter(|line| *line == "Cyber")
                .count(),
            1
        );
        assert!(!history(&screen).lines().any(|line| line == "You"));
        let thought = screen
            .messages
            .iter()
            .find(|line| line.to_string() == "quiet thought")
            .unwrap();
        assert!(thought.spans.iter().all(|span| span.style.fg == Some(MUTED)
            && span.style.add_modifier.contains(Modifier::ITALIC)));
        let spans = screen
            .messages
            .iter()
            .flat_map(|line| &line.spans)
            .collect::<Vec<_>>();
        assert!(spans
            .iter()
            .any(|span| span.content == "guard" && span.style.fg == Some(CODE)));
        assert!(spans
            .iter()
            .any(|span| span.content == "docs" && span.style.fg == Some(LINK)));
        assert!(spans
            .iter()
            .any(|span| span.content.starts_with("# Findings") && span.style.fg == Some(ACCENT)));
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        for bg in [USER_BG, PENDING_BG, SUCCESS_BG, ERROR_BG, CODE_BG] {
            assert!(
                buffer.content.iter().any(|cell| cell.bg == bg),
                "missing {bg:?}"
            );
        }
        let user_row = (0..40)
            .find(|y| buffer[(1, *y)].bg == USER_BG && buffer[(1, *y)].symbol() == "I")
            .unwrap();
        assert_eq!(buffer[(0, user_row)].symbol(), " ");
        assert_eq!(buffer[(0, user_row)].bg, USER_BG);
        assert_eq!(buffer[(99, user_row)].bg, USER_BG);
        assert!(buffer
            .content
            .iter()
            .any(|cell| cell.symbol() == "╭" && cell.fg == ACCENT));
        assert!(buffer.content.iter().any(|cell| cell.fg == ERROR));
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn omp_completion_is_borderless_aligned_gold_including_description() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.insert_text("/");
        screen.completions = vec![
            CompletionItem {
                value: "/help ".into(),
                description: "Show help".into(),
            },
            CompletionItem {
                value: "/new ".into(),
                description: "Create a session".into(),
            },
        ];
        screen.completion_selected = 1;
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let selected_y = (0..20).find(|y| buffer[(0, *y)].symbol() == ">").unwrap();
        assert_eq!(buffer[(36, selected_y)].symbol(), "C");
        for x in [0, 2, 3, 36, 38, 44] {
            assert_eq!(buffer[(x, selected_y)].fg, ACCENT);
            assert_eq!(buffer[(x, selected_y)].bg, Color::Reset);
        }
        assert_eq!(
            buffer
                .content
                .iter()
                .filter(|cell| cell.symbol() == "╭")
                .count(),
            1
        );
        assert_eq!(
            select_row("你好", "description", true, 70).spans[2]
                .content
                .len(),
            30
        );
        assert!(!select_row("/help", "hidden description", false, 30)
            .to_string()
            .contains("hidden"));
        screen.update_completions(Some(&owner));
        let snapshot = render(&mut screen, 90, 30);
        assert_eq!(
            snapshot
                .lines()
                .filter(|line| line.starts_with("  /") || line.starts_with("> /"))
                .count(),
            10
        );
        assert!(!snapshot.contains("Message · / commands"));
        assert!(!snapshot.contains("Commands · Tab complete"));
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn omp_tool_preview_caps_visual_rows_and_expands_real_history_after_resize() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let output = format!("{}ACTUAL-END", "wide output ".repeat(1000));
        screen.tool_call("read-1", "read_file", r#"{"path":"wide.txt"}"#);
        screen.tool_result("read-1", "read_file", &output, false);
        render(&mut screen, 30, 16);
        assert!(screen.max_scroll < 20);
        assert!(!history(&screen).contains("ACTUAL-END"));
        screen.toggle_tools();
        assert!(history(&screen).contains("ACTUAL-END"));
        screen.toggle_tools();
        render(&mut screen, 90, 20);
        assert!(screen.tools[0].end - screen.tools[0].start <= 6);
        assert!(!history(&screen).contains("ACTUAL-END"));
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn omp_cards_render_edit_read_download_and_shell_with_status_decorations() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.tool_call("read-1", "read_file", r#"{"path":"src/main.rs"}"#);
        screen.tool_result(
            "read-1",
            "read_file",
            "line 1\nline 2\nline 3\nline 4\nline 5",
            false,
        );
        screen.tool_call(
            "edit-1",
            "edit",
            r#"{"path":"src/main.rs","input":"PUT 1:\n+new_fn()"}"#,
        );
        screen.tool_result("edit-1", "edit", "@@ -1 +1 @@\n-old\n+new", false);
        screen.tool_call(
            "dl-1",
            "download_file",
            r#"{"url":"https://example.com/asset.zip","output":"assets/asset.zip"}"#,
        );
        screen.tool_progress("dl-1", "download_file", "chunk 1\nchunk 2\n");
        let pending_view = render(&mut screen, 90, 30);
        assert!(pending_view.contains("Downloading"));
        assert!(pending_view.contains("assets/asset.zip"));
        screen.tool_result(
            "dl-1",
            "download_file",
            "已下载: assets/asset.zip\n大小: 12 KB (12288 bytes)\n耗时: 0.3s",
            false,
        );
        screen.tool_call("sh-1", "shell", r#"{"command":"cargo test"}"#);
        screen.tool_result("sh-1", "shell", "test 1 ... ok\ntest 2 ... FAILED", true);

        let text = render(&mut screen, 100, 35);
        for expected in [
            "╭─ ✓ Read  src/main.rs",
            "╭─ ✓ Edit  src/main.rs",
            "╭─ ✓ Downloaded  assets/asset.zip",
            "╭─ ✗ Shell  cargo test",
            "failed",
            "Ctrl+O details",
        ] {
            assert!(text.contains(expected), "missing {expected} in:\n{text}");
        }
        let _ = std::fs::remove_dir_all(owner.cwd);
    }
    #[tokio::test]
    async fn omp_cards_render_separate_subagent_cards_with_task_and_realtime_progress() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let delegate_args = serde_json::json!({
            "tasks": [
                {
                    "name": "solve-shishangwunanshi",
                    "system_prompt": "You are a binary expert.",
                    "task": "分析二进制文件 shishangwunanshi，找到其中的 flag",
                    "tools": ["shell"]
                },
                {
                    "name": "solve-windows-pwd",
                    "system_prompt": "You are a windows expert.",
                    "task": "提取 Windows 密码哈希并解密",
                    "tools": ["shell"]
                }
            ]
        })
        .to_string();

        screen.tool_call("del-1", "delegate_tasks", &delegate_args);

        // Before progress: both cards exist and show running
        let text_initial = render(&mut screen, 100, 30);
        assert!(text_initial.contains("╭─ ◇ Subagent [1/2]  solve-shishangwunanshi"));
        assert!(text_initial.contains("Task:"));
        assert!(text_initial.contains("shishangwunanshi"));
        assert!(text_initial.contains("Progress: queued"));
        assert!(text_initial.contains("╭─ ◇ Subagent [2/2]  solve-windows-pwd"));
        assert!(text_initial.contains("Windows"));
        // Progress arrives
        screen.tool_progress(
            "del-1",
            "delegate_tasks",
            "[1/2] solve-shishangwunanshi started\n[1/2] solve-shishangwunanshi: running tool: shell\n[2/2] solve-windows-pwd started\n",
        );
        let text_progress = render(&mut screen, 100, 30);
        assert!(text_progress.contains("Progress: running tool: shell"));
        assert!(text_progress.contains("Progress: started"));

        // Tool result arrives
        let delegate_result = serde_json::json!({
            "results": [
                {
                    "name": "solve-shishangwunanshi",
                    "status": "completed",
                    "output": "Found flag: cyber{shishang_wunan_shi}"
                },
                {
                    "name": "solve-windows-pwd",
                    "status": "error",
                    "error": "wordlist not found"
                }
            ]
        })
        .to_string();

        screen.tool_result("del-1", "delegate_tasks", &delegate_result, false);

        let text_done = render(&mut screen, 100, 30);
        assert!(text_done.contains("╭─ ✓ Subagent [1/2]  solve-shishangwunanshi"));
        assert!(text_done.contains("Result: Found flag: cyber{shishang_wunan_shi}"));
        assert!(text_done.contains("╭─ ✗ Subagent [2/2]  solve-windows-pwd"));
        assert!(text_done.contains("Error: wordlist not found"));

        // Test expanded view with toggle_tools (Ctrl+O)
        screen.toggle_tools();
        let text_expanded = render(&mut screen, 100, 30);
        assert!(text_expanded.contains("System: You are a binary expert."));
        assert!(text_expanded.contains("Tools: shell"));

        // Toggle back to collapsed
        screen.toggle_tools();
        let text_collapsed = render(&mut screen, 100, 30);
        assert!(!text_collapsed.contains("System: You are a binary expert."));

        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn omp_welcome_lists_at_most_four_persisted_sessions_without_fake_services() {
        let mut owner = crate::headless::tests::test_runner().await;
        for index in 0..6 {
            owner.entries = vec![ChatEntry::User(format!("Saved task {index}"))];
            owner.save().unwrap();
            owner.create_session().unwrap();
        }
        let mut screen = CliScreen::new(&owner);
        assert_eq!(screen.recent_sessions.len(), 4);
        let snapshot = render(&mut screen, 110, 30);
        assert!(snapshot.contains("Recent sessions"));
        for session in &screen.recent_sessions {
            assert!(snapshot.contains(&single_line(session)));
        }
        for fake in ["LSP", "git status", "manual mode", "agents", "π"] {
            assert!(!snapshot.contains(fake));
        }
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    fn input_key(
        screen: &mut CliScreen,
        runner: &mut Option<SessionRunner>,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> bool {
        let (events, _rx) = mpsc::unbounded_channel();
        handle_key(
            screen,
            KeyEvent::new(code, modifiers),
            runner,
            &Arc::new(PermissionBroker::deny_all()),
            &events,
            &mut None,
            &mut None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn notice_event_renders_amber_notice_entry() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.event(AgentEvent::Notice("⚠️ 输出被截断".into()));
        assert!(
            screen
                .messages
                .iter()
                .any(|line| line.to_string().contains("输出被截断")),
            "Notice 必须写入可见消息流"
        );
    }

    #[tokio::test]
    async fn provider_form_context_length_cycles_presets_and_keeps_custom() {
        let mut owner = crate::headless::tests::test_runner().await;
        let action = cli_commands::execute(&mut owner, "/provider add-with-kind openai").unwrap();
        let CliAction::Form(form) = action else {
            panic!("expected form");
        };
        let mut screen = CliScreen::new(&owner);
        screen.form = Some(FormState::new(form));
        let mut runner = Some(owner);
        let idx = screen
            .form
            .as_ref()
            .unwrap()
            .form
            .fields
            .iter()
            .position(|f| f.name == "context_length")
            .expect("表单必须含 context_length 字段");
        screen.form.as_mut().unwrap().selected = idx;
        // → 切到第一个预设 128K
        input_key(&mut screen, &mut runner, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(
            screen.form.as_ref().unwrap().inputs[idx].lines().join(""),
            "131072"
        );
        // → 依次前进到 256K
        input_key(&mut screen, &mut runner, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(
            screen.form.as_ref().unwrap().inputs[idx].lines().join(""),
            "262144"
        );
        // ← 回到 128K，再 ← 回到「默认(留空)」，再 ← 环绕到 1M
        input_key(&mut screen, &mut runner, KeyCode::Left, KeyModifiers::NONE);
        input_key(&mut screen, &mut runner, KeyCode::Left, KeyModifiers::NONE);
        assert!(screen.form.as_ref().unwrap().inputs[idx]
            .lines()
            .join("")
            .is_empty());
        input_key(&mut screen, &mut runner, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(
            screen.form.as_ref().unwrap().inputs[idx].lines().join(""),
            "1048576"
        );
        // → 从 1M 环绕回「默认(留空)」：预设环无死胡同
        input_key(&mut screen, &mut runner, KeyCode::Right, KeyModifiers::NONE);
        assert!(screen.form.as_ref().unwrap().inputs[idx]
            .lines()
            .join("")
            .is_empty());
        // 直接输入数字 = 自定义值：←/→ 不覆盖用户手输
        for digit in ["6", "5", "5", "3", "6"] {
            input_key(
                &mut screen,
                &mut runner,
                KeyCode::Char(digit.chars().next().unwrap()),
                KeyModifiers::NONE,
            );
        }
        assert_eq!(
            screen.form.as_ref().unwrap().inputs[idx].lines().join(""),
            "65536"
        );
        input_key(&mut screen, &mut runner, KeyCode::Left, KeyModifiers::NONE);
        input_key(&mut screen, &mut runner, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(
            screen.form.as_ref().unwrap().inputs[idx].lines().join(""),
            "65536"
        );
    }

    #[tokio::test]
    async fn command_tasks_return_owner_and_persist_cancelled_history() {
        let mut owner = crate::headless::tests::test_runner().await;
        owner.entries = vec![
            ChatEntry::User("original task input".into()),
            ChatEntry::Assistant("original answer".into()),
        ];
        let mut screen = CliScreen::new(&owner);
        let mut runner = Some(owner);
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, mut rx) = mpsc::unbounded_channel();
        let mut active = None;
        let mut cancel = None;
        screen.insert_text("/compact");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert!(screen.busy);
        assert!(runner.is_none());
        assert!(active.is_some());
        screen.insert_text("/cancel");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        let (owner, outcome) = active.take().unwrap().await.unwrap();
        while let Ok(event) = rx.try_recv() {
            screen.event(event);
        }
        screen.sync(&owner);
        assert!(outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Cancelled")));
        assert!(history(&screen).contains("original task input"));
        assert!(
            matches!(owner.entries.last(), Some(ChatEntry::TurnSummary { status, .. }) if status == "cancelled")
        );
        assert_eq!(
            serde_json::to_value(owner.read_entries(&owner.index.current).unwrap()).unwrap(),
            serde_json::to_value(&owner.entries).unwrap()
        );
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn approval_paste_is_ignored_and_escape_denies_without_cancelling_task() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.busy = true;
        let (reply, mut approval_rx) = oneshot::channel();
        screen.approval = Some(PermissionRequest {
            tool: "shell".into(),
            arguments: serde_json::json!({"command": "echo safe"}),
            nonce: "request-code".into(),
            reply,
            confidence: 0.0,
            risk_reason: String::new(),
        });
        screen.insert_text("once request-code\n/quit\n");
        screen.update_completions(None);
        assert!(screen.approval_input.is_empty());
        assert!(screen.input.is_empty());
        assert!(matches!(
            approval_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|cell| cell.fg == AMBER));
        let (tx, mut cancel_rx) = oneshot::channel();
        let mut cancel = Some(tx);
        let (events, _rx) = mpsc::unbounded_channel();
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut None,
            &Arc::new(PermissionBroker::deny_all()),
            &events,
            &mut None,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(approval_rx.try_recv(), Ok(PermissionDecision::Deny));
        assert!(matches!(
            cancel_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(screen.busy);
        let _ = std::fs::remove_dir_all(owner.cwd);
    }
    #[tokio::test]
    async fn ctrl_c_cancels_busy_task_and_second_ctrl_c_forces_exit() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.busy = true;
        screen.queued_prompts.push_back(QueuedPrompt {
            text: "queued text".into(),
            displayed: true,
            kind: QueuedKind::Prompt,
        });
        let (tx, mut cancel_rx) = oneshot::channel();
        let mut cancel = Some(tx);
        let (events, _rx) = mpsc::unbounded_channel();

        // First Ctrl+C: cancels the running task, clears queued prompts, does not exit yet
        let quit = handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut None,
            &Arc::new(PermissionBroker::deny_all()),
            &events,
            &mut None,
            &mut cancel,
        )
        .unwrap();
        assert!(!quit);
        assert_eq!(cancel_rx.try_recv(), Ok(()));
        assert!(cancel.is_none());
        assert!(screen.queued_prompts.is_empty());
        assert!(screen.status.contains("Cancelling"));

        // Second Ctrl+C while busy / cancelling: forces immediate exit!
        let quit = handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut None,
            &Arc::new(PermissionBroker::deny_all()),
            &events,
            &mut None,
            &mut cancel,
        )
        .unwrap();
        assert!(quit);
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn ctrl_c_idle_behavior_clears_then_exits() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.busy = false;
        screen.insert_text("some draft");
        let (events, _rx) = mpsc::unbounded_channel();
        let mut cancel = None;

        // First Ctrl+C with text: clears composer, does not exit
        let quit = handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut None,
            &Arc::new(PermissionBroker::deny_all()),
            &events,
            &mut None,
            &mut cancel,
        )
        .unwrap();
        assert!(!quit);
        assert!(screen.input.is_empty());

        // Second Ctrl+C with empty input: immediately exits!
        let quit = handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut None,
            &Arc::new(PermissionBroker::deny_all()),
            &events,
            &mut None,
            &mut cancel,
        )
        .unwrap();
        assert!(quit);
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn approval_number_selects_scope_and_enter_confirms() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let (reply, mut approval_rx) = oneshot::channel();
        screen.approval = Some(PermissionRequest {
            tool: "write_file".into(),
            arguments: serde_json::json!({"path": "report.md"}),
            nonce: "request-code".into(),
            reply,
            confidence: 0.0,
            risk_reason: String::new(),
        });
        render(&mut screen, 100, 24);
        assert!(screen.approval_visible);

        page_key(&mut screen, KeyCode::Char('2'));
        assert_eq!(screen.approval_choice, Some(ApprovalChoice::Session));
        render(&mut screen, 100, 24);
        page_key(&mut screen, KeyCode::Enter);

        assert_eq!(approval_rx.try_recv(), Ok(PermissionDecision::AllowSession));
        assert!(screen.approval.is_none());
        assert!(screen.approval_choice.is_none());
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn approval_button_navigation_supports_deny_option() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let (reply, mut approval_rx) = oneshot::channel();
        screen.approval = Some(PermissionRequest {
            tool: "shell".into(),
            arguments: serde_json::json!({"command": "rm -rf tmp"}),
            nonce: "nonce".into(),
            reply,
            confidence: 0.0,
            risk_reason: String::new(),
        });
        render(&mut screen, 90, 24);
        // Press 3 directly selects Deny button
        page_key(&mut screen, KeyCode::Char('3'));
        assert_eq!(screen.approval_choice, Some(ApprovalChoice::Deny));
        // Tab wraps around to 1 (Once)
        page_key(&mut screen, KeyCode::Tab);
        assert_eq!(screen.approval_choice, Some(ApprovalChoice::Once));
        // Left wraps backwards to 3 (Deny)
        page_key(&mut screen, KeyCode::Left);
        assert_eq!(screen.approval_choice, Some(ApprovalChoice::Deny));
        page_key(&mut screen, KeyCode::Enter);
        assert_eq!(approval_rx.try_recv(), Ok(PermissionDecision::Deny));
        assert!(screen.approval.is_none());
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn clear_refresh_recomputes_context_and_resets_real_usage() {
        let mut owner = crate::headless::tests::test_runner().await;
        owner.entries = vec![ChatEntry::User("x".repeat(400))];
        let mut screen = CliScreen::new(&owner);
        screen.event(AgentEvent::Usage(cyber_agent::Usage {
            prompt_tokens: 123,
            ..Default::default()
        }));
        assert!(screen.used_tokens > 0);
        let mut runner = Some(owner);
        screen.insert_text("/clear");
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.used_tokens, 0);
        assert!(!screen.usage_reported);
        assert!(screen.messages.is_empty());
        assert!(screen.footer().contains("History cleared"));
        let _ = std::fs::remove_dir_all(runner.unwrap().cwd);
    }

    #[tokio::test]
    async fn slash_menu_navigates_completes_and_escape_preserves_input() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let cwd = owner.cwd.clone();
        let mut runner = Some(owner);
        input_key(
            &mut screen,
            &mut runner,
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        );
        assert_eq!(screen.completions.len(), cli_commands::commands().len());
        assert!(!screen
            .completions
            .iter()
            .any(|item| item.value.trim() == "/mode"));
        input_key(&mut screen, &mut runner, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(screen.completion_selected, 1);
        input_key(&mut screen, &mut runner, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(screen.completion_selected, 0);
        // Numpad 2 (Down) and 8 (Up) select in command palette
        input_key(
            &mut screen,
            &mut runner,
            KeyCode::Char('2'),
            KeyModifiers::NONE,
        );
        assert_eq!(screen.completion_selected, 1);
        input_key(
            &mut screen,
            &mut runner,
            KeyCode::Char('8'),
            KeyModifiers::NONE,
        );
        assert_eq!(screen.completion_selected, 0);
        // When scrolled up, menu navigation is prioritized
        screen.scroll = 5;
        input_key(&mut screen, &mut runner, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(screen.completion_selected, 1);
        assert_eq!(screen.scroll, 5);
        input_key(&mut screen, &mut runner, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(screen.completion_selected, 0);
        screen.scroll = 0;
        let selected = screen.completions[0].value.clone();
        input_key(&mut screen, &mut runner, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(screen.input.lines().join("\n"), selected);
        input_key(&mut screen, &mut runner, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(screen.input.lines().join("\n"), selected);
        assert!(screen.completion_closed);
        input_key(
            &mut screen,
            &mut runner,
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        );
        assert!(!screen.completion_closed);
        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn enter_selects_then_executes_and_tab_supports_secondary_arguments() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let cwd = owner.cwd.clone();
        let mut runner = Some(owner);
        screen.insert_text("/hel");
        screen.update_completions(runner.as_ref());
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.input.lines().join("\n"), "/help ");
        assert!(screen.messages.is_empty());
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);
        assert!(history(&screen).contains("Commands"));
        assert!(screen.input.is_empty());
        screen.insert_text("/effort ma");
        screen.update_completions(runner.as_ref());
        input_key(&mut screen, &mut runner, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(screen.input.lines().join("\n"), "/effort max ");
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.effort, ThinkingIntensity::Max);
        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn completion_panel_guide_render_and_direct_enter_activation() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let cwd = owner.cwd.clone();
        let mut runner = Some(owner);

        // 1. 输入 /mcp：验证渲染快照中首行左栏为 /mcp，右栏展示 打开 MCP 面板
        screen.insert_text("/mcp");
        screen.update_completions(runner.as_ref());
        let snapshot = render(&mut screen, 100, 30);
        let first_row = snapshot
            .lines()
            .find(|l| l.contains("/mcp") && l.replace(' ', "").contains("打开MCP面板"))
            .expect("snapshot should render /mcp with panel guide description");
        assert!(first_row.contains("> /mcp"));
        assert!(first_row.replace(' ', "").contains("打开MCP面板"));

        // 回车选定第 0 项：直接触发 MCP 面板激活
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.panel, Some(Panel::Mcp));

        // 关闭面板并清空输入
        screen.panel = None;
        screen.input = composer();
        screen.completions.clear();

        // 2. 带空格输入 /mcp ：首项为引导项，后续项为子命令
        screen.insert_text("/mcp ");
        screen.update_completions(runner.as_ref());
        let snapshot_spaced = render(&mut screen, 100, 30);
        let first_row_spaced = snapshot_spaced
            .lines()
            .find(|l| l.contains("/mcp") && l.replace(' ', "").contains("打开MCP面板"))
            .expect("snapshot should render /mcp with panel guide description in spaced mode");
        assert!(first_row_spaced.contains("> /mcp"));
        assert!(first_row_spaced.replace(' ', "").contains("打开MCP面板"));
        assert!(snapshot_spaced.contains("/mcp connect"));

        // 回车选定第 0 项引导项：直接触发 MCP 面板激活
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.panel, Some(Panel::Mcp));

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn busy_slash_commands_run_locally_or_queue_without_owner_and_never_spawn_another_turn() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.busy = true;
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _rx) = mpsc::unbounded_channel();
        let (tx, mut rx) = oneshot::channel();
        let mut cancel = Some(tx);
        let mut active = None;
        // 需要 runner 的指令（runner 已被 take 走）排队待回合收尾执行，输入框清空。
        let mut expected = 0;
        for text in ["/new", "/compact"] {
            screen.input = composer();
            screen.insert_text(text);
            assert!(!handle_key(
                &mut screen,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &mut None,
                &permissions,
                &events,
                &mut active,
                &mut cancel
            )
            .unwrap());
            assert!(screen.input.lines().join("\n").is_empty());
            expected += 1;
            assert_eq!(screen.queued_prompts.len(), expected);
            assert_eq!(
                screen.queued_prompts.back().unwrap().kind,
                QueuedKind::Command
            );
            assert!(active.is_none());
            assert!(cancel.is_some());
        }
        let queued: Vec<&str> = screen
            .queued_prompts
            .iter()
            .map(|q| q.text.as_str())
            .collect();
        assert_eq!(queued, vec!["/new", "/compact"]);

        // `/model` 空参是只读面板指令：busy 下立即打开模型面板，不入队。
        screen.input = composer();
        screen.insert_text("/model");
        assert!(!handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut None,
            &permissions,
            &events,
            &mut active,
            &mut cancel
        )
        .unwrap());
        assert!(screen.input.lines().join("\n").is_empty());
        assert_eq!(screen.queued_prompts.len(), expected);
        assert_eq!(screen.panel, Some(Panel::ModelPicker));
        assert!(active.is_none());
        assert!(cancel.is_some());
        // 关闭模型面板，后续按键走普通输入路径。
        screen.panel = None;
        screen.model_picker = None;

        // 后段验证普通文本分支：先清空指令队列，保持断言独立。
        screen.queued_prompts.clear();

        // 普通文本在 busy 时按 Enter：平滑追加到 queued_prompts，清空输入框
        screen.input = composer();
        screen.insert_text("ordinary text");
        assert!(!handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut None,
            &permissions,
            &events,
            &mut active,
            &mut cancel
        )
        .unwrap());
        assert!(screen.input.lines().join("\n").is_empty());
        assert_eq!(screen.queued_prompts.len(), 1);
        assert_eq!(screen.queued_prompts[0].text, "ordinary text");
        assert_eq!(screen.status, "已追加指示（下一步自动读取）");
        assert!(active.is_none());
        assert!(cancel.is_some());

        // 普通文本在 busy 时按 Alt+Enter：立即打断并记录
        screen.input = composer();
        screen.insert_text("steer text");
        assert!(!handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            &mut None,
            &permissions,
            &events,
            &mut active,
            &mut cancel
        )
        .unwrap());
        assert!(screen.input.lines().join("\n").is_empty());
        assert_eq!(screen.queued_prompts.len(), 2);
        assert_eq!(screen.queued_prompts[1].text, "steer text");
        assert_eq!(screen.status, "已立即打断并读取新指示…");
        assert_eq!(rx.try_recv(), Ok(()));
        assert!(cancel.is_none());
        // Recreate cancel channel for subsequent /cancel check
        let (new_tx, new_rx) = oneshot::channel();
        cancel = Some(new_tx);
        rx = new_rx;
        screen.input = composer();
        screen.insert_text("/cancel");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut None,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(rx.try_recv(), Ok(()));
        assert_eq!(screen.status, "Cancelling");
        screen.insert_text("/quit");
        assert!(handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut None,
            &permissions,
            &events,
            &mut active,
            &mut cancel
        )
        .unwrap());
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn busy_slash_commands_execute_locally_without_runner() {
        let owner = crate::headless::tests::test_runner().await;
        let cwd = owner.cwd.clone();
        let mut screen = CliScreen::new(&owner);
        screen.busy = true;
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _rx) = mpsc::unbounded_channel();
        let (tx, _rx2) = oneshot::channel();
        let mut cancel = Some(tx);
        let mut active = None;

        macro_rules! submit {
            ($text:expr) => {{
                screen.input = composer();
                screen.insert_text($text);
                assert!(!handle_key(
                    &mut screen,
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    &mut None,
                    &permissions,
                    &events,
                    &mut active,
                    &mut cancel,
                )
                .unwrap());
            }};
        }

        // 渲染缓冲区内宽字符占两格（第二格为空符号），CJK/带空格断言前先去空白。
        // （`flat` 为模块级测试辅助函数）

        // `/help` 立即输出命令帮助（与空闲路径同文案），不入队。
        submit!("/help");
        let help = history(&screen);
        assert!(help.contains("/help"), "{help}");
        assert!(help.contains("显示此帮助"), "{help}");
        assert!(screen.input.lines().join("\n").is_empty());
        assert!(screen.queued_prompts.is_empty());

        // `/todo add …` 立即写入共享清单。
        submit!("/todo add 排队测试");
        assert_eq!(screen.todos.lock().unwrap().len(), 1);
        let text = render(&mut screen, 120, 24);
        assert!(flat(&text).contains("已添加任务#1"), "{text}");

        // `/todo close|open` 只切换可见性，不入队。
        submit!("/todo close");
        assert_eq!(screen.todo_view, TodoView::Collapsed);
        assert!(screen.queued_prompts.is_empty());
        submit!("/todo open");
        assert_eq!(screen.todo_view, TodoView::Summary);

        // `/mode manual` 立即切换审批模式。
        submit!("/mode manual");
        assert_eq!(screen.permission_mode, PermissionMode::Manual);
        let text = render(&mut screen, 120, 24);
        assert!(flat(&text).contains("已切换审批模式为：手动审批"), "{text}");

        // `/mode <非法>` 立即报错且不改变模式。
        submit!("/mode nope");
        assert_eq!(screen.permission_mode, PermissionMode::Manual);
        let text = render(&mut screen, 120, 24);
        assert!(flat(&text).contains("未知审批模式"), "{text}");

        // `/model` 空参是只读面板指令：立即打开模型面板，不入队。
        submit!("/model");
        assert!(screen.queued_prompts.is_empty());
        assert_eq!(screen.panel, Some(Panel::ModelPicker));
        assert!(screen.input.lines().join("\n").is_empty());
        // 关闭模型面板，后续按键走普通输入路径。
        screen.panel = None;
        screen.model_picker = None;

        // 未知指令立即报错且不入队。
        submit!("/nosuchcmd");
        let text = render(&mut screen, 120, 24);
        assert!(flat(&text).contains("Unknowncommand;use/help"), "{text}");
        assert!(screen.queued_prompts.is_empty());

        assert!(active.is_none());
        assert!(cancel.is_some());

        let _ = std::fs::remove_dir_all(cwd);
    }

    /// 回合进行中（runner 已被 take 走）的只读指令必须立即生效，且输出与空闲路径逐字节一致。
    #[tokio::test]
    async fn busy_readonly_commands_run_immediately_without_runner() {
        const READONLY: [&str; 17] = [
            "/help",
            "/tools",
            "/think",
            "/effort",
            "/max_steps",
            "/env list",
            "/web status",
            "/vision status",
            "/skill list",
            "/mcp status",
            "/toolbox list",
            "/memory list",
            "/subagents status",
            "/ctf status",
            "/provider list",
            "/model",
            "/sessions",
        ];
        let owner = crate::headless::tests::test_runner().await;
        let cwd = owner.cwd.clone();
        let mut busy = CliScreen::new(&owner);
        busy.busy = true;
        let mut runner = Some(owner);
        let mut idle = CliScreen::new(runner.as_ref().unwrap());
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _rx) = mpsc::unbounded_channel();
        let (tx, _rx2) = oneshot::channel();
        let mut cancel = Some(tx);
        let mut active = None;
        let mut no_runner: Option<SessionRunner> = None;

        macro_rules! busy_submit {
            ($text:expr) => {{
                busy.panel = None;
                busy.picker = None;
                busy.input = composer();
                busy.insert_text($text);
                assert!(!handle_key(
                    &mut busy,
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    &mut no_runner,
                    &permissions,
                    &events,
                    &mut active,
                    &mut cancel,
                )
                .unwrap());
            }};
        }
        macro_rules! idle_submit {
            ($text:expr) => {{
                idle.panel = None;
                idle.picker = None;
                idle.input = composer();
                idle.insert_text($text);
                assert!(!handle_key(
                    &mut idle,
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    &mut runner,
                    &permissions,
                    &events,
                    &mut active,
                    &mut cancel,
                )
                .unwrap());
            }};
        }

        // 同一指令在 busy 与空闲下必须产生完全相同的历史输出。
        for text in READONLY {
            busy_submit!(text);
            assert!(busy.queued_prompts.is_empty(), "{text} 不应排队");
            assert!(active.is_none(), "{text} 不应启动新回合");
            assert!(cancel.is_some(), "{text} 不应取消当前回合");
            let busy_history = history(&busy);

            idle_submit!(text);
            let idle_history = history(&idle);

            assert_eq!(
                busy_history, idle_history,
                "{text} 的 busy 输出必须与空闲一致"
            );
        }

        // 立即输出的内容（不是「已排队」提示）。
        busy_submit!("/tools");
        let text = history(&busy);
        assert!(text.contains("Tools"), "{text}");
        assert!(text.contains("ctf_challenge"), "{text}");

        busy_submit!("/provider list");
        let text = history(&busy);
        let provider = runner
            .as_ref()
            .unwrap()
            .ctx
            .providers
            .sorted_names()
            .into_iter()
            .next()
            .unwrap();
        assert!(text.contains(&provider), "{text}");

        // `/effort` 经 remap 与 `/think` 输出同一条文案（busy 下不再报 Unknown）。
        let base = history(&busy);
        busy_submit!("/think");
        let after_think = history(&busy);
        let intensity = runner
            .as_ref()
            .unwrap()
            .ctx
            .config
            .agent
            .thinking_intensity
            .as_str();
        let think_delta = after_think
            .strip_prefix(&base)
            .expect("历史只追加")
            .to_string();
        assert!(think_delta.contains("Thinking"), "{think_delta}");
        assert!(think_delta.contains(intensity), "{think_delta}");
        busy_submit!("/effort");
        let after_effort = history(&busy);
        let effort_delta = after_effort.strip_prefix(&after_think).expect("历史只追加");
        assert_eq!(think_delta, effort_delta, "/effort 必须等价于 /think");

        // 面板类只读指令就地打开面板。
        busy_submit!("/model");
        assert_eq!(busy.panel, Some(Panel::ModelPicker));
        assert!(busy.queued_prompts.is_empty());
        busy_submit!("/sessions");
        assert_eq!(
            busy.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Sessions)
        );

        let _ = std::fs::remove_dir_all(cwd);
    }

    /// busy 下 `/settings` 打开的是真实配置快照（而非 `Config::default()` 空面板），
    /// 且 Ctrl+S 只提示不落盘、不显示假成功。
    #[tokio::test]
    async fn busy_settings_opens_populated_panel_and_defers_save() {
        let owner = crate::headless::tests::test_runner().await;
        let cwd = owner.cwd.clone();
        let config_path = owner.ctx.paths.config_file.display().to_string();
        let providers_path = owner.ctx.paths.providers_file.display().to_string();
        let default_provider = owner.ctx.config.agent.default_provider.clone();
        let sessions_count = owner.index.sessions.len();
        let mut screen = CliScreen::new(&owner);
        screen.busy = true;
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _rx) = mpsc::unbounded_channel();
        let (tx, _rx2) = oneshot::channel();
        let mut cancel = Some(tx);
        let mut active = None;
        let mut no_runner: Option<SessionRunner> = None;

        screen.input = composer();
        screen.insert_text("/settings");
        assert!(!handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut no_runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap());
        assert_eq!(screen.panel, Some(Panel::Settings));
        assert!(screen.queued_prompts.is_empty());
        let settings = screen.settings.as_ref().expect("设置面板已打开");
        assert_eq!(settings.config_path, config_path);
        assert_eq!(settings.providers_path, providers_path);
        assert_eq!(settings.providers_draft.default_provider, default_provider);
        assert_eq!(settings.sessions_count, sessions_count);

        // 改草稿后 Ctrl+S：只保留草稿并提示延后，不落盘、不显示假成功。
        let settings = screen.settings.as_mut().unwrap();
        settings.config_draft.agent.max_steps = 777;
        settings.dirty = true;
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut no_runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert!(screen.status.contains("回合结束后"), "{}", screen.status);
        assert_eq!(screen.panel, Some(Panel::Settings));
        let settings = screen.settings.as_ref().unwrap();
        assert_eq!(settings.config_draft.agent.max_steps, 777);
        assert!(settings.dirty, "未落盘时 dirty 必须保留");
        // 真实配置文件不存在（busy 下不得写盘）。
        let _ = std::fs::remove_dir_all(cwd);
    }

    /// busy 下需要 runner 的设置操作（编辑表单 / CTF 开关）必须提示延后执行。
    #[tokio::test]
    async fn busy_settings_runner_only_actions_report_turn_in_progress() {
        let owner = crate::headless::tests::test_runner().await;
        let cwd = owner.cwd.clone();
        let mut screen = CliScreen::new(&owner);
        screen.busy = true;
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _rx) = mpsc::unbounded_channel();
        let (tx, _rx2) = oneshot::channel();
        let mut cancel = Some(tx);
        let mut active = None;
        let mut no_runner: Option<SessionRunner> = None;

        screen.input = composer();
        screen.insert_text("/settings");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut no_runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert!(screen.settings.is_some());

        // ToolsMcp 第 2 行（CTF 开关）需要 runner：Enter 进入编辑态，Space 才落到开关分支。
        {
            let settings = screen.settings.as_mut().unwrap();
            settings.tab = SettingsTab::ToolsMcp;
            settings.selected_row = 1;
        }
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut no_runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert!(screen.settings.as_ref().unwrap().editing);
        assert!(!screen.status.contains("回合结束后"));
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut no_runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert!(screen.status.contains("回合结束后"), "{}", screen.status);
        assert!(screen.settings.is_some());
        assert!(!screen.ctf_enabled, "busy 下不得改动 CTF 开关");

        // Providers 页 `E`（编辑表单）需要 runner。
        {
            let settings = screen.settings.as_mut().unwrap();
            settings.tab = SettingsTab::Providers;
            settings.selected_row = 0;
        }
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut no_runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert!(screen.status.contains("回合结束后"), "{}", screen.status);
        assert!(screen.form.is_none(), "busy 下不得打开 providers 编辑表单");

        let _ = std::fs::remove_dir_all(cwd);
    }

    /// busy 期间在面板里选中（会话选择器 Enter / `n`）：等价于在输入框敲同一指令入队，
    /// 不得静默丢弃。
    #[tokio::test]
    async fn busy_picker_selection_queues_instead_of_dropping() {
        let owner = crate::headless::tests::test_runner().await;
        let cwd = owner.cwd.clone();
        let mut screen = CliScreen::new(&owner);
        screen.busy = true;
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _rx) = mpsc::unbounded_channel();
        let (tx, _rx2) = oneshot::channel();
        let mut cancel = Some(tx);
        let mut active = None;
        let mut no_runner: Option<SessionRunner> = None;

        // `/sessions` 在 busy 下立即打开会话选择器（只读面板）。
        screen.input = composer();
        screen.insert_text("/sessions");
        assert!(!handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut no_runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap());
        let selected = screen.picker.as_ref().expect("会话选择器已打开").items[0]
            .command
            .clone();
        assert!(selected.starts_with("/sessions "), "{selected}");

        // Enter 选中：入队并回执，面板关闭。
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut no_runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert!(screen.picker.is_none());
        assert_eq!(screen.queued_prompts.len(), 1);
        assert_eq!(screen.queued_prompts[0].kind, QueuedKind::Command);
        assert_eq!(screen.queued_prompts[0].text, selected);
        assert!(history(&screen).contains("已排队"), "{}", history(&screen));

        // `n`（新建会话）同样入队。
        screen.input = composer();
        screen.insert_text("/sessions");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut no_runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut no_runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(screen.queued_prompts.len(), 2);
        assert_eq!(screen.queued_prompts[1].text, "/new");
        assert!(active.is_none());
        assert!(cancel.is_some());

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn drain_queued_inputs_runs_commands_in_order_and_stops_at_prompt() {
        let owner = crate::headless::tests::test_runner().await;
        let cwd = owner.cwd.clone();
        let mut screen = CliScreen::new(&owner);
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _rx) = mpsc::unbounded_channel();
        let mut active = None;
        let mut cancel = None;
        let mut runner = Some(owner);

        let queue_command = |screen: &mut CliScreen, text: &str| {
            screen.queued_prompts.push_back(QueuedPrompt {
                text: text.to_string(),
                displayed: true,
                kind: QueuedKind::Command,
            });
        };

        // 空队列：无操作。
        assert!(!drain_queued_inputs(
            &mut screen,
            &mut runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap());
        assert!(active.is_none());
        assert!(!screen.busy);

        // 两条指令按输入顺序执行，均不开启新回合。
        queue_command(&mut screen, "/mode manual");
        queue_command(&mut screen, "/todo close");
        assert!(!drain_queued_inputs(
            &mut screen,
            &mut runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap());
        assert_eq!(screen.permission_mode, PermissionMode::Manual);
        assert_eq!(screen.todo_view, TodoView::Collapsed);
        assert!(screen.queued_prompts.is_empty());
        assert!(active.is_none());

        // 出错（未知指令）不阻断后续队列。
        queue_command(&mut screen, "/nosuch");
        queue_command(&mut screen, "/todo open");
        assert!(!drain_queued_inputs(
            &mut screen,
            &mut runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap());
        let text = render(&mut screen, 120, 24);
        assert!(text.contains("Unknown command; use /help"), "{text}");
        assert_eq!(screen.todo_view, TodoView::Summary);
        assert!(screen.queued_prompts.is_empty());

        // `/quit` 指令要求退出。
        queue_command(&mut screen, "/quit");
        assert!(drain_queued_inputs(
            &mut screen,
            &mut runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap());
        assert!(screen.queued_prompts.is_empty());

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn forms_mask_secrets_route_paste_and_navigate_without_chat_leaks() {
        let mut owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let action = cli_commands::execute(&mut owner, "/provider add").unwrap();
        let CliAction::Form(mut form) = action else {
            panic!("expected provider form");
        };
        let secret = form.fields.iter().position(|field| field.secret).unwrap();
        form.fields[secret].value = "private-test-key".into();
        screen.form = Some(FormState::new(form));
        screen.form.as_mut().unwrap().selected = secret;
        screen.insert_text("-pasted");
        let mut runner = Some(owner);
        for (width, height) in [(100, 30), (40, 16), (10, 5), (1, 1), (0, 0)] {
            let snapshot = render(&mut screen, width, height);
            assert!(!snapshot.contains("private-test-key"));
            assert!(!snapshot.contains("-pasted"));
            if width == 100 {
                assert!(snapshot.contains("****************"));
            }
        }
        assert!(screen.input.is_empty());
        assert!(screen.messages.is_empty());
        input_key(&mut screen, &mut runner, KeyCode::Tab, KeyModifiers::NONE);
        input_key(
            &mut screen,
            &mut runner,
            KeyCode::BackTab,
            KeyModifiers::SHIFT,
        );
        assert_eq!(screen.form.as_ref().unwrap().selected, secret);
        screen.form.as_mut().unwrap().capture();
        assert_eq!(
            screen.form.as_ref().unwrap().form.fields[secret].value,
            "private-test-key-pasted"
        );
        input_key(&mut screen, &mut runner, KeyCode::Esc, KeyModifiers::NONE);
        assert!(screen.form.is_none());
        assert!(!history(&screen).contains("private-test-key"));
        let _ = std::fs::remove_dir_all(runner.unwrap().cwd);
    }

    #[tokio::test]
    async fn memory_rule_form_saves_with_control_s() {
        let mut owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let CliAction::Form(form) = cli_commands::execute(&mut owner, "/memory rule add").unwrap()
        else {
            panic!("expected memory form");
        };
        screen.form = Some(FormState::new(form));
        let initial_rules = owner.ctx.config.memory.rules.len();
        let state = screen.form.as_mut().unwrap();
        for (field, input) in state.form.fields.iter().zip(&mut state.inputs) {
            if field.name == "prompt" {
                *input = composer();
                input.insert_str("Use focused changes");
            }
        }
        let mut runner = Some(owner);
        input_key(
            &mut screen,
            &mut runner,
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        );
        assert!(screen.form.is_none(), "{}", screen.status);
        assert_eq!(
            runner.as_ref().unwrap().ctx.config.memory.rules.len(),
            initial_rules + 1
        );
        assert!(screen.messages.is_empty());
        let _ = std::fs::remove_dir_all(runner.unwrap().cwd);
    }

    #[tokio::test]
    async fn session_picker_requires_two_delete_keys_and_resets_confirmation_on_navigation() {
        let mut owner = crate::headless::tests::test_runner().await;
        owner.create_session().unwrap();
        owner.create_session().unwrap();
        let mut screen = CliScreen::new(&owner);
        let CliAction::Picker(picker) = cli_commands::execute(&mut owner, "/sessions").unwrap()
        else {
            panic!("expected picker");
        };
        screen.picker = Some(picker);
        let mut runner = Some(owner);
        let count = runner.as_ref().unwrap().index.sessions.len();
        input_key(
            &mut screen,
            &mut runner,
            KeyCode::Char('d'),
            KeyModifiers::NONE,
        );
        assert!(screen.delete_pending.is_some());
        assert_eq!(runner.as_ref().unwrap().index.sessions.len(), count);
        // CJK 宽字符在缓冲区中占两格（第二格为空符号），需先剥离空格再断言。
        let rendered = render(&mut screen, 100, 25).replace(' ', "");
        assert!(rendered.contains("再按一次确认删除"), "{rendered}");
        input_key(&mut screen, &mut runner, KeyCode::Down, KeyModifiers::NONE);
        assert!(screen.delete_pending.is_none());
        input_key(
            &mut screen,
            &mut runner,
            KeyCode::Char('d'),
            KeyModifiers::NONE,
        );
        input_key(
            &mut screen,
            &mut runner,
            KeyCode::Char('d'),
            KeyModifiers::NONE,
        );
        assert_eq!(runner.as_ref().unwrap().index.sessions.len(), count - 1);
        let _ = std::fs::remove_dir_all(runner.unwrap().cwd);
    }

    #[tokio::test]
    async fn session_picker_displays_session_title_in_rendered_popup() {
        let mut owner = crate::headless::tests::test_runner().await;
        owner
            .entries
            .push(ChatEntry::User("webftp_exploit_plan".into()));
        owner.save().unwrap();
        let cur_title = owner.index.current_meta().unwrap().title.clone();
        assert_eq!(cur_title, "webftp_exploit_plan");

        let mut screen = CliScreen::new(&owner);
        let CliAction::Picker(picker) = cli_commands::execute(&mut owner, "/session").unwrap()
        else {
            panic!("expected picker for /session alias");
        };
        assert!(picker
            .items
            .iter()
            .any(|item| item.label == "webftp_exploit_plan"));
        screen.picker = Some(picker);
        let rendered = render(&mut screen, 100, 25);
        assert!(
            rendered.contains("webftp_exploit_plan"),
            "Picker 面板应展示会话标题: {rendered}"
        );
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn model_picker_executes_selected_engine_command_without_chat_submission() {
        let mut owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let CliAction::Picker(picker) = cli_commands::execute(&mut owner, "/model").unwrap() else {
            panic!("expected models");
        };
        assert!(!picker.items.is_empty());
        let expected = picker.items[0].command.clone();
        screen.picker = Some(picker);
        let mut runner = Some(owner);
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);
        let words = expected.split_whitespace().collect::<Vec<_>>();
        assert_eq!(screen.provider, words[1]);
        assert_eq!(screen.model, words[2]);
        assert!(screen.picker.is_none());
        assert!(runner.as_ref().unwrap().entries.is_empty());
        assert!(!screen.busy);
        let _ = std::fs::remove_dir_all(runner.unwrap().cwd);
    }

    #[tokio::test]
    async fn tool_details_toggle_live_and_reopened_history_without_losing_stream_tail() {
        let mut owner = crate::headless::tests::test_runner().await;
        owner.entries = vec![
            ChatEntry::ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: "{\n  file: real.rs\n}".into(),
            },
            ChatEntry::ToolResult {
                id: "1".into(),
                name: "read".into(),
                output: "first line\npreview two\npreview three\npreview four\nactual second line"
                    .into(),
                is_error: false,
            },
        ];
        let mut screen = CliScreen::new(&owner);
        let collapsed = screen.messages.clone();
        assert!(!history(&screen).contains("actual second line"));
        page_key(&mut screen, KeyCode::Char('x'));
        screen.toggle_tools();
        assert!(history(&screen).contains("actual second line"));
        screen.response_text(false, "stream tail");
        screen.toggle_tools();
        screen.response_text(false, " continued");
        assert!(history(&screen).ends_with("stream tail continued"));
        screen.sync(&owner);
        assert_eq!(screen.messages, collapsed);
        screen.toggle_tools();
        screen.sync(&owner);
        assert!(history(&screen).contains("actual second line"));
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn todo_cli_commands_and_footer_progress_and_persistence() {
        let mut owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let orig_id = owner.index.current.clone();

        // 1. Initial state has no todo
        assert!(screen.todo_summary().is_empty());

        // 2. Add two tasks via /todo add
        let act1 = cli_commands::execute(&mut owner, "/todo add 第一阶段信息收集").unwrap();
        let CliAction::Output { text, .. } = act1 else {
            panic!("expected output");
        };
        assert!(text.contains("#1"));

        let act2 = cli_commands::execute(&mut owner, "/todo add 第二阶段漏洞挖掘").unwrap();
        let CliAction::Output { text, .. } = act2 else {
            panic!("expected output");
        };
        assert!(text.contains("#2"));

        // 3. /todo list
        let list_act = cli_commands::execute(&mut owner, "/todo list").unwrap();
        let CliAction::Output { text, .. } = list_act else {
            panic!("expected output");
        };
        assert!(text.contains("#1 第一阶段信息收集"));
        assert!(text.contains("#2 第二阶段漏洞挖掘"));

        // 4. Verify footer summary
        assert_eq!(screen.todo_summary(), "Todo: [0/2] (▶ 第一阶段信息收集)");
        assert!(screen.footer().contains("Todo: [0/2] (▶ 第一阶段信息收集)"));

        // 5. Complete task 1
        let done_act = cli_commands::execute(&mut owner, "/todo done 1").unwrap();
        let CliAction::Output { text, .. } = done_act else {
            panic!("expected output");
        };
        assert!(text.contains("已标记为完成"));

        assert_eq!(screen.todo_summary(), "Todo: [1/2] (▶ 第二阶段漏洞挖掘)");

        // 6. Test session isolation: new session resets todos
        owner.create_session().unwrap();
        screen.sync(&owner);
        assert!(owner.todos().is_empty());
        assert!(screen.todo_summary().is_empty());

        // 7. Switching back restores the todos
        owner.select_session(&orig_id).unwrap();
        screen.sync(&owner);
        assert_eq!(owner.todos().len(), 2);
        assert_eq!(screen.todo_summary(), "Todo: [1/2] (▶ 第二阶段漏洞挖掘)");

        // 8. /todo clear
        let clear_act = cli_commands::execute(&mut owner, "/todo clear").unwrap();
        let CliAction::Output { text, .. } = clear_act else {
            panic!("expected output");
        };
        assert!(text.contains("已清空"));
        assert!(owner.todos().is_empty());
        assert!(screen.todo_summary().is_empty());

        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn todo_pinned_panel_renders_above_composer_and_toggles_visibility() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let cwd = owner.cwd.clone();
        let mut runner = Some(owner);

        // Initially empty -> no todo table
        let initial_snapshot = render(&mut screen, 100, 30);
        assert!(!initial_snapshot.contains("todo close"));

        // Add task 1
        screen.insert_text("/todo add 基础资产与入口信息收集");
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);

        // Add task 2
        screen.insert_text("/todo add Web接口漏洞探测");
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);

        // Pinned panel should now render above composer
        let snapshot_open = render(&mut screen, 100, 30);
        assert!(
            snapshot_open.contains("todo close"),
            "Pinned panel should show: {snapshot_open}"
        );
        assert!(
            snapshot_open.contains("[0/2]"),
            "Pinned panel should show progress [0/2]"
        );
        assert!(
            snapshot_open.contains("#1"),
            "Pinned panel should show task 1"
        );
        assert!(
            snapshot_open.contains("#2"),
            "Pinned panel should show task 2"
        );

        // Close panel via /todo close
        screen.insert_text("/todo close");
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.todo_view, TodoView::Collapsed);

        let snapshot_closed = render(&mut screen, 100, 30);
        assert!(
            !flat(&snapshot_closed).contains("输入/todoclose收起"),
            "Collapsed view should not draw the table title: {snapshot_closed}"
        );
        assert!(
            flat(&snapshot_closed).contains("📋任务清单[0/2]"),
            "Collapsed view should keep the 1-line progress strip: {snapshot_closed}"
        );
        assert!(
            flat(&snapshot_closed).contains("[Alt+↑]展开"),
            "Collapsed strip should advertise Alt+↑: {snapshot_closed}"
        );
        assert!(
            snapshot_closed.contains("todo open"),
            "Footer should remind how to reopen: {snapshot_closed}"
        );

        // Reopen panel via /todo open
        screen.insert_text("/todo open");
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.todo_view, TodoView::Summary);

        let snapshot_reopened = render(&mut screen, 100, 30);
        assert!(
            flat(&snapshot_reopened).contains("输入/todoclose收起"),
            "Reopened panel should show again: {snapshot_reopened}"
        );
        assert!(
            flat(&snapshot_reopened).contains("[Alt+↑]全展"),
            "Summary title should advertise full view: {snapshot_reopened}"
        );

        // Clear tasks via /todo clear
        screen.insert_text("/todo clear");
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);

        let snapshot_cleared = render(&mut screen, 100, 30);
        assert!(
            !snapshot_cleared.contains("📋"),
            "Cleared list should remove both table and strip: {snapshot_cleared}"
        );

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn todo_view_keys_cycle_three_states_with_alt_arrows() {
        let owner = crate::headless::tests::test_runner().await;
        let cwd = owner.cwd.clone();
        let mut screen = CliScreen::new(&owner);
        let mut runner = Some(owner);

        // 8 条任务：100x30 下精简态内区 5 行 → 可见 4 条 + 折叠提示。
        for i in 1..=8 {
            screen.insert_text(&format!("/todo add 任务 {i}"));
            input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);
        }
        assert_eq!(screen.todo_view, TodoView::Summary);

        let summary = render(&mut screen, 100, 30);
        assert!(summary.contains("[ ] #4"), "精简态应可见前 4 条: {summary}");
        assert!(
            !summary.contains("[ ] #8"),
            "精简态不应绘制第 8 条: {summary}"
        );
        assert!(flat(&summary).contains("[Alt+↑]全展"), "{summary}");

        // Alt+↑ 第二档 → 全量（含「全部 N 项」标题），再按封顶不翻转。
        assert!(!input_key(
            &mut screen,
            &mut runner,
            KeyCode::Up,
            KeyModifiers::ALT
        ));
        assert_eq!(screen.todo_view, TodoView::Full);
        let full = render(&mut screen, 100, 30);
        assert!(full.contains("[ ] #8"), "全量态应绘制全部: {full}");
        assert!(flat(&full).contains("全部8项"), "{full}");
        input_key(&mut screen, &mut runner, KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(screen.todo_view, TodoView::Full, "到顶不翻转");

        // Alt+↓ → 精简 → 收起（保留 1 行进度条），再按到底不翻转。
        input_key(&mut screen, &mut runner, KeyCode::Down, KeyModifiers::ALT);
        assert_eq!(screen.todo_view, TodoView::Summary);
        assert!(!render(&mut screen, 100, 30).contains("[ ] #8"));

        input_key(&mut screen, &mut runner, KeyCode::Down, KeyModifiers::ALT);
        assert_eq!(screen.todo_view, TodoView::Collapsed);
        let collapsed = render(&mut screen, 100, 30);
        assert!(flat(&collapsed).contains("📋任务清单[0/8]"), "{collapsed}");
        assert!(flat(&collapsed).contains("[Alt+↑]展开"), "{collapsed}");
        assert!(
            !flat(&collapsed).contains("输入/todoclose收起"),
            "收起态不画表格标题: {collapsed}"
        );
        input_key(&mut screen, &mut runner, KeyCode::Down, KeyModifiers::ALT);
        assert_eq!(screen.todo_view, TodoView::Collapsed, "到底不翻转");

        // 面板态由更早的分支接管：Alt+↑ 不改视图、不写状态提示。
        screen.panel = Some(Panel::Shortcuts);
        let status_before = screen.status.clone();
        input_key(&mut screen, &mut runner, KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(screen.todo_view, TodoView::Collapsed);
        assert_eq!(screen.status, status_before);

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn cli_screen_retry_clears_stream_and_highlights_status() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.event(AgentEvent::Token("半截流式内容".into()));
        assert!(
            !screen.messages.is_empty(),
            "Token 应在 messages 中有未定稿片段"
        );
        assert!(screen.response_start.is_some());

        screen.event(AgentEvent::Retry {
            attempt: 1,
            max_retries: 5,
            delay_secs: 3,
            error: "stream: connection reset".into(),
        });

        assert!(
            screen.messages.is_empty(),
            "Retry 事件应当清空未定稿的流式片段"
        );
        assert!(screen.response_start.is_none());
        assert!(screen.stream.is_empty());
        assert!(screen.status.contains("正在重试"));
        assert!(screen.status.contains("1/5"));
        assert_eq!(screen.footer_status_style().fg, Some(AMBER));
    }

    #[tokio::test]
    async fn major_component_snapshots_preserve_palette_and_narrow_layouts() {
        let mut owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.message("You", "Inspect the parser", ACCENT);
        screen.response_text(true, "Check boundaries first");
        screen.response_text(false, "**Result**\n```rust\nlet safe = true;\n```");
        screen.tool_call("read-1", "read_file", r#"{"path":"parser.rs"}"#);
        screen.tool_result("read-1", "read_file", "actual contents", false);
        screen.tool_call("shell-1", "shell", r#"{"command":"false"}"#);
        screen.tool_result("shell-1", "shell", "permission denied", true);
        screen.turn_summary(2000, 1_700_000_000, "done", Some("任务完成"));
        let snapshot = render(&mut screen, 110, 35);
        for marker in [
            "Cyber Master V",
            "Inspect the parser",
            "Thinking",
            "Check boundaries first",
            "Result",
            "let safe = true;",
            "permission denied",
            "Worked for 2s",
            "ctx --",
        ] {
            assert!(snapshot.contains(marker), "missing {marker}: {snapshot}");
        }
        screen.insert_text("/pro");
        screen.update_completions(Some(&owner));
        let snapshot = render(&mut screen, 110, 35);
        assert!(snapshot.contains("/provider"));
        assert!(!snapshot.contains("Commands · Tab complete"));
        assert!(render(&mut screen, 30, 12).contains("/provider"));
        screen.panel = Some(Panel::Shortcuts);
        assert!(render(&mut screen, 110, 40).contains("Ctrl+O"));
        screen.panel = None;
        let CliAction::Picker(picker) = cli_commands::execute(&mut owner, "/model").unwrap() else {
            panic!("expected models");
        };
        screen.picker = Some(picker);
        assert!(render(&mut screen, 110, 35).contains("Models"));
        for (width, height) in [(30, 12), (10, 5), (1, 1), (0, 0)] {
            render(&mut screen, width, height);
        }
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    fn history(screen: &CliScreen) -> String {
        screen
            .messages
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn empty_assistants_and_interleaved_reasoning_share_response_heading() {
        let mut runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        screen.event(AgentEvent::Token(String::new()));
        assert!(screen.messages.is_empty());
        let events = [
            AgentEvent::Reasoning("actual thought".into()),
            AgentEvent::Token(String::new()),
            AgentEvent::Reasoning(" continued".into()),
            AgentEvent::Token("same answer".into()),
            AgentEvent::Reasoning("another thought".into()),
            AgentEvent::Token("same answer".into()),
        ];
        for event in events {
            match &event {
                AgentEvent::Token(text) => {
                    crate::chat::append_stream_text(&mut runner.entries, text, false)
                }
                AgentEvent::Reasoning(text) => {
                    crate::chat::append_stream_text(&mut runner.entries, text, true)
                }
                _ => unreachable!(),
            }
            screen.event(event);
        }
        let live = screen.messages.clone();
        screen.sync(&runner);
        assert_eq!(screen.messages, live);
        assert_eq!(
            history(&screen)
                .lines()
                .filter(|line| *line == "Thinking")
                .count(),
            1
        );
        assert_eq!(
            history(&screen)
                .lines()
                .filter(|line| *line == "Cyber")
                .count(),
            1
        );
        assert_eq!(history(&screen).matches("same answer").count(), 2);
        assert!(history(&screen).contains("Thinking\nactual thought continued"));
        assert!(!history(&screen).contains("Worked for"));
        runner.entries = vec![
            ChatEntry::Assistant(String::new()),
            ChatEntry::Thinking("actual thought".into()),
            ChatEntry::Assistant(String::new()),
            ChatEntry::Thinking(" continued".into()),
            ChatEntry::Assistant(String::new()),
            ChatEntry::Assistant("same answer".into()),
            ChatEntry::Thinking("another thought".into()),
            ChatEntry::Assistant("same answer".into()),
            ChatEntry::Assistant(" \n".into()),
        ];
        screen.sync(&runner);
        assert_eq!(screen.messages, live);
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn tool_boundaries_have_one_heading_per_response_group() {
        let mut runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        screen.event(AgentEvent::Reasoning("inspect".into()));
        screen.event(AgentEvent::Token("before".into()));
        screen.event(AgentEvent::ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: "{}".into(),
        });
        screen.event(AgentEvent::ToolResult {
            id: "1".into(),
            name: "read".into(),
            output: "result".into(),
            is_error: false,
        });
        screen.event(AgentEvent::Reasoning("consider".into()));
        screen.event(AgentEvent::Token("after".into()));
        let live = screen.messages.clone();
        assert_eq!(
            history(&screen)
                .lines()
                .filter(|line| *line == "Cyber")
                .count(),
            1
        );
        runner.entries = vec![
            ChatEntry::Thinking("inspect".into()),
            ChatEntry::Assistant("before".into()),
            ChatEntry::Assistant(String::new()),
            ChatEntry::ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
            ChatEntry::ToolResult {
                id: "1".into(),
                name: "read".into(),
                output: "result".into(),
                is_error: false,
            },
            ChatEntry::Assistant(String::new()),
            ChatEntry::Thinking("consider".into()),
            ChatEntry::Assistant("after".into()),
        ];
        screen.sync(&runner);
        assert_eq!(screen.messages, live);
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn markdown_streaming_and_reopened_history_preserve_styles_and_cache() {
        let mut runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        screen.message("You", "stable history", ACCENT);
        screen.history_view.update(&screen.messages, 80);
        let prefix = screen.history_view.rows[2].spans[0].content.as_ptr();
        let markdown = "  **bold**\n- item\n> quote\n```rust\nlet x = 1;\n```";
        for (index, token) in [
            "  ",
            "**bo",
            "ld**\n- item\n> quote\n```rust\nlet x = 1;",
            "\n```",
        ]
        .into_iter()
        .enumerate()
        {
            screen.event(AgentEvent::Token(token.into()));
            if index == 0 {
                screen.event(AgentEvent::Reasoning("provider reasoning".into()));
            }
            screen.history_view.update(&screen.messages, 80);
            assert_eq!(
                screen.history_view.rows[2].spans[0].content.as_ptr(),
                prefix
            );
        }
        let live = screen.messages.clone();
        assert!(
            live.iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content == "bold"
                    && span.style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(history(&screen).contains("- item\n│ quote\n│ let x = 1;"));
        runner.entries = vec![
            ChatEntry::User("stable history".into()),
            ChatEntry::Thinking("provider reasoning".into()),
            ChatEntry::Assistant(markdown.into()),
        ];
        screen.sync(&runner);
        assert_eq!(screen.messages, live);
        let reopened = CliScreen::new(&runner);
        assert_eq!(screen.messages, reopened.messages);
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn summaries_render_persisted_status_and_local_time_only() {
        let mut runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        screen.event(AgentEvent::Done);
        assert!(!history(&screen).contains("Worked for"));
        let finished_at = 1_700_000_000;
        let local = chrono::DateTime::from_timestamp(finished_at as i64, 0)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string();
        for status in ["done", "cancelled", "error"] {
            runner.entries = vec![ChatEntry::TurnSummary {
                elapsed_ms: 3_999,
                finished_at,
                status: status.into(),
                summary: None,
            }];
            screen.sync(&runner);
            assert_eq!(
                screen.messages.last().unwrap().to_string(),
                format!("Worked for 3s · {status} {local}")
            );
            assert_eq!(screen.messages.last().unwrap().style.fg, Some(DIM));
            assert_eq!(screen.messages, CliScreen::new(&runner).messages);
        }
        screen.turn_summary(0, u64::MAX, "error", None);
        assert!(history(&screen).ends_with("error --:--"));
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn usage_is_reported_independently_of_cache_and_survives_same_session_sync() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        screen.has_run = true;
        assert!(screen.footer().contains("ctx -- │ cache -- │ ↑-- ↓--"));
        screen.event(AgentEvent::Usage(cyber_agent::Usage {
            prompt_tokens: 51_100,
            completion_tokens: 255,
            ..Default::default()
        }));
        assert!(screen.footer().contains("cache -- │ ↑51.1k ↓255"));
        screen.event(AgentEvent::Usage(cyber_agent::Usage {
            cache_hit_tokens: 1,
            cache_miss_tokens: 99,
            ..Default::default()
        }));
        screen.event(AgentEvent::ContextUpdate {
            used_tokens: 1,
            effective_context_length: Some(100),
        });
        assert!(screen
            .footer()
            .contains("ctx 99% │ cache 1.0% │ ↑51.1k ↓255"));
        let footer = screen.footer();
        screen.sync(&runner);
        assert_eq!(screen.footer(), footer);
        assert!(CliScreen::new(&runner).footer().is_empty());
        for capacity in [None, Some(0), Some(100)] {
            screen.event(AgentEvent::ContextUpdate {
                used_tokens: usize::MAX,
                effective_context_length: capacity,
            });
            assert!(screen.footer().contains(if capacity == Some(100) {
                "ctx 0%"
            } else {
                "ctx --"
            }));
        }
        screen.reset_usage();
        for _ in 0..2 {
            screen.event(AgentEvent::Usage(cyber_agent::Usage {
                prompt_tokens: u64::MAX,
                completion_tokens: u64::MAX,
                cache_hit_tokens: u64::MAX,
                cache_miss_tokens: u64::MAX,
            }));
        }
        assert_eq!(screen.prompt_tokens, u128::from(u64::MAX) * 2);
        assert_eq!(screen.completion_tokens, u128::from(u64::MAX) * 2);
        assert!(screen.footer().contains("cache 50.0%"));
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn new_and_selected_sessions_reset_usage_but_model_changes_do_not() {
        let runner = crate::headless::tests::test_runner().await;
        let original_session = runner.index.current.clone();
        let mut screen = CliScreen::new(&runner);
        let mut runner = Some(runner);
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _rx) = mpsc::unbounded_channel();
        screen.event(AgentEvent::Usage(cyber_agent::Usage {
            prompt_tokens: 123,
            ..Default::default()
        }));
        let provider = runner
            .as_ref()
            .unwrap()
            .ctx
            .config
            .agent
            .default_provider
            .clone();
        let configured = runner
            .as_mut()
            .unwrap()
            .ctx
            .providers
            .providers
            .get_mut(&provider)
            .unwrap();
        configured.model = "changed-model".into();
        configured.models.insert(
            "changed-model".into(),
            cyber_core::ModelConfig {
                context_length: Some(1000),
                ..Default::default()
            },
        );
        runner.as_mut().unwrap().entries = vec![ChatEntry::User("x".repeat(400))];
        screen.sync(runner.as_ref().unwrap());
        assert_eq!(screen.model, "changed-model");
        assert_eq!(screen.context_length, Some(1000));
        assert_eq!(screen.prompt_tokens, 123);
        assert!(screen.used_tokens > 0);
        screen.input.insert_str("/new");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.used_tokens, 0);
        assert!(!screen.usage_reported);
        assert!(screen.messages.is_empty());
        screen.event(AgentEvent::Usage(cyber_agent::Usage {
            prompt_tokens: 321,
            cache_hit_tokens: 3,
            cache_miss_tokens: 7,
            ..Default::default()
        }));
        screen
            .input
            .insert_str(format!("/sessions {original_session}"));
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.session, original_session);
        assert!(screen.footer().contains("cache -- │ ↑-- ↓--"));
        assert_eq!(screen.cache_hit_tokens, 0);
        assert_eq!(screen.cache_miss_tokens, 0);
        let _ = std::fs::remove_dir_all(&runner.unwrap().cwd);
    }

    fn render(screen: &mut CliScreen, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 去空白后的渲染文本：宽字符在缓冲区内占两格（第二格为空白），直接 `contains`
    /// 会因格子间的空白失败，故 CJK 断言一律先压掉空白。
    fn flat(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// `/update` 安装脚本调用记录：`(version, wait_for_exit)`。
    type UpdateCalls = Arc<std::sync::Mutex<Vec<(String, bool)>>>;

    /// `/update` 状态机夹具：安装脚本启动器替换为只记录参数、不落盘的假实现。
    fn update_test_screen(owner: &SessionRunner) -> (CliScreen, UpdateCalls) {
        let mut screen = CliScreen::new(owner);
        let calls = Arc::new(std::sync::Mutex::new(Vec::<(String, bool)>::new()));
        let recorder = Arc::clone(&calls);
        screen.update_launcher = Arc::new(
            move |version: &str, wait: bool| -> std::io::Result<std::path::PathBuf> {
                recorder
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((version.to_string(), wait));
                Ok(std::path::PathBuf::from("C:/tmp/update.ps1"))
            },
        );
        (screen, calls)
    }

    fn release_info(version: &str) -> cyber_core::update::ReleaseInfo {
        cyber_core::update::ReleaseInfo {
            version: version.to_string(),
            html_url: format!("https://example.invalid/v{version}"),
            release_notes: Some("notes".into()),
            published_at: Some("2026-01-01T00:00:00Z".into()),
        }
    }

    fn screen_messages(screen: &CliScreen) -> String {
        flat(
            &screen
                .messages
                .iter()
                .map(|line| line.to_string())
                .collect::<String>(),
        )
    }

    #[tokio::test]
    async fn update_check_reports_the_new_version_without_prompting() {
        let owner = crate::headless::tests::test_runner().await;
        let (mut screen, calls) = update_test_screen(&owner);
        screen.deliver_update(UpdateCheckResult {
            kind: cli_commands::CliUpdate::Check,
            info: Some(release_info("9.9.9")),
        });
        assert_eq!(screen.new_version.as_deref(), Some("9.9.9"));
        assert!(screen.update_pending.is_none());
        assert!(
            calls.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "/update check 不得启动安装"
        );
        assert!(screen_messages(&screen).contains("9.9.9"));
        assert!(screen.status.contains("9.9.9"), "{}", screen.status);
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn update_prompt_installs_on_y_and_cancels_otherwise() {
        let owner = crate::headless::tests::test_runner().await;
        let (mut screen, calls) = update_test_screen(&owner);
        let mut runner = None;

        screen.deliver_update(UpdateCheckResult {
            kind: cli_commands::CliUpdate::Prompt,
            info: Some(release_info("9.9.9")),
        });
        assert!(screen.update_pending.is_some(), "无参数检查必须进入确认态");
        assert!(screen.status.contains("按 y"), "{}", screen.status);

        input_key(
            &mut screen,
            &mut runner,
            KeyCode::Char('y'),
            KeyModifiers::NONE,
        );
        let expected_wait = cyber_core::update::needs_exit_before_install();
        assert_eq!(
            calls.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            vec![("9.9.9".to_string(), expected_wait)]
        );
        assert!(screen.update_pending.is_none());
        let text = screen_messages(&screen);
        if expected_wait {
            assert!(text.contains("更新已安排"), "{text}");
        } else {
            assert!(text.contains("已开始后台安装"), "{text}");
        }
        assert!(
            screen.input.lines().join("").is_empty(),
            "确认键不得进入输入框"
        );

        // 确认态下其它键只取消，不启动安装脚本。
        screen.deliver_update(UpdateCheckResult {
            kind: cli_commands::CliUpdate::Prompt,
            info: Some(release_info("9.9.9")),
        });
        assert!(screen.update_pending.is_some());
        assert!(!input_key(
            &mut screen,
            &mut runner,
            KeyCode::Char('x'),
            KeyModifiers::NONE
        ));
        assert!(screen.update_pending.is_none());
        assert_eq!(calls.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
        assert!(screen.status.contains("已取消更新"), "{}", screen.status);
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn update_refuses_to_install_while_a_turn_is_running() {
        let owner = crate::headless::tests::test_runner().await;
        let (mut screen, calls) = update_test_screen(&owner);
        let mut runner = None;
        screen.busy = true;
        screen.update_pending = Some(release_info("9.9.9"));
        assert!(!input_key(
            &mut screen,
            &mut runner,
            KeyCode::Char('y'),
            KeyModifiers::NONE
        ));
        assert!(calls.lock().unwrap().is_empty(), "回合进行中不得启动安装");
        assert!(screen.update_pending.is_none());
        assert!(screen_messages(&screen).contains("回合进行中"));
        let _ = std::fs::remove_dir_all(owner.cwd);
    }

    #[tokio::test]
    async fn update_without_network_channel_falls_back_to_the_local_cache() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        assert!(screen.update_check_tx.is_none());
        let mut runner = Some(owner);
        screen.insert_text("/update check");
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!screen.update_checking && screen.update_check_tx.is_none());
        assert!(screen_messages(&screen).contains("更新检查"));
        let _ = std::fs::remove_dir_all(runner.unwrap().cwd);
    }

    /// 打开 `/provider add-with-kind openai-compatible` 的全屏 provider 表单。
    async fn provider_form_screen() -> (CliScreen, Option<SessionRunner>) {
        let mut owner = crate::headless::tests::test_runner().await;
        let action =
            cli_commands::execute(&mut owner, "/provider add-with-kind openai-compatible").unwrap();
        let CliAction::Form(form) = action else {
            panic!("expected provider form");
        };
        let screen = CliScreen::new(&owner);
        let mut screen = screen;
        screen.form = Some(FormState::new(form));
        (screen, Some(owner))
    }

    #[tokio::test]
    async fn form_model_picker_selects_and_writes_back() {
        let (mut screen, mut runner) = provider_form_screen().await;
        let idx = screen
            .form
            .as_ref()
            .unwrap()
            .form
            .fields
            .iter()
            .position(|f| f.name == "model")
            .unwrap();
        screen.form.as_mut().unwrap().selected = idx;

        screen.form.as_mut().unwrap().open_model_picker(
            Ok(vec!["a".into(), "b".into()]),
            None,
            "p",
        );
        let picker = screen.form.as_ref().unwrap().model_picker.as_ref().unwrap();
        assert!(!picker.manual);
        assert_eq!(picker.entries.len(), 2);
        assert!(picker.error.is_none());

        input_key(&mut screen, &mut runner, KeyCode::Down, KeyModifiers::NONE);
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);
        let form = screen.form.as_ref().unwrap();
        assert!(form.model_picker.is_none(), "确认后必须关闭浮层");
        assert_eq!(form.model_input(), "b");
    }

    /// 模型列表统一按名称首字母（不区分大小写）升序：Provider 表单「选择默认模型」浮层、
    /// 设置中心 Providers 的 `M` 模型选择、`/model` 双栏面板右栏。
    #[tokio::test]
    async fn model_lists_sort_alphabetically_by_name() {
        // 1) Provider 表单「选择默认模型」：接口返回顺序不保证有序 → 列表按首字母排序，
        //    光标仍停在当前 model 字段值对应的条目上。
        let (mut screen, runner) = provider_form_screen().await;
        let idx = screen
            .form
            .as_ref()
            .unwrap()
            .form
            .fields
            .iter()
            .position(|f| f.name == "model")
            .unwrap();
        screen.form.as_mut().unwrap().selected = idx;
        screen.form.as_mut().unwrap().set_input("model", "GLM-4.6");
        screen.form.as_mut().unwrap().open_model_picker(
            Ok(vec![
                "o3-mini".into(),
                "deepseek-v3".into(),
                "GLM-4.6".into(),
                "claude-sonnet-4-5".into(),
                "Llama-3".into(),
            ]),
            None,
            "p",
        );
        let picker = screen.form.as_ref().unwrap().model_picker.as_ref().unwrap();
        let ids: Vec<&str> = picker.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "claude-sonnet-4-5",
                "deepseek-v3",
                "GLM-4.6",
                "Llama-3",
                "o3-mini"
            ]
        );
        assert_eq!(picker.entries[picker.selected].id, "GLM-4.6");

        // 浮层按 entries 顺序逐行渲染；把 model 字段换成不在列表中的值，
        // 使列表项在帧中唯一出现，从而校验可见顺序。
        screen.form.as_mut().unwrap().model_picker = None;
        screen
            .form
            .as_mut()
            .unwrap()
            .set_input("model", "zzz-manual");
        screen.form.as_mut().unwrap().open_model_picker(
            Ok(vec!["o3-mini".into(), "GLM-4.6".into(), "claude-3".into()]),
            None,
            "p",
        );
        let rendered = render(&mut screen, 120, 40);
        let pos = |needle: &str| {
            rendered
                .find(needle)
                .unwrap_or_else(|| panic!("缺少 {needle}: {rendered}"))
        };
        assert!(pos("claude-3") < pos("GLM-4.6"), "{rendered}");
        assert!(pos("GLM-4.6") < pos("o3-mini"), "{rendered}");
        if let Some(r) = runner {
            let _ = std::fs::remove_dir_all(r.cwd);
        }

        // 2) 设置中心 Providers 的 `M` 模型选择（配置内模型映射乱序）。
        let mut owner = crate::headless::tests::test_runner().await;
        {
            let p = owner.ctx.providers.providers.get_mut("anthropic").unwrap();
            for id in ["GLM-4.6", "claude-3", "o3"] {
                p.models
                    .insert(id.into(), cyber_core::ModelConfig::default());
            }
            p.model = "GLM-4.6".into();
        }
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('5'),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::Providers
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('m'),
            KeyModifiers::NONE,
        );
        let picker = screen.picker.as_ref().expect("M 应打开模型选择");
        let labels: Vec<&str> = picker.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["claude-3", "GLM-4.6 (当前使用)", "o3"]);

        // 3) `/model` 双栏面板右栏：同一 provider 的模型列表顺序一致。
        let mut state = CliModelPickerState::from_view(&runner_opt.as_ref().unwrap().view());
        state.provider_selected = state
            .providers
            .sorted_names()
            .iter()
            .position(|n| n == "anthropic")
            .unwrap();
        state.refresh_models();
        assert_eq!(state.models, vec!["claude-3", "GLM-4.6", "o3"]);

        if let Some(r) = runner_opt {
            let _ = std::fs::remove_dir_all(r.cwd);
        }
    }

    #[tokio::test]
    async fn form_model_picker_manual_fallback_on_error() {
        let (mut screen, mut runner) = provider_form_screen().await;
        let idx = screen
            .form
            .as_ref()
            .unwrap()
            .form
            .fields
            .iter()
            .position(|f| f.name == "model")
            .unwrap();
        screen.form.as_mut().unwrap().selected = idx;

        screen
            .form
            .as_mut()
            .unwrap()
            .open_model_picker(Err("boom".into()), None, "p");
        assert!(
            screen
                .form
                .as_ref()
                .unwrap()
                .model_picker
                .as_ref()
                .unwrap()
                .manual
        );
        assert_eq!(
            screen
                .form
                .as_ref()
                .unwrap()
                .model_picker
                .as_ref()
                .unwrap()
                .error
                .as_deref(),
            Some("boom")
        );

        // 手输模式：model 字段恢复可编辑（按键不被浮层吞掉）
        input_key(
            &mut screen,
            &mut runner,
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        );
        assert!(
            screen.form.as_ref().unwrap().model_input().contains('x'),
            "手输模式下必须能编辑 model 字段"
        );
    }

    #[tokio::test]
    async fn fullscreen_provider_form_renders_thinking_rows_and_picker() {
        let (mut screen, _runner) = provider_form_screen().await;

        let rendered = render(&mut screen, 120, 40).replace(' ', "");
        assert!(rendered.contains("思考模式thinking.type"), "{rendered}");
        assert!(rendered.contains("思考强度thinking.effort"), "{rendered}");

        screen
            .form
            .as_mut()
            .unwrap()
            .open_model_picker(Ok(vec!["glm-5.3".into()]), None, "p");
        let rendered = render(&mut screen, 120, 40).replace(' ', "");
        assert!(rendered.contains("选择默认模型"), "{rendered}");
        assert!(rendered.contains("◈推理"), "{rendered}");
    }

    #[tokio::test]
    async fn form_model_picker_esc_closes_only_picker_not_form() {
        let (mut screen, mut runner) = provider_form_screen().await;
        let idx = screen
            .form
            .as_ref()
            .unwrap()
            .form
            .fields
            .iter()
            .position(|f| f.name == "model")
            .unwrap();
        screen.form.as_mut().unwrap().selected = idx;
        screen.form.as_mut().unwrap().open_model_picker(
            Ok(vec!["m-a".into(), "m-b".into()]),
            None,
            "p",
        );

        // Esc：只关模型面板，Provider 编辑表单必须保留
        input_key(&mut screen, &mut runner, KeyCode::Esc, KeyModifiers::NONE);
        let form = screen.form.as_ref().expect("Esc 不得关闭 Provider 表单");
        assert!(form.model_picker.is_none(), "Esc 必须关闭模型列表面板");
        assert_eq!(form.model_input(), "", "关闭面板不得改动 model 字段");

        // 面板内 Alt+Enter 不得当作确认（防某些终端把 ESC+CR 解析成 Alt+Enter 时误写 model）
        screen.form.as_mut().unwrap().open_model_picker(
            Ok(vec!["m-a".into(), "m-b".into()]),
            None,
            "p",
        );
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::ALT);
        let form = screen.form.as_ref().expect("Alt+Enter 不得关闭表单");
        assert!(form.model_picker.is_some(), "面板内 Alt+Enter 不应当作确认");
        assert_eq!(form.model_input(), "", "Alt+Enter 不得写入 model 字段");
        // 普通 Enter 仍然是确认
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            screen.form.as_ref().unwrap().model_input(),
            "m-a",
            "普通 Enter 应确认选中项"
        );
        screen.form.as_mut().unwrap().set_model_input("");

        // 手输兜底模式同理：Esc 只退出面板
        screen
            .form
            .as_mut()
            .unwrap()
            .open_model_picker(Err("boom".into()), None, "p");
        assert!(screen.form.as_ref().unwrap().model_picker.is_some());
        input_key(&mut screen, &mut runner, KeyCode::Esc, KeyModifiers::NONE);
        let form = screen.form.as_ref().expect("手输模式下 Esc 也不得关闭表单");
        assert!(form.model_picker.is_none());

        // 面板已关闭时 Esc 仍应关闭表单（原有语义不变）
        input_key(&mut screen, &mut runner, KeyCode::Esc, KeyModifiers::NONE);
        assert!(screen.form.is_none(), "无面板时 Esc 应关闭表单");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn form_model_picker_survives_probe_failure() {
        // 探针失败（不可达端点）后浮层必须保持打开，状态行给出失败原因。
        let (mut screen, mut runner) = provider_form_screen().await;
        screen
            .form
            .as_mut()
            .unwrap()
            .set_input("endpoint", "http://127.0.0.1:1");
        let idx = screen
            .form
            .as_ref()
            .unwrap()
            .form
            .fields
            .iter()
            .position(|f| f.name == "model")
            .unwrap();
        screen.form.as_mut().unwrap().selected = idx;
        screen.form.as_mut().unwrap().open_model_picker(
            Ok(vec!["m-a".into(), "m-b".into()]),
            None,
            "p",
        );

        input_key(
            &mut screen,
            &mut runner,
            KeyCode::Char('t'),
            KeyModifiers::NONE,
        );

        let form = screen.form.as_ref().unwrap();
        assert!(
            form.model_picker.as_ref().is_some_and(|p| !p.manual),
            "探针结束后面板必须保持打开"
        );
        assert_eq!(form.model_picker.as_ref().unwrap().entries.len(), 2);
        assert!(
            screen.status.contains("探针失败"),
            "status = {}",
            screen.status
        );
    }

    fn page_key(screen: &mut CliScreen, code: KeyCode) {
        let (broker, _requests) = PermissionBroker::interactive();
        let (events, _rx) = mpsc::unbounded_channel();
        handle_key(
            screen,
            KeyEvent::new(code, KeyModifiers::NONE),
            &mut None,
            &Arc::new(broker),
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn ctf_mode_renders_persistent_badge_on_footer() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);

        // CTF 未开启时：无 CTF model 标记
        screen.ctf_enabled = false;
        let text_disabled = render(&mut screen, 120, 24);
        assert!(!text_disabled.contains("CTF model"));

        // CTF 开启时：输入框底部常亮展示 CTF model
        screen.ctf_enabled = true;
        let text_enabled = render(&mut screen, 120, 24);
        assert!(text_enabled.contains("CTF model"));

        // 验证位于底部 footer 行且靠右
        let last_line = text_enabled.lines().last().unwrap();
        assert!(last_line.contains("CTF model"));

        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    #[tokio::test]
    async fn long_approval_keeps_controls_visible_and_scrolls_only_arguments() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let (reply, _rx) = oneshot::channel();
        let nonce = "0123456789abcdef0123456789abcdef";
        screen.approval = Some(PermissionRequest {
            tool: "shell".into(),
            arguments: serde_json::json!({"command": format!("{}END-OF-ARGS", "long parameter ".repeat(2000))}),
            nonce: nonce.into(),
            reply,
            confidence: 0.0,
            risk_reason: String::new(),
        });
        for height in [12, 13, 16, 30] {
            let text = render(&mut screen, 80, height);
            assert!(screen.approval_visible, "height {height}: {text}");
            assert!(screen.approval_max_scroll > 0);
            page_key(&mut screen, KeyCode::PageDown);
            assert_eq!(screen.approval_scroll, 10);
            assert_eq!(screen.scroll, 0);
            render(&mut screen, 80, height);
            page_key(&mut screen, KeyCode::PageUp);
            assert_eq!(screen.approval_scroll, 0);
        }
        for _ in 0..1000 {
            page_key(&mut screen, KeyCode::PageDown);
        }
        assert_eq!(screen.approval_scroll, screen.approval_max_scroll);
        render(&mut screen, 120, 40);
        assert!(screen.approval_scroll <= screen.approval_max_scroll);
        page_key(&mut screen, KeyCode::PageUp);
        assert_eq!(
            screen.approval_scroll,
            screen.approval_max_scroll.saturating_sub(10)
        );
        render(&mut screen, 120, 40);
        assert!(screen.approval_scroll <= screen.approval_max_scroll);
        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    #[tokio::test]
    async fn clipped_approval_requires_expansion_and_cannot_confirm() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let (reply, mut rx) = oneshot::channel();
        screen.approval = Some(PermissionRequest {
            tool: "shell".into(),
            arguments: serde_json::json!({"command": "x".repeat(10000)}),
            nonce: "0123456789abcdef0123456789abcdef".into(),
            reply,
            confidence: 0.0,
            risk_reason: String::new(),
        });
        page_key(&mut screen, KeyCode::Char('1'));
        for (width, height) in [(80, 10), (80, 8), (30, 12), (10, 5), (1, 1), (0, 0)] {
            render(&mut screen, width, height);
            assert!(!screen.approval_visible);
            page_key(&mut screen, KeyCode::Enter);
            assert!(screen.approval.is_some());
            assert!(matches!(
                rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
        }
        render(&mut screen, 80, 12);
        assert!(screen.approval_visible);
        page_key(&mut screen, KeyCode::Enter);
        assert_eq!(rx.try_recv().unwrap(), PermissionDecision::AllowOnce);
        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    #[tokio::test]
    async fn history_beyond_u16_shows_latest_and_clamps_scroll_on_resize() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        screen.messages = vec![Line::raw(format!("{}LATEST", "x".repeat(80 * 70000)))];
        assert!(render(&mut screen, 80, 20).contains("LATEST"));
        assert!(screen.max_scroll > u16::MAX as usize);
        screen.scroll = usize::MAX;
        render(&mut screen, 80, 20);
        assert_eq!(screen.scroll, screen.max_scroll);
        page_key(&mut screen, KeyCode::PageUp);
        assert_eq!(screen.scroll, screen.max_scroll);
        render(&mut screen, 160, 40);
        assert_eq!(screen.scroll, screen.max_scroll);
        screen.messages = vec![Line::raw("short history")];
        assert!(render(&mut screen, 80, 20).contains("short history"));
        assert_eq!(screen.scroll, 0);
        assert_eq!(screen.max_scroll, 0);
        page_key(&mut screen, KeyCode::PageUp);
        assert_eq!(screen.scroll, 0);
        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    #[tokio::test]
    async fn test_auto_scroll_locks_content_when_scrolled_up_and_resumes_at_bottom() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        for i in 0..30 {
            screen.messages.push(Line::raw(format!("msg line {i:02}")));
        }
        render(&mut screen, 80, 20);
        assert!(screen.auto_scroll);
        // 用户上滚进入浏览历史状态（触发 auto_scroll = false）
        screen.scroll = 5;
        screen.auto_scroll = false;
        // 在浏览状态下按 Up 键继续上滚 1 行
        page_key(&mut screen, KeyCode::Up);
        assert_eq!(screen.scroll, 6);
        assert!(!screen.auto_scroll);
        // 按 Down 键下滚 1 行恢复到 5
        page_key(&mut screen, KeyCode::Down);
        assert_eq!(screen.scroll, 5);
        assert!(!screen.auto_scroll);
        let start_0 = screen.max_scroll - screen.scroll;
        let rendered_0 = render(&mut screen, 80, 20);

        // 模拟模型流式追加 10 行新内容并渲染
        for i in 30..40 {
            screen.messages.push(Line::raw(format!("msg line {i:02}")));
        }
        let rendered_1 = render(&mut screen, 80, 20);
        let start_1 = screen.max_scroll - screen.scroll;

        // 起始行完全静止，没有被顶上去
        assert_eq!(start_0, start_1);
        // 且正文文本完全一致（忽略最右侧随总行数动态调整的滚动条滑块）
        let clean = |s: &str| -> Vec<String> {
            s.lines()
                .map(|l| l.trim_end_matches(['│', '█', ' ']).to_string())
                .collect()
        };
        assert_eq!(clean(&rendered_0), clean(&rendered_1));
        assert!(!screen.auto_scroll);

        // 用户按多次 Down 回到底部
        for _ in 0..15 {
            page_key(&mut screen, KeyCode::Down);
        }
        assert_eq!(screen.scroll, 0);
        assert!(screen.auto_scroll);

        // 模拟再次流式追加 10 行
        for i in 40..50 {
            screen.messages.push(Line::raw(format!("msg line {i:02}")));
        }
        render(&mut screen, 80, 20);
        // 处于自动跟随态
        assert_eq!(screen.scroll, 0);
        assert!(screen.auto_scroll);
        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    #[tokio::test]
    async fn test_auto_scroll_end_key_resets_and_indicator_in_footer() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        screen.has_run = true;
        for i in 0..30 {
            screen.messages.push(Line::raw(format!("msg line {i:02}")));
        }
        render(&mut screen, 80, 20);

        // 初始跟随底部，footer 无暂停提示
        let footer_init = screen.footer_line();
        let has_pause_tag = footer_init
            .spans
            .iter()
            .any(|s| s.content.contains("自动滚动已暂停"));
        assert!(!has_pause_tag);

        // 上滚后：出现暂停提示
        page_key(&mut screen, KeyCode::PageUp);
        assert!(!screen.auto_scroll);
        assert!(screen.scroll > 0);
        let footer_scrolled = screen.footer_line();
        let pause_span = footer_scrolled
            .spans
            .iter()
            .find(|s| s.content.contains("自动滚动已暂停"))
            .expect("应显示暂停提示");
        assert!(pause_span.content.contains("按 End 恢复"));
        assert_eq!(pause_span.style.fg, Some(AMBER));
        assert!(pause_span.style.add_modifier.contains(Modifier::BOLD));

        // 按 End 键立即复位
        page_key(&mut screen, KeyCode::End);
        assert_eq!(screen.scroll, 0);
        assert!(screen.auto_scroll);

        // footer 提示消失
        let footer_after_end = screen.footer_line();
        assert!(!footer_after_end
            .spans
            .iter()
            .any(|s| s.content.contains("自动滚动已暂停")));
        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    #[tokio::test]
    async fn prompt_history_cycles_with_up_and_down_and_restores_draft() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        screen.prompt_history = vec!["first query".into(), "second query".into()];
        screen.input.insert_str("my draft");

        let (broker, _rx) = PermissionBroker::interactive();
        let broker = Arc::new(broker);
        let (events, _rx_ev) = mpsc::unbounded_channel();
        let mut runner_opt = Some(runner);

        // Press Up: loads latest ("second query")
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut runner_opt,
            &broker,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.input.lines().join("\n"), "second query");

        // Press Up again: loads earlier ("first query")
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut runner_opt,
            &broker,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.input.lines().join("\n"), "first query");

        // Press Down: moves back to "second query"
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut runner_opt,
            &broker,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.input.lines().join("\n"), "second query");

        // Press Down: restores "my draft"
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut runner_opt,
            &broker,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.input.lines().join("\n"), "my draft");
        assert!(screen.history_index.is_none());
        let _ = std::fs::remove_dir_all(runner_opt.unwrap().cwd);
    }

    #[tokio::test]
    async fn f2_and_mode_command_cycle_permission_modes() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let (broker, _rx) = PermissionBroker::interactive();
        let broker = Arc::new(broker);
        let (events, _rx_ev) = mpsc::unbounded_channel();
        let mut runner_opt = Some(runner);

        assert_eq!(screen.permission_mode, PermissionMode::Auto);
        assert!(screen.footer().is_empty());
        screen.has_run = true;
        assert!(screen.footer().contains("模式: 自动审批 (F2)"));

        // Press F2 -> Unlimited
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE),
            &mut runner_opt,
            &broker,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.permission_mode, PermissionMode::Unlimited);
        assert_eq!(broker.mode(), PermissionMode::Unlimited);
        assert!(screen.footer().contains("模式: 无限制 (F2)"));

        // Press F2 again -> Manual
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE),
            &mut runner_opt,
            &broker,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.permission_mode, PermissionMode::Manual);
        assert_eq!(broker.mode(), PermissionMode::Manual);
        assert!(screen.footer().contains("模式: 手动审批 (F2)"));
        let _ = std::fs::remove_dir_all(runner_opt.unwrap().cwd);
    }

    #[tokio::test]
    async fn turn_summary_renders_ctf_and_general_summaries() {
        let mut runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);

        // 1. General task summary (describes outcome without mechanical tool names)
        screen.turn_summary(
            5000,
            1_700_000_000,
            "done",
            Some("任务完成：已列出当前工作目录条目"),
        );
        render(&mut screen, 100, 20);
        assert!(history(&screen).contains("※summary：任务完成：已列出当前工作目录条目"));

        // 2. CTF Solved summary (extracts real solution flow from answer even when tools were called)
        let mut ch = cyber_core::CtfChallenge::new("web-sqli".into(), cyber_core::CtfCategory::Web);
        ch.status = cyber_core::CtfStatus::Solved;
        ch.flag = Some("NSSCTF{flag_sqli_success}".into());
        runner.replace_challenges(vec![ch]).unwrap();

        let dummy_tools = vec![crate::headless::ToolCallRecord {
            id: "1".into(),
            name: "shell".into(),
            arguments: "{}".into(),
            output: "ok".into(),
            is_error: false,
        }];
        let answer_with_steps = "经过排查与利用：\n### 解题流程\n1. 审计 search.php 发现未过滤的单引号输入点\n2. 构造布尔盲注 Payload 爆破数据库 flags 表\n3. 提取出 flag 字段获取 Flag";
        let summary = runner.generate_turn_summary("done", answer_with_steps, &dummy_tools, None);
        assert!(summary.contains("题目【web-sqli】已解出 (Flag: NSSCTF{flag_sqli_success})"));
        assert!(summary.contains("解题流程：1. 审计 search.php 发现未过滤的单引号输入点 -> 2. 构造布尔盲注 Payload 爆破数据库 flags 表 -> 3. 提取出 flag 字段获取 Flag"));
        assert!(!summary.contains("shell"), "解题流程中绝不包含内部工具名");
        assert!(
            !summary.contains("调用工具"),
            "解题流程中绝不输出无意义的'调用工具'"
        );

        // 3. CTF InProgress blocker summary
        let mut ch_prog =
            cyber_core::CtfChallenge::new("pwn-stack".into(), cyber_core::CtfCategory::Pwn);
        ch_prog.status = cyber_core::CtfStatus::InProgress;
        ch_prog.key_points = Some("开启了 Canary 与 PIE 保护，需先泄露基址".into());
        runner.replace_challenges(vec![ch_prog]).unwrap();

        let blocker_summary = runner.generate_turn_summary("done", "探测保护机制中", &[], None);
        assert!(
            blocker_summary.contains("题目【pwn-stack】尚未解出，卡点：开启了 Canary 与 PIE 保护")
        );
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn thinking_title_and_lazy_footer_match_activity_states() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        screen.model = "deepseek-v4-1-flash-260910".into();
        screen.effort = ThinkingIntensity::Low;

        // 1. Fresh session before run: no footer metrics, title unchanged
        assert!(!screen.has_run);
        assert!(screen.footer().is_empty());
        let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
        term.draw(|f| screen.draw(f)).unwrap();
        let idle_fg = term.backend().buffer()[(0, 21)].fg;
        assert_eq!(idle_fg, effort_color(ThinkingIntensity::Low));
        let text = render(&mut screen, 120, 24);
        assert!(text.contains("deepseek-v4-1-flash-260910 · low"));
        assert!(!text.contains("Cy >"));
        assert!(!text.contains("ctx"));
        assert!(!text.contains("cache"));

        // 2. Active run (thinking / processing): title shows Cy > 0s > model · effort and changes to thinking color
        screen.busy = true;
        screen.thinking_started = Some(std::time::Instant::now());
        term.draw(|f| screen.draw(f)).unwrap();
        let busy_fg = term.backend().buffer()[(0, 21)].fg;
        assert_eq!(busy_fg, ACCENT);
        assert_ne!(
            idle_fg, busy_fg,
            "normal mode and thinking mode must be two different colors"
        );
        let busy_text = render(&mut screen, 120, 24);
        assert!(busy_text.contains("Cy > 0s > deepseek-v4-1-flash-260910 · low"));
        assert!(
            !busy_text.contains("(0s)"),
            "should not have parentheses around duration"
        );
        // 3. After run completes: footer displays metrics line
        screen.busy = false;
        screen.has_run = true;
        screen.thinking_started = None;
        let done_text = render(&mut screen, 120, 24);
        assert!(done_text.contains("deepseek-v4-1-flash-260910 · low"));
        assert!(!done_text.contains("Cy >"));
        assert!(screen
            .footer()
            .contains("deepseek-v4-1-flash-260910 │ ctx -- │ cache -- │ ↑-- ↓--"));
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[test]
    fn wrapped_viewport_preserves_styles_unicode_and_unchanged_prefix() {
        let mut view = WrappedViewport::default();
        let mut lines = vec![Line::styled(
            "\u{4f60}\u{597d}ab",
            Style::default().fg(ACCENT),
        )];
        view.update(&lines, 4);
        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.rows[0].to_string(), "\u{4f60}\u{597d}");
        assert_eq!(view.rows[1].to_string(), "ab");
        assert_eq!(view.rows[0].style.fg, Some(ACCENT));
        let prefix = view.rows[0].spans[0].content.as_ptr();
        view.update(&lines, 4);
        assert_eq!(view.rows[0].spans[0].content.as_ptr(), prefix);
        lines.push(Line::raw("tail"));
        view.update(&lines, 4);
        assert_eq!(view.rows[0].spans[0].content.as_ptr(), prefix);
        lines[1] = Line::raw("changed tail");
        view.update(&lines, 4);
        assert_eq!(view.rows[0].spans[0].content.as_ptr(), prefix);
        assert_eq!(view.window(2, 1)[0].to_string(), "chan");
    }

    #[tokio::test]
    async fn test_cli_image_pasting_and_placeholder_generation() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);

        // 1. 测试直接粘贴带引号的文件路径（如 Windows 资源管理器复制）
        let temp_dir = std::env::temp_dir();
        let test_img = temp_dir.join("cli_test_paste.png");
        std::fs::write(&test_img, [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]).unwrap();

        let quoted_path = format!("\"{}\"", test_img.display());
        let handled = screen.handle_text_image_paste(&quoted_path);
        assert!(handled, "应识别带双引号的文件路径为图片");
        assert_eq!(screen.attached_images.len(), 1);
        assert_eq!(screen.attached_images[0].id, 1);
        assert_eq!(screen.input.lines().join(""), "[image:1]");

        // 2. 测试粘贴图片 URL
        let handled_url = screen.handle_text_image_paste("https://example.com/captcha.jpg");
        assert!(handled_url, "应识别图片 URL");
        assert_eq!(screen.attached_images.len(), 2);
        assert_eq!(screen.attached_images[1].id, 2);
        assert_eq!(screen.input.lines().join(""), "[image:1][image:2]");

        // 3. 测试占位符展开
        let expanded = screen.expand_attached_placeholders("请分析 [image:1] 以及 [image:2]");
        assert!(expanded.contains("cli_test_paste.png"));
        assert!(expanded.contains("https://example.com/captcha.jpg"));

        // 4. 测试徽章渲染
        let style = Style::default();
        let mut spans = Vec::new();
        CliScreen::render_cli_text_with_image_badges(&mut spans, "输入 [image:1] 占位", style);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "输入 ");
        assert_eq!(spans[1].content, "🖼️ [image:1]");
        assert_eq!(spans[2].content, " 占位");

        let _ = std::fs::remove_file(test_img);
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn test_cli_paste_debouncing() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);

        // 模拟已发生一次粘贴
        screen.last_paste_instant = Some(std::time::Instant::now());
        screen.insert_text("hello");
        assert_eq!(screen.input.lines().join(""), "hello");

        // 紧接着尝试调用剪贴板粘贴方法
        assert!(
            !screen.handle_clipboard_image_only(),
            "防重保护应拦截高频调用"
        );
        assert!(
            !screen.handle_clipboard_image_or_text(),
            "防重保护应拦截高频调用"
        );
        assert_eq!(screen.input.lines().join(""), "hello", "内容不应被重复插入");
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn test_footer_status_highlighting() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        screen.has_run = true;

        // 1. 操作提示（如 Model selected）高亮为电光青色加粗
        screen.status = "Model selected".into();
        let line = screen.footer_line();
        let status_span = line
            .spans
            .iter()
            .find(|s| s.content.contains("Model selected"))
            .unwrap();
        assert_eq!(status_span.style.fg, Some(Color::Rgb(80, 240, 220)));
        assert!(status_span.style.add_modifier.contains(Modifier::BOLD));

        // 2. CTF 模式更新同样高亮
        screen.status = "CTF mode updated".into();
        let line = screen.footer_line();
        let ctf_span = line
            .spans
            .iter()
            .find(|s| s.content.contains("CTF mode updated"))
            .unwrap();
        assert_eq!(ctf_span.style.fg, Some(Color::Rgb(80, 240, 220)));
        assert!(ctf_span.style.add_modifier.contains(Modifier::BOLD));

        // 3. 取消 / 错误提示为红色加粗
        screen.status = "Cancelling".into();
        let line = screen.footer_line();
        let cancel_span = line
            .spans
            .iter()
            .find(|s| s.content.contains("Cancelling"))
            .unwrap();
        assert_eq!(cancel_span.style.fg, Some(ERROR));
        assert!(cancel_span.style.add_modifier.contains(Modifier::BOLD));

        // 4. 工作中提示为琥珀金加粗
        screen.status = "Working".into();
        let line = screen.footer_line();
        let work_span = line
            .spans
            .iter()
            .find(|s| s.content.contains("Working"))
            .unwrap();
        assert_eq!(work_span.style.fg, Some(AMBER));
        assert!(work_span.style.add_modifier.contains(Modifier::BOLD));
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    async fn snapshot(width: u16, height: u16) -> (String, ratatui::buffer::Buffer) {
        let runner = crate::headless::tests::test_runner().await;
        let cwd = runner.cwd.clone();
        let mut screen = CliScreen::new(&runner);
        screen.model = "deepseek-v4-flash-0731".into();
        screen.effort = ThinkingIntensity::Max;
        screen.has_run = true;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::remove_dir_all(cwd);
        (text, buffer)
    }

    #[tokio::test]
    async fn header_and_composer_match_requested_layout() {
        let (text, buffer) = snapshot(120, 30).await;
        assert!(text.contains(&format!("Cyber Master V{}", env!("CARGO_PKG_VERSION"))));
        assert!(text.contains("deepseek-v4-flash-0731 with xhigh effort"));
        assert!(text.contains("Try \"refactor <filepath>\""));
        assert!(text
            .lines()
            .last()
            .unwrap()
            .contains("ctx -- │ cache -- │ ↑-- ↓--"));
        assert!(!text.contains("manual mode"));
        assert_eq!(buffer[(15, 2)].fg, MUTED);
        assert_eq!(buffer[(15, 3)].fg, MUTED);
        assert_eq!(buffer[(0, 27)].symbol(), "╭");
        assert_eq!(buffer[(0, 28)].symbol(), "╰");
        assert_eq!(buffer[(119, 28)].symbol(), "╯");
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 1)].symbol(), "_");
    }
    #[tokio::test]
    async fn header_and_welcome_display_newer_version_in_amber() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        screen.new_version = Some("0.9.9".into());

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..30)
            .map(|y| {
                (0..120)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        let clean_text = text.replace(' ', "");
        // 顶栏黄色标出新版本
        assert!(clean_text.contains("(新版本V0.9.9)"), "{text}");
        // 欢迎页提示使用 /update
        assert!(
            clean_text.contains("检测到新版本V0.9.9，可运行/update更新（或退出后cyberupdate）"),
            "{text}"
        );

        // 验证顶栏新版本文字以 AMBER (黄色) 标出
        let found_amber = (0..5).any(|y| {
            (0..120).any(|x| {
                let cell = &buffer[(x, y)];
                cell.symbol() == "(" && cell.fg == AMBER
            })
        });
        assert!(found_amber, "顶栏新版本标记必须以 AMBER (黄色) 标出");
    }

    #[tokio::test]
    async fn narrow_and_tiny_terminals_do_not_panic() {
        for (width, height) in [(30, 12), (10, 5), (1, 1), (0, 0)] {
            let _ = snapshot(width, height).await;
        }
    }

    #[test]
    fn effort_labels_reflect_runtime_intensity() {
        assert_eq!(effort_label(ThinkingIntensity::Auto), "medium");
        assert_eq!(effort_label(ThinkingIntensity::Max), "xhigh");
    }

    #[tokio::test]
    async fn pasted_multiline_commands_remain_in_composer() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        screen.insert_text("refactor this\n/new\n/quit\n");
        assert_eq!(
            screen.input.lines().join("\n"),
            "refactor this\n/new\n/quit\n"
        );
        assert!(!screen.busy);
        assert!(screen.panel.is_none());
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[test]
    fn rapid_key_bursts_buffer_enter_instead_of_submitting() {
        let mut paste = PasteDetector::new();
        assert_eq!(
            paste.observe(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            KeyDisposition::Process
        );
        assert_eq!(
            paste.observe(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
            KeyDisposition::Buffer
        );
        assert_eq!(
            paste.observe(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            KeyDisposition::Buffer
        );
        assert_eq!(paste.flush().as_deref(), Some("b\n"));
    }

    #[tokio::test]
    async fn effort_command_updates_runtime_and_shortcuts_are_functional() {
        let runner = crate::headless::tests::test_runner().await;
        let cwd = runner.cwd.clone();
        let mut screen = CliScreen::new(&runner);
        let mut runner = Some(runner);
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _rx) = mpsc::unbounded_channel();
        screen.input.insert_str("/effort max");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.effort, ThinkingIntensity::Max);
        assert_eq!(
            runner.as_ref().unwrap().ctx.config.agent.thinking_intensity,
            ThinkingIntensity::Max
        );
        page_key(&mut screen, KeyCode::Char('?'));
        assert!(matches!(screen.panel, Some(Panel::Shortcuts)));
        page_key(&mut screen, KeyCode::Esc);
        assert!(screen.panel.is_none());
        page_key(&mut screen, KeyCode::Left);
        assert!(screen.panel.is_none());
        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn ctf_panel_toggles_with_ctrl_t_and_navigates_in_cli() {
        let runner = crate::headless::tests::test_runner().await;
        let cwd = runner.cwd.clone();
        let mut screen = CliScreen::new(&runner);
        let mut runner = Some(runner);
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _rx) = mpsc::unbounded_channel();

        // 1. Press Ctrl+T when CTF disabled -> shows hint message, panel stays None
        assert!(!screen.ctf_enabled);
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(screen.panel.is_none());
        assert!(history(&screen).contains("CTF 模式未开启"));
        // 2. Enable CTF and add challenge via command
        screen.input.insert_str("/ctf enable");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(screen.ctf_enabled);

        screen.input.insert_str("/ctf add webftp_auth_bypass web");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();

        // 3. Press Ctrl+T -> opens CTF panel
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.panel, Some(Panel::Ctf));
        assert!(!screen.ctf_detail_view);
        let rendered_panel = render(&mut screen, 100, 30);
        assert!(rendered_panel.contains("webftp_auth_bypass"));
        assert!(rendered_panel.contains("进 行 中") || rendered_panel.contains("进行中"));

        // 4. Press Enter -> opens challenge detail
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(screen.ctf_detail_view);
        let rendered_detail = render(&mut screen, 100, 30);
        assert!(rendered_detail.contains("webftp_auth_bypass"));
        assert!(rendered_detail.contains("Esc"));

        // 5. Press Esc -> exits detail back to list
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.panel, Some(Panel::Ctf));
        assert!(!screen.ctf_detail_view);

        // 6. Press Ctrl+T -> closes panel
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(screen.panel.is_none());

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn ctf_panel_reflects_runtime_tool_registered_challenges() {
        let runner = crate::headless::tests::test_runner().await;
        let cwd = runner.cwd.clone();
        let mut screen = CliScreen::new(&runner);
        let mut runner = Some(runner);
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _rx) = mpsc::unbounded_channel();

        // 1. Enable CTF
        screen.input.insert_str("/ctf enable");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(screen.ctf_enabled);

        // 2. Open CTF panel: currently empty
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.panel, Some(Panel::Ctf));
        assert_eq!(screen.ctf_challenges_count(), 0);

        // 3. Simulate background / LLM tool registration via runner.registries.tools execute ctf_challenge register
        let owner = runner.as_mut().unwrap();
        let register_input = serde_json::json!({
            "action": "register",
            "name": "sqli_blind_runtime",
            "category": "web",
            "description": "runtime tool registered challenge",
            "target": "http://10.10.10.10:8080"
        });
        let tool_ctx = cyber_agent::ToolCtx::new(owner.cwd.clone(), Vec::new(), None, Vec::new());
        let tool_result = owner
            .registries
            .tools
            .execute("ctf_challenge", register_input, &tool_ctx)
            .await;
        assert!(tool_result.is_ok());

        // 4. Without calling sync or restarting, panel view should reflect the challenge immediately!
        assert_eq!(screen.ctf_challenges_count(), 1);
        let rendered = render(&mut screen, 100, 30);
        assert!(rendered.contains("sqli_blind_runtime"));
        assert!(rendered.contains("[WEB]"));

        // 5. Navigate enter into detail
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(screen.ctf_detail_view);
        let detail_rendered = render(&mut screen, 100, 30);
        assert!(detail_rendered.contains("sqli_blind_runtime"));
        assert!(detail_rendered.contains("http://10.10.10.10:8080"));

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn slash_ctf_opens_ctf_panel() {
        let runner = crate::headless::tests::test_runner().await;
        let cwd = runner.cwd.clone();
        let mut screen = CliScreen::new(&runner);
        let mut runner = Some(runner);
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _rx) = mpsc::unbounded_channel();

        // 1. /ctf with no args returns Panel(Panel::Ctf) and enables CTF mode
        let action = cli_commands::execute(runner.as_mut().unwrap(), "/ctf").unwrap();
        assert!(matches!(action, CliAction::Panel(Panel::Ctf)));
        assert!(runner.as_ref().unwrap().ctf_enabled);

        // Applying action opens Panel::Ctf and initializes screen state
        apply_action(
            &mut screen,
            action,
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.panel, Some(Panel::Ctf));
        assert!(screen.ctf_enabled);
        assert_eq!(screen.ctf_selected, 0);
        assert!(!screen.ctf_detail_view);

        // 2. /ctf panel and /ctf open also return Panel(Panel::Ctf)
        let action_panel = cli_commands::execute(runner.as_mut().unwrap(), "/ctf panel").unwrap();
        assert!(matches!(action_panel, CliAction::Panel(Panel::Ctf)));

        let action_open = cli_commands::execute(runner.as_mut().unwrap(), "/ctf open").unwrap();
        assert!(matches!(action_open, CliAction::Panel(Panel::Ctf)));

        // 3. /ctf status outputs "enabled"
        let action_status = cli_commands::execute(runner.as_mut().unwrap(), "/ctf status").unwrap();
        if let CliAction::Output { text, .. } = action_status {
            assert_eq!(text, "enabled");
        } else {
            panic!("Expected Output action for /ctf status");
        }

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn ctf_panel_full_keyboard_operations() {
        let runner = crate::headless::tests::test_runner().await;
        let cwd = runner.cwd.clone();
        let mut screen = CliScreen::new(&runner);
        let mut runner = Some(runner);
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _rx) = mpsc::unbounded_channel();

        // 1. Add two challenges
        cli_commands::execute(runner.as_mut().unwrap(), "/ctf add web_sqli web").unwrap();
        cli_commands::execute(runner.as_mut().unwrap(), "/ctf add pwn_rop pwn").unwrap();

        // 2. Open CTF panel via /ctf
        screen.input.insert_str("/ctf");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.panel, Some(Panel::Ctf));
        assert_eq!(screen.ctf_challenges_count(), 2);
        assert_eq!(screen.ctf_selected, 0);
        assert!(!screen.ctf_detail_view);

        // 3. Test navigation: j (down), k (up)
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.ctf_selected, 1);

        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.ctf_selected, 0);

        // 4. Test status toggle: s (InProgress -> Solved -> InProgress)
        let c0 = screen.ctf_challenge_at(0).unwrap();
        assert_eq!(c0.status, cyber_core::CtfStatus::InProgress);
        assert!(c0.end_time.is_none());

        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        let c0 = screen.ctf_challenge_at(0).unwrap();
        assert_eq!(c0.status, cyber_core::CtfStatus::Solved);
        assert!(c0.end_time.is_some());
        assert!(history(&screen).contains("「web_sqli」→ 已完成"));

        // 5. Test global toggle: g (false -> true -> false)
        assert!(!c0.is_global);
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        let c0 = screen.ctf_challenge_at(0).unwrap();
        assert!(c0.is_global);
        assert!(history(&screen).contains("「web_sqli」→ 全局 ★"));

        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        let c0 = screen.ctf_challenge_at(0).unwrap();
        assert!(!c0.is_global);
        assert!(history(&screen).contains("「web_sqli」→ 仅本 session"));

        // 6. Test set all global: G
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(screen.ctf_challenge_at(0).unwrap().is_global);
        assert!(screen.ctf_challenge_at(1).unwrap().is_global);
        assert!(history(&screen).contains("已将 2 道题目设为全局"));

        // 7. Test Enter -> detail view, navigation inside detail view, q -> back to list
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(screen.ctf_detail_view);

        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.ctf_detail_scroll, 1);

        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.ctf_detail_scroll, 0);

        // Press 'q' in detail view -> returns to list view
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.panel, Some(Panel::Ctf));
        assert!(!screen.ctf_detail_view);

        // 8. Test Writeup 'w':
        // For solved challenge: enters detail view, press 'w' -> triggers writeup task
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(screen.ctf_detail_view);

        let mut active = None;
        let mut cancel = None;
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        // Panel closed and active task started
        assert_eq!(screen.panel, None);
        assert!(active.is_some());
        if let Some(tx) = cancel {
            let _ = tx.send(());
        }
        if let Some(task) = active {
            let (r, _) = task.await.unwrap();
            screen.sync(&r);
            runner = Some(r);
            screen.busy = false;
        }

        // 9. Reopen CTF panel via /ctf, test edit form 'e' and delete 'd'
        screen.input.insert_str("/ctf");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.panel, Some(Panel::Ctf));
        assert_eq!(screen.ctf_challenges_count(), 2);

        // Select challenge 1 (pwn_rop)
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.ctf_selected, 1);

        // Press 'e' -> opens edit form
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(screen.ctf_edit_form.is_some());

        // Press Esc inside edit form -> cancels edit form
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(screen.ctf_edit_form.is_none());
        assert_eq!(screen.panel, Some(Panel::Ctf));

        // 10. Test delete 'd'
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.ctf_challenges_count(), 1);
        assert!(history(&screen).contains("已删除「pwn_rop」"));

        // Delete remaining challenge
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.ctf_challenges_count(), 0);
        assert!(history(&screen).contains("已删除「web_sqli」"));

        // 11. Press 'q' in list view -> closes panel
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.panel, None);

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn ctf_panel_edit_form_save() {
        let runner = crate::headless::tests::test_runner().await;
        let cwd = runner.cwd.clone();
        let mut screen = CliScreen::new(&runner);
        let mut runner = Some(runner);
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _rx) = mpsc::unbounded_channel();

        // 1. Add challenge
        cli_commands::execute(runner.as_mut().unwrap(), "/ctf add sqli_form web").unwrap();

        // 2. Open CTF panel via /ctf
        screen.input.insert_str("/ctf");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(screen.panel, Some(Panel::Ctf));

        // 3. Press 'e' -> opens edit form
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(screen.ctf_edit_form.is_some());

        // Verify render with edit form active
        let rendered_modal = render(&mut screen, 100, 30);
        assert!(rendered_modal.contains("sqli_form"));

        // Modify field in form (description)
        if let Some(form) = screen.ctf_edit_form.as_mut() {
            form.description = "New description via form".into();
            // Focus Save button (index 9)
            form.focused = 9;
        }

        // 4. Press Enter on Save button -> saves form
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();

        // Form closed, challenge updated
        assert!(screen.ctf_edit_form.is_none());
        assert!(history(&screen).contains("「sqli_form」已保存"));
        let updated = screen.ctf_challenge_at(0).unwrap();
        assert_eq!(updated.description, "New description via form");

        // Verify runner's saved challenges reflect the update
        let runner_challenges = runner.as_mut().unwrap().challenges().unwrap();
        assert_eq!(runner_challenges[0].description, "New description via form");

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[test]
    fn clean_consolidates_carriage_returns() {
        assert_eq!(clean("foo\rbar"), "bar");
        assert_eq!(clean("10%\r20%\r100%"), "100%");
        assert_eq!(clean("line 1\r\nline 2\r\n"), "line 1\nline 2\n");
        assert_eq!(clean("\r\r\r"), "");
        assert_eq!(
            clean("normal text\nsecond line"),
            "normal text\nsecond line"
        );
    }

    #[tokio::test]
    async fn ctrl_l_triggers_needs_clear() {
        let runner = crate::headless::tests::test_runner().await;
        let cwd = runner.cwd.clone();
        let mut screen = CliScreen::new(&runner);
        let mut runner = Some(runner);
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _) = mpsc::unbounded_channel();
        let mut active = None;
        let mut cancel = None;

        screen.needs_clear = false;
        let handled = handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
            &mut runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();

        assert!(!handled, "Ctrl+L should not exit run_cli loop");
        assert!(screen.needs_clear, "Ctrl+L must set needs_clear to true");
        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn tool_result_triggers_needs_clear() {
        let runner = crate::headless::tests::test_runner().await;
        let cwd = runner.cwd.clone();
        let mut screen = CliScreen::new(&runner);

        screen.needs_clear = false;
        screen.tool_call("call_1", "shell", r#"{"command":"echo hi"}"#);
        screen.tool_result("call_1", "shell", "hi\n", false);
        assert!(
            screen.needs_clear,
            "tool_result must set needs_clear to true"
        );
        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn queued_prompts_auto_spawn_next_turn_on_completion() {
        let owner = crate::headless::tests::test_runner().await;
        let cwd = owner.cwd.clone();
        let mut screen = CliScreen::new(&owner);
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, mut rx) = mpsc::unbounded_channel();
        let mut active = None;
        let mut cancel = None;
        let mut runner = Some(owner);

        // 启动第一轮任务
        assert!(spawn_turn(
            &mut screen,
            &mut runner,
            "first turn".into(),
            false,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        ));
        assert!(screen.busy);
        assert!(active.is_some());

        // 1. 在 busy 状态下按 Enter 平滑追加指令
        screen.input = composer();
        screen.insert_text("appended question");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(screen.queued_prompts.len(), 1);

        // 2. 在 busy 状态下按 Alt+Enter 立即打断
        screen.input = composer();
        screen.insert_text("immediate steer question");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            &mut runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(screen.queued_prompts.len(), 2);
        assert_eq!(screen.status, "已立即打断并读取新指示…");

        // 第一轮任务因打断而结束，模拟事件循环的收尾逻辑
        let (restored, _outcome) = active.take().unwrap().await.unwrap();
        while let Ok(event) = rx.try_recv() {
            screen.event(event);
        }
        screen.sync(&restored);
        runner = Some(restored);
        screen.busy = false;
        screen.active_steering_tx = None;

        // 收尾时从 queued_prompts 取出未消费的指示自动启动下一轮任务
        assert!(!drain_queued_inputs(
            &mut screen,
            &mut runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap());

        // 验证第二轮任务已自动启动
        assert!(screen.busy);
        assert!(active.is_some());

        let _ = active.take().unwrap().await;
        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn auto_mode_approves_delegate_tasks_without_popup() {
        let (broker, mut requests) = PermissionBroker::interactive();
        assert_eq!(
            broker.mode(),
            PermissionMode::Auto,
            "interactive 默认必须是 Auto 模式"
        );

        let delegate_call_args = serde_json::json!({
            "tasks": [
                {
                    "name": "sub1",
                    "system_prompt": "analyze",
                    "task": "check security",
                    "tools": ["read_file"]
                }
            ]
        });

        // 在 Auto 模式下调用 delegate_tasks
        let authorized = broker
            .authorize("delegate_tasks", &delegate_call_args)
            .await;
        assert!(authorized, "Auto 模式下 delegate_tasks 必须自动放行");
        assert!(
            requests.try_recv().is_err(),
            "Auto 模式下绝对不应向 requests 通道发送弹窗请求"
        );
    }

    fn settings_key_with_runner(
        screen: &mut CliScreen,
        runner: &mut Option<SessionRunner>,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) {
        let (broker, _requests) = PermissionBroker::interactive();
        let (events, _rx) = mpsc::unbounded_channel();
        handle_key(
            screen,
            KeyEvent::new(code, modifiers),
            runner,
            &Arc::new(broker),
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn setup_mode_esc_without_provider_keeps_panel() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        screen.setup_mode = true;
        screen.panel = Some(Panel::Settings);
        screen.settings = Some(CliSettingsState::new(
            &Config::default(),
            &ProvidersConfig::default(),
        ));

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );

        assert_eq!(screen.panel, Some(Panel::Settings));
        assert!(screen.settings.is_some());
        assert!(screen.status.contains("尚未完成配置"));
    }

    #[tokio::test]
    async fn setup_mode_save_writes_completed_marker_and_allows_exit() {
        const KEY_VAR: &str = "CYBER_SETUP_MARKER_TEST_KEY";
        let owner = crate::headless::tests::test_runner().await;
        let cyber_home = owner.ctx.paths.cyber_home.clone();
        cyber_core::init::ensure_global_init(&owner.ctx.paths).unwrap();
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        screen.setup_mode = true;
        screen.panel = Some(Panel::Settings);
        let mut providers = ProvidersConfig::default();
        providers.providers.insert(
            "deepseek".into(),
            cyber_core::ProviderConfig {
                kind: "openai".into(),
                base_url: "https://api.deepseek.com/v1".into(),
                model: "deepseek-chat".into(),
                api_key: format!("${{{KEY_VAR}}}"),
                ..Default::default()
            },
        );
        providers.default_provider = "deepseek".into();
        let mut config = Config::default();
        config.agent.default_provider = "deepseek".into();
        screen.settings = Some(CliSettingsState::new(&config, &providers));
        std::env::set_var(KEY_VAR, "sk-setup-marker-test");

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        );

        assert!(
            screen.status.contains("已保存"),
            "status = {}",
            screen.status
        );
        let state = std::fs::read_to_string(cyber_home.join("setup.toml")).unwrap();
        assert!(state.contains("completed = true"), "state = {state}");
        let written: Config =
            toml::from_str(&std::fs::read_to_string(cyber_home.join("config.toml")).unwrap())
                .unwrap();
        assert_eq!(written.agent.default_provider, "deepseek");

        // 配置可用后 Esc 才会离开向导（handle_key 返回 true = 退出主循环）。
        let (broker, _requests) = PermissionBroker::interactive();
        let (events, _rx) = mpsc::unbounded_channel();
        let exit = handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut runner_opt,
            &Arc::new(broker),
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(exit);
        std::env::remove_var(KEY_VAR);
    }

    #[tokio::test]
    async fn toolbox_tab_renders_long_description_prefix() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.panel = Some(Panel::Settings);
        let mut settings = CliSettingsState::new(&Config::default(), &ProvidersConfig::default());
        settings.tab = SettingsTab::Toolbox;
        settings.custom_tools = vec![cyber_core::CustomToolConfig {
            name: "fenjing_crack".into(),
            description:
                "Fenjing 攻击指定表单参数:数据中注入点写 PAYLOAD,自动检测 WAF 生成绕过 payload,-e 执行命令。"
                    .into(),
            command: "python -m fenjing crack -u {url}".into(),
            ..Default::default()
        }];
        screen.settings = Some(settings);

        let width = 120u16;
        let height = 30u16;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| {
                let settings = screen.settings.as_ref().unwrap();
                draw_settings_panel(f, f.area(), settings, &screen);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        // 超长描述必须显示可见前缀，而不是被整段丢掉只剩「…」
        assert!(
            text.contains("[fenjing_crack] Fenjing"),
            "长描述必须显示前缀: {text}"
        );
        assert!(text.contains('…'), "超长行必须补省略号: {text}");

        // 回归：AI 扫描入口固定在首行，自定义工具列表在其下方（入口原先在末尾）。
        // 宽字符在缓冲区内占两格（第二格为空符号），逐行压掉空白后再断言。
        let compact_lines: Vec<String> = text
            .lines()
            .map(|line| line.chars().filter(|c| !c.is_whitespace()).collect())
            .collect();
        let scan_line = compact_lines
            .iter()
            .position(|line| line.contains("AI智能扫描本地安全工具"))
            .unwrap_or_else(|| panic!("扫描入口必须可见: {text}"));
        let tool_line = compact_lines
            .iter()
            .position(|line| line.contains("[fenjing_crack]"))
            .unwrap_or_else(|| panic!("自定义工具行必须可见: {text}"));
        assert!(scan_line < tool_line, "扫描入口必须在工具列表之上: {text}");
        assert!(
            compact_lines[scan_line].contains('▶'),
            "默认焦点必须落在首行（扫描入口）: {text}"
        );
        assert!(
            !compact_lines[tool_line].contains('▶'),
            "未选中的工具行不应显示焦点指针: {text}"
        );
    }

    #[tokio::test]
    async fn toolbox_tab_adds_tool_while_the_scan_row_is_selected() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);
        screen.panel = Some(Panel::Settings);
        let mut settings = CliSettingsState::from_view(&runner_opt.as_ref().unwrap().view());
        settings.tab = SettingsTab::Toolbox;
        screen.settings = Some(settings);

        // 空清单：整个列表只有 AI 扫描入口一行，A 仍须能录入自定义工具。
        assert_eq!(
            SettingsTab::Toolbox.max_row(screen.settings.as_ref().unwrap()),
            0
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        );
        assert!(matches!(
            screen.form.as_ref().map(|state| &state.form.kind),
            Some(FormKind::CustomTool {
                original_name: None
            })
        ));
        assert_eq!(screen.settings_return_tab, Some(SettingsTab::Toolbox));

        if let Some(r) = runner_opt {
            let _ = std::fs::remove_dir_all(r.cwd);
        }
    }

    #[tokio::test]
    async fn toolbox_scan_form_renders_fields_and_model_picker() {
        let owner = crate::headless::tests::test_runner().await;
        let _screen = CliScreen::new(&owner);
        let mut form = FormState::new(cli_commands::toolbox_scan_form(
            "anthropic".into(),
            "claude-sonnet-4-5".into(),
        ));

        let width = 120u16;
        let height = 34u16;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let render = |terminal: &mut Terminal<TestBackend>, form: &FormState| -> (String, String) {
            terminal
                .draw(|f| draw_form_dialog(f, f.area(), form, ""))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let text: String = (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            // 宽字符在缓冲区内占两格（第二格为空符号），CJK 断言前先去掉所有空白。
            let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            (text, compact)
        };

        let (text, compact) = render(&mut terminal, &form);
        assert!(
            compact.contains("AI智能扫描本地安全工具"),
            "标题缺失: {text}"
        );
        assert!(
            compact.contains("扫描目标(本地路径或提示词"),
            "目标标签缺失: {text}"
        );
        assert!(compact.contains("扫描服务商"), "服务商标签缺失: {text}");
        assert!(compact.contains("扫描模型"), "模型标签缺失: {text}");
        assert!(compact.contains("仅预览不写盘"), "预览标签缺失: {text}");
        assert!(text.contains("anthropic"), "服务商默认值缺失: {text}");
        assert!(text.contains("claude-sonnet-4-5"), "模型默认值缺失: {text}");

        // 模型行按 Enter 打开的浮层必须画在表单之上（Provider 表单之外的路径）。
        form.open_model_picker(
            Ok(vec!["claude-haiku-4".into(), "claude-opus-4".into()]),
            None,
            "anthropic",
        );
        let (text, compact) = render(&mut terminal, &form);
        // 块标题里的宽字符会被边框残留的 ─ 分隔，改为断言浮层图例与列表内容。
        assert!(
            compact.contains("↑/↓选择·Enter确认"),
            "模型浮层图例缺失: {text}"
        );
        assert!(
            text.contains("claude-opus-4"),
            "模型列表浮层必须渲染: {text}"
        );
    }

    #[tokio::test]
    async fn settings_toolbox_tab_lists_tools_and_gates_scan_in_setup_mode() {
        let owner = crate::headless::tests::test_runner().await;
        let tools_dir = owner.ctx.paths.tools_dir.clone();
        cyber_core::custom_tool::save_custom_tool(
            &tools_dir,
            &cyber_core::CustomToolConfig {
                name: "probe".into(),
                description: "探测".into(),
                command: "probe -x".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        screen.panel = Some(Panel::Settings);
        let mut settings = CliSettingsState::from_view(&runner_opt.as_ref().unwrap().view());
        settings.tab = SettingsTab::Toolbox;
        screen.settings = Some(settings);

        let settings = screen.settings.as_ref().unwrap();
        assert_eq!(settings.custom_tools.len(), 1);
        assert_eq!(SettingsTab::Toolbox.max_row(settings), 1);

        // A → 打开自定义工具表单（返回设置中心时回到工具库页）
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        );
        assert!(matches!(
            screen.form.as_ref().map(|state| &state.form.kind),
            Some(FormKind::CustomTool {
                original_name: None
            })
        ));
        assert_eq!(screen.settings_return_tab, Some(SettingsTab::Toolbox));
        assert_eq!(screen.panel, None);

        // Esc 关闭表单 → 回到设置中心工具库页
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, Some(Panel::Settings));
        assert_eq!(screen.settings.as_ref().unwrap().tab, SettingsTab::Toolbox);

        // 首行 Enter：向导模式下只提示，不触发扫描
        screen.setup_mode = true;
        screen.settings.as_mut().unwrap().selected_row = 0;
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, Some(Panel::Settings));
        assert!(
            screen.status.contains("Ctrl+S"),
            "status = {}",
            screen.status
        );
        assert!(screen.form.is_none());

        // 首行 Enter（非向导模式）：打开 AI 扫描表单，默认带出扫描服务商与模型
        screen.setup_mode = false;
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, None);
        assert_eq!(screen.settings_return_tab, Some(SettingsTab::Toolbox));
        let form = screen.form.as_ref().expect("扫描表单必须打开");
        assert!(matches!(form.form.kind, FormKind::ToolboxScan));
        // 默认 Provider（Config::default = openai）与其配置模型
        assert_eq!(form.input_value("provider"), "openai");
        assert_eq!(form.input_value("model"), "gpt-4o");
        assert_eq!(form.input_value("target"), "");
        assert_eq!(form.input_value("preview"), "false");

        // Esc 关闭扫描表单 → 回到设置中心工具库页
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, Some(Panel::Settings));
        assert_eq!(screen.settings.as_ref().unwrap().tab, SettingsTab::Toolbox);
    }

    #[tokio::test]
    async fn settings_panel_open_via_action_and_key() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        // 1. Slash command execution -> CliAction::Settings
        let action = cli_commands::execute(runner_opt.as_mut().unwrap(), "/settings").unwrap();
        assert!(matches!(action, CliAction::Settings));

        let (broker, _requests) = PermissionBroker::interactive();
        let (events, _rx) = mpsc::unbounded_channel();
        apply_action(
            &mut screen,
            action,
            &mut runner_opt,
            &Arc::new(broker),
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();

        assert_eq!(screen.panel, Some(Panel::Settings));
        assert!(screen.settings.is_some());
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::AgentModel
        );

        // 2. Close via Esc when clean
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, None);
        assert!(screen.settings.is_none());

        // 3. Open via F3
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, Some(Panel::Settings));
        assert!(screen.settings.is_some());

        // 4. Toggle close via F3
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, None);

        // 5. Open via Ctrl+,
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char(','),
            KeyModifiers::CONTROL,
        );
        assert_eq!(screen.panel, Some(Panel::Settings));
    }

    #[tokio::test]
    async fn settings_tab_navigation_and_direct_keys() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        // Open settings
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::AgentModel
        );

        // Tab forward cycling
        let expected_tabs = [
            SettingsTab::UiWorkflow,
            SettingsTab::Subagents,
            SettingsTab::ToolsMcp,
            SettingsTab::Providers,
            SettingsTab::EnvMemory,
            SettingsTab::StorageSystem,
            SettingsTab::Toolbox,
            SettingsTab::About,
            SettingsTab::AgentModel,
        ];
        for expected in expected_tabs {
            settings_key_with_runner(
                &mut screen,
                &mut runner_opt,
                KeyCode::Tab,
                KeyModifiers::NONE,
            );
            assert_eq!(screen.settings.as_ref().unwrap().tab, expected);
            assert_eq!(screen.settings.as_ref().unwrap().selected_row, 0);
        }

        // Shift+Tab backward cycling
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Tab,
            KeyModifiers::SHIFT,
        );
        assert_eq!(screen.settings.as_ref().unwrap().tab, SettingsTab::About);

        // Direct keys 1-9
        let direct_keys = [
            ('1', SettingsTab::AgentModel),
            ('2', SettingsTab::UiWorkflow),
            ('3', SettingsTab::Subagents),
            ('4', SettingsTab::ToolsMcp),
            ('5', SettingsTab::Providers),
            ('6', SettingsTab::EnvMemory),
            ('7', SettingsTab::StorageSystem),
            ('8', SettingsTab::Toolbox),
            ('9', SettingsTab::About),
        ];
        for (ch, expected) in direct_keys {
            settings_key_with_runner(
                &mut screen,
                &mut runner_opt,
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            );
            assert_eq!(screen.settings.as_ref().unwrap().tab, expected);
        }
    }

    /// 只读态 ←/→（及 h/l）切换设置标签页，且不改变值。
    #[tokio::test]
    async fn settings_arrow_keys_switch_tabs_in_read_only() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::AgentModel
        );
        let orig_theme = screen
            .settings
            .as_ref()
            .unwrap()
            .config_draft
            .ui
            .theme
            .clone();

        // → 切到下一个标签页
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Right,
            KeyModifiers::NONE,
        );
        {
            let s = screen.settings.as_ref().unwrap();
            assert_eq!(s.tab, SettingsTab::UiWorkflow);
            assert_eq!(s.selected_row, 0);
            assert!(!s.editing);
        }
        // ← 切回上一个
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Left,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::AgentModel
        );

        // l / h 与 →/← 同义
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('l'),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::UiWorkflow
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('h'),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::AgentModel
        );

        // 首个标签页再 ← 环绕到最后一个
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Left,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.settings.as_ref().unwrap().tab, SettingsTab::About);

        // 只读态切标签不得改动配置值
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .ui
                .theme
                .clone(),
            orig_theme
        );
        assert!(!screen.settings.as_ref().unwrap().dirty);
    }

    /// Enter 进入编辑态；禁则不关闭面板，聚焦行只读；Esc 退出编辑态，再 Esc 才关闭。
    #[tokio::test]
    async fn settings_enter_enters_edit_and_esc_exits_without_closing() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        screen
            .settings
            .as_mut()
            .unwrap()
            .set_tab(SettingsTab::Subagents);
        screen.settings.as_mut().unwrap().selected_row = 0;
        let orig_enabled = screen
            .settings
            .as_ref()
            .unwrap()
            .config_draft
            .agent
            .subagents
            .enabled;

        // Enter 进入编辑态：不立即改值，也不开弹窗
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        {
            let s = screen.settings.as_ref().unwrap();
            assert!(s.editing, "Enter 必须进入编辑态");
            assert_eq!(s.config_draft.agent.subagents.enabled, orig_enabled);
        }
        assert_eq!(screen.panel, Some(Panel::Settings));

        // Esc 退出编辑态，面板保持打开
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert!(!screen.settings.as_ref().unwrap().editing);
        assert_eq!(screen.panel, Some(Panel::Settings));
        assert!(screen.settings.is_some());

        // 无改动时再次 Esc 才关闭面板
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, None);
        assert!(screen.settings.is_none());
    }

    /// 编辑态 ←/→ 改值；Enter 退出后 ←/→ 恢复为切换标签页。
    #[tokio::test]
    async fn settings_edit_state_arrows_adjust_and_enter_finishes() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        screen.settings.as_mut().unwrap().selected_row = 5; // max_steps
        let orig_steps = screen
            .settings
            .as_ref()
            .unwrap()
            .config_draft
            .agent
            .max_steps;

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert!(screen.settings.as_ref().unwrap().editing);
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Right,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .agent
                .max_steps,
            orig_steps + 10
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::AgentModel
        );

        // Enter 退出编辑态
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert!(!screen.settings.as_ref().unwrap().editing);

        // 退出后 ←/→ 恢复切标签页语义
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Right,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::UiWorkflow
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .agent
                .max_steps,
            orig_steps + 10
        );
    }

    /// 只读态值行上的空格被吞掉（仅提示），进入编辑态后才生效。
    #[tokio::test]
    async fn settings_read_only_space_on_value_row_is_noop() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        screen.settings.as_mut().unwrap().selected_row = 4; // auto_tool_call
        let orig_tool = screen
            .settings
            .as_ref()
            .unwrap()
            .config_draft
            .agent
            .auto_tool_call;

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        );
        {
            let s = screen.settings.as_ref().unwrap();
            assert_eq!(s.config_draft.agent.auto_tool_call, orig_tool);
            assert!(!s.editing);
            assert!(!s.dirty);
        }
        assert!(screen.status.contains("按 Enter 进入编辑"));

        // 进入编辑态后空格生效
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .agent
                .auto_tool_call,
            !orig_tool
        );
    }

    /// 动作行（服务商 / Skill）的 Enter 仍是原行为：设默认 / 开详情弹窗。
    #[tokio::test]
    async fn settings_action_row_enter_still_opens_dialog() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        {
            let s = screen.settings.as_mut().unwrap();
            s.providers_draft.providers.insert(
                "prov_alpha".into(),
                cyber_core::ProviderConfig {
                    kind: "openai".into(),
                    base_url: "https://api.alpha.com".into(),
                    api_key: "sk-test".into(),
                    model: "model-alpha".into(),
                    max_tokens: 4096,
                    temperature: 0.7,
                    price: None,
                    models: std::collections::HashMap::new(),
                    chat_endpoint: None,
                    models_endpoint: None,
                    thinking: None,
                },
            );
            s.set_tab(SettingsTab::Providers);
            s.selected_row = 0;
            // 预设一个错误默认值：Enter 必须把它改成当前高亮行
            s.config_draft.agent.default_provider = "not_a_provider".into();
        }
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        {
            let s = screen.settings.as_ref().unwrap();
            let expected = s.providers_draft.sorted_names()[0].clone();
            assert_eq!(s.config_draft.agent.default_provider, expected);
            assert!(!s.editing, "动作行 Enter 不得进入编辑态");
        }

        // ToolsMcp 的 Skill 行：Enter 打开只读详情弹窗
        {
            let s = screen.settings.as_mut().unwrap();
            s.skills = vec![SkillSummary {
                name: "skill-alpha".into(),
                ..Default::default()
            }];
            s.set_tab(SettingsTab::ToolsMcp);
            s.selected_row = 3;
        }
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        {
            let s = screen.settings.as_ref().unwrap();
            assert!(s.skill_detail.is_some());
            assert!(!s.editing);
        }
    }

    /// Tab / 数字键切换标签页时清除编辑态。
    #[tokio::test]
    async fn settings_tab_switch_clears_editing() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        screen.settings.as_mut().unwrap().selected_row = 2;

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert!(screen.settings.as_ref().unwrap().editing);
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Tab,
            KeyModifiers::NONE,
        );
        {
            let s = screen.settings.as_ref().unwrap();
            assert_eq!(s.tab, SettingsTab::UiWorkflow);
            assert!(!s.editing);
        }

        // 数字直达键同样清除编辑态
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert!(screen.settings.as_ref().unwrap().editing);
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('3'),
            KeyModifiers::NONE,
        );
        {
            let s = screen.settings.as_ref().unwrap();
            assert_eq!(s.tab, SettingsTab::Subagents);
            assert!(!s.editing);
        }
    }

    /// `focused_row_is_value` 的行分区：EnvMemory 前三段判定与 `max_row` 同式。
    #[test]
    fn focused_row_is_value_partitions_value_and_action_rows() {
        let mut state = CliSettingsState::new(&Config::default(), &ProvidersConfig::default());
        assert!(state.focused_row_is_value()); // AgentModel 行 0

        state.selected_row = 1;
        assert!(!state.focused_row_is_value()); // 只读展示行（对应模型）
        state.selected_row = 9;
        assert!(!state.focused_row_is_value()); // 只读展示行（识图专属模型）

        state.tab = SettingsTab::Providers;
        assert!(!state.focused_row_is_value());
        state.tab = SettingsTab::Toolbox;
        assert!(!state.focused_row_is_value());

        // EnvMemory：env_slots(1) + mem_slots(1) 之前是值行，其后（记忆列表）是动作行
        state.tab = SettingsTab::EnvMemory;
        state.selected_row = 0;
        assert!(state.focused_row_is_value());
        state.selected_row = 1;
        assert!(state.focused_row_is_value());
        state.selected_row = 2;
        assert!(!state.focused_row_is_value());

        state.config_draft.env.vars = vec![cyber_core::EnvVar {
            key: "A".into(),
            value: "1".into(),
            sensitive: false,
        }];
        state.config_draft.memory.rules = vec![cyber_core::MemoryRule {
            enabled: true,
            scope: "both".into(),
            prompt: "p".into(),
        }];
        state.selected_row = 1;
        assert!(state.focused_row_is_value()); // 记忆规则行
        state.selected_row = 2;
        assert!(!state.focused_row_is_value()); // 记忆列表行

        // ToolsMcp：0/1 是开关值行，2（MCP 控制台）起是动作行
        state.tab = SettingsTab::ToolsMcp;
        state.selected_row = 1;
        assert!(state.focused_row_is_value());
        state.selected_row = 2;
        assert!(!state.focused_row_is_value());
    }

    #[tokio::test]
    async fn settings_in_place_value_adjustments() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );

        // Tab 1: AgentModel
        // Row 2: Thinking intensity
        screen.settings.as_mut().unwrap().selected_row = 2;
        let orig_effort = screen
            .settings
            .as_ref()
            .unwrap()
            .config_draft
            .agent
            .thinking_intensity;
        // 只读态：先 Enter 进入编辑态
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Right,
            KeyModifiers::NONE,
        );
        // 编辑态内改值后 Enter 退出编辑
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        let new_effort = screen
            .settings
            .as_ref()
            .unwrap()
            .config_draft
            .agent
            .thinking_intensity;
        assert_ne!(orig_effort, new_effort);
        assert!(screen.settings.as_ref().unwrap().dirty);

        // Row 4: Auto tool call toggle
        screen.settings.as_mut().unwrap().selected_row = 4;
        let orig_tool = screen
            .settings
            .as_ref()
            .unwrap()
            .config_draft
            .agent
            .auto_tool_call;
        // 只读态：空格被吞掉，Enter 进入编辑态后才可切换
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .agent
                .auto_tool_call,
            !orig_tool
        );

        // Row 5: Max steps increment
        screen.settings.as_mut().unwrap().selected_row = 5;
        let orig_steps = screen
            .settings
            .as_ref()
            .unwrap()
            .config_draft
            .agent
            .max_steps;
        // 只读态：先 Enter 进入编辑态
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Right,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .agent
                .max_steps,
            orig_steps + 10
        );

        // Shift+Right -> +50
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Right,
            KeyModifiers::SHIFT,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .agent
                .max_steps,
            orig_steps + 60
        );
        // 退出编辑态
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );

        // Reset tab via 'r'
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('r'),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .agent
                .max_steps,
            500
        );

        // Tab 2: UiWorkflow
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('2'),
            KeyModifiers::NONE,
        );
        // Row 0: Theme
        screen.settings.as_mut().unwrap().selected_row = 0;
        let orig_theme = screen
            .settings
            .as_ref()
            .unwrap()
            .config_draft
            .ui
            .theme
            .clone();
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Right,
            KeyModifiers::NONE,
        );
        assert_ne!(
            orig_theme,
            screen.settings.as_ref().unwrap().config_draft.ui.theme
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );

        // Tab 3: Subagents
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('3'),
            KeyModifiers::NONE,
        );
        screen.settings.as_mut().unwrap().selected_row = 0;
        let orig_sub = screen
            .settings
            .as_ref()
            .unwrap()
            .config_draft
            .agent
            .subagents
            .enabled;
        // Enter 进入编辑态，Space 切换开关，Enter 退出编辑态
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .agent
                .subagents
                .enabled,
            !orig_sub
        );
    }

    #[tokio::test]
    async fn settings_agent_model_retry_configuration() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        // 打开设置面板
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::AgentModel
        );
        assert_eq!(
            SettingsTab::AgentModel.max_row(screen.settings.as_ref().unwrap()),
            11
        );

        // 行 10: 异常重试次数（默认 5 次）
        screen.settings.as_mut().unwrap().selected_row = 10;
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .agent
                .retry_attempts,
            5
        );
        // 只读态：先 Enter 进入编辑态
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        // 按 3 次 Right 增加到 8 次
        for _ in 0..3 {
            settings_key_with_runner(
                &mut screen,
                &mut runner_opt,
                KeyCode::Right,
                KeyModifiers::NONE,
            );
        }
        // 退出编辑态
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .agent
                .retry_attempts,
            8
        );

        // 行 11: 重试时间间隔（默认 3 秒）
        screen.settings.as_mut().unwrap().selected_row = 11;
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .agent
                .retry_delay_secs,
            3
        );
        // 只读态：先 Enter 进入编辑态
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        // 按 2 次 Right 增加到 5 秒
        for _ in 0..2 {
            settings_key_with_runner(
                &mut screen,
                &mut runner_opt,
                KeyCode::Right,
                KeyModifiers::NONE,
            );
        }
        // 退出编辑态
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .agent
                .retry_delay_secs,
            5
        );

        // 保存设置 (Ctrl+S)
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(
            screen.panel,
            Some(Panel::Settings),
            "保存后面板应保持打开以便继续查看或调整"
        );
        assert!(screen.settings.is_some());
        assert!(!screen.settings.as_ref().unwrap().dirty);
        assert_eq!(
            runner_opt.as_ref().unwrap().ctx.config.agent.retry_attempts,
            8
        );
        assert_eq!(
            runner_opt
                .as_ref()
                .unwrap()
                .ctx
                .config
                .agent
                .retry_delay_secs,
            5
        );

        // 按 Esc 显式退出设置面板
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, None, "按 Esc 显式关闭面板");
        assert!(screen.settings.is_none());
    }

    #[tokio::test]
    async fn settings_unsaved_discard_guard() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );

        // Make dirty
        screen.settings.as_mut().unwrap().selected_row = 4;
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert!(screen.settings.as_ref().unwrap().dirty);

        // First Esc: triggers pending_discard_confirm, does not close
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, Some(Panel::Settings));
        assert!(screen.settings.as_ref().unwrap().pending_discard_confirm);

        // Verify rendered modal
        let rendered = render(&mut screen, 100, 30);
        assert!(rendered.contains("Enter / Ctrl+S"));

        // Arrow key cancels discard confirm
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Left,
            KeyModifiers::NONE,
        );
        assert!(!screen.settings.as_ref().unwrap().pending_discard_confirm);
        assert_eq!(screen.panel, Some(Panel::Settings));

        // First Esc again -> pending_discard_confirm
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert!(screen.settings.as_ref().unwrap().pending_discard_confirm);

        // Second Esc -> confirms discard and exits
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, None);
        assert!(screen.settings.is_none());
        assert_eq!(screen.status, "已放弃未保存修改");
    }

    #[tokio::test]
    async fn settings_save_and_persist() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );

        // Change thinking intensity to Max
        screen
            .settings
            .as_mut()
            .unwrap()
            .config_draft
            .agent
            .thinking_intensity = ThinkingIntensity::Max;
        screen.settings.as_mut().unwrap().dirty = true;

        // Save with Ctrl+S
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        );

        assert_eq!(
            screen.panel,
            Some(Panel::Settings),
            "Ctrl+S 保存后面板应保持打开"
        );
        assert!(screen.settings.is_some());
        assert!(
            !screen.settings.as_ref().unwrap().dirty,
            "保存后 dirty 应重置为 false"
        );
        assert!(screen.status.contains("已保存并立即生效"));

        // 验证视觉徽标更新为已实时保存，按钮提示包含 Esc 关闭
        let rendered = render(&mut screen, 100, 30);
        let compact = rendered.replace(' ', "");
        assert!(compact.contains("配置已实时保存"));
        assert!(compact.contains("Esc关闭"));
        // Verify memory state updated
        let r = runner_opt.as_ref().unwrap();
        assert_eq!(
            r.ctx.config.agent.thinking_intensity,
            ThinkingIntensity::Max
        );
        assert_eq!(screen.effort, ThinkingIntensity::Max);

        // Verify disk file updated
        let saved_cfg: Config =
            toml::from_str(&std::fs::read_to_string(&r.ctx.paths.config_file).unwrap()).unwrap();
        assert_eq!(saved_cfg.agent.thinking_intensity, ThinkingIntensity::Max);

        // Subsequent Esc exits cleanly without discard modal
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, None);
        assert!(screen.settings.is_none());
    }

    #[tokio::test]
    async fn settings_discard_confirm_save_and_exit() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );

        // Modify a setting to make it dirty
        screen.settings.as_mut().unwrap().selected_row = 10;
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Right,
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert!(screen.settings.as_ref().unwrap().dirty);

        // Esc triggers discard confirm modal
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, Some(Panel::Settings));
        assert!(screen.settings.as_ref().unwrap().pending_discard_confirm);

        // In confirm modal, pressing 's' (or Enter / Ctrl+S) saves and exits (exit = true)
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('s'),
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, None, "弹窗确认保存后应退出面板");
        assert!(screen.settings.is_none());
        assert!(screen.status.contains("已保存并立即生效"));
    }

    #[tokio::test]
    async fn settings_single_key_s_saves_in_place() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );

        // Modify a setting to make it dirty
        screen.settings.as_mut().unwrap().selected_row = 10;
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Right,
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert!(screen.settings.as_ref().unwrap().dirty);

        // Press 's' (without Ctrl) saves in place
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('s'),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.panel,
            Some(Panel::Settings),
            "'s' 保存后面板应保持打开"
        );
        assert!(screen.settings.is_some());
        assert!(
            !screen.settings.as_ref().unwrap().dirty,
            "dirty 状态应已清除"
        );
        assert!(screen.status.contains("已保存并立即生效"));

        // Then Esc closes directly
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, None);
        assert!(screen.settings.is_none());
    }

    #[tokio::test]
    async fn settings_panel_rendering_all_tabs() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );

        for tab_idx in 1..=7 {
            let ch = char::from_digit(tab_idx, 10).unwrap();
            settings_key_with_runner(
                &mut screen,
                &mut runner_opt,
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            );

            for (w, h) in [(80, 24), (100, 30), (120, 35)] {
                let rendered = render(&mut screen, w, h);
                assert!(
                    rendered.contains("Settings"),
                    "w={w}, h={h} should contain title"
                );
                assert!(
                    rendered.contains(ch),
                    "w={w}, h={h} should display active tab number {ch}"
                );
                assert!(
                    rendered.contains("Esc"),
                    "tab={tab_idx}, w={w}, h={h} should display bottom buttons"
                );
            }
        }
    }

    #[tokio::test]
    async fn settings_tab_7_focus_following_and_rendering() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );

        // Switch to Tab 7
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('7'),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::StorageSystem
        );

        // Render at narrow 80x24, 100x30, and wide 120x35
        for (w, h) in [(80, 24), (100, 30), (120, 35)] {
            let rendered = render(&mut screen, w, h);
            assert!(
                rendered.contains("7.") || rendered.contains("存储"),
                "w={w}, h={h} must show tab 7 in tab bar"
            );
            assert!(
                rendered.contains("Log Level") || rendered.contains("日志"),
                "w={w}, h={h} must show tab 7 content"
            );
        }
    }

    #[tokio::test]
    async fn settings_provider_and_env_vertical_focus_following() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );

        // 1. Providers Tab vertical focus following
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('5'),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::Providers
        );

        // Add 8 providers so content is long (2 + 8 * 3 = 26 lines)
        let settings = screen.settings.as_mut().unwrap();
        for i in 1..=8 {
            let name = format!("prov_test_{:02}", i);
            settings.providers_draft.providers.insert(
                name.clone(),
                cyber_core::ProviderConfig {
                    kind: "openai".into(),
                    base_url: format!("https://api.p{:02}.com", i),
                    api_key: "sk-test".into(),
                    model: format!("model-{:02}", i),
                    max_tokens: 4096,
                    temperature: 0.7,
                    price: None,
                    models: std::collections::HashMap::new(),
                    chat_endpoint: None,
                    models_endpoint: None,
                    thinking: None,
                },
            );
        }

        // Navigate down to the last provider
        let last_idx = screen
            .settings
            .as_ref()
            .unwrap()
            .providers_draft
            .providers
            .len()
            - 1;
        for _ in 0..last_idx {
            settings_key_with_runner(
                &mut screen,
                &mut runner_opt,
                KeyCode::Down,
                KeyModifiers::NONE,
            );
        }
        assert_eq!(screen.settings.as_ref().unwrap().selected_row, last_idx);

        // In height 20 (content area height only ~9 lines), prov_test_08 must be rendered and visible!
        let rendered_bottom = render(&mut screen, 80, 20);
        assert!(
            rendered_bottom.contains("prov_test_08"),
            "focused bottom provider must be visible in scrolled view: {rendered_bottom}"
        );
        assert!(
            rendered_bottom.contains("model-08"),
            "focused bottom provider details must be visible: {rendered_bottom}"
        );

        // Navigate up to provider 0
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Home,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.settings.as_ref().unwrap().selected_row, 0);

        let rendered_top = render(&mut screen, 80, 20);
        let first_name = &screen
            .settings
            .as_ref()
            .unwrap()
            .providers_draft
            .sorted_names()[0];
        assert!(
            rendered_top.contains(first_name),
            "focused top provider must be visible: {rendered_top}"
        );

        // 2. Env & Memory Tab vertical focus following
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('6'),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::EnvMemory
        );

        let settings = screen.settings.as_mut().unwrap();
        for i in 1..=8 {
            settings.config_draft.env.vars.push(cyber_core::EnvVar {
                key: format!("VAR_KEY_{:02}", i),
                value: format!("val_{:02}", i),
                sensitive: false,
            });
        }
        for i in 1..=4 {
            settings
                .config_draft
                .memory
                .rules
                .push(cyber_core::MemoryRule {
                    enabled: true,
                    scope: "both".into(),
                    prompt: format!("memory rule prompt {:02}", i),
                });
        }

        // Navigate to the last memory rule
        let total_items =
            settings.config_draft.env.vars.len() + settings.config_draft.memory.rules.len();
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::End,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().selected_row,
            total_items - 1
        );

        let rendered_mem = render(&mut screen, 80, 20);
        assert!(
            rendered_mem.contains("memory rule prompt 04"),
            "focused last memory rule must be visible: {rendered_mem}"
        );
    }

    #[test]
    fn test_tools_mcp_max_row_with_skills() {
        let mut state = CliSettingsState::new(&Config::default(), &ProvidersConfig::default());
        assert_eq!(SettingsTab::ToolsMcp.max_row(&state), 2);

        state.skills = vec![
            SkillSummary {
                name: "skill-1".into(),
                ..Default::default()
            },
            SkillSummary {
                name: "skill-2".into(),
                ..Default::default()
            },
        ];
        assert_eq!(SettingsTab::ToolsMcp.max_row(&state), 4);

        state.skills = (0..115)
            .map(|i| SkillSummary {
                name: format!("skill-{i}"),
                ..Default::default()
            })
            .collect();
        assert_eq!(SettingsTab::ToolsMcp.max_row(&state), 117);
    }

    #[tokio::test]
    async fn test_skill_detail_modal_navigation_and_rendering() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        // Open settings (F3)
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );

        // Switch to Tab 4 (ToolsMcp)
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('4'),
            KeyModifiers::NONE,
        );
        assert_eq!(screen.settings.as_ref().unwrap().tab, SettingsTab::ToolsMcp);

        // Inject 3 skills with full details
        let sample_skills = vec![
            SkillSummary {
                name: "test-skill-1".into(),
                source: "项目级".into(),
                description: "Skill 1 description".into(),
                triggers: vec!["t1".into(), "t2".into()],
                tools: vec!["bash".into()],
                allowed_tools: vec!["bash".into()],
                disable_model_invocation: false,
                path: "/path/to/skill1/SKILL.md".into(),
                body: "# Skill 1\n\nInstructions for skill 1\n```bash\necho 1\n```".into(),
            },
            SkillSummary {
                name: "test-skill-2".into(),
                source: "全局".into(),
                description: "Skill 2 description".into(),
                triggers: vec!["test2".into()],
                tools: vec!["python".into()],
                allowed_tools: vec![],
                disable_model_invocation: true,
                path: "/path/to/skill2/SKILL.md".into(),
                body: "# Skill 2\n\nInstructions for skill 2".into(),
            },
            SkillSummary {
                name: "test-skill-3".into(),
                source: "项目级".into(),
                description: "Skill 3 description".into(),
                triggers: vec![],
                tools: vec![],
                allowed_tools: vec![],
                disable_model_invocation: false,
                path: "/path/to/skill3/SKILL.md".into(),
                body: String::new(),
            },
        ];
        screen.settings.as_mut().unwrap().skills = sample_skills;

        // Move to row 3 (first skill)
        screen.settings.as_mut().unwrap().selected_row = 3;

        // Render tab content: verify skill list is visible
        let rendered_list = render(&mut screen, 100, 30);
        assert!(rendered_list.contains("test-skill-1"));
        assert!(rendered_list.contains("Skill 1 description"));

        // Press Enter to open skill detail modal
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        let detail = screen.settings.as_ref().unwrap().skill_detail.as_ref();
        assert!(detail.is_some());
        assert_eq!(detail.unwrap().skill_index, 0);
        assert_eq!(detail.unwrap().scroll, 0);

        // Render with modal open: verify title, metadata, and markdown body
        let rendered_modal = render(&mut screen, 100, 30);
        let compact = rendered_modal.replace(' ', "");
        assert!(compact.contains("Skill详情[1/3]:test-skill-1"));
        assert!(rendered_modal.contains("Instructions for skill 1"));
        assert!(rendered_modal.contains("t1, t2"));

        // Scroll down in modal
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Down,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .skill_detail
                .as_ref()
                .unwrap()
                .scroll,
            1
        );

        // Scroll up in modal
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Up,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .skill_detail
                .as_ref()
                .unwrap()
                .scroll,
            0
        );

        // Right arrow switches to next skill (index 1) and syncs selected_row
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Right,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .skill_detail
                .as_ref()
                .unwrap()
                .skill_index,
            1
        );
        assert_eq!(screen.settings.as_ref().unwrap().selected_row, 4);

        // Modal shows skill 2 content
        let rendered_modal2 = render(&mut screen, 100, 30);
        let compact2 = rendered_modal2.replace(' ', "");
        assert!(compact2.contains("Skill详情[2/3]:test-skill-2"));
        assert!(compact2.contains("仅显式调用"));
        // Left arrow switches back to skill 0 and syncs selected_row
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Left,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .skill_detail
                .as_ref()
                .unwrap()
                .skill_index,
            0
        );
        assert_eq!(screen.settings.as_ref().unwrap().selected_row, 3);

        // Esc closes modal and returns to list with selected_row preserved
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert!(screen.settings.as_ref().unwrap().skill_detail.is_none());
        assert_eq!(screen.settings.as_ref().unwrap().selected_row, 3);
        assert_eq!(screen.panel, Some(Panel::Settings));

        // Press 'o' to re-open modal
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('o'),
            KeyModifiers::NONE,
        );
        assert!(screen.settings.as_ref().unwrap().skill_detail.is_some());

        // Press 'q' to close modal
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        );
        assert!(screen.settings.as_ref().unwrap().skill_detail.is_none());
        assert_eq!(screen.settings.as_ref().unwrap().selected_row, 3);
    }

    // ── 子代理面板 / 后台任务面板 ───────────────────────────────────────────

    fn ctrl_key(screen: &mut CliScreen, code: KeyCode) {
        let (broker, _requests) = PermissionBroker::interactive();
        let (events, _rx) = mpsc::unbounded_channel();
        handle_key(
            screen,
            KeyEvent::new(code, KeyModifiers::CONTROL),
            &mut None,
            &Arc::new(broker),
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
    }

    fn seed_subagent(screen: &mut CliScreen, name: &str, status: SubagentStatus) -> u64 {
        let run_id = screen.subagents.start(name);
        screen.subagents.append_line(
            run_id,
            format!("tool shell args: {{\"command\":\"nmap {name}\"}}"),
        );
        screen
            .subagents
            .append_line(run_id, "shell => PORT 80 http".to_string());
        match status {
            SubagentStatus::Running => {}
            SubagentStatus::Completed => {
                screen.subagents.finish(
                    run_id,
                    status,
                    Some(format!("{name} 发现 2 个漏洞")),
                    None,
                );
            }
            other => {
                screen
                    .subagents
                    .finish(run_id, other, None, Some("failed".into()));
            }
        }
        run_id
    }

    #[tokio::test]
    async fn subagent_panel_list_enter_view_esc_back() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let run_id = seed_subagent(&mut screen, "recon_ports", SubagentStatus::Running);

        ctrl_key(&mut screen, KeyCode::Char('g'));
        assert_eq!(screen.panel, Some(Panel::Subagents));
        assert_eq!(screen.subagent_panel.as_ref().unwrap().selected, 0);
        assert!(screen.subagent_view.is_none());

        // Enter → 面板关闭，覆盖对话视图打开（跟随底部）
        page_key(&mut screen, KeyCode::Enter);
        assert_eq!(screen.panel, None);
        assert!(screen.subagent_panel.is_none());
        let view = screen.subagent_view.as_ref().unwrap();
        assert_eq!(view.run_id, run_id);
        assert!(view.follow_bottom);
        assert_eq!(view.scroll, 0);

        // Esc → 返回对话（视图关闭，面板不打开）
        page_key(&mut screen, KeyCode::Esc);
        assert!(screen.subagent_view.is_none());
        assert_eq!(screen.panel, None);

        // Ctrl+G 可重开列表
        ctrl_key(&mut screen, KeyCode::Char('g'));
        assert_eq!(screen.panel, Some(Panel::Subagents));
        assert!(screen.subagent_view.is_none());
        drop(runner);
    }

    #[tokio::test]
    async fn jobs_panel_state_machine_and_mutual_exclusion() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        // 真实后台任务：k 按下后 detached task 收到 kill 信号并转 Killed。
        let long_cmd = if cfg!(windows) {
            "ping 127.0.0.1 -n 60"
        } else {
            "sleep 60"
        };
        let job_id = cyber_agent::background::spawn_shell_job(
            &screen.background,
            std::env::temp_dir(),
            Vec::new(),
            long_cmd.into(),
        )
        .unwrap();

        // 先开子代理面板，再开后台面板 → 互斥（子代理状态被清空）
        ctrl_key(&mut screen, KeyCode::Char('g'));
        assert_eq!(screen.panel, Some(Panel::Subagents));
        ctrl_key(&mut screen, KeyCode::Char('b'));
        assert_eq!(screen.panel, Some(Panel::Jobs));
        assert!(screen.subagent_panel.is_none());

        page_key(&mut screen, KeyCode::Enter);
        assert_eq!(screen.jobs_panel.as_ref().unwrap().detail, Some(job_id));
        page_key(&mut screen, KeyCode::Esc);
        assert!(screen.jobs_panel.as_ref().unwrap().detail.is_none());

        // k 终止 Running → detached task 收到信号后状态转 Killed
        page_key(&mut screen, KeyCode::Char('k'));
        for _ in 0..300 {
            let status = screen
                .background
                .snapshot()
                .into_iter()
                .find(|job| job.id == job_id)
                .map(|job| job.status)
                .unwrap_or(JobStatus::Running);
            if status.is_finished() {
                assert!(
                    matches!(status, JobStatus::Killed),
                    "应为 Killed：{status:?}"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let status = screen
            .background
            .snapshot()
            .into_iter()
            .find(|job| job.id == job_id)
            .map(|job| job.status)
            .unwrap_or(JobStatus::Running);
        assert!(
            matches!(status, JobStatus::Killed),
            "k 后应转 Killed：{status:?}"
        );

        // r 清理已结束 → 列表清空且 selected clamp
        page_key(&mut screen, KeyCode::Char('r'));
        assert!(screen.background.snapshot().is_empty());
        assert_eq!(screen.jobs_panel.as_ref().unwrap().selected, 0);

        // 再按 Ctrl+B 关闭
        ctrl_key(&mut screen, KeyCode::Char('b'));
        assert_eq!(screen.panel, None);
        assert!(screen.jobs_panel.is_none());
        drop(runner);
    }

    #[tokio::test]
    async fn subagent_panel_renders_list_and_empty_states() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);

        // 空态
        ctrl_key(&mut screen, KeyCode::Char('g'));
        let rendered = render(&mut screen, 80, 24).replace(' ', "");
        assert!(rendered.contains("子代理(Subagents)"), "{rendered}");
        assert!(rendered.contains("暂无子代理运行记录"), "{rendered}");

        // 列表（徽标着色、列布局、提示行）
        seed_subagent(&mut screen, "recon_ports", SubagentStatus::Running);
        seed_subagent(&mut screen, "vuln_scan", SubagentStatus::Completed);
        seed_subagent(
            &mut screen,
            "step_limited",
            SubagentStatus::StepLimitReached,
        );
        seed_subagent(&mut screen, "loop_broken", SubagentStatus::LoopDetected);
        let rendered = render(&mut screen, 100, 24).replace(' ', "");
        assert!(rendered.contains("recon_ports"), "{rendered}");
        assert!(rendered.contains("⏱运行中"), "{rendered}");
        assert!(rendered.contains("vuln_scan"), "{rendered}");
        assert!(rendered.contains("✓完成"), "{rendered}");
        assert!(rendered.contains("step_limited"), "{rendered}");
        assert!(rendered.contains("⚠步数耗尽"), "{rendered}");
        assert!(rendered.contains("loop_broken"), "{rendered}");
        assert!(rendered.contains("✗死循环"), "{rendered}");
        assert!(rendered.contains("Enter覆盖对话查看"), "{rendered}");
        drop(runner);
    }

    #[tokio::test]
    async fn subagent_view_renders_like_conversation() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let run_id = seed_subagent(&mut screen, "recon_ports", SubagentStatus::Running);
        screen
            .subagents
            .append_line(run_id, "thinking: 先枚举端口".into());
        screen
            .subagents
            .append_line(run_id, "thinking: 再扫描服务".into());
        screen.subagents.append_line(run_id, "结论：完成".into());
        screen.message("You", "主对话消息", ACCENT);

        ctrl_key(&mut screen, KeyCode::Char('g'));
        page_key(&mut screen, KeyCode::Enter);
        assert!(screen.subagent_view.is_some());
        let rendered = render(&mut screen, 100, 30).replace(' ', "");
        // 转录以对话样式渲染：连续 thinking 行合并为单个 Thinking 块 + 工具卡 + markdown 正文
        assert_eq!(rendered.matches("Thinking").count(), 1, "{rendered}");
        assert!(rendered.contains("先枚举端口"), "{rendered}");
        assert!(rendered.contains("再扫描服务"), "{rendered}");
        assert!(rendered.contains("Shell"), "{rendered}");
        assert!(rendered.contains("PORT80"), "{rendered}");
        assert!(rendered.contains("结论：完成"), "{rendered}");
        // 覆盖生效：主对话消息不渲染
        assert!(!rendered.contains("主对话消息"), "{rendered}");
        drop(runner);
    }

    #[tokio::test]
    async fn subagent_view_merges_thinking_and_renders_markdown() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let run_id = screen.subagents.start("thinking");
        // 流式推理逐词成行 → 必须合并为一个 Thinking 块（一个头）
        for word in ["Let", "me", "just", "count", "端口", "node_", "modules"] {
            screen
                .subagents
                .append_line(run_id, format!("thinking: {word}"));
        }
        // 跨行 markdown：粗体 + 行内代码 + 列表（合并渲染才生效）
        screen
            .subagents
            .append_line(run_id, "**加粗** 与 `代码`".into());
        screen.subagents.append_line(run_id, "- 列表项".into());

        ctrl_key(&mut screen, KeyCode::Char('g'));
        page_key(&mut screen, KeyCode::Enter);
        let raw = render(&mut screen, 100, 30);
        let rendered = raw.replace(' ', "");
        assert_eq!(rendered.matches("Thinking").count(), 1, "{rendered}");
        // 词片段应连接为同一行流式文本（而非一词一行）
        assert!(rendered.contains("Letmejustcount端口"), "{rendered}");
        // 事件边界切在词中间（node_ / modules）：拼接后不得插入空格
        assert!(raw.contains("node_modules"), "{raw}");
        assert!(!raw.contains("node_ modules"), "{raw}");
        assert!(rendered.contains("加粗"), "{rendered}");
        assert!(!rendered.contains("**"), "{rendered}");
        assert!(rendered.contains("列表项"), "{rendered}");
        drop(runner);
    }

    #[tokio::test]
    async fn subagent_view_renders_multiline_thinking_with_newlines_and_lists() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let run_id = screen.subagents.start("thinking_multiline");
        screen
            .subagents
            .append_line(run_id, "thinking: 思考第一段分析目标\n".into());
        screen.subagents.append_line(run_id, "thinking: \n".into());
        screen
            .subagents
            .append_line(run_id, "thinking: - 步骤一：探测端口\n".into());
        screen
            .subagents
            .append_line(run_id, "thinking: - 步骤二：漏洞验证\n".into());

        ctrl_key(&mut screen, KeyCode::Char('g'));
        page_key(&mut screen, KeyCode::Enter);
        let raw = render(&mut screen, 100, 30);
        let rendered = raw.replace(' ', "");
        assert_eq!(rendered.matches("Thinking").count(), 1, "{rendered}");
        assert!(rendered.contains("思考第一段分析目标"), "{rendered}");
        assert!(rendered.contains("步骤一：探测端口"), "{rendered}");
        assert!(rendered.contains("步骤二：漏洞验证"), "{rendered}");
        drop(runner);
    }

    #[test]
    fn subagent_view_renders_multiline_thinking_and_skips_empty() {
        let archive = SubagentArchive::default();
        let id = archive.start("test");
        archive.append_line(id, "thinking: Analysis of target:\n".into());
        archive.append_line(id, "thinking: \n".into());
        archive.append_line(id, "thinking: - Item 1\n".into());
        archive.append_line(id, "thinking: - Item 2\n".into());
        let snapshot = archive.snapshot();
        let run = &snapshot[0];
        let lines = build_subagent_view_lines(run, true, 80);
        let texts: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(texts.iter().any(|t| t.contains("Thinking")));
        assert!(texts.iter().any(|t| t.contains("Analysis of target")));
        assert!(texts.iter().any(|t| t.contains("Item 1")));
        assert!(texts.iter().any(|t| t.contains("Item 2")));
        assert!(lines.len() >= 4, "lines count: {}", lines.len());

        let empty_id = archive.start("empty");
        archive.append_line(empty_id, "thinking: \n".into());
        archive.append_line(empty_id, "thinking:    \n".into());
        let snapshot = archive.snapshot();
        let empty_run = snapshot.iter().find(|r| r.id == empty_id).unwrap();
        let empty_lines = build_subagent_view_lines(empty_run, true, 80);
        let empty_texts: Vec<String> = empty_lines.iter().map(|l| l.to_string()).collect();
        assert!(!empty_texts.iter().any(|t| t.contains("Thinking")));
    }

    #[tokio::test]
    async fn subagent_view_renders_markdown_tables() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let run_id = screen.subagents.start("tables");
        // 最终答案含 GFM 表格：应渲染为对齐列而非裸 | 管道
        screen.subagents.append_line(run_id, "## 结果".into());
        screen
            .subagents
            .append_line(run_id, "| 扩展名 | 文件数 |".into());
        screen
            .subagents
            .append_line(run_id, "|--------|--------|".into());
        screen.subagents.append_line(run_id, "| .rs | 116 |".into());
        screen.subagents.append_line(run_id, "| .py | 23 |".into());

        ctrl_key(&mut screen, KeyCode::Char('g'));
        page_key(&mut screen, KeyCode::Enter);
        let rendered = render(&mut screen, 100, 30).replace(' ', "");
        assert!(rendered.contains("├"), "{rendered}"); // 表头分隔线
        assert!(rendered.contains("扩展名"), "{rendered}");
        assert!(rendered.contains(".rs"), "{rendered}");
        assert!(rendered.contains("116"), "{rendered}");
        // 裸管道表行已被表格渲染消费（logo 自带 |，不能整体断言无 |）
        assert!(!rendered.contains("|扩展名|"), "{rendered}");
        drop(runner);
    }

    #[tokio::test]
    async fn subagent_view_pairs_parallel_tools_and_shows_args() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let run_id = screen.subagents.start("parallel");
        // 并行调用：两个 tool 行相邻、结果行交错（不紧跟调用行）
        screen.subagents.append_line(
            run_id,
            r#"tool shell args: {"command":"nmap -sV 10.0.0.5"}"#.into(),
        );
        screen
            .subagents
            .append_line(run_id, r#"tool read args: {"path":"notes.md"}"#.into());
        screen
            .subagents
            .append_line(run_id, "shell => PORT 80 http".into());
        screen.subagents.append_line(run_id, "read => hello".into());
        screen
            .subagents
            .finish(run_id, SubagentStatus::Completed, None, None);

        ctrl_key(&mut screen, KeyCode::Char('g'));
        page_key(&mut screen, KeyCode::Enter);
        let rendered = render(&mut screen, 100, 30).replace(' ', "");
        // 参数（标题/正文）与结果（卡片正文）都展示；结果行被卡片消费不再裸显
        assert!(rendered.contains("nmap-sV10.0.0.5"), "{rendered}");
        assert!(rendered.contains("notes.md"), "{rendered}");
        assert!(rendered.contains("PORT80"), "{rendered}");
        assert!(rendered.contains("hello"), "{rendered}");
        assert!(!rendered.contains("=>"), "{rendered}");
        drop(runner);
    }

    #[tokio::test]
    async fn subagent_view_shows_args_for_pending_tools() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let run_id = screen.subagents.start("pending");
        // 运行中被查看：工具无结果行 → Pending 卡，参数必须可见
        screen.subagents.append_line(
            run_id,
            r#"tool shell args: {"command":"nmap -sV 10.0.0.5"}"#.into(),
        );

        ctrl_key(&mut screen, KeyCode::Char('g'));
        page_key(&mut screen, KeyCode::Enter);
        let rendered = render(&mut screen, 100, 30).replace(' ', "");
        assert!(rendered.contains("◇Shell"), "{rendered}");
        assert!(rendered.contains("nmap-sV10.0.0.5"), "{rendered}");
        drop(runner);
    }

    #[tokio::test]
    async fn subagent_view_keeps_updating_at_transcript_cap() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let run_id = screen.subagents.start("cap");
        for i in 0..500 {
            screen
                .subagents
                .append_line(run_id, format!("cap line {i:03}"));
        }
        ctrl_key(&mut screen, KeyCode::Char('g'));
        page_key(&mut screen, KeyCode::Enter);
        let rendered = render(&mut screen, 80, 30).replace(' ', "");
        assert!(rendered.contains("capline499"), "{rendered}");

        // 触发上限丢最旧（行数恒 500）：视图必须仍重建并显示新行
        screen.subagents.append_line(run_id, "cap line 500".into());
        let rendered = render(&mut screen, 80, 30).replace(' ', "");
        assert!(rendered.contains("capline500"), "{rendered}");
        assert!(!rendered.contains("capline000"), "{rendered}");
        drop(runner);
    }

    #[test]
    fn subagent_lines_fingerprint_detects_any_content_change() {
        let base = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            subagent_lines_fingerprint(&base),
            subagent_lines_fingerprint(&base.clone())
        );
        let appended = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_ne!(
            subagent_lines_fingerprint(&base),
            subagent_lines_fingerprint(&appended)
        );
        // 上限丢最旧：行数不变但内容变化
        let dropped_front = vec!["b".to_string(), "c".to_string()];
        assert_ne!(
            subagent_lines_fingerprint(&base),
            subagent_lines_fingerprint(&dropped_front)
        );
        let mutated = vec!["a".to_string(), "x".to_string()];
        assert_ne!(
            subagent_lines_fingerprint(&base),
            subagent_lines_fingerprint(&mutated)
        );
    }

    #[tokio::test]
    async fn subagent_view_scroll_and_follow() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let run_id = seed_subagent(&mut screen, "bulk", SubagentStatus::Running);
        for i in 0..60 {
            screen
                .subagents
                .append_line(run_id, format!("token line {i:02}"));
        }
        ctrl_key(&mut screen, KeyCode::Char('g'));
        page_key(&mut screen, KeyCode::Enter);

        // ↑ 脱离跟随并上滚
        page_key(&mut screen, KeyCode::Up);
        let view = screen.subagent_view.as_ref().unwrap();
        assert!(!view.follow_bottom);
        assert_eq!(view.scroll, 1);

        // End 回底恢复跟随
        page_key(&mut screen, KeyCode::End);
        let view = screen.subagent_view.as_ref().unwrap();
        assert!(view.follow_bottom);
        assert_eq!(view.scroll, 0);

        // 新行到达：跟随底部自动显示最新行（旧行滚出视口）
        screen.subagents.append_line(run_id, "NEWEST LINE".into());
        let rendered = render(&mut screen, 80, 30).replace(' ', "");
        assert!(rendered.contains("NEWESTLINE"), "{rendered}");
        assert!(!rendered.contains("tokenline00"), "{rendered}");
        drop(runner);
    }

    #[tokio::test]
    async fn subagent_view_keys_do_not_break_composer() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        seed_subagent(&mut screen, "recon_ports", SubagentStatus::Running);
        ctrl_key(&mut screen, KeyCode::Char('g'));
        page_key(&mut screen, KeyCode::Enter);

        screen.insert_text("hi");
        page_key(&mut screen, KeyCode::Char('x'));
        assert!(
            screen.input.lines()[0].contains("hix"),
            "{}",
            screen.input.lines().join("\n")
        );
        assert!(screen.subagent_view.is_some());
        drop(runner);
    }

    #[tokio::test]
    async fn subagent_view_locks_content_when_scrolled_up() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let run_id = seed_subagent(&mut screen, "bulk", SubagentStatus::Running);
        for i in 0..60 {
            screen
                .subagents
                .append_line(run_id, format!("token line {i:02}"));
        }
        ctrl_key(&mut screen, KeyCode::Char('g'));
        page_key(&mut screen, KeyCode::Enter);
        render(&mut screen, 80, 30);

        // ↑ 上滚 5 行（脱离跟随态）
        for _ in 0..5 {
            page_key(&mut screen, KeyCode::Up);
        }
        let view_0 = screen.subagent_view.as_ref().unwrap();
        assert!(!view_0.follow_bottom);
        assert_eq!(view_0.scroll, 5);
        let rendered_0 = render(&mut screen, 80, 30);

        // 新行到达
        for i in 60..70 {
            screen
                .subagents
                .append_line(run_id, format!("token line {i:02}"));
        }
        let rendered_1 = render(&mut screen, 80, 30);
        let view_1 = screen.subagent_view.as_ref().unwrap();
        // 增量自动补偿给 scroll，确保内容保持静止
        assert!(!view_1.follow_bottom);
        assert_eq!(view_1.scroll, 15);
        let content_0: Vec<_> = rendered_0
            .lines()
            .filter(|l| l.contains("token line"))
            .map(|l| l.trim_end_matches(['│', '█', ' ']))
            .collect();
        let content_1: Vec<_> = rendered_1
            .lines()
            .filter(|l| l.contains("token line"))
            .map(|l| l.trim_end_matches(['│', '█', ' ']))
            .collect();
        assert_eq!(content_0, content_1);

        // 按 End 恢复跟随
        page_key(&mut screen, KeyCode::End);
        let view_end = screen.subagent_view.as_ref().unwrap();
        assert!(view_end.follow_bottom);
        assert_eq!(view_end.scroll, 0);
        drop(runner);
    }

    #[tokio::test]
    async fn jobs_panel_renders_list_detail_and_empty_states() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);

        ctrl_key(&mut screen, KeyCode::Char('b'));
        let rendered = render(&mut screen, 80, 24).replace(' ', "");
        assert!(rendered.contains("后台任务(BackgroundJobs)"), "{rendered}");
        assert!(rendered.contains("暂无后台任务"), "{rendered}");

        let (shell_id, _kill_rx) = screen
            .background
            .start(JobKind::Shell, "cargo build".into());
        screen
            .background
            .append_line(shell_id, "Finished release".into());
        screen
            .background
            .set_status(shell_id, JobStatus::Finished(0));
        let rendered = render(&mut screen, 80, 24).replace(' ', "");
        assert!(rendered.contains("cargobuild"), "{rendered}");
        assert!(rendered.contains("✓完成(0)"), "{rendered}");
        assert!(rendered.contains("k终止"), "{rendered}");
        assert!(rendered.contains("r清理"), "{rendered}");

        // 详情（跟随底部 + 输出行）
        page_key(&mut screen, KeyCode::Enter);
        let rendered = render(&mut screen, 100, 30).replace(' ', "");
        assert!(rendered.contains("Finishedrelease"), "{rendered}");
        assert!(rendered.contains("跟随底部"), "{rendered}");
        drop(runner);
    }

    #[tokio::test]
    async fn bg_shell_command_busy_usable_and_job_visible() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner = Some(owner);
        // 模拟 busy：runner 被 take（spawn_turn 会快照 background_env）
        let owned = runner.take().unwrap();
        screen.busy = true;
        screen.background_env = Some((owned.cwd.clone(), Vec::<(String, String)>::new()));

        screen.insert_text("/bg shell echo cli_bg_test");
        let (broker, _requests) = PermissionBroker::interactive();
        let (events, _rx) = mpsc::unbounded_channel();
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &Arc::new(broker),
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        let jobs = screen.background.snapshot();
        assert_eq!(jobs.len(), 1, "busy 时 /bg shell 应可用");
        assert!(jobs[0].name.contains("echo cli_bg_test"));
        // 等待任务结束并检查输出
        for _ in 0..200 {
            let status = screen
                .background
                .snapshot()
                .into_iter()
                .find(|job| job.id == jobs[0].id)
                .map(|job| job.status)
                .unwrap_or(JobStatus::Running);
            if status.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let job = screen
            .background
            .snapshot()
            .into_iter()
            .find(|job| job.id == jobs[0].id)
            .unwrap();
        assert!(
            job.lines.iter().any(|line| line.contains("cli_bg_test")),
            "输出应含 cli_bg_test：{:?}",
            job.lines
        );
    }

    #[tokio::test]
    async fn bg_run_busy_returns_busy_message() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.busy = true;
        let mut runner = None;
        screen.insert_text("/bg run scan the target");
        let (broker, _requests) = PermissionBroker::interactive();
        let (events, _rx) = mpsc::unbounded_channel();
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &Arc::new(broker),
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(
            history(&screen).contains("AI 正在运行"),
            "busy 时 /bg run 应返回忙碌提示：{}",
            history(&screen)
        );
        assert!(screen.background.snapshot().is_empty());
    }

    #[tokio::test]
    async fn bg_run_completes_and_injects_system_entry_once() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner = Some(owner);
        screen.insert_text("/bg run background probe task");
        let (broker, _requests) = PermissionBroker::interactive();
        let (events, _rx) = mpsc::unbounded_channel();
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &Arc::new(broker),
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(
            screen.background.snapshot().len(),
            1,
            "/bg run 应启动后台子代理"
        );
        // 等待后台子代理完成（mock provider tool-loop）
        for _ in 0..500 {
            let finished = screen
                .background
                .snapshot()
                .iter()
                .all(|job| job.status.is_finished());
            if finished {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (broker, _requests) = PermissionBroker::interactive();
        let permissions = Arc::new(broker);
        let (events, _rx) = mpsc::unbounded_channel();
        let mut active: Option<ActiveTurn> = None;
        let mut cancel: Option<oneshot::Sender<()>> = None;
        drain_background_completions(
            &mut screen,
            &mut runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        // 完成后应自动开启思考状态！
        assert!(screen.busy);
        assert!(active.is_some());
        // 等待自动开启的回合完成
        let (restored, _) = active.take().unwrap().await.unwrap();
        runner = Some(restored);
        screen.busy = false;
        let owner = runner.as_ref().unwrap();
        assert!(
            owner.entries.iter().any(
                |entry| matches!(entry, ChatEntry::System(text) if text.contains("后台子代理 #")
                    && text.contains("完成"))
            ),
            "注入消息应含 后台子代理 #：{:?}",
            owner.entries
        );
        // 再次 drain 不重复注入（reported 标记）
        let before = owner.entries.len();
        drain_background_completions(
            &mut screen,
            &mut runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        let after = runner.as_ref().unwrap().entries.len();
        assert_eq!(before, after, "reported 标记应阻止重复注入");
        assert!(!screen.busy);
        assert!(active.is_none());
    }

    #[tokio::test]
    async fn background_subagent_result_renders_markdown_tables_and_formatting() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner = Some(owner);

        let (job_id, _kill_rx) = screen
            .background
            .start(JobKind::Subagent, "loc-analysis".into());
        let run_id = screen.subagents.start("loc-analysis");
        screen.background.link_archive(job_id, run_id);

        let table_markdown = "已执行脚本，结果如下：\n\n## 统计结果\n\n| 扩展名 | 文件数 |\n|--------|--------|\n| .rs | 116 |\n| .py | 17 |";
        screen.subagents.finish(
            run_id,
            SubagentStatus::Completed,
            Some(table_markdown.into()),
            None,
        );
        screen.background.set_status(job_id, JobStatus::Finished(0));

        let (broker, _requests) = PermissionBroker::interactive();
        let permissions = Arc::new(broker);
        let (events, _rx) = mpsc::unbounded_channel();
        let mut active: Option<ActiveTurn> = None;
        let mut cancel: Option<oneshot::Sender<()>> = None;
        drain_background_completions(
            &mut screen,
            &mut runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();

        // 渲染对话主界面
        let rendered = render(&mut screen, 100, 30).replace(' ', "");
        // 断言：System 注入结果按 Markdown 渲染，包含表头分隔线与对齐列，而不是裸管道
        assert!(rendered.contains("System"), "{rendered}");
        assert!(rendered.contains("后台子代理#"), "{rendered}");
        assert!(rendered.contains("统计结果"), "{rendered}");
        assert!(rendered.contains("├"), "表格应渲染分隔线：{rendered}");
        assert!(rendered.contains(".rs"), "{rendered}");
        assert!(rendered.contains("116"), "{rendered}");
        assert!(
            !rendered.contains("|扩展名|"),
            "表格不应残留裸管道：{rendered}"
        );
        if let Some(act) = active {
            let _ = act.await;
        }
    }

    #[tokio::test]
    async fn background_shell_job_completion_injects_prompt_and_starts_turn() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner = Some(owner);

        let (job_id, _kill_rx) = screen
            .background
            .start(JobKind::Shell, "ping 127.0.0.1".into());
        screen
            .background
            .append_line(job_id, "64 bytes from 127.0.0.1: icmp_seq=1 ttl=64".into());
        screen
            .background
            .append_line(job_id, "64 bytes from 127.0.0.1: icmp_seq=2 ttl=64".into());
        screen.background.set_status(job_id, JobStatus::Finished(0));

        let (broker, _requests) = PermissionBroker::interactive();
        let permissions = Arc::new(broker);
        let (events, _rx) = mpsc::unbounded_channel();
        let mut active: Option<ActiveTurn> = None;
        let mut cancel: Option<oneshot::Sender<()>> = None;

        // 空闲状态下 drain
        drain_background_completions(
            &mut screen,
            &mut runner,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();

        // 必须自动开启思考状态！
        assert!(screen.busy);
        assert!(active.is_some());

        // 渲染对话界面：应展示后台命令完成通知与代码块
        let rendered = render(&mut screen, 100, 30).replace(' ', "");
        assert!(rendered.contains("后台任务#"), "{rendered}");
        assert!(rendered.contains("ping127.0.0.1"), "{rendered}");
        assert!(rendered.contains("icmp_seq=1"), "{rendered}");

        // 等待回合执行完成，验证 prompt 注入生效
        let (restored, outcome) = active.take().unwrap().await.unwrap();
        runner = Some(restored);
        screen.busy = false;
        assert!(outcome.error.is_none());
        let owner = runner.as_ref().unwrap();
        assert!(owner.entries.iter().any(|entry| matches!(
            entry,
            ChatEntry::User(prompt) if prompt.contains("[后台命令执行完成通知]")
                && prompt.contains("ping 127.0.0.1")
        )));
    }
    #[tokio::test]
    async fn subagents_stop_slash_command_terminates_running_subagents() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner = Some(owner);

        // 1. 无运行中子代理时 stop
        screen.insert_text("/subagents stop");
        let (broker, _requests) = PermissionBroker::interactive();
        let (events, _rx) = mpsc::unbounded_channel();
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &Arc::new(broker),
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(history(&screen).contains("当前没有运行中的子代理"));

        // 2. 存在运行中的后台子代理时执行 /subagents stop
        let (job_id, _kill_rx) = screen.background.start(JobKind::Subagent, "worker".into());
        let run_id = screen.subagents.start("worker");
        screen.background.link_archive(job_id, run_id);

        screen.insert_text("/subagents stop");
        let (broker, _requests) = PermissionBroker::interactive();
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &Arc::new(broker),
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(history(&screen).contains("已终止 1 个运行中的子代理"));
        assert_eq!(
            screen.subagents.snapshot()[0].status,
            SubagentStatus::Killed
        );

        // 3. busy 状态下执行 /subagents stop #id
        screen.busy = true;
        let (job_id2, _rx) = screen.background.start(JobKind::Subagent, "worker2".into());
        let run_id2 = screen.subagents.start("worker2");
        screen.background.link_archive(job_id2, run_id2);

        screen.insert_text(&format!("/subagents stop #{run_id2}"));
        let (broker, _requests) = PermissionBroker::interactive();
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner,
            &Arc::new(broker),
            &events,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert!(history(&screen).contains(&format!("已终止子代理 #{run_id2}")));
        assert_eq!(
            screen.subagents.snapshot()[1].status,
            SubagentStatus::Killed
        );
    }
    #[tokio::test]
    async fn test_scrollbar_rendering_and_mouse_drag_scrolling() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);

        for i in 0..50 {
            screen
                .messages
                .push(Line::raw(format!("line message {i:02}")));
        }

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();

        assert!(screen.max_scroll > 0);
        assert_eq!(screen.scroll, 0);
        assert!(screen.auto_scroll);
        let area = screen.history_area;
        assert!(area.height > 0);

        let buffer = terminal.backend().buffer();
        let scrollbar_col = area.right().saturating_sub(1);
        let mut has_track = false;
        let mut has_thumb = false;
        for row in area.top()..area.bottom() {
            let sym = buffer[(scrollbar_col, row)].symbol();
            if sym == "│" {
                has_track = true;
            } else if sym == "█" {
                has_thumb = true;
            }
        }
        assert!(has_track || has_thumb);
        assert!(has_thumb, "初始底部态应有滑块");

        let bottom_sym = buffer[(scrollbar_col, area.bottom() - 1)].symbol();
        assert_eq!(bottom_sym, "█", "底部态滑块应在最底端");

        // 1. 模拟鼠标点击滑动条顶部
        screen.apply_scrollbar_click(area.top());
        assert_eq!(screen.scroll, screen.max_scroll);
        assert!(!screen.auto_scroll);

        terminal.draw(|frame| screen.draw(frame)).unwrap();
        let buffer_top = terminal.backend().buffer();
        let top_sym = buffer_top[(scrollbar_col, area.top())].symbol();
        assert_eq!(top_sym, "█", "点击顶部后滑块应在最顶端");

        // 2. 模拟鼠标拖拽至中点
        let mid_row = area.top() + area.height / 2;
        screen.apply_scrollbar_click(mid_row);
        let half_max = screen.max_scroll / 2;
        let diff = (screen.scroll as isize - half_max as isize).abs();
        assert!(
            diff <= 3,
            "拖拽至中点 scroll ({}) 应该接近 50% ({})",
            screen.scroll,
            half_max
        );
        assert!(!screen.auto_scroll);

        // 3. 模拟拖拽回底部
        screen.apply_scrollbar_click(area.bottom() - 1);
        assert_eq!(screen.scroll, 0);
        assert!(screen.auto_scroll);

        drop(runner);
    }

    #[tokio::test]
    async fn test_subagent_view_scrollbar_rendering_and_mouse_click() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let run_id = screen.subagents.start("agent-scrollbar-test");
        for i in 0..50 {
            screen
                .subagents
                .append_line(run_id, format!("agent token {i:02}"));
        }
        screen.subagent_view = Some(SubagentViewState {
            run_id,
            follow_bottom: true,
            scroll: 0,
            tools_expanded: false,
            built: None,
            viewport: WrappedViewport {
                padding: 1,
                ..WrappedViewport::default()
            },
            max_scroll: 0,
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();

        let area = screen.history_area;
        let view = screen.subagent_view.as_ref().unwrap();
        assert!(view.max_scroll > 0);
        assert_eq!(view.scroll, 0);
        assert!(view.follow_bottom);

        let buffer = terminal.backend().buffer();
        let scrollbar_col = area.right().saturating_sub(1);
        let bottom_sym = buffer[(scrollbar_col, area.bottom() - 1)].symbol();
        assert_eq!(bottom_sym, "█", "子代理视图底部态滑块应在最底端");

        // 点击顶部
        screen.apply_scrollbar_click(area.top());
        let view = screen.subagent_view.as_ref().unwrap();
        assert_eq!(view.scroll, view.max_scroll);
        assert!(!view.follow_bottom);

        // 点击底部恢复
        screen.apply_scrollbar_click(area.bottom() - 1);
        let view = screen.subagent_view.as_ref().unwrap();
        assert_eq!(view.scroll, 0);
        assert!(view.follow_bottom);

        drop(runner);
    }

    #[test]
    fn test_wrapped_viewport_coord_and_text_extraction() {
        let mut vp = WrappedViewport {
            padding: 1,
            ..WrappedViewport::default()
        };
        let lines = vec![
            Line::from("Hello, world!"),
            Line::from("\tTabbed line"),
            Line::from("你好，世界！Cyber Master"),
        ];
        vp.update(&lines, 20);

        let c0 = vp.coord_from_viewport(0, 0).unwrap();
        assert_eq!(c0, ContentCoord::new(0, 0));

        let c1 = vp.coord_from_viewport(0, 1).unwrap();
        assert_eq!(c1, ContentCoord::new(0, 0));

        let c2 = vp.coord_from_viewport(0, 2).unwrap();
        assert_eq!(c2, ContentCoord::new(0, 1));

        let c_tab1 = vp.coord_from_viewport(1, 1).unwrap();
        assert_eq!(c_tab1, ContentCoord::new(1, 0));
        let c_tab2 = vp.coord_from_viewport(1, 4).unwrap();
        assert_eq!(c_tab2, ContentCoord::new(1, 0));
        let c_tab3 = vp.coord_from_viewport(1, 5).unwrap();
        assert_eq!(c_tab3, ContentCoord::new(1, 1));

        let c_cjk0 = vp.coord_from_viewport(2, 1).unwrap();
        assert_eq!(c_cjk0, ContentCoord::new(2, 0));
        let c_cjk1 = vp.coord_from_viewport(2, 2).unwrap();
        assert_eq!(c_cjk1, ContentCoord::new(2, 0));
        let c_cjk2 = vp.coord_from_viewport(2, 3).unwrap();
        assert_eq!(c_cjk2, ContentCoord::new(2, 1));

        let sel = TextSelection {
            anchor: ContentCoord::new(0, 7),
            cursor: ContentCoord::new(2, 2),
            selecting: false,
        };
        let extracted = crate::selection::extract_text(&vp.source, &sel);
        assert_eq!(extracted, "world!\n\tTabbed line\n你好");
    }

    #[tokio::test]
    async fn test_selection_invariance_on_scroll_stream_and_resize() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);

        for i in 0..10 {
            screen.message(
                "Assistant",
                &format!("Message content number {i:02} for testing invariance."),
                ACCENT,
            );
        }

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();

        let anchor = ContentCoord::new(2, 8);
        let cursor = ContentCoord::new(2, 25);
        screen.selection = Some(TextSelection {
            anchor,
            cursor,
            selecting: false,
        });

        let text_initial =
            crate::selection::extract_text(&screen.messages, screen.selection.as_ref().unwrap());
        assert_eq!(text_initial, "content number 00");

        // 1. 模拟滚轮上滚滚动 5 行
        screen.scroll = 5;
        screen.auto_scroll = false;
        terminal.draw(|frame| screen.draw(frame)).unwrap();

        let sel = screen.selection.as_ref().unwrap();
        assert_eq!(sel.anchor, anchor);
        assert_eq!(sel.cursor, cursor);
        let text_after_scroll = crate::selection::extract_text(&screen.messages, sel);
        assert_eq!(text_after_scroll, "content number 00");

        // 2. 模拟大模型流式追加 50 条新消息
        for i in 10..60 {
            screen.message(
                "Assistant",
                &format!("Streaming new token and chunk {i:02}"),
                ACCENT,
            );
        }
        terminal.draw(|frame| screen.draw(frame)).unwrap();

        let sel_after_stream = screen.selection.as_ref().unwrap();
        assert_eq!(sel_after_stream.anchor, anchor);
        assert_eq!(sel_after_stream.cursor, cursor);
        let text_after_stream = crate::selection::extract_text(&screen.messages, sel_after_stream);
        assert_eq!(text_after_stream, "content number 00");

        // 3. 模拟窗口缩放 (80 -> 40 列宽) 触发重新折行
        terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();

        let sel_after_resize = screen.selection.as_ref().unwrap();
        assert_eq!(sel_after_resize.anchor, anchor);
        assert_eq!(sel_after_resize.cursor, cursor);
        let text_after_resize = crate::selection::extract_text(&screen.messages, sel_after_resize);
        assert_eq!(text_after_resize, "content number 00");

        drop(runner);
    }

    #[tokio::test]
    async fn test_mouse_edge_drag_dynamic_acceleration() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);

        for i in 0..100 {
            screen.message("Assistant", &format!("Line {i:03} content text"), ACCENT);
        }

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();

        let area = screen.history_area;
        assert!(screen.max_scroll >= 20);

        let mid_row = area.y + area.height / 2;
        screen.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 5,
            row: mid_row,
            modifiers: KeyModifiers::NONE,
        });

        assert!(screen.selection.is_some());
        let sel = screen.selection.as_ref().unwrap();
        assert!(sel.selecting);
        let init_anchor = sel.anchor;

        // 1. 向上拖拽超出顶边界 1 行 (row = area.y - 1)
        let old_scroll = screen.scroll;
        screen.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: area.x + 5,
            row: area.y.saturating_sub(1),
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(screen.scroll, old_scroll + 1);

        // 2. 向上拖拽超出顶边界 5 行 (row = area.y - 5)
        let old_scroll_5 = screen.scroll;
        screen.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: area.x + 5,
            row: area.y.saturating_sub(5),
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(screen.scroll, old_scroll_5 + 5);

        let current_sel = screen.selection.as_ref().unwrap();
        assert_eq!(current_sel.anchor, init_anchor);
        assert!(current_sel.cursor.line_idx <= init_anchor.line_idx);

        // 3. 向下拖拽超出底边界 3 行
        let old_scroll_down = screen.scroll;
        screen.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: area.x + 5,
            row: area.y + area.height + 2,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(screen.scroll, old_scroll_down.saturating_sub(3));

        drop(runner);
    }

    #[tokio::test]
    async fn test_mouse_shift_click_cross_screen_expansion() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);

        for i in 0..120 {
            screen.message("Assistant", &format!("Numbered message row {i:03}"), ACCENT);
        }

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();

        let area = screen.history_area;

        screen.scroll = screen.max_scroll;
        screen.auto_scroll = false;
        terminal.draw(|frame| screen.draw(frame)).unwrap();

        let anchor_coord = ContentCoord::new(2, 0);
        screen.selection = Some(TextSelection::new(anchor_coord));

        // 模拟滚轮向下滚动多屏直达底部，让 anchor 彻底滚出视口几屏之远
        screen.scroll = 0;
        terminal.draw(|frame| screen.draw(frame)).unwrap();

        // 按住 Shift 在当前视口靠后位置发送 Down(MouseButton::Left)
        screen.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.right() - 5,
            row: area.bottom() - 2,
            modifiers: KeyModifiers::SHIFT,
        });

        let sel = screen.selection.as_ref().unwrap();
        assert_eq!(sel.anchor, anchor_coord);
        assert!(sel.cursor.line_idx > 40);

        let multi_page_text = crate::selection::extract_text(&screen.messages, sel);
        assert!(multi_page_text.contains("Numbered message row 000"));
        assert!(multi_page_text.contains("Numbered message row 040"));
        assert_eq!(screen.status, "✔ 已扩展选区并复制到剪贴板");

        drop(runner);
    }

    #[tokio::test]
    async fn test_mouse_click_then_shift_click_selection() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);

        screen.message(
            "Assistant",
            "Quick brown fox jumps over the lazy dog.",
            ACCENT,
        );

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();

        let area = screen.history_area;

        // 1. 普通单击 (Down + Up, 无 Shift)
        screen.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 3,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        screen.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x + 3,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!screen.has_selection());
        assert!(screen.selection.as_ref().is_some_and(|s| s.is_empty()));

        // 2. 按住 Shift 在另一位置单击 (Down + Up, 带 Shift)
        screen.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 15,
            row: area.y + 1,
            modifiers: KeyModifiers::SHIFT,
        });
        screen.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x + 15,
            row: area.y + 1,
            modifiers: KeyModifiers::SHIFT,
        });
        assert!(screen.has_selection());
        let sel = screen.selection.as_ref().unwrap();
        assert_eq!(sel.anchor.line_idx, 1);
        let text = crate::selection::extract_text(&screen.messages, sel);
        assert!(!text.is_empty());
        assert_eq!(screen.status, "✔ 已选中文本并复制到剪贴板");

        drop(runner);
    }

    #[tokio::test]
    async fn test_mouse_click_clear_drag_selection_and_key_shortcuts() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);

        screen.message(
            "Assistant",
            "Quick brown fox jumps over the lazy dog.",
            ACCENT,
        );

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();

        let area = screen.history_area;

        // 1. 单击同一坐标：选区应当自动清除
        screen.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 3,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(screen.selection.is_some());
        screen.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x + 3,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!screen.has_selection());
        assert!(screen.selection.as_ref().is_some_and(|s| s.is_empty()));

        // 2. 拖拽不同坐标：生成非空选区并复制
        screen.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 2,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        screen.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: area.x + 15,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        screen.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x + 15,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(screen.has_selection());
        assert_eq!(screen.status, "✔ 已选中文本并复制到剪贴板");

        // 3. 有选区时按 Ctrl+C：复制并清除选区，不退出程序
        let mut runner_opt = Some(runner);
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _) = mpsc::unbounded_channel();
        let mut active = None;
        let mut cancel = None;

        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let res = handle_key(
            &mut screen,
            ctrl_c,
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        );
        assert!(!res.unwrap());
        assert!(!screen.has_selection());

        // 4. 有选区时按 Esc：直接清除选区
        screen.selection = Some(TextSelection {
            anchor: ContentCoord::new(0, 0),
            cursor: ContentCoord::new(0, 5),
            selecting: false,
        });
        assert!(screen.has_selection());

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let res_esc = handle_key(
            &mut screen,
            esc,
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        );
        assert!(!res_esc.unwrap());
        assert!(!screen.has_selection());
    }

    fn model_picker_state(providers: cyber_core::ProvidersConfig) -> CliModelPickerState {
        CliModelPickerState {
            focus_models: false,
            provider_selected: 0,
            model_selected: 0,
            providers,
            default_provider: String::new(),
            models: Vec::new(),
            provider_scroll: 0,
            model_scroll: 0,
            probing_model: None,
            fetching: None,
            fetch_error: None,
            fetched: false,
            fetch_id: 0,
        }
    }

    /// 接口拉取结果归并/丢弃语义：成功 → 接口模型 ∪ 本地配置且光标不跳；失败 → 保留本地
    /// 配置并记录原因；`fetch_id` / 服务商不匹配的串台结果一律丢弃。
    #[test]
    fn model_picker_fetch_merges_api_models_and_drops_stale_results() {
        let mut providers = cyber_core::ProvidersConfig::default();
        let mut alpha = cyber_core::ProviderConfig {
            kind: "openai".into(),
            model: "alpha-current".into(),
            ..Default::default()
        };
        alpha
            .models
            .insert("alpha-local".into(), cyber_core::ModelConfig::default());
        providers.providers.insert("alpha".into(), alpha);
        providers.providers.insert(
            "beta".into(),
            cyber_core::ProviderConfig {
                kind: "openai".into(),
                model: "beta-current".into(),
                ..Default::default()
            },
        );
        let mut state = model_picker_state(providers);
        state.refresh_models();
        assert_eq!(state.models, vec!["alpha-current", "alpha-local"]);

        // 串台结果（fetch_id 不匹配）与错位服务商结果都被丢弃，拉取中标记保留。
        state.fetching = Some("alpha".into());
        state.fetch_id = 1;
        state.deliver_fetch(ModelFetchResult {
            fetch_id: 0,
            provider: "alpha".into(),
            result: Ok(vec!["stale".into()]),
        });
        state.deliver_fetch(ModelFetchResult {
            fetch_id: 1,
            provider: "beta".into(),
            result: Ok(vec!["beta-live".into()]),
        });
        assert_eq!(state.models, vec!["alpha-current", "alpha-local"]);
        assert_eq!(state.fetching.as_deref(), Some("alpha"));
        assert!(state.fetch_error.is_none());

        // 拉取失败：本地配置列表保留 + 失败原因可见，不再显示「拉取中」。
        state.deliver_fetch(ModelFetchResult {
            fetch_id: 1,
            provider: "alpha".into(),
            result: Err("GET /v1/models 失败 401".into()),
        });
        assert_eq!(state.models, vec!["alpha-current", "alpha-local"]);
        assert!(state.fetching.is_none());
        assert_eq!(
            state.fetch_error.as_deref(),
            Some("GET /v1/models 失败 401")
        );

        // 成功：接口模型并入列表（排序去重），光标停在其原先选中的模型上。
        state.model_selected = 1; // alpha-local
        state.fetching = Some("alpha".into());
        state.fetch_error = None;
        state.fetch_id = 2;
        state.deliver_fetch(ModelFetchResult {
            fetch_id: 2,
            provider: "alpha".into(),
            result: Ok(vec!["z-live".into(), "alpha-local".into()]),
        });
        assert_eq!(state.models, vec!["alpha-current", "alpha-local", "z-live"]);
        assert_eq!(state.models[state.model_selected], "alpha-local");
        assert!(state.fetching.is_none() && state.fetch_error.is_none());

        // 空结果视为失败（不把右栏清空）。
        state.fetching = Some("alpha".into());
        state.fetch_id = 3;
        state.deliver_fetch(ModelFetchResult {
            fetch_id: 3,
            provider: "alpha".into(),
            result: Ok(Vec::new()),
        });
        assert_eq!(state.models, vec!["alpha-current", "alpha-local", "z-live"]);
        assert_eq!(state.fetch_error.as_deref(), Some("接口未返回任何模型"));
    }

    /// 面板只在左栏 `Enter`（选定 provider）后联网拉取：打开与切换 provider 都只显示本地配置；
    /// `base_url` 已含 `/v1` 时只请求 `{base}/v1/models`（不得补成 `/v1/v1`）。
    #[tokio::test]
    async fn model_picker_fetches_only_after_provider_confirm() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut owner = crate::headless::tests::test_runner().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        owner.ctx.providers.providers.clear();
        let versioned = cyber_core::ProviderConfig {
            kind: "openai".into(),
            base_url: format!("http://{addr}/v1"),
            api_key: "test-only-key".into(),
            model: "cfg-model".into(),
            ..Default::default()
        };
        owner
            .ctx
            .providers
            .providers
            .insert("aaa-stub".into(), versioned);
        let bare = cyber_core::ProviderConfig {
            kind: "openai".into(),
            base_url: format!("http://{addr}"),
            api_key: "test-only-key".into(),
            model: "cfg-model".into(),
            ..Default::default()
        };
        owner
            .ctx
            .providers
            .providers
            .insert("bbb-stub".into(), bare);
        owner.ctx.config.agent.default_provider = "aaa-stub".into();

        // 两个 provider 各一次请求：第一次必须命中 /v1/models（base_url 已含版本段），
        // 第二次（未含版本段）先按 {base}/models 走。
        let server = tokio::spawn(async move {
            let mut paths = Vec::new();
            for idx in 0..2usize {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 2048];
                let n = socket.read(&mut buf).await.unwrap();
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                paths.push(
                    request
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string(),
                );
                let body = match idx {
                    0 => r#"{"data":[{"id":"live-a"},{"id":"live-b"}]}"#.to_string(),
                    _ => r#"{"data":[{"id":"live-c"}]}"#.to_string(),
                };
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                let _ = socket.shutdown().await;
            }
            paths
        });

        let mut screen = CliScreen::new(&owner);
        let (tx, mut rx) = mpsc::unbounded_channel::<ModelFetchResult>();
        screen.model_fetch_tx = Some(tx);
        screen.model_picker = Some(CliModelPickerState::from_view(&owner.view()));
        screen.panel = Some(Panel::ModelPicker);
        screen.refresh_model_picker_local();

        // 打开面板：只显示本地配置清单，不联网。
        let state = screen.model_picker.as_ref().unwrap();
        assert!(state.fetching.is_none(), "打开面板不得自动联网拉取");
        assert!(!state.fetched);
        assert_eq!(state.models, vec!["cfg-model".to_string()]);
        let frame = render(&mut screen, 120, 30).replace(' ', "");
        assert!(frame.contains("按Enter选定provider并从接口拉取"), "{frame}");
        assert!(frame.contains("cfg-model"), "{frame}");

        let mut runner_opt = Some(owner);
        // 左栏 Enter（选定 provider）→ 才联网拉取。
        handle_model_picker_key(
            &mut screen,
            &mut runner_opt,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        let state = screen.model_picker.as_ref().unwrap();
        assert_eq!(state.fetching.as_deref(), Some("aaa-stub"));
        // 拉取中：状态条可见，本地配置模型仍在列表中（不因拉取而清空）。
        let frame = render(&mut screen, 120, 30).replace(' ', "");
        assert!(frame.contains("正在从接口拉取"), "{frame}");
        assert!(frame.contains("cfg-model"), "{frame}");

        let fetch = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("拉取应在超时前返回结果")
            .expect("拉取结果通道不应关闭");
        screen.model_picker.as_mut().unwrap().deliver_fetch(fetch);
        let state = screen.model_picker.as_ref().unwrap();
        assert_eq!(
            state.models,
            vec!["cfg-model".to_string(), "live-a".into(), "live-b".into()]
        );
        assert!(state.fetching.is_none() && state.fetch_error.is_none());
        assert!(state.fetched, "拿到接口结果后必须标记为已拉取");
        let frame = render(&mut screen, 120, 30).replace(' ', "");
        assert!(frame.contains("live-a"), "{frame}");
        assert!(!frame.contains("正在从接口拉取"), "{frame}");
        assert!(!frame.contains("按Enter选定provider"), "{frame}");

        // 回到左栏（Enter 已把焦点切到模型栏），再切到下一个 provider → 只换本地清单，不联网。
        handle_model_picker_key(
            &mut screen,
            &mut runner_opt,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        )
        .unwrap();
        assert!(!screen.model_picker.as_ref().unwrap().focus_models);
        handle_model_picker_key(
            &mut screen,
            &mut runner_opt,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        )
        .unwrap();
        let state = screen.model_picker.as_ref().unwrap();
        assert_eq!(state.provider_selected, 1);
        assert!(
            state.fetching.is_none(),
            "切换 provider 不得自动联网；需再按 Enter"
        );
        assert!(!state.fetched);
        assert_eq!(state.models, vec!["cfg-model".to_string()]);

        // 再按 Enter → 才拉取该 provider 的模型。
        handle_model_picker_key(
            &mut screen,
            &mut runner_opt,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        let state = screen.model_picker.as_ref().unwrap();
        assert_eq!(state.fetching.as_deref(), Some("bbb-stub"));
        let fetch = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("Enter 后应拉取")
            .expect("拉取结果通道不应关闭");
        screen.model_picker.as_mut().unwrap().deliver_fetch(fetch);
        let state = screen.model_picker.as_ref().unwrap();
        assert_eq!(state.models, vec!["cfg-model".to_string(), "live-c".into()]);

        let paths = server.await.unwrap();
        assert_eq!(paths, vec!["/v1/models".to_string(), "/models".to_string()]);

        if let Some(r) = runner_opt {
            let _ = std::fs::remove_dir_all(r.cwd);
        }
    }

    #[tokio::test]
    async fn model_picker_enter_and_r_key_refetch_from_api() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut owner = crate::headless::tests::test_runner().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut paths = Vec::new();
            for body in [
                r#"{"data":[{"id":"live-a"}]}"#,
                r#"{"data":[{"id":"live-b"}]}"#,
                r#"{"data":[{"id":"live-c"}]}"#,
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 2048];
                let n = socket.read(&mut buf).await.unwrap();
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                paths.push(
                    request
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string(),
                );
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                let _ = socket.shutdown().await;
            }
            paths
        });

        owner.ctx.providers.providers.insert(
            "aaa-stub".into(),
            cyber_core::ProviderConfig {
                kind: "openai".into(),
                base_url: format!("http://{addr}/v1"),
                api_key: "test-only-key".into(),
                model: "cfg-model".into(),
                ..Default::default()
            },
        );
        owner.ctx.config.agent.default_provider = "aaa-stub".into();

        let mut screen = CliScreen::new(&owner);
        let (tx, mut rx) = mpsc::unbounded_channel::<ModelFetchResult>();
        screen.model_fetch_tx = Some(tx);
        screen.model_picker = Some(CliModelPickerState::from_view(&owner.view()));
        screen.panel = Some(Panel::ModelPicker);
        screen.refresh_model_picker_local();

        let mut runner_opt = Some(owner);

        // 打开面板：只有本地清单，不联网
        let state = screen.model_picker.as_ref().unwrap();
        assert!(state.fetching.is_none(), "打开面板不得自动拉取");
        assert_eq!(state.models, vec!["cfg-model".to_string()]);

        // 左栏 Enter（选定 provider）→ 拉取 #1
        let fetch_id = screen.model_picker.as_ref().unwrap().fetch_id;
        assert!(!screen.model_picker.as_ref().unwrap().focus_models);
        handle_model_picker_key(
            &mut screen,
            &mut runner_opt,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        let state = screen.model_picker.as_ref().unwrap();
        assert!(state.focus_models, "Enter 应切到模型栏");
        assert_eq!(state.fetching.as_deref(), Some("aaa-stub"));
        assert_ne!(state.fetch_id, fetch_id, "Enter 必须发起拉取");

        // 拉取进行中再按 r / 再回左栏按 Enter 都不重复发起
        let in_flight_id = screen.model_picker.as_ref().unwrap().fetch_id;
        for code in [KeyCode::Char('r'), KeyCode::Tab, KeyCode::Enter] {
            handle_model_picker_key(
                &mut screen,
                &mut runner_opt,
                KeyEvent::new(code, KeyModifiers::NONE),
            )
            .unwrap();
        }
        assert_eq!(
            screen.model_picker.as_ref().unwrap().fetch_id,
            in_flight_id,
            "拉取进行中不得重复发起请求"
        );

        let fetch = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("Enter 后应发起拉取")
            .expect("通道不应关闭");
        screen.model_picker.as_mut().unwrap().deliver_fetch(fetch);
        let state = screen.model_picker.as_ref().unwrap();
        assert_eq!(state.models, vec!["cfg-model".to_string(), "live-a".into()]);
        assert!(state.fetched);

        // 回到左栏并再次 Enter（重新选定同一 provider）→ 重新从接口拉取
        {
            let state = screen.model_picker.as_mut().unwrap();
            state.focus_models = false;
        }
        handle_model_picker_key(
            &mut screen,
            &mut runner_opt,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        let state = screen.model_picker.as_ref().unwrap();
        assert!(state.focus_models, "Enter 应切到模型栏");
        assert_eq!(
            state.fetching.as_deref(),
            Some("aaa-stub"),
            "选定 provider 必须重新拉取"
        );

        let fetch = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("Enter 后应重新拉取")
            .expect("通道不应关闭");
        screen.model_picker.as_mut().unwrap().deliver_fetch(fetch);
        assert_eq!(
            screen.model_picker.as_ref().unwrap().models,
            vec!["cfg-model".to_string(), "live-b".into()]
        );

        // r 手动重新拉取
        handle_model_picker_key(
            &mut screen,
            &mut runner_opt,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(
            screen.model_picker.as_ref().unwrap().fetching.is_some(),
            "r 必须触发重新拉取"
        );
        let fetch = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("r 后应重新拉取")
            .expect("通道不应关闭");
        screen.model_picker.as_mut().unwrap().deliver_fetch(fetch);
        assert_eq!(
            screen.model_picker.as_ref().unwrap().models,
            vec!["cfg-model".to_string(), "live-c".into()]
        );

        // base_url 已含 /v1 → 三次请求都必须是 /v1/models（不得出现 /v1/v1）
        assert_eq!(server.await.unwrap(), vec!["/v1/models".to_string(); 3]);

        if let Some(r) = runner_opt {
            let _ = std::fs::remove_dir_all(r.cwd);
        }
    }

    #[tokio::test]
    async fn panel_model_picker_dual_column_and_key_navigation() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let (permissions, _rx) = PermissionBroker::interactive();
        let permissions = Arc::new(permissions);
        let (events, _rx_ev) = mpsc::unbounded_channel();
        let mut active = None;
        let mut cancel = None;
        let mut runner_opt = Some(owner);

        // 1. /model 激活 ModelPicker 双栏面板
        let action = cli_commands::execute(runner_opt.as_mut().unwrap(), "/model").unwrap();
        apply_action(
            &mut screen,
            action,
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();

        assert_eq!(screen.panel, Some(Panel::ModelPicker));
        assert!(screen.model_picker.is_some());

        // 2. 双栏渲染验证
        let rendered = render(&mut screen, 100, 30);
        assert!(rendered.contains("Model Picker") || rendered.contains("模型选择"));
        assert!(rendered.contains("Providers") || rendered.contains("服务商"));
        assert!(rendered.contains("Models") || rendered.contains("模型"));
        assert!(rendered.contains('★') || rendered.contains('默'));
        assert!(rendered.contains('✓') || rendered.contains('当'));

        // 3. Tab 切栏：初始在 Providers (focus_models=false) -> Tab 切换到 Models (focus_models=true)
        assert!(!screen.model_picker.as_ref().unwrap().focus_models);
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert!(screen.model_picker.as_ref().unwrap().focus_models);

        // 4. 方向键切回左栏 (Left)
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert!(!screen.model_picker.as_ref().unwrap().focus_models);

        // 5. 左栏按 Enter 下钻到右栏
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert!(screen.model_picker.as_ref().unwrap().focus_models);

        // 6. 右栏按 Enter 确认选择并持久化退出
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(screen.panel, None);
        assert!(screen.model_picker.is_none());
        assert!(screen.status.contains("已切换模型为"));
    }

    #[test]
    fn panel_is_fullscreen_classification() {
        assert!(Panel::Settings.is_fullscreen());
        assert!(Panel::Mcp.is_fullscreen());
        assert!(!Panel::Shortcuts.is_fullscreen());
        assert!(!Panel::Ctf.is_fullscreen());
        assert!(!Panel::Subagents.is_fullscreen());
        assert!(!Panel::Jobs.is_fullscreen());
        assert!(Panel::ModelPicker.is_fullscreen());
        assert!(Panel::About.is_fullscreen());
    }

    #[tokio::test]
    async fn slash_model_opens_fullscreen_model_picker_panel() {
        let mut owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let action = cli_commands::execute(&mut owner, "/model").unwrap();
        assert!(
            matches!(&action, CliAction::Picker(p) if p.kind == crate::cli_commands::PickerKind::Models)
                || matches!(action, CliAction::Panel(Panel::ModelPicker))
        );

        let (permissions, _rx) = PermissionBroker::interactive();
        let permissions = Arc::new(permissions);
        let (events, _rx_ev) = mpsc::unbounded_channel();
        let mut active = None;
        let mut cancel = None;
        let mut runner_opt = Some(owner);

        apply_action(
            &mut screen,
            action,
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();

        assert_eq!(screen.panel, Some(Panel::ModelPicker));
        assert!(screen.model_picker.is_some());
        assert!(screen.panel.unwrap().is_fullscreen());

        let rendered = render(&mut screen, 120, 36);
        assert!(rendered.contains("Model Picker") || rendered.contains("模型选择"));
        assert!(rendered.contains("Providers") || rendered.contains("服务商"));
        assert!(rendered.contains("Models") || rendered.contains("模型"));
    }

    #[tokio::test]
    async fn slash_mcp_opens_fullscreen_mcp_panel() {
        let mut owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let action = cli_commands::execute(&mut owner, "/mcp").unwrap();
        assert!(matches!(action, CliAction::Panel(Panel::Mcp)));

        let (permissions, _rx) = PermissionBroker::interactive();
        let permissions = Arc::new(permissions);
        let (events, _rx_ev) = mpsc::unbounded_channel();
        let mut active = None;
        let mut cancel = None;
        let mut runner_opt = Some(owner);

        apply_action(
            &mut screen,
            action,
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();

        assert_eq!(screen.panel, Some(Panel::Mcp));
        assert!(screen.mcp_panel.is_some());

        let rendered = render(&mut screen, 120, 36);
        assert!(rendered.contains("MCP"));
        assert!(rendered.contains("Servers"));
        assert!(rendered.contains("Config"));
        assert!(rendered.contains("Tools"));

        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(
            screen.mcp_panel.as_ref().unwrap().focus,
            crate::views::mcp_panel::McpFocusPane::ServerDetail
        );

        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(screen.panel, None);
        assert!(screen.mcp_panel.is_none());
    }

    #[tokio::test]
    async fn settings_panel_renders_fullscreen_canvas() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.panel = Some(Panel::Settings);
        screen.settings = Some(CliSettingsState::from_view(&owner.view()));

        let rendered = render(&mut screen, 120, 36);
        assert!(rendered.contains("Cyber Master"));
        assert!(rendered.contains("Settings"));
        assert!(rendered.contains("Agent"));
        assert!(rendered.contains("Ctrl+S"));
    }

    #[tokio::test]
    async fn settings_mouse_wheel_moves_selection_like_arrow_keys() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.panel = Some(Panel::Settings);
        let mut s = CliSettingsState::new(&Config::default(), &ProvidersConfig::default());
        s.tab = SettingsTab::AgentModel; // max_row = 11
        screen.settings = Some(s);

        // 滚轮向下 = ↓：0 -> 1
        assert!(screen.settings_scroll(false));
        assert_eq!(screen.settings.as_ref().unwrap().selected_row, 1);
        // 滚轮向上 = ↑：1 -> 0
        assert!(screen.settings_scroll(true));
        assert_eq!(screen.settings.as_ref().unwrap().selected_row, 0);
        // 顶部继续向上不越界
        assert!(screen.settings_scroll(true));
        assert_eq!(screen.settings.as_ref().unwrap().selected_row, 0);
        // 底部封顶于 max_row
        for _ in 0..20 {
            screen.settings_scroll(false);
        }
        assert_eq!(screen.settings.as_ref().unwrap().selected_row, 11);
    }

    #[tokio::test]
    async fn providers_tab_shortcuts_open_picker_and_form_and_esc_returns_to_settings() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);
        let (permissions, _rx) = PermissionBroker::interactive();
        let permissions = Arc::new(permissions);
        let (events, _ev_rx) = mpsc::unbounded_channel();
        let mut active = None;
        let mut cancel = None;

        screen.panel = Some(Panel::Settings);
        let mut s = CliSettingsState::from_view(&runner_opt.as_ref().unwrap().view());
        s.tab = SettingsTab::Providers;
        screen.settings = Some(s);

        // Press 'A' to open wizard picker
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(screen.panel, None);
        assert!(screen.picker.is_some());
        assert_eq!(screen.settings_return_tab, Some(SettingsTab::Providers));

        // Esc returns to Settings/Providers
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(screen.panel, Some(Panel::Settings));
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::Providers
        );
        assert!(screen.picker.is_none());
        assert!(screen.settings_return_tab.is_none());

        // Press 'M' to open models picker
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(screen.panel, None);
        assert!(screen.picker.is_some());
        assert_eq!(screen.settings_return_tab, Some(SettingsTab::Providers));

        // Esc returns to Settings/Providers
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(screen.panel, Some(Panel::Settings));
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::Providers
        );
        assert!(screen.picker.is_none());

        // Press 'E' to open edit form
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(screen.panel, None);
        assert!(screen.form.is_some());
        assert_eq!(screen.settings_return_tab, Some(SettingsTab::Providers));

        // Esc returns to Settings/Providers
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(screen.panel, Some(Panel::Settings));
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::Providers
        );
        assert!(screen.form.is_none());
    }

    #[tokio::test]
    async fn providers_tab_speed_test_records_and_renders_result() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.panel = Some(Panel::Settings);
        let mut s = CliSettingsState::from_view(&owner.view());
        s.tab = SettingsTab::Providers;
        s.provider_test_results
            .insert("openai".into(), (true, "发现 12 款可用模型".into(), 42));
        screen.settings = Some(s);

        let rendered = render(&mut screen, 120, 36);
        let compact = rendered.replace(' ', "");
        assert!(compact.contains("连通正常"), "{rendered}");
        assert!(compact.contains("42ms"), "{rendered}");
        assert!(compact.contains("发现12款可用模型"), "{rendered}");
    }

    #[tokio::test]
    async fn form_navigation_up_down_and_kind_quick_switch() {
        let mut owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let action = cli_commands::execute(&mut owner, "/provider add").unwrap();
        let form = match action {
            CliAction::Form(f) => f,
            _ => panic!("Expected form"),
        };
        screen.form = Some(FormState::new(form));
        let mut runner_opt = Some(owner);
        let (permissions, _rx) = PermissionBroker::interactive();
        let permissions = Arc::new(permissions);
        let (events, _ev_rx) = mpsc::unbounded_channel();
        let mut active = None;
        let mut cancel = None;

        // Initial selected is 0 (name)
        assert_eq!(screen.form.as_ref().unwrap().selected, 0);

        // Press Down -> 1 (kind)
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(screen.form.as_ref().unwrap().selected, 1);
        assert_eq!(screen.form.as_ref().unwrap().form.fields[1].name, "kind");

        // Press Space to cycle kind
        let old_kind = screen.form.as_ref().unwrap().inputs[1].lines().join("");
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        let new_kind = screen.form.as_ref().unwrap().inputs[1].lines().join("");
        assert_ne!(old_kind, new_kind);
        assert!(cyber_core::PROVIDER_KINDS.contains(&new_kind.as_str()));

        // Press Up -> 0
        handle_key(
            &mut screen,
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(screen.form.as_ref().unwrap().selected, 0);
    }

    #[tokio::test]
    async fn provider_wizard_preset_labels_have_no_empty_parentheses() {
        let mut runner = crate::headless::tests::test_runner().await;
        let action = cli_commands::execute(&mut runner, "/provider wizard").unwrap();
        if let CliAction::Picker(picker) = action {
            assert_eq!(picker.kind, PickerKind::General);
            assert_eq!(picker.title, "Add Provider from Preset or Custom");
            for item in &picker.items {
                assert!(
                    !item.label.contains("()"),
                    "Label should not contain empty parentheses: {}",
                    item.label
                );
            }
            let custom_opt = picker
                .items
                .iter()
                .find(|i| i.label.contains("自定义服务商接入"));
            assert!(
                custom_opt.is_some(),
                "Custom option must be present in wizard"
            );
            assert_eq!(custom_opt.unwrap().command, "/provider add-custom");
        } else {
            panic!("Expected CliAction::Picker for /provider wizard");
        }
    }

    #[tokio::test]
    async fn custom_provider_flow_routes_through_protocol_picker_not_model_panel() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _ev_rx) = mpsc::unbounded_channel();
        let mut active = None;
        let mut cancel = None;
        let mut runner_opt = Some(runner);

        let action =
            cli_commands::execute(runner_opt.as_mut().unwrap(), "/provider add-custom").unwrap();
        if let CliAction::Picker(picker) = &action {
            assert_eq!(picker.kind, PickerKind::General);
            assert_eq!(picker.title, "Select Protocol Kind");
        } else {
            panic!("Expected Picker for /provider add-custom");
        }

        apply_action(
            &mut screen,
            action,
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();

        assert!(screen.picker.is_some());
        assert_ne!(screen.panel, Some(Panel::ModelPicker));
        assert_eq!(
            screen.picker.as_ref().unwrap().title,
            "Select Protocol Kind"
        );

        let form_action = cli_commands::execute(
            runner_opt.as_mut().unwrap(),
            "/provider add-with-kind openai-compatible",
        )
        .unwrap();

        apply_action(
            &mut screen,
            form_action,
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();

        assert!(screen.form.is_some());
        assert_eq!(
            screen.form.as_ref().unwrap().form.title,
            "Add Custom Provider"
        );
        let kind_idx = screen
            .form
            .as_ref()
            .unwrap()
            .form
            .fields
            .iter()
            .position(|f| f.name == "kind")
            .unwrap();
        assert_eq!(
            screen.form.as_ref().unwrap().inputs[kind_idx]
                .lines()
                .join(""),
            "openai-compatible"
        );
    }

    #[tokio::test]
    async fn fullscreen_provider_picker_and_form_render_without_truncation() {
        let mut runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);

        let picker_action = cli_commands::execute(&mut runner, "/provider wizard").unwrap();
        if let CliAction::Picker(picker) = picker_action {
            screen.picker = Some(picker);
            screen.picker_selected = 0;
            let rendered = render(&mut screen, 120, 40);
            let compact = rendered.replace(' ', "");
            assert!(compact.contains("服务商预设库与自定义接入"), "{rendered}");
            assert!(compact.contains("服务商选项"), "{rendered}");
            assert!(compact.contains("详细规格与配置指南"), "{rendered}");
            assert!(compact.contains("DeepSeek官方"), "{rendered}");
            assert!(compact.contains("自定义服务商接入"), "{rendered}");
        }

        let form_action =
            cli_commands::execute(&mut runner, "/provider add-with-kind openai-compatible")
                .unwrap();
        if let CliAction::Form(form) = form_action {
            screen.picker = None;
            screen.form = Some(FormState::new(form));
            let rendered = render(&mut screen, 120, 40);
            let compact = rendered.replace(' ', "");
            assert!(compact.contains("服务商接入配置"), "{rendered}");
            assert!(compact.contains("配置字段输入"), "{rendered}");
            assert!(compact.contains("参数向导与协议规范"), "{rendered}");
            assert!(compact.contains("服务商标识(name)"), "{rendered}");
            assert!(compact.contains("协议类型(kind)"), "{rendered}");
            assert!(compact.contains("[openai-compatible]"), "{rendered}");
            // 高级设置区标题与自定义端点字段
            assert!(compact.contains("高级设置高级选项"), "{rendered}");
            assert!(
                compact.contains("自定义对话端点chat_endpoint"),
                "{rendered}"
            );
            assert!(
                compact.contains("自定义模型列表端点models_endpoint"),
                "{rendered}"
            );

            // 小窗口下光标移动到最后一个字段：跟随滚屏必须把高级端点字段带入视口
            let fields_len = screen.form.as_ref().unwrap().form.fields.len();
            screen.form.as_mut().unwrap().selected = fields_len - 1;
            let small = render(&mut screen, 100, 20).replace(' ', "");
            assert!(
                small.contains("自定义模型列表端点models_endpoint"),
                "focused advanced field must stay visible: {small}"
            );
        }
    }

    #[test]
    fn test_env_memory_max_row_and_slot_partition() {
        let mut state = CliSettingsState::new(&Config::default(), &ProvidersConfig::default());
        state.tab = SettingsTab::EnvMemory;

        // 1. Both empty -> max_row = 1 (slot 0: env, slot 1: memory)
        state.config_draft.env.vars.clear();
        state.config_draft.memory.rules.clear();
        assert_eq!(state.tab.max_row(&state), 1);

        // 2. Empty env, 2 rules -> env_slots=1, mem_slots=2, max_row = 2 (0: env, 1..=2: rules)
        state
            .config_draft
            .memory
            .rules
            .push(cyber_core::MemoryRule {
                enabled: true,
                scope: "both".into(),
                prompt: "rule 1".into(),
            });
        state
            .config_draft
            .memory
            .rules
            .push(cyber_core::MemoryRule {
                enabled: true,
                scope: "project".into(),
                prompt: "rule 2".into(),
            });
        assert_eq!(state.tab.max_row(&state), 2);

        // 3. 2 env, empty rules -> env_slots=2, mem_slots=1, max_row = 2 (0..=1: env, 2: rules)
        state.config_draft.memory.rules.clear();
        state.config_draft.env.vars.push(cyber_core::EnvVar {
            key: "VAR1".into(),
            value: "val1".into(),
            sensitive: false,
        });
        state.config_draft.env.vars.push(cyber_core::EnvVar {
            key: "VAR2".into(),
            value: "val2".into(),
            sensitive: true,
        });
        assert_eq!(state.tab.max_row(&state), 2);

        // 4. 2 env, 2 rules -> env_slots=2, mem_slots=2, max_row = 3 (0..=1: env, 2..=3: rules)
        state
            .config_draft
            .memory
            .rules
            .push(cyber_core::MemoryRule {
                enabled: true,
                scope: "both".into(),
                prompt: "rule 1".into(),
            });
        state
            .config_draft
            .memory
            .rules
            .push(cyber_core::MemoryRule {
                enabled: false,
                scope: "global".into(),
                prompt: "rule 2".into(),
            });
        assert_eq!(state.tab.max_row(&state), 3);

        // 5. 2 env + 2 rules + 3 memories -> env_slots=2, mem_slots=2, 记忆槽位 4..=6
        state.memories_global = vec![
            cyber_core::MemoryEntry {
                index: 1,
                content: "g1".into(),
            },
            cyber_core::MemoryEntry {
                index: 2,
                content: "g2".into(),
            },
        ];
        state.memories_project = vec![cyber_core::MemoryEntry {
            index: 1,
            content: "p1".into(),
        }];
        assert_eq!(state.total_memories(), 3);
        assert_eq!(state.tab.max_row(&state), 6);
        assert_eq!(state.memory_at(0), Some((MemoryGroup::Global, 0)));
        assert_eq!(state.memory_at(1), Some((MemoryGroup::Global, 1)));
        assert_eq!(state.memory_at(2), Some((MemoryGroup::Project, 0)));
        assert_eq!(state.memory_at(3), None);
    }

    fn buffer_of(screen: &mut CliScreen, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// 校验弹窗（弹窗矩形由父区域按 85%/最小 70x20 规则居中算出）边框每一格是否完整。
    ///
    /// 断言的是 **终端实际显示的内容**：`TestBackend` 缓冲是 ratatui 缓冲 diff 应用后的
    /// 结果，宽字符尾随单元格被 diff 跳过时，边框会在这里缺格。
    fn modal_border_violations(buf: &ratatui::buffer::Buffer, w: u16, h: u16) -> Vec<String> {
        let width = (w * 85 / 100).max(70).min(w);
        let height = (h * 85 / 100).max(20).min(h);
        let x0 = (w - width) / 2;
        let y0 = (h - height) / 2;
        let right = x0 + width - 1;
        let bottom = y0 + height - 1;
        let mut bad = Vec::new();
        if !(x0..=right).any(|x| buf[(x, y0)].symbol() == "📖") {
            bad.push("弹窗标题未出现在预期位置".into());
        }
        for y in (y0 + 1)..bottom {
            if buf[(x0, y)].symbol() != "│" {
                bad.push(format!("左边框 y={y} 为 {:?}", buf[(x0, y)].symbol()));
            }
            if buf[(right, y)].symbol() != "│" {
                bad.push(format!("右边框 y={y} 为 {:?}", buf[(right, y)].symbol()));
            }
        }
        for x in (x0 + 1)..right {
            if buf[(x, bottom)].symbol() != "─" {
                bad.push(format!("下边框 x={x} 为 {:?}", buf[(x, bottom)].symbol()));
            }
        }
        for (x, y, want, name) in [
            (x0, y0, "┌", "左上角"),
            (right, y0, "┐", "右上角"),
            (x0, bottom, "└", "左下角"),
            (right, bottom, "┘", "右下角"),
        ] {
            if buf[(x, y)].symbol() != want {
                bad.push(format!("{name} ({x},{y}) 为 {:?}", buf[(x, y)].symbol()));
            }
        }
        bad
    }

    /// 弹窗边框必须完整：底层文本的宽字符（CJK/emoji）尾随单元格会被 ratatui 的缓冲
    /// diff 跳过，曾导致「记忆详情」竖边框与左上/左下角缺一格，看起来像被文字挤掉。
    #[tokio::test]
    async fn test_detail_modals_border_survives_underlying_wide_text() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('6'),
            KeyModifiers::NONE,
        );
        {
            let s = screen.settings.as_mut().unwrap();
            s.config_draft.env.vars.clear();
            s.config_draft.memory.rules.clear();
            s.memories_global = vec![MemoryEntry {
                index: 1,
                content: "全局记忆内容 一二三四五六七八九十".into(),
            }];
            s.memories_project = vec![MemoryEntry {
                index: 1,
                content: "项目记忆 甲乙丙丁".into(),
            }];
            s.selected_row = 2;
            s.memory_detail = Some(MemoryDetailModal {
                group: MemoryGroup::Global,
                entry: 0,
                scroll: 0,
            });
        }
        for (w, h) in [(100u16, 24u16), (80, 20), (60, 18), (140, 40)] {
            let buf = buffer_of(&mut screen, w, h);
            let bad = modal_border_violations(&buf, w, h);
            assert!(
                bad.is_empty(),
                "记忆详情弹窗边框在 {w}x{h} 下被文字挤掉: {bad:?}"
            );
        }

        // 同构的 Skill 详情弹窗使用同一套清屏逻辑，边框同样必须完整
        {
            let s = screen.settings.as_mut().unwrap();
            s.memory_detail = None;
            s.tab = SettingsTab::ToolsMcp;
            s.skills = vec![SkillSummary {
                name: "demo-skill".into(),
                description: "演示技能 一二三四五六七八九十".into(),
                body: "# 标题\n正文一二三四五六七八九十".into(),
                ..Default::default()
            }];
            s.skill_detail = Some(SkillDetailModal {
                skill_index: 0,
                scroll: 0,
            });
        }
        for (w, h) in [(100u16, 24u16), (80, 20), (140, 40)] {
            let buf = buffer_of(&mut screen, w, h);
            let bad = modal_border_violations(&buf, w, h);
            assert!(
                bad.is_empty(),
                "Skill 详情弹窗边框在 {w}x{h} 下被文字挤掉: {bad:?}"
            );
        }

        if let Some(r) = runner_opt {
            let _ = std::fs::remove_dir_all(r.cwd);
        }
    }

    #[tokio::test]
    async fn test_env_memory_list_rendering_and_detail_modal() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        // Open settings (F3) then switch to tab 6 (EnvMemory)
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('6'),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::EnvMemory
        );

        // env_slots = 1 + mem_slots = 1 -> first memory row is selected_row == 2
        {
            let s = screen.settings.as_mut().unwrap();
            s.config_draft.env.vars.clear();
            s.config_draft.memory.rules.clear();
            s.memories_global = vec![MemoryEntry {
                index: 1,
                content: "全局偏好 Python".into(),
            }];
            s.memories_project = vec![MemoryEntry {
                index: 1,
                content: "项目使用 tokio".into(),
            }];
            s.selected_row = 2;
        }

        let rendered = render(&mut screen, 120, 40).replace(' ', "");
        for needle in [
            "用户记忆列表",
            "全局记忆",
            "项目级记忆",
            "全局偏好Python",
            "项目使用tokio",
        ] {
            assert!(
                rendered.contains(needle),
                "rendered settings must contain {needle}: {rendered}"
            );
        }

        // Enter on a memory row opens the read-only detail modal
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        {
            let detail = screen
                .settings
                .as_ref()
                .unwrap()
                .memory_detail
                .as_ref()
                .expect("memory detail modal must open");
            assert_eq!(detail.group, MemoryGroup::Global);
            assert_eq!(detail.entry, 0);
            assert_eq!(detail.scroll, 0);
        }

        let modal = render(&mut screen, 120, 40).replace(' ', "");
        for needle in ["记忆详情[1/2]", "全局偏好Python"] {
            assert!(
                modal.contains(needle),
                "memory detail modal must contain {needle}: {modal}"
            );
        }

        // Down scrolls the modal body
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Down,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .memory_detail
                .as_ref()
                .unwrap()
                .scroll,
            1
        );

        // Right jumps to the next memory across groups and syncs the outer cursor
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Right,
            KeyModifiers::NONE,
        );
        {
            let settings = screen.settings.as_ref().unwrap();
            let detail = settings.memory_detail.as_ref().unwrap();
            assert_eq!(detail.group, MemoryGroup::Project);
            assert_eq!(detail.entry, 0);
            assert_eq!(detail.scroll, 0);
            assert_eq!(settings.selected_row, 3);
        }

        // Esc closes the modal, keeping the list cursor where the modal left it
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        {
            let settings = screen.settings.as_ref().unwrap();
            assert!(settings.memory_detail.is_none());
            assert_eq!(settings.selected_row, 3);
        }
        assert_eq!(screen.panel, Some(Panel::Settings));

        if let Some(r) = runner_opt {
            let _ = std::fs::remove_dir_all(r.cwd);
        }
    }

    #[tokio::test]
    async fn test_env_memory_shortcuts_and_interactions() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let mut runner_opt = Some(runner);

        // Open settings panel with F(3), then switch to tab 6 (EnvMemory)
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('6'),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::EnvMemory
        );

        // Add 1 env var and 1 memory rule into config_draft
        {
            let s = screen.settings.as_mut().unwrap();
            s.config_draft.env.vars.clear();
            s.config_draft.memory.rules.clear();
            s.config_draft.env.vars.push(cyber_core::EnvVar {
                key: "FOO_TOKEN".into(),
                value: "abc-123".into(),
                sensitive: false,
            });
            s.config_draft.memory.rules.push(cyber_core::MemoryRule {
                enabled: true,
                scope: "both".into(),
                prompt: "remember test rule".into(),
            });
            s.selected_row = 0; // Focus on FOO_TOKEN
        }

        // Space on env var toggles sensitive（只读态空格被吞掉，需先 Enter 进入编辑态）
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert!(screen.settings.as_ref().unwrap().config_draft.env.vars[0].sensitive);
        assert!(screen.status.contains("已开启脱敏保护"));

        // 'E' on env var opens /env edit form（Enter 已被保留为「进入编辑态」）
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('e'),
            KeyModifiers::NONE,
        );
        assert!(screen.form.is_some());
        assert_eq!(screen.settings_return_tab, Some(SettingsTab::EnvMemory));

        // Esc returns to settings EnvMemory
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert!(screen.form.is_none());
        assert_eq!(screen.panel, Some(Panel::Settings));
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::EnvMemory
        );

        // Press 'A' on env partition opens /env add form
        screen.settings.as_mut().unwrap().selected_row = 0;
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        );
        assert!(screen.form.is_some());
        assert_eq!(
            screen.form.as_ref().unwrap().form.title,
            "Add Environment Variable"
        );

        // Esc returns
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );

        // Move to memory rule partition (selected_row = 1)
        screen.settings.as_mut().unwrap().selected_row = 1;

        // Space on memory rule toggles enabled（需先 Enter 进入编辑态）
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        );
        assert!(!screen.settings.as_ref().unwrap().config_draft.memory.rules[0].enabled);
        assert!(screen.status.contains("已关闭"));

        // Right on memory rule cycles scope: "both" -> "project"（编辑态内）
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Right,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().config_draft.memory.rules[0].scope,
            "project"
        );

        // Right again: "project" -> "global"
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Right,
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().config_draft.memory.rules[0].scope,
            "global"
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );

        // 'E' on memory rule opens /memory rule edit form（Enter 已被保留为「进入编辑态」）
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('e'),
            KeyModifiers::NONE,
        );
        assert!(screen.form.is_some());
        assert_eq!(screen.settings_return_tab, Some(SettingsTab::EnvMemory));

        // Esc returns
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );

        // Press 'A' on memory partition opens /memory rule add form
        screen.settings.as_mut().unwrap().selected_row = 1;
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        );
        assert!(screen.form.is_some());
        assert_eq!(screen.form.as_ref().unwrap().form.title, "Memory Rule");

        // Esc returns
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );

        // Press 'D' on memory rule deletes it
        screen.settings.as_mut().unwrap().selected_row = 1;
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('d'),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .memory
                .rules
                .len(),
            0
        );

        // Press 'D' on env var deletes it
        screen.settings.as_mut().unwrap().selected_row = 0;
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('d'),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .env
                .vars
                .len(),
            0
        );

        if let Some(r) = runner_opt {
            let _ = std::fs::remove_dir_all(r.cwd);
        }
    }

    #[tokio::test]
    async fn test_env_memory_rendering_badges_and_hints() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let mut runner_opt = Some(runner);

        // Open settings panel with F(3), then switch to tab 6 (EnvMemory)
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('6'),
            KeyModifiers::NONE,
        );
        {
            let s = screen.settings.as_mut().unwrap();
            s.config_draft.env.vars = vec![
                cyber_core::EnvVar {
                    key: "SECRET_KEY".into(),
                    value: "secret-data".into(),
                    sensitive: true,
                },
                cyber_core::EnvVar {
                    key: "PUBLIC_URL".into(),
                    value: "http://localhost:8080".into(),
                    sensitive: false,
                },
            ];
            s.config_draft.memory.rules = vec![cyber_core::MemoryRule {
                enabled: true,
                scope: "both".into(),
                prompt: "always check tests".into(),
            }];
            s.selected_row = 0; // Focus on SECRET_KEY in Env partition
        }

        let rendered = render(&mut screen, 120, 40);
        let compact = rendered.replace(' ', "");
        assert!(compact.contains("SECRET_KEY"), "{rendered}");
        assert!(compact.contains("PUBLIC_URL"), "{rendered}");
        assert!(compact.contains("脱敏保护"), "{rendered}");
        assert!(compact.contains("明文公开"), "{rendered}");
        assert!(
            compact.contains("sk-************************"),
            "{rendered}"
        );
        assert!(compact.contains("alwayschecktests"), "{rendered}");
        assert!(compact.contains("开启"), "{rendered}");
        assert!(compact.contains("全局与项目"), "{rendered}");
        assert!(compact.contains("当前聚焦"), "{rendered}");

        if let Some(r) = runner_opt {
            let _ = std::fs::remove_dir_all(r.cwd);
        }
    }

    #[tokio::test]
    async fn test_question_custom_input_chinese_text_does_not_leak_to_composer() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let (reply, _rx) = tokio::sync::oneshot::channel();
        let req = cyber_agent::QuestionRequest {
            id: "test".into(),
            questions: vec![cyber_agent::QuestionItem {
                id: "q1".into(),
                question: "选择方案".into(),
                header: None,
                options: vec![cyber_agent::QuestionOption {
                    label: "A".into(),
                    description: None,
                    recommended: false,
                }],
                multi: false,
            }],
            reply,
        };
        let mut q_state = crate::question_ui::QuestionUiState::new(req);
        // 进入自定义输入编辑模式
        q_state.start_custom_editing();
        screen.question_state = Some(q_state);

        // 模拟中文输入法一次性提交多个中文字符（或粘贴）
        screen.insert_text("需要启用全文检索和向量扩展");

        // 验证：内容完整进入 question_state 的 custom_textarea，底部 input 保持完全为空！
        let q_state_ref = screen.question_state.as_ref().unwrap();
        assert_eq!(
            q_state_ref.custom_textarea.lines().join(""),
            "需要启用全文检索和向量扩展"
        );
        assert!(screen.input.is_empty(), "底部输入框必须为空，绝不能泄漏！");
    }

    #[tokio::test]
    async fn test_question_non_editing_paste_does_not_leak_to_composer() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let (reply, _rx) = tokio::sync::oneshot::channel();
        let req = cyber_agent::QuestionRequest {
            id: "test".into(),
            questions: vec![cyber_agent::QuestionItem {
                id: "q1".into(),
                question: "选择方案".into(),
                header: None,
                options: vec![cyber_agent::QuestionOption {
                    label: "A".into(),
                    description: None,
                    recommended: false,
                }],
                multi: false,
            }],
            reply,
        };
        let q_state = crate::question_ui::QuestionUiState::new(req);
        assert!(!q_state.custom_editing);
        screen.question_state = Some(q_state);

        // 在非自定义编辑模式下意外粘贴或快速按键
        screen.insert_text("随意内容");

        // 验证：绝不进入底部 input
        assert!(screen.input.is_empty());
    }

    #[tokio::test]
    async fn env_form_dialog_renders_modal_style_and_masks_secret() {
        let mut runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        runner.ctx.config.env.vars.push(cyber_core::EnvVar {
            key: "MY_TOKEN".into(),
            value: "abcdef123456".into(),
            sensitive: true,
        });
        let CliAction::Form(form) =
            cli_commands::execute(&mut runner, "/env edit MY_TOKEN").unwrap()
        else {
            panic!("expected form");
        };
        screen.form = Some(FormState::new(form));
        let rendered = render(&mut screen, 120, 40);
        let compact = rendered.replace(' ', "");
        assert!(compact.contains("编辑环境变量"), "{rendered}");
        assert!(
            compact.contains("EditEnvironmentVariable(MY_TOKEN)"),
            "{rendered}"
        );
        assert!(compact.contains("变量名称(key)"), "{rendered}");
        assert!(compact.contains("变量内容(value)"), "{rendered}");
        assert!(compact.contains("脱敏保护(sensitive)"), "{rendered}");
        assert!(compact.contains("🔒[脱敏保护]"), "{rendered}");
        assert!(compact.contains("************"), "{rendered}");
        assert!(
            !rendered.contains("abcdef123456"),
            "敏感值绝不能出现在渲染结果中: {rendered}"
        );
        assert!(compact.contains("[Esc]取消"), "{rendered}");
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn memory_rule_dialog_renders_modal_style_badges() {
        let mut runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        let CliAction::Form(form) = cli_commands::execute(&mut runner, "/memory rule add").unwrap()
        else {
            panic!("expected form");
        };
        screen.form = Some(FormState::new(form));
        let rendered = render(&mut screen, 120, 40);
        let compact = rendered.replace(' ', "");
        assert!(compact.contains("新增记忆规则"), "{rendered}");
        assert!(compact.contains("启用状态(enabled)"), "{rendered}");
        assert!(compact.contains("[●开启]"), "{rendered}");
        assert!(compact.contains("作用域(scope)"), "{rendered}");
        assert!(compact.contains("[全局与项目(both)]"), "{rendered}");
        assert!(compact.contains("规则提示(prompt)"), "{rendered}");
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    /// 由设置面板打开的弹层（编辑环境变量 / 编辑记忆规则 / Models 模型选择器）必须以
    /// 设置中心为背景：弹层打开期间 `panel` 仍为 `None`，但绘制不得回落到对话界面。
    #[tokio::test]
    async fn settings_opened_dialogs_keep_settings_panel_as_background() {
        let mut owner = crate::headless::tests::test_runner().await;
        cyber_core::custom_tool::save_custom_tool(
            &owner.ctx.paths.tools_dir,
            &cyber_core::CustomToolConfig {
                name: "probe".into(),
                description: "探测".into(),
                command: "probe -x".into(),
                ..Default::default()
            },
        )
        .unwrap();
        owner.ctx.config.env.vars.push(cyber_core::EnvVar {
            key: "DASCTF_ACCESS_KEY".into(),
            value: "demo".into(),
            sensitive: false,
        });
        owner.ctx.config.memory.rules.push(cyber_core::MemoryRule {
            enabled: true,
            scope: "both".into(),
            prompt: "demo rule".into(),
        });
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('6'),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::EnvMemory
        );

        // 1) 编辑环境变量表单：selected_row = 0 即第一个环境变量（E 打开表单，Enter 现在是「进入编辑态」）
        screen.settings.as_mut().unwrap().selected_row = 0;
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('e'),
            KeyModifiers::NONE,
        );
        assert!(screen.form.is_some(), "E 应打开编辑环境变量表单");
        let frame = render(&mut screen, 120, 36);
        assert!(
            frame
                .lines()
                .next()
                .unwrap_or_default()
                .replace(' ', "")
                .contains("全局设置中心"),
            "编辑环境变量弹层的背景必须是设置中心，而不是对话界面: {frame}"
        );
        assert!(frame.replace(' ', "").contains("编辑环境变量"), "{frame}");
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert!(screen.form.is_none());
        assert_eq!(screen.panel, Some(Panel::Settings));

        // 2) 编辑记忆规则表单：env_slots = 1，selected_row = 1 即第一条规则（E 打开表单）
        screen.settings.as_mut().unwrap().selected_row = 1;
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('e'),
            KeyModifiers::NONE,
        );
        assert!(screen.form.is_some(), "E 应打开编辑记忆规则表单");
        let frame = render(&mut screen, 120, 36);
        assert!(
            frame
                .lines()
                .next()
                .unwrap_or_default()
                .replace(' ', "")
                .contains("全局设置中心"),
            "{frame}"
        );
        assert!(frame.replace(' ', "").contains("编辑记忆规则"), "{frame}");
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert!(screen.form.is_none());

        // 3) Models 模型选择弹层
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('5'),
            KeyModifiers::NONE,
        );
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::Providers
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('m'),
            KeyModifiers::NONE,
        );
        assert!(screen.picker.is_some(), "'M' 应打开 Models 弹层");
        let frame = render(&mut screen, 120, 36);
        assert!(
            frame
                .lines()
                .next()
                .unwrap_or_default()
                .replace(' ', "")
                .contains("全局设置中心"),
            "{frame}"
        );
        assert!(frame.replace(' ', "").contains("🤖Models("), "{frame}");
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert!(screen.picker.is_none());
        assert_eq!(screen.panel, Some(Panel::Settings));
        let frame = render(&mut screen, 120, 36);
        assert!(
            frame
                .lines()
                .next()
                .unwrap_or_default()
                .replace(' ', "")
                .contains("全局设置中心"),
            "{frame}"
        );

        // 4) 工具库 → 编辑自定义工具表单（原先会回落到对话界面）
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('8'),
            KeyModifiers::NONE,
        );
        assert_eq!(screen.settings.as_ref().unwrap().tab, SettingsTab::Toolbox);
        // 行 1 = 第一个自定义工具（行 0 是 AI 扫描入口）。
        screen.settings.as_mut().unwrap().selected_row = 1;
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert!(
            matches!(
                screen.form.as_ref().map(|state| &state.form.kind),
                Some(FormKind::CustomTool { .. })
            ),
            "Enter 应打开编辑自定义工具表单"
        );
        let frame = render(&mut screen, 120, 36);
        let compact = frame.replace(' ', "");
        assert!(
            frame
                .lines()
                .next()
                .unwrap_or_default()
                .replace(' ', "")
                .contains("全局设置中心"),
            "编辑自定义工具弹层的背景必须是设置中心，而不是对话界面: {frame}"
        );
        assert!(compact.contains("编辑自定义工具"), "{frame}");
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert!(screen.form.is_none());
        assert_eq!(screen.panel, Some(Panel::Settings));

        // 5) 工具库首行 → AI 智能扫描表单（同一缺陷路径）
        screen.settings.as_mut().unwrap().selected_row = 0;
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert!(matches!(
            screen.form.as_ref().map(|state| &state.form.kind),
            Some(FormKind::ToolboxScan)
        ));
        let frame = render(&mut screen, 120, 36);
        let compact = frame.replace(' ', "");
        assert!(
            frame
                .lines()
                .next()
                .unwrap_or_default()
                .replace(' ', "")
                .contains("全局设置中心"),
            "扫描表单弹层的背景必须是设置中心: {frame}"
        );
        assert!(compact.contains("AI智能扫描本地安全工具"), "{frame}");
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, Some(Panel::Settings));

        if let Some(r) = runner_opt {
            let _ = std::fs::remove_dir_all(r.cwd);
        }
    }

    /// 设置中心「9. 关于」页：设置中心为背景、内容含四段说明、底部说明书滚到底可见，且只读。
    #[tokio::test]
    async fn about_tab_renders_manual_sections_with_settings_background() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('9'),
            KeyModifiers::NONE,
        );
        assert_eq!(screen.settings.as_ref().unwrap().tab, SettingsTab::About);
        assert_eq!(
            SettingsTab::About.max_row(screen.settings.as_ref().unwrap()),
            0
        );
        assert!(screen.about.is_some(), "进入关于页必须采集内容快照");
        assert_eq!(screen.about_scroll, 0);

        let frame = render(&mut screen, 120, 40);
        let compact = frame.replace(' ', "");
        assert!(
            frame
                .lines()
                .next()
                .unwrap_or_default()
                .replace(' ', "")
                .contains("全局设置中心"),
            "关于页的背景必须是设置中心面板: {frame}"
        );
        assert!(
            compact.contains(&format!("CyberMasterV{}", env!("CARGO_PKG_VERSION"))),
            "{frame}"
        );
        assert!(
            compact.contains("关于"),
            "页签或标题必须出现「关于」: {frame}"
        );
        assert!(compact.contains("[版本与更新]"), "{frame}");
        assert!(compact.contains("可更新版本"), "{frame}");
        assert!(compact.contains("[项目信息]"), "{frame}");
        assert!(
            compact.contains("https://github.com/chuzouX/cyber-master"),
            "{frame}"
        );
        assert!(compact.contains("MIT"), "{frame}");
        assert!(compact.contains("[运行环境]"), "{frame}");
        assert!(compact.contains("配置文件"), "{frame}");
        assert!(compact.contains("[核心能力]"), "{frame}");

        // 页面底部的说明书：滚到底后必须出现快捷键表与斜杠命令速查
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::End,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.about_scroll, usize::MAX);
        // 内容约 85 行（含 30 条斜杠命令），120x40 一屏只容得下末尾的命令速查，
        // 故用更高的视口让「快捷键说明书」与「斜杠命令速查」同时落在滚底视图内。
        let frame = render(&mut screen, 120, 70);
        let compact = frame.replace(' ', "");
        assert!(compact.contains("[快捷键说明书]"), "{frame}");
        assert!(compact.contains("Ctrl+B"), "{frame}");
        assert!(compact.contains("[斜杠命令速查]"), "{frame}");
        assert!(compact.contains("/about"), "{frame}");

        // 只读：↑/↓ 只滚动内容，不移动设置焦点行、不切换页签
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Down,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.settings.as_ref().unwrap().selected_row, 0);
        assert_eq!(screen.settings.as_ref().unwrap().tab, SettingsTab::About);
        // R（恢复默认）在只读页不得把页签标记为已修改（否则 Esc 会平白弹出「未保存丢弃确认」）
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('r'),
            KeyModifiers::NONE,
        );
        assert!(!screen.settings.as_ref().unwrap().dirty);
        assert_eq!(screen.settings.as_ref().unwrap().tab, SettingsTab::About);
    }

    /// `/about` 打开全屏只读面板，可滚动，Esc 关闭。
    #[tokio::test]
    async fn about_panel_opens_via_slash_scrolls_and_closes() {
        let mut owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let action = cli_commands::execute(&mut owner, "/about").unwrap();
        assert!(matches!(action, CliAction::Panel(Panel::About)));
        assert!(Panel::About.is_fullscreen());

        let (permissions, _rx) = PermissionBroker::interactive();
        let permissions = Arc::new(permissions);
        let (events, _rx_ev) = mpsc::unbounded_channel();
        let mut active = None;
        let mut cancel = None;
        let mut runner_opt = Some(owner);
        apply_action(
            &mut screen,
            action,
            &mut runner_opt,
            &permissions,
            &events,
            &mut active,
            &mut cancel,
        )
        .unwrap();
        assert_eq!(screen.panel, Some(Panel::About));
        assert!(screen.about.is_some());
        assert_eq!(screen.about_scroll, 0);

        let frame = render(&mut screen, 120, 40);
        let compact = frame.replace(' ', "");
        assert!(
            compact.contains(&format!(
                "关于/About·CyberMasterV{}",
                env!("CARGO_PKG_VERSION")
            )),
            "{frame}"
        );
        assert!(
            compact.contains("检查更新"),
            "面板底部须有按键提示: {frame}"
        );

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Down,
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Down,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.about_scroll, 2);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Esc,
            KeyModifiers::NONE,
        );
        assert_eq!(screen.panel, None, "Esc 必须关闭关于面板");
    }

    /// 关于页按 U 走既有更新检查（本地缓存路径，不联网），检查结果落地时同步「可更新版本」行。
    #[tokio::test]
    async fn about_page_update_check_and_refresh() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);

        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::F(3),
            KeyModifiers::NONE,
        );
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('9'),
            KeyModifiers::NONE,
        );

        let before = screen.messages.len();
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Char('u'),
            KeyModifiers::NONE,
        );
        assert!(!screen.update_checking);
        assert!(screen.messages.len() > before, "U 必须产出更新检查结果提示");
        assert!(
            screen
                .messages
                .iter()
                .any(|line| line.to_string().contains("更新检查")),
            "更新检查结果必须写入对话区"
        );
        assert_eq!(screen.settings.as_ref().unwrap().tab, SettingsTab::About);

        // 检查结果落地时必须同步「可更新版本」行
        screen.deliver_update(UpdateCheckResult {
            kind: cli_commands::CliUpdate::Check,
            info: Some(cyber_core::update::ReleaseInfo {
                version: "9.9.9".into(),
                html_url: "https://example.invalid/release".into(),
                release_notes: None,
                published_at: None,
            }),
        });
        assert_eq!(screen.new_version.as_deref(), Some("9.9.9"));
        assert_eq!(
            screen.about.as_ref().unwrap().latest.as_deref(),
            Some("9.9.9")
        );
        let frame = render(&mut screen, 120, 40);
        assert!(frame.replace(' ', "").contains("V9.9.9"), "{frame}");
    }

    fn models_picker(label: &str, detail: &str) -> CommandPicker {
        CommandPicker {
            title: "Models (demo)".into(),
            items: vec![cli_commands::PickerItem {
                label: label.into(),
                detail: detail.into(),
                command: "/model demo demo-model".into(),
            }],
            kind: PickerKind::Models,
        }
    }

    #[tokio::test]
    async fn models_picker_dialog_renders_modal_style_with_selected_summary() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        screen.picker = Some(models_picker("demo-model (当前使用)", "Provider: demo"));
        let rendered = render(&mut screen, 120, 40);
        let compact = rendered.replace(' ', "");
        assert!(compact.contains("🤖Models(demo)[1/1]"), "{rendered}");
        assert!(compact.contains("当前选中:"), "{rendered}");
        assert!(compact.contains("demo-model(当前使用)"), "{rendered}");
        assert!(compact.contains("[当前启用]"), "{rendered}");
        assert!(compact.contains("Provider:demo"), "{rendered}");
        assert!(compact.contains("选项列表"), "{rendered}");
        assert!(
            compact.contains("[↑/↓]选择·[Enter]确认·[Esc]关闭"),
            "{rendered}"
        );
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn picker_dialog_narrow_and_tiny_sizes_do_not_panic() {
        let runner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&runner);
        screen.picker = Some(models_picker("demo-model", "Provider: demo"));
        for (w, h) in [(39u16, 7u16), (40, 8), (60, 12), (1, 1), (0, 0)] {
            let _ = render(&mut screen, w, h);
        }
        // 对话区（sections[1]）需 ≥ 8 行才渲染模态框；100x25 时约 16 行。
        let compact = render(&mut screen, 100, 25).replace(' ', "");
        assert!(
            compact.contains("当前选中:"),
            "100x25 应渲染模态正文: {compact}"
        );
        let _ = std::fs::remove_dir_all(runner.cwd);

        // dialog_area 边界：40x8 为最小可渲染区域，39x7 直接早退（不绘制、不 panic）。
        let picker = models_picker("demo-model", "Provider: demo");
        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        terminal
            .draw(|frame| draw_picker_dialog(frame, Rect::new(0, 0, 40, 8), &picker, 0, false, ""))
            .unwrap();
        let mut text = String::new();
        for y in 0..8 {
            for x in 0..40 {
                text.push_str(terminal.backend().buffer()[(x, y)].symbol());
            }
        }
        assert!(
            text.replace(' ', "").contains("选项列表")
                && text.replace(' ', "").contains("demo-model"),
            "40x8 应渲染模态框且聚焦行可见: {text}"
        );
        terminal
            .draw(|frame| draw_picker_dialog(frame, Rect::new(0, 0, 39, 7), &picker, 0, false, ""))
            .unwrap();
    }

    /// Ctrl+D 不再退出进程：空输入与有输入下均返回 `false`（不退出）、不破坏输入内容；
    /// shortcuts 面板也不得再宣传该快捷键（退出仍走 Ctrl+C / `/quit`）。
    #[tokio::test]
    async fn ctrl_d_no_longer_exits_the_process() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner = Some(owner);

        assert!(
            !input_key(
                &mut screen,
                &mut runner,
                KeyCode::Char('d'),
                KeyModifiers::CONTROL
            ),
            "空输入 Ctrl+D 不应退出进程"
        );
        assert!(screen.input.is_empty(), "空输入 Ctrl+D 不应写入输入框");

        screen.input.insert_str("token");
        assert!(
            !input_key(
                &mut screen,
                &mut runner,
                KeyCode::Char('d'),
                KeyModifiers::CONTROL
            ),
            "有输入 Ctrl+D 不应退出进程"
        );
        assert_eq!(
            screen.input.lines().join(""),
            "token",
            "有输入 Ctrl+D 只能由输入框自行处理，不得清空内容"
        );

        screen.input = composer();
        screen.panel = Some(Panel::Shortcuts);
        let rendered = render(&mut screen, 110, 40);
        assert!(
            !rendered.contains("Ctrl+D"),
            "shortcuts 不应再列出 Ctrl+D: {rendered}"
        );
        assert!(
            rendered.contains("PgUp / PgDn"),
            "shortcuts 应渲染到 Ctrl+D 原位置之后，缺席断言才成立: {rendered}"
        );
        let _ = std::fs::remove_dir_all(runner.unwrap().cwd);
    }

    /// 移除「空输入 Ctrl+D 退出」后，Ctrl+D 不得落入面板的 `d` 单键动作
    /// （设置面板 Providers/Env/Memory 段均为「删除」）。
    #[tokio::test]
    async fn ctrl_d_in_settings_does_not_trigger_delete_shortcuts() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner_opt = Some(owner);
        let (permissions, _rx) = PermissionBroker::interactive();
        let permissions = Arc::new(permissions);
        let (events, _ev_rx) = mpsc::unbounded_channel();
        let mut active = None;
        let mut cancel = None;

        screen.panel = Some(Panel::Settings);
        let mut settings = CliSettingsState::from_view(&runner_opt.as_ref().unwrap().view());
        settings.tab = SettingsTab::Providers;
        let providers_before = settings.providers_draft.providers.len();
        assert!(
            providers_before > 1,
            "测试前提：默认模板需有多个 provider 才能触发删除分支"
        );
        screen.settings = Some(settings);

        let press_ctrl_d = |screen: &mut CliScreen,
                            runner: &mut Option<SessionRunner>,
                            permissions: &Arc<PermissionBroker>,
                            events: &mpsc::UnboundedSender<AgentEvent>,
                            active: &mut Option<ActiveTurn>,
                            cancel: &mut Option<oneshot::Sender<()>>| {
            handle_key(
                screen,
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                runner,
                permissions,
                events,
                active,
                cancel,
            )
            .unwrap()
        };

        assert!(
            !press_ctrl_d(
                &mut screen,
                &mut runner_opt,
                &permissions,
                &events,
                &mut active,
                &mut cancel
            ),
            "Ctrl+D 不应退出进程"
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .providers_draft
                .providers
                .len(),
            providers_before,
            "Ctrl+D 不得触发 provider 删除"
        );

        // 同法覆盖 EnvMemory 段：该段 `d` 为删除环境变量/记忆规则。
        {
            let settings = screen.settings.as_mut().unwrap();
            settings.tab = SettingsTab::EnvMemory;
            settings.config_draft.env.vars.push(cyber_core::EnvVar {
                key: "KEEP_ME".into(),
                value: "1".into(),
                sensitive: false,
            });
        }
        let env_before = screen
            .settings
            .as_ref()
            .unwrap()
            .config_draft
            .env
            .vars
            .len();
        assert!(env_before > 0);
        assert!(
            !press_ctrl_d(
                &mut screen,
                &mut runner_opt,
                &permissions,
                &events,
                &mut active,
                &mut cancel
            ),
            "Ctrl+D 不应退出进程"
        );
        assert_eq!(
            screen
                .settings
                .as_ref()
                .unwrap()
                .config_draft
                .env
                .vars
                .len(),
            env_before,
            "Ctrl+D 不得触发环境变量删除"
        );
        let _ = std::fs::remove_dir_all(runner_opt.unwrap().cwd);
    }

    /// `/exit` 是 `/quit` 的别名：空闲（命令层解析）与 busy（快捷键层早退）两条路径都必须真的退出。
    #[tokio::test]
    async fn exit_alias_quits_in_idle_and_busy_paths() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let mut runner = Some(owner);

        // 空闲：与 /quit 一样产出 CliAction::Quit（保存会话）。
        let mut owner = runner.take().unwrap();
        for line in ["/exit", "/EXIT"] {
            assert!(
                matches!(
                    cli_commands::execute(&mut owner, line).unwrap(),
                    CliAction::Quit
                ),
                "{line} 应与 /quit 等价"
            );
        }
        runner = Some(owner);

        // busy：输入经 handle_key 的 busy 分支（classify_busy_slash）解析，/exit 必须直接退出。
        screen.busy = true;
        screen.input = composer();
        screen.insert_text("/exit");
        let (permissions, _rx) = PermissionBroker::interactive();
        let (events, _ev_rx) = mpsc::unbounded_channel();
        assert!(
            handle_key(
                &mut screen,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &mut runner,
                &Arc::new(permissions),
                &events,
                &mut None,
                &mut None,
            )
            .unwrap(),
            "busy 状态下 /exit 必须直接退出"
        );
        let _ = std::fs::remove_dir_all(runner.unwrap().cwd);
    }

    /// 定位缓冲区内最上（且最左）的圆角框，校验四条边框字符是否被内容覆写。
    fn rounded_frame_violations(buffer: &ratatui::buffer::Buffer, w: u16, h: u16) -> Vec<String> {
        let mut origin = None;
        'find: for y in 0..h {
            for x in 0..w {
                if buffer[(x, y)].symbol() == "╭" {
                    origin = Some((x, y));
                    break 'find;
                }
            }
        }
        let Some((x0, y0)) = origin else {
            return vec!["未找到任何圆角框".into()];
        };
        let x1 = (x0 + 1..w).find(|&x| buffer[(x, y0)].symbol() == "╮");
        let Some(x1) = x1 else {
            return vec![format!("顶边 y={y0} 缺少右上角 ╮")];
        };
        let y1 = (y0 + 1..h).find(|&y| buffer[(x0, y)].symbol() == "╰");
        let Some(y1) = y1 else {
            return vec![format!("左边 x={x0} 缺少左下角 ╰")];
        };
        let mut bad = Vec::new();
        for y in y0..=y1 {
            let (left_expect, right_expect) = if y == y0 {
                ("╭", "╮")
            } else if y == y1 {
                ("╰", "╯")
            } else {
                ("│", "│")
            };
            for (x, expect) in [(x0, left_expect), (x1, right_expect)] {
                let got = buffer[(x, y)].symbol();
                if got != expect {
                    bad.push(format!("({x},{y}) 期望 {expect} 实际 {got:?}"));
                }
            }
        }
        bad
    }

    #[tokio::test]
    async fn sessions_picker_borders_intact_over_long_chat_content() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let long_title =
            "测试提问 → 仅测试提问（无需后续操作）  你好 你是什么模型 dkrw7ul84mtw 67 messages";
        screen.messages.push(Line::from(format!(
            " 工具调用、单选/多选、推荐标记均正常返回 {long_title}"
        )));
        for _ in 0..40 {
            screen.messages.push(Line::from(format!(
                "你好 你是什么模型 这是一段较长的对话内容用于验证边框是否被覆盖 {long_title}"
            )));
        }
        screen.picker = Some(cli_commands::CommandPicker {
            title: format!("Sessions · {long_title}"),
            kind: cli_commands::PickerKind::Sessions,
            items: vec![
                cli_commands::PickerItem {
                    label: long_title.into(),
                    detail: "dkrw7ul84mtw  67 messages".into(),
                    command: "/sessions 1".into(),
                },
                cli_commands::PickerItem {
                    label:
                        "多选测试 → 优先启用 custom_* 自定义工具（含超长的会话标题用于测试边框）"
                            .into(),
                    detail: "abc12345678  12 messages".into(),
                    command: "/sessions 2".into(),
                },
            ],
        });

        for (w, h) in [(120u16, 40u16), (100, 30), (80, 24), (60, 18)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|frame| {
                    draw_picker_dialog(
                        frame,
                        Rect::new(0, 0, w, h),
                        screen.picker.as_ref().unwrap(),
                        0,
                        false,
                        "测试提问 → 仅测试提问",
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let bad = rounded_frame_violations(buffer, w, h);
            assert!(bad.is_empty(), "会话选择器 w={w} 边框损坏: {bad:#?}");

            // 框体外围必须留白：对话文字不得紧贴边框（与 Skill 详情弹窗一致）。
            let frame_area = dialog_area(Rect::new(0, 0, w, h)).unwrap();
            let (x0, y0) = (frame_area.x, frame_area.y);
            let x1 = frame_area.x + frame_area.width - 1;
            let y1 = frame_area.y + frame_area.height - 1;
            let mut adjacency = Vec::new();
            for y in y0..=y1 {
                if x0 > 0 && buffer[(x0 - 1, y)].symbol() != " " {
                    adjacency.push(format!("左边贴边 ({},{y})", x0 - 1));
                }
                if x1 + 1 < w && buffer[(x1 + 1, y)].symbol() != " " {
                    adjacency.push(format!("右边贴边 ({},{y})", x1 + 1));
                }
            }
            for x in x0..=x1 {
                if y0 > 0 && buffer[(x, y0 - 1)].symbol() != " " {
                    adjacency.push(format!("上边贴边 ({x},{})", y0 - 1));
                }
                if y1 + 1 < h && buffer[(x, y1 + 1)].symbol() != " " {
                    adjacency.push(format!("下边贴边 ({x},{})", y1 + 1));
                }
            }
            assert!(
                adjacency.is_empty(),
                "会话选择器 w={w} 边框外圈未留白: {adjacency:#?}"
            );
        }
    }
}
