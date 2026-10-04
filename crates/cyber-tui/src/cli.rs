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
use cyber_core::{Config, EnvVar, MemoryRule, ProvidersConfig, ThinkingIntensity};
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
    self, CliAction, CommandForm, CommandPicker, CompletionItem, PickerKind,
};
use crate::headless::{HeadlessOutcome, SessionRunner};
#[allow(unused_imports)]
use crate::selection::default_selection_style;
use crate::selection::{ContentCoord, TextSelection};
use crate::theme::Theme;

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
enum Panel {
    Shortcuts,
    Ctf,
    Settings,
    Subagents,
    Jobs,
}

/// 设置面板标签页（共 7 个大类，全面覆盖所有设置）
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
        }
    }

    pub fn max_row(self, state: &CliSettingsState) -> usize {
        match self {
            Self::AgentModel => 11, // 0..=11 (12 rows)
            Self::UiWorkflow => 6,  // 0..=6 (7 rows)
            Self::Subagents => 4,   // 0..=4 (5 rows)
            Self::ToolsMcp => 1,    // 0..=1 (2 editable toggles)
            Self::Providers => state.providers_draft.providers.len().saturating_sub(1),
            Self::EnvMemory => {
                let env_count = state.config_draft.env.vars.len();
                let mem_count = state.config_draft.memory.rules.len();
                (env_count + mem_count).saturating_sub(1)
            }
            Self::StorageSystem => 1, // 0..=1 (2 rows: retention, log_level)
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
}

/// CLI 设置面板运行时状态
#[derive(Clone, Debug)]
pub struct CliSettingsState {
    pub tab: SettingsTab,
    pub selected_row: usize,
    pub dirty: bool,
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
    pub config_path: String,
    pub providers_path: String,
    pub sessions_dir: String,
    pub sessions_count: usize,
    pub has_project_config: bool,
}

impl CliSettingsState {
    pub fn new(config: &Config, providers: &ProvidersConfig) -> Self {
        Self {
            tab: SettingsTab::AgentModel,
            selected_row: 0,
            dirty: false,
            config_draft: config.clone(),
            providers_draft: providers.clone(),
            list_selected: 0,
            pending_discard_confirm: false,
            mcp_servers: Vec::new(),
            skills: Vec::new(),
            config_path: "~/.cyber/config.toml".into(),
            providers_path: "~/.cyber/providers.toml".into(),
            sessions_dir: "~/.cyber/sessions".into(),
            sessions_count: 0,
            has_project_config: false,
        }
    }

    pub(crate) fn from_runner(runner: &SessionRunner) -> Self {
        let mut state = Self::new(&runner.ctx.config, &runner.ctx.providers);
        state.config_path = runner.ctx.paths.config_file.display().to_string();
        state.providers_path = runner.ctx.paths.providers_file.display().to_string();
        state.sessions_dir = runner.ctx.paths.history_dir.display().to_string();
        state.sessions_count = runner.index.sessions.len();
        state.has_project_config = runner.ctx.project.is_some();
        if let Ok(mcp_cfg) = cyber_mcp::McpServersConfig::load(&runner.ctx.paths.mcp_servers_file) {
            state.mcp_servers = mcp_cfg
                .servers
                .into_iter()
                .map(|s| {
                    let (connected, tool_count) = match runner.registries.mcp.as_ref() {
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
        state.skills = runner
            .registries
            .skills
            .iter()
            .map(|s| SkillSummary {
                name: s.name().to_string(),
                source: match s.source {
                    cyber_skills::SkillSource::Global => "全局".into(),
                    cyber_skills::SkillSource::Project => "项目级".into(),
                },
            })
            .collect();
        state
    }

    pub fn next_tab(&mut self) {
        let all = SettingsTab::all();
        let idx = all.iter().position(|&t| t == self.tab).unwrap_or(0);
        self.tab = all[(idx + 1) % all.len()];
        self.selected_row = 0;
        self.list_selected = 0;
    }

    pub fn prev_tab(&mut self) {
        let all = SettingsTab::all();
        let idx = all.iter().position(|&t| t == self.tab).unwrap_or(0);
        self.tab = all[(idx + all.len() - 1) % all.len()];
        self.selected_row = 0;
        self.list_selected = 0;
    }

    pub fn set_tab(&mut self, tab: SettingsTab) {
        self.tab = tab;
        self.selected_row = 0;
        self.list_selected = 0;
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
        }
        self.dirty = true;
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

#[derive(Debug, Clone)]
pub struct QueuedPrompt {
    pub text: String,
    pub displayed: bool,
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
    pub todo_closed: bool,
    last_window_title: String,
    pub needs_clear: bool,
    pub queued_prompts: std::collections::VecDeque<QueuedPrompt>,
    pub active_steering_tx: Option<cyber_agent::SteeringSender>,
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
    pub attached_images: Vec<cyber_agent::AttachedImage>,
    pub next_image_id: usize,
    pub last_ctrl_c: Option<std::time::Instant>,
    pub last_paste_instant: Option<std::time::Instant>,
    pub history_area: Rect,
    #[allow(dead_code)]
    pub selection: Option<TextSelection>,
    pub is_dragging_scrollbar: bool,
}

struct FormState {
    form: CommandForm,
    inputs: Vec<TextArea<'static>>,
    selected: usize,
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
        }
    }

    fn capture(&mut self) {
        for (field, input) in self.form.fields.iter_mut().zip(&self.inputs) {
            field.value = input.lines().join("\n");
        }
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
        } else if let Some(form) = &mut self.form {
            if let Some(input) = form.inputs.get_mut(form.selected) {
                input.insert_str(clean(text));
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
        let changed = self.input.lines().join("\n") != item.value;
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
            todo_closed: false,
            last_window_title: String::new(),
            needs_clear: false,
            queued_prompts: std::collections::VecDeque::new(),
            active_steering_tx: None,
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
            attached_images: Vec::new(),
            next_image_id: 1,
            last_ctrl_c: None,
            last_paste_instant: None,
            history_area: Rect::default(),
            is_dragging_scrollbar: false,
            selection: None,
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
                    self.todo_closed = false;
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
                                sel.selecting = false;
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
                    sel.selecting = false;
                    if sel.is_empty() {
                        self.selection = None;
                    } else {
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
                        self.selection = Some(sel);
                    }
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
        let show_todo_table = !self.todo_closed && !todo_items.is_empty();
        let todo_height = if show_todo_table {
            (todo_items.len() as u16 + 2)
                .clamp(3, 7)
                .min(area.height.saturating_sub(header_height + input_height + 4))
        } else {
            0
        };
        let sections = Layout::vertical([
            Constraint::Length(header_height),
            Constraint::Min(0),
            Constraint::Length(todo_height),
            Constraint::Length(input_height + 1),
            Constraint::Length(1),
        ])
        .split(area);
        self.history_area = sections[1];
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
                        format!(" 💡 检测到新版本 V{newer}，可运行 cyber update 进行更新"),
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
            let catalog = cli_commands::commands();
            let lines = self
                .completions
                .iter()
                .enumerate()
                .skip(offset)
                .take(menu.height as usize)
                .map(|(index, item)| {
                    let usage = catalog
                        .iter()
                        .find(|spec| spec.name == item.value.trim())
                        .map(|spec| spec.usage)
                        .unwrap_or(&item.value);
                    select_row(
                        usage,
                        &item.description,
                        index == self.completion_selected,
                        menu.width,
                    )
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), menu);
        }
        if show_todo_table && sections[2].height >= 2 {
            draw_todo_table(frame, sections[2], &todo_items);
        }
        let input_area = sections[3];
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
            footer.push(self.append_ctf_mode_badge(last_line, sections[4].width as usize));
        }
        frame.render_widget(Paragraph::new(footer), sections[4]);
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
                    let text = format!(
                        "Enter          Send / select completion\nAlt/Shift+Enter New line\nTab · Up/Down   Complete / choose command\nF2 / Ctrl+P     Cycle mode (manual/auto/unlimited)\nF3 / Ctrl+,     Open Settings center panel\nCtrl+G          Toggle subagent panel (live transcripts)\nCtrl+B          Toggle background jobs panel\nUp / Down       History prompts / scroll line\nMouse Wheel     Scroll chat / approval arguments\nCtrl+O          Toggle tool details\nCtrl+T          Toggle CTF challenges panel (when CTF enabled)\nCtrl+C          Cancel task\nCtrl+D          Exit (empty input)\nPgUp / PgDn     Scroll conversation\n?               Toggle shortcuts (empty input)\n\n{}\n\nEsc closes this panel.",
                        cli_commands::commands()
                            .iter()
                            .map(|spec| format!("{:<30} {}", spec.usage, spec.desc))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                    popup(frame, sections[1], title, &text);
                }
                Panel::Ctf => {
                    let width = sections[1]
                        .width
                        .saturating_sub(6)
                        .clamp(48, 86)
                        .min(sections[1].width);
                    let height = sections[1]
                        .height
                        .saturating_sub(2)
                        .max(10)
                        .min(sections[1].height);
                    let popup_area = Rect::new(
                        sections[1].x + (sections[1].width.saturating_sub(width)) / 2,
                        sections[1].y + (sections[1].height.saturating_sub(height)) / 2,
                        width,
                        height,
                    );
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
            }
        } else if let Some(form) = &self.form {
            let lines = form
                .form
                .fields
                .iter()
                .zip(&form.inputs)
                .enumerate()
                .map(|(index, (field, input))| {
                    let value = if field.secret {
                        "*".repeat(input.lines().join("\n").chars().count().min(32))
                    } else {
                        clean(&input.lines().join("\n"))
                    };
                    select_row(
                        &field.name,
                        &value,
                        index == form.selected,
                        sections[1]
                            .width
                            .saturating_sub(4)
                            .min(90)
                            .saturating_sub(2),
                    )
                    .to_string()
                })
                .collect::<Vec<_>>();
            let visible = sections[1].height.saturating_sub(5).max(1) as usize;
            let offset = form.selected.saturating_sub(visible.saturating_sub(1));
            let text = format!(
                "{}\n\nTab/Shift+Tab fields · Enter next/save\nCtrl+S save · Esc cancel\n{}",
                lines
                    .into_iter()
                    .skip(offset)
                    .take(visible)
                    .collect::<Vec<_>>()
                    .join("\n"),
                clean(&self.status)
            );
            popup(
                frame,
                sections[1],
                &format!(" {} ", clean(&form.form.title)),
                &text,
            );
        } else if let Some(picker) = &self.picker {
            let visible = sections[1].height.saturating_sub(5).max(1) as usize;
            let offset = self
                .picker_selected
                .saturating_sub(visible.saturating_sub(1));
            let text = picker
                .items
                .iter()
                .enumerate()
                .skip(offset)
                .take(visible)
                .map(|(index, item)| {
                    select_row(
                        &item.label,
                        &item.detail,
                        index == self.picker_selected,
                        sections[1]
                            .width
                            .saturating_sub(4)
                            .min(90)
                            .saturating_sub(2),
                    )
                    .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n");
            let hint = if self.delete_pending.is_some() {
                "Press d again to delete selected session · Esc cancel"
            } else if matches!(picker.kind, PickerKind::Sessions) {
                "Up/Down choose · Enter open · n new · d delete · Esc close"
            } else {
                "Up/Down choose · Enter select · Esc close"
            };
            popup(
                frame,
                sections[1],
                &format!(" {} ", clean(&picker.title)),
                &format!("{text}\n\n{hint}"),
            );
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

        let task_state = if let Some(r) = task_result {
            let status_str = r.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status_str == "completed" {
                ToolCardState::Success
            } else {
                ToolCardState::Error
            }
        } else if card.state == ToolCardState::Pending {
            if let Some(last_msg) = task_progress_msgs.last() {
                if last_msg.contains("completed") {
                    ToolCardState::Success
                } else if last_msg.contains("error")
                    || last_msg.contains("timed_out")
                    || last_msg.contains("failed")
                {
                    ToolCardState::Error
                } else {
                    ToolCardState::Pending
                }
            } else {
                ToolCardState::Pending
            }
        } else if card.state == ToolCardState::Error {
            ToolCardState::Error
        } else {
            ToolCardState::Success
        };

        let (icon, color, bg, default_status) = match task_state {
            ToolCardState::Pending => ("◇", ACCENT, PENDING_BG, "running"),
            ToolCardState::Success => ("✓", SUCCESS, SUCCESS_BG, "done"),
            ToolCardState::Error => ("✗", ERROR, ERROR_BG, "failed"),
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
            Line::from(vec![
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
            ])
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
        for line in body.iter().take(shown) {
            let content = if expanded {
                line.clone()
            } else {
                let clipped = clip_cells(line, usize::from(width.saturating_sub(4).max(1)) * 3);
                clipped_body |= clipped != *line;
                clipped
            };
            let fg_color = if task_state == ToolCardState::Error
                && (line.starts_with("Error:") || line.starts_with("failed"))
            {
                ERROR
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
            Line::from(vec![
                Span::styled("╰─ ", Style::default().fg(color).bg(bg)),
                Span::styled(footer, Style::default().fg(DIM).bg(bg)),
            ])
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
        Line::from(vec![
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
        ])
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
    for line in body.iter().skip(start).take(shown) {
        let content = if expanded {
            line.clone()
        } else {
            let clipped = clip_cells(line, usize::from(width.saturating_sub(4).max(1)) * 3);
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
        Line::from(vec![
            Span::styled("╰─ ", Style::default().fg(color).bg(bg)),
            Span::styled(footer, Style::default().fg(DIM).bg(bg)),
        ])
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

fn draw_todo_table(frame: &mut Frame, area: Rect, items: &[cyber_core::TodoItem]) {
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

    let title = format!(" 📋 任务清单 [{completed}/{total}] · 输入 /todo close 收起 ");

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

    let will_truncate = items.len() > max_lines;
    let display_count = if will_truncate {
        max_lines.saturating_sub(1)
    } else {
        items.len().min(max_lines)
    };

    let mut lines = Vec::with_capacity(max_lines);
    for item in items.iter().take(display_count) {
        let (symbol, color) = match item.status {
            cyber_core::TodoStatus::Pending => ("[ ]", MUTED),
            cyber_core::TodoStatus::InProgress => ("[>]", ACCENT),
            cyber_core::TodoStatus::Completed => ("[x]", SUCCESS),
            cyber_core::TodoStatus::Failed => ("[!]", ERROR),
        };
        let mut spans = vec![
            Span::styled(
                format!(" {symbol} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("#{} ", item.id), Style::default().fg(MUTED)),
            Span::styled(clean(&item.title), Style::default().fg(FG)),
        ];
        if let Some(notes) = &item.notes {
            if !notes.trim().is_empty() {
                spans.push(Span::styled(
                    format!(" (备注: {})", clean(notes)),
                    Style::default().fg(MUTED),
                ));
            }
        }
        lines.push(Line::from(spans));
    }

    if will_truncate {
        let remaining = items.len().saturating_sub(display_count);
        lines.push(Line::from(vec![Span::styled(
            format!("   ... 还有 {remaining} 项任务（输入 /todo list 查看全部）"),
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        )]));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

/// 居中面板几何：宽 `area.width-6` clamp 60..100，高 `area.height-2` clamp 12..30。
/// 过小的区域返回 None（面板不渲染）。
fn panel_geometry(area: Rect) -> Option<Rect> {
    let width = area.width.saturating_sub(6).clamp(60, 100).min(area.width);
    let height = area.height.saturating_sub(2).clamp(12, 30).min(area.height);
    if width < 20 || height < 6 {
        return None;
    }
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
    frame.render_widget(Clear, popup_area);
    if state.selected >= snapshot.len() && !snapshot.is_empty() {
        state.selected = snapshot.len() - 1;
    }

    let title = if snapshot.is_empty() {
        Line::from(vec![Span::styled(
            " ◉ 子代理 (Subagents) ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )])
    } else {
        Line::from(vec![
            Span::styled(
                " ◉ 子代理 (Subagents) ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("· {} 次运行 ", snapshot.len()),
                Style::default().fg(DIM),
            ),
        ])
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

    // 列宽：前缀 2 + #id 4 + 状态 8 + 名称 16，其余给最新活动。
    const PREFIX_W: usize = 2;
    const ID_W: usize = 4;
    const BADGE_W: usize = 8;
    const NAME_W: usize = 16;
    let last_budget = inner_width
        .saturating_sub(PREFIX_W + ID_W + BADGE_W + NAME_W)
        .max(1);

    let mut body: Vec<Line<'static>> = Vec::new();
    // 表头 + 分隔线
    body.push(Line::from(vec![
        Span::styled("  ", Style::default().fg(DIM)),
        Span::styled(pad_cells("#", 3) + " ", Style::default().fg(DIM)),
        Span::styled(pad_cells("状态", BADGE_W), Style::default().fg(DIM)),
        Span::styled(pad_cells("名称", NAME_W), Style::default().fg(DIM)),
        Span::styled("最新活动", Style::default().fg(DIM)),
    ]));
    body.push(Line::styled(
        "─".repeat(inner_width),
        Style::default().fg(DIM),
    ));
    for (index, run) in snapshot.iter().enumerate() {
        let (badge, badge_color) = subagent_badge(run.status);
        let last = run
            .lines
            .last()
            .map(|line| single_line(line))
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
        body.push(Line::styled(
            "   暂无子代理运行记录".to_string(),
            Style::default().fg(MUTED),
        ));
        body.push(Line::styled(
            "   （由 delegate_tasks 或 /bg run 产生；Ctrl+B 查看后台任务）".to_string(),
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
            clip_cells(footer_hint, inner_width),
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
            Line::from(vec![
                Span::styled(
                    " ⚡ 后台任务 ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("#{} · {kind} · {}", job.id, single_line(&job.name)),
                    Style::default().fg(FG).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" · {badge}"), Style::default().fg(color)),
            ])
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
                        clip_cells(&format!("{elapsed:>5.1}s  {line}"), inner_width),
                        Style::default().fg(FG),
                    ));
                    shown += 1;
                }
            } else {
                for line in &job.lines {
                    body.push(Line::styled(
                        clip_cells(line, inner_width),
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
                inner_width,
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
                inner_width,
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
                "   暂无后台任务（/bg shell <cmd> 或 /bg run <prompt> 启动）".to_string(),
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
            clip_cells(footer_hint, inner_width),
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
    let width = bounds
        .width
        .saturating_sub(4)
        .max(4)
        .min(bounds.width)
        .min(90);
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
    frame.render_widget(Clear, area);
    frame.render_widget(
        paragraph.block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(title)
                .title_style(Style::default().fg(ACCENT))
                .border_style(Style::default().fg(ACCENT)),
        ),
        area,
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

fn render_setting_row(
    selected: bool,
    label: &'static str,
    value: String,
    hint: String,
    _width: u16,
) -> Line<'static> {
    let pointer = if selected { "▶ " } else { "  " };
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

    let label_padded = format!("{:<26}", label);
    let val_padded = format!("{:<22}", value);

    Line::from(vec![
        Span::styled(pointer, ptr_style),
        Span::styled(label_padded, label_style),
        Span::styled(val_padded, val_style),
        Span::raw(" "),
        Span::styled(hint, Style::default().fg(DIM)),
    ])
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
            "默认服务商 (Provider)",
            format!("◄ [ {} ] ►", prov),
            format!("◄/► 切换 (已配置 {} 个 Provider)", prov_count),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 1,
            "对应模型 (Model)",
            model,
            "由所选 Provider 决定 (/model 细调)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 2,
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
            "工具执行步数上限 (Max Steps)",
            format!("◄ [ {} 步 ] ►", settings.config_draft.agent.max_steps),
            "◄/► 调整 (范围: 1-1000 步)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 6,
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
            "异常重试次数 (Retry Attempts)",
            format!("◄ [ {} 次 ] ►", settings.config_draft.agent.retry_attempts),
            "◄/► 调整 (范围: 0-20 次，0 为不重试)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 11,
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
            "主题配色 (Theme)",
            format!("◄ [ {} ] ►", settings.config_draft.ui.theme),
            "即时生效 (支持 cyberpunk/dracula/nord 等)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 1,
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
            "默认启动模式 (Default Mode)",
            format!("◄ [ {} ] ►", settings.config_draft.ui.default_mode),
            "重启生效 (chat / workflow / dashboard)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 3,
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
            "单轮最大任务数 (Max Tasks)",
            format!("◄ [ {} 个 ] ►", sub.max_tasks),
            "一次最多拆分派发的子任务上限 (1-64)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 2,
            "最大并行执行数 (Parallel)",
            format!("◄ [ {} 并发 ] ►", sub.max_parallel),
            "后台同时运行的独立子代理线程上限 (1-16)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 3,
            "单任务超时时限 (Timeout)",
            format!("◄ [ {} 秒 ] ►", sub.timeout_secs),
            "单个子任务最大执行时限 (10-3600秒)".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 4,
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

    let mut lines = vec![
        render_setting_row(
            settings.selected_row == 0,
            "优先容器执行 (Docker)",
            if settings.config_draft.tools.prefer_docker {
                "[ ● 开启 ]".into()
            } else {
                "[ ○ 关闭 ]".into()
            },
            "若环境安装 Docker，则优先容器隔离执行".into(),
            area.width,
        ),
        render_setting_row(
            settings.selected_row == 1,
            "CTF 渗透答题模式 (CTF Mode)",
            if screen.ctf_enabled {
                "[ ● 开启 ]".into()
            } else {
                "[ ○ 关闭 ]".into()
            },
            "启用 Writeup 自动生成与专属解题工具".into(),
            area.width,
        ),
        render_setting_row(
            false,
            "额外环境变量 PATH",
            extra_path_str,
            "附加注入到 Shell 子进程的 PATH 变量".into(),
            area.width,
        ),
        Line::raw(""),
    ];

    lines.push(Line::from(vec![Span::styled(
        format!("  [已配置 MCP 服务 ({} 个)]", settings.mcp_servers.len()),
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

    lines.push(Line::from(vec![Span::styled(
        format!("  [已加载 Skills 技能 ({} 个)]", settings.skills.len()),
        Style::default().fg(CODE).add_modifier(Modifier::BOLD),
    )]));
    if settings.skills.is_empty() {
        lines.push(Line::styled(
            "    • 暂无已加载 Skill",
            Style::default().fg(DIM),
        ));
    } else {
        let skills_str = settings
            .skills
            .iter()
            .take(6)
            .map(|s| format!("{} ({})", s.name, s.source))
            .collect::<Vec<_>>()
            .join(" · ");
        lines.push(Line::from(vec![
            Span::raw("    • "),
            Span::styled(skills_str, Style::default().fg(MUTED)),
        ]));
    }

    render_scrollable_content(
        frame,
        area,
        lines,
        settings.selected_row,
        settings.selected_row,
    );
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
            lines.push(Line::raw(""));

            let end_line = start_line + 1;
            if is_sel {
                focused_start = start_line;
                focused_end = end_line;
            }
        }
    }

    render_scrollable_content(frame, area, lines, focused_start, focused_end);
}

fn draw_tab_env_memory(frame: &mut Frame, area: Rect, settings: &CliSettingsState) {
    let mut lines = Vec::new();
    let env_count = settings.config_draft.env.vars.len();
    let mem_count = settings.config_draft.memory.rules.len();
    let mut focused_start = 0;
    let mut focused_end = 0;

    let env_header = if env_count > 0 {
        format!(
            "  [自定义环境变量 (注入 Shell/Agent 子进程)] [{}/{} 项] (A 添加 · D 删除 · 空格 切换脱敏):",
            if settings.selected_row < env_count {
                settings.selected_row + 1
            } else {
                env_count
            },
            env_count
        )
    } else {
        "  [自定义环境变量 (注入 Shell/Agent 子进程)] (A 添加 · D 删除 · 空格 切换脱敏):"
            .to_string()
    };

    lines.push(Line::styled(
        env_header,
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ));
    if settings.config_draft.env.vars.is_empty() {
        lines.push(Line::styled(
            "    (暂无环境变量，按 A 添加)",
            Style::default().fg(DIM),
        ));
    } else {
        for (i, var) in settings.config_draft.env.vars.iter().enumerate() {
            let is_sel = i == settings.selected_row;
            let start_line = lines.len();
            let pointer = if is_sel { "▶ " } else { "  " };
            let ptr_style = if is_sel {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM)
            };
            let val_display = if var.sensitive {
                "sk-************************ (脱敏保护)"
            } else {
                &var.value
            };
            lines.push(Line::from(vec![
                Span::styled(pointer, ptr_style),
                Span::styled(
                    format!("{:<20}", var.key),
                    Style::default().fg(FG).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    val_display,
                    Style::default().fg(if var.sensitive { MUTED } else { FG }),
                ),
            ]));
            if is_sel {
                focused_start = start_line;
                focused_end = start_line;
            }
        }
    }

    lines.push(Line::raw(""));

    let mem_header = if mem_count > 0 {
        format!(
            "  [长期用户记忆约定规则 (Memory Rules)] [{}/{} 项] (空格 切换启用/禁用 · Tab 切换作用域 · A/D 增删):",
            if settings.selected_row >= env_count {
                settings.selected_row - env_count + 1
            } else {
                mem_count
            },
            mem_count
        )
    } else {
        "  [长期用户记忆约定规则 (Memory Rules)] (空格 切换启用/禁用 · Tab 切换作用域 · A/D 增删):"
            .to_string()
    };

    lines.push(Line::styled(
        mem_header,
        Style::default().fg(CODE).add_modifier(Modifier::BOLD),
    ));
    if settings.config_draft.memory.rules.is_empty() {
        lines.push(Line::styled(
            "    (暂无记忆规则，按 A 添加)",
            Style::default().fg(DIM),
        ));
    } else {
        for (i, rule) in settings.config_draft.memory.rules.iter().enumerate() {
            let row_idx = env_count + i;
            let is_sel = row_idx == settings.selected_row;
            let start_line = lines.len();
            let pointer = if is_sel { "▶ " } else { "  " };
            let ptr_style = if is_sel {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM)
            };
            let status_badge = if rule.enabled {
                Span::styled("[ ● 开启 ]", Style::default().fg(SUCCESS))
            } else {
                Span::styled("[ ○ 关闭 ]", Style::default().fg(DIM))
            };
            let scope_badge = match rule.scope.as_str() {
                "both" => "[全局与项目]",
                "project" => "[项目级]",
                _ => "[全局]",
            };
            lines.push(Line::from(vec![
                Span::styled(pointer, ptr_style),
                status_badge,
                Span::raw(" "),
                Span::styled(format!("{:<10}", scope_badge), Style::default().fg(MUTED)),
                Span::raw(" "),
                Span::styled(&rule.prompt, Style::default().fg(FG)),
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

fn draw_settings_panel(
    frame: &mut Frame,
    bounds: Rect,
    settings: &CliSettingsState,
    screen: &CliScreen,
) {
    if bounds.width < 40 || bounds.height < 6 {
        return;
    }

    let width = bounds
        .width
        .saturating_sub(2)
        .clamp(60, 120)
        .min(bounds.width);
    let height = bounds
        .height
        .saturating_sub(1)
        .clamp(6, 36)
        .min(bounds.height);
    let popup_area = Rect::new(
        bounds.x + (bounds.width.saturating_sub(width)) / 2,
        bounds.y + (bounds.height.saturating_sub(height)) / 2,
        width,
        height,
    );

    frame.render_widget(Clear, popup_area);

    let border_color = if settings.dirty { ACCENT } else { DIM };
    let dirty_badge = if settings.dirty {
        " [● 已修改] "
    } else {
        ""
    };
    let title = Line::from(vec![
        Span::styled(
            " ⚙ 设置中心 (Settings) ",
            Style::default().fg(CODE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            dirty_badge,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
    ]);
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
    }

    let tip_text = match settings.tab {
        SettingsTab::AgentModel => "💡 提示: 思考强度用于控制 DeepSeek reasoning / Claude thinking 预算。按 Ctrl+S 立即保存生效。",
        SettingsTab::UiWorkflow => "💡 提示: 若需使用终端原生划词复制功能，可在此处将鼠标捕获关闭。主题与鼠标即时生效。",
        SettingsTab::Subagents => "💡 提示: 子任务并发数受本地 CPU 与服务商 API 频率限制，推荐配置为 2~6 个并发。",
        SettingsTab::ToolsMcp => "💡 提示: MCP 服务器配置存储于 mcp.json，可输入 /mcp 查看详细状态或通过配置文件增删。",
        SettingsTab::Providers => "💡 提示: 按 Enter 即可快速将高亮服务商切换为全局默认 Provider；按 A 键可打开预设/自定义向导添加服务商，T 键测试连通性，M 键浏览模型。",
        SettingsTab::EnvMemory => "💡 提示: 标记为脱敏保护的环境变量在终端界面和日志中均会自动遮蔽，保障凭据安全。",
        SettingsTab::StorageSystem => "💡 提示: 日志级别调整后将在后台日志中实时生效；如需排查详细工具执行流，建议调至 debug 级别。",
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
        SettingsTab::EnvMemory => Line::from(vec![
            Span::styled(
                " [ A 添加 ] ",
                Style::default().fg(CODE).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(" [ D 删除 ] ", Style::default().fg(ERROR)),
            Span::raw(" "),
            Span::styled(" [ Space 切换 ] ", Style::default().fg(ACCENT)),
            Span::raw(" "),
            Span::styled(
                " [ S 保存 (Ctrl+S) ] ",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(" [ Esc 关闭 ] ", Style::default().fg(MUTED)),
        ]),
        _ => Line::from(vec![
            Span::styled(
                " [ S 保存生效 (Ctrl+S) ] ",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(" [ Esc 放弃退出 ] ", Style::default().fg(MUTED)),
            Span::raw("  "),
            Span::styled(" [ R 恢复默认 ] ", Style::default().fg(CODE)),
        ]),
    };
    frame.render_widget(Paragraph::new(buttons), chunks[4]);

    let nav_hint = match settings.tab {
        SettingsTab::Providers => "操作: ↑/↓ 选择 · Enter 设为默认 · A 添加 (预设/自定义协议) · T 测活 · M 模型 · E 编辑 · D 删除 · Esc 关闭",
        SettingsTab::EnvMemory => "操作: ↑/↓ 选择项目 · A/D 增删 · Space 切换脱敏/启用 · Tab 切换分类 · 1-7 直达 · Esc 关闭",
        _ => "操作: ↑/↓ 选择项目 · ←/→ 微调数值 · Tab 轮换标签 · 1-7 直达分类 · Esc 关闭",
    };
    frame.render_widget(
        Paragraph::new(Line::styled(nav_hint, Style::default().fg(DIM))),
        chunks[5],
    );

    if settings.pending_discard_confirm {
        draw_discard_modal(frame, popup_area);
    }
}

fn save_settings_state(
    screen: &mut CliScreen,
    runner: &mut Option<SessionRunner>,
    permissions: &Arc<PermissionBroker>,
) {
    let Some(settings) = screen.settings.take() else {
        screen.panel = None;
        return;
    };
    if let Some(r) = runner.as_mut() {
        let mouse_changed = settings.config_draft.ui.mouse != r.ctx.config.ui.mouse;
        let new_mouse = settings.config_draft.ui.mouse;

        if let Err(e) = cyber_core::save_config(&settings.config_draft, &r.ctx.paths.config_file) {
            screen.status = format!("保存配置失败: {e}");
            screen.panel = None;
            return;
        }

        let _ = cyber_core::save_providers(&settings.providers_draft, &r.ctx.paths.providers_file);
        r.ctx.providers = settings.providers_draft;

        r.ctx.config = settings.config_draft;

        if let Some(mode_str) = &r.ctx.config.agent.permission_mode {
            if let Some(mode) = PermissionMode::parse(mode_str) {
                screen.permission_mode = mode;
                permissions.set_mode(mode);
            }
        }
        screen.effort = r.ctx.config.agent.thinking_intensity;

        if mouse_changed {
            if new_mouse {
                let _ = execute!(io::stdout(), EnableMouseCapture);
            } else {
                let _ = execute!(io::stdout(), DisableMouseCapture);
            }
        }

        screen.sync(r);
    }
    screen.panel = None;
    screen.status = "✔ 设置已保存并立即生效".into();
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
                    save_settings_state(screen, runner, permissions);
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

    if control && (key.code == KeyCode::Char('s') || key.code == KeyCode::Char('S')) {
        save_settings_state(screen, runner, permissions);
        return Ok(false);
    }

    if key.code == KeyCode::Esc {
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
        return Ok(false);
    }

    if let KeyCode::Char(ch @ '1'..='7') = key.code {
        if let Some(settings) = screen.settings.as_mut() {
            let tabs = SettingsTab::all();
            let idx = (ch as usize) - ('1' as usize);
            if let Some(&tab) = tabs.get(idx) {
                settings.set_tab(tab);
            }
        }
        return Ok(false);
    }

    if key.code == KeyCode::Char('r') || key.code == KeyCode::Char('R') {
        if let Some(settings) = screen.settings.as_mut() {
            settings.reset_current_tab();
            screen.status = format!("已将 {} 恢复为默认配置", settings.tab.title());
        }
        return Ok(false);
    }

    if key.code == KeyCode::Char('s') || key.code == KeyCode::Char('S') {
        save_settings_state(screen, runner, permissions);
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
            _ => {}
        },
        SettingsTab::Providers => {
            let names = settings.providers_draft.sorted_names();
            if let Some(name) = names.get(settings.selected_row) {
                match key.code {
                    KeyCode::Enter => {
                        settings.config_draft.agent.default_provider = name.clone();
                        settings.dirty = true;
                    }
                    KeyCode::Char('a') | KeyCode::Char('A') => {
                        if let Some(r) = runner.as_mut() {
                            if let Ok(CliAction::Picker(p)) =
                                cli_commands::execute(r, "/provider wizard")
                            {
                                screen.picker = Some(p);
                                screen.picker_selected = 0;
                            }
                        }
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
                                    screen.message(
                                        "Ping",
                                        &format!(
                                            "🟢 [{target_name}] 连通正常 (延迟: {rtt}ms，发现 {} 款可用模型)",
                                            models.len()
                                        ),
                                        SUCCESS,
                                    );
                                    screen.status =
                                        format!("服务商 [{target_name}] 连通正常 ({rtt}ms)");
                                }
                                Err(e) => {
                                    screen.message(
                                        "Ping",
                                        &format!("🔴 [{target_name}] 连通失败 ({rtt}ms): {e}"),
                                        ERROR,
                                    );
                                    screen.status = format!("服务商 [{target_name}] 连通失败");
                                }
                            }
                        }
                    }
                    KeyCode::Char('m') | KeyCode::Char('M') => {
                        if let Some(r) = runner.as_mut() {
                            if let Ok(CliAction::Picker(p)) =
                                cli_commands::execute(r, &format!("/model {name}"))
                            {
                                screen.picker = Some(p);
                                screen.picker_selected = 0;
                            }
                        }
                    }
                    KeyCode::Char('e') | KeyCode::Char('E') => {
                        if let Some(r) = runner.as_mut() {
                            if let Ok(CliAction::Form(form)) =
                                cli_commands::execute(r, &format!("/provider edit {name}"))
                            {
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
                if let Some(r) = runner.as_mut() {
                    if let Ok(CliAction::Picker(p)) = cli_commands::execute(r, "/provider wizard") {
                        screen.picker = Some(p);
                        screen.picker_selected = 0;
                    }
                }
            }
        }
        SettingsTab::EnvMemory => {
            let env_len = settings.config_draft.env.vars.len();
            if settings.selected_row < env_len {
                match key.code {
                    KeyCode::Char(' ') => {
                        if let Some(var) = settings
                            .config_draft
                            .env
                            .vars
                            .get_mut(settings.selected_row)
                        {
                            var.sensitive = !var.sensitive;
                            settings.dirty = true;
                        }
                    }
                    KeyCode::Char('d') | KeyCode::Char('D') => {
                        settings.config_draft.env.vars.remove(settings.selected_row);
                        settings.dirty = true;
                        let max_r = settings.tab.max_row(settings);
                        settings.selected_row = settings.selected_row.min(max_r);
                    }
                    KeyCode::Char('a') | KeyCode::Char('A') => {
                        let new_key = format!("CUSTOM_VAR_{}", env_len + 1);
                        settings.config_draft.env.vars.push(EnvVar {
                            key: new_key,
                            value: "value".into(),
                            sensitive: false,
                        });
                        settings.dirty = true;
                    }
                    _ => {}
                }
            } else {
                let rule_idx = settings.selected_row - env_len;
                if let Some(rule) = settings.config_draft.memory.rules.get_mut(rule_idx) {
                    match key.code {
                        KeyCode::Char(' ') | KeyCode::Enter => {
                            rule.enabled = !rule.enabled;
                            settings.dirty = true;
                        }
                        KeyCode::Tab => {
                            rule.scope = match rule.scope.as_str() {
                                "both" => "project".into(),
                                "project" => "global".into(),
                                _ => "both".into(),
                            };
                            settings.dirty = true;
                        }
                        KeyCode::Char('d') | KeyCode::Char('D') => {
                            settings.config_draft.memory.rules.remove(rule_idx);
                            settings.dirty = true;
                            let max_r = settings.tab.max_row(settings);
                            settings.selected_row = settings.selected_row.min(max_r);
                        }
                        KeyCode::Char('a') | KeyCode::Char('A') => {
                            settings.config_draft.memory.rules.push(MemoryRule {
                                enabled: true,
                                scope: "both".into(),
                                prompt: "新记忆规则".into(),
                            });
                            settings.dirty = true;
                        }
                        _ => {}
                    }
                } else if key.code == KeyCode::Char('a') || key.code == KeyCode::Char('A') {
                    settings.config_draft.memory.rules.push(MemoryRule {
                        enabled: true,
                        scope: "both".into(),
                        prompt: "新记忆规则".into(),
                    });
                    settings.dirty = true;
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

pub async fn run_cli(cwd: &Path, mock: bool) -> color_eyre::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        color_eyre::eyre::bail!(
            "Interactive CLI requires a terminal; use `cyber run` for scripts."
        );
    }
    let runner = SessionRunner::new(cwd, mock).await?;
    let mut screen = CliScreen::new(&runner);
    let mut runner = Some(runner);
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
            &format!("💡 检测到新版本 V{newer}，可运行 cyber update 进行更新"),
            AMBER,
        );
    }
    let (update_tx, mut update_rx) = mpsc::unbounded_channel::<String>();
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
                }
                Some(new_ver) = update_rx.recv() => {
                    if screen.new_version.as_deref() != Some(&new_ver) {
                        screen.new_version = Some(new_ver.clone());
                        screen.message(
                            "Notice",
                            &format!("💡 检测到新版本 V{new_ver}，可运行 cyber update 进行更新"),
                            AMBER,
                        );
                    }
                }
                _ = tick.tick() => {
                    if !TERMINAL_ACTIVE.load(Ordering::SeqCst) {
                        color_eyre::eyre::bail!("A task panicked; the terminal was restored. See the diagnostic above.");
                    }
                    #[cfg(windows)]
                    if crate::win_paste::check_ctrl_v_rising_edge(&mut was_ctrl_v_down)
                        && crate::win_paste::is_terminal_foreground()
                        && screen.approval.is_none()
                        && screen.form.is_none()
                        && screen.handle_clipboard_image_only()
                    {
                        screen.update_completions(runner.as_ref());
                    }
                    if let Some(text) = paste.flush_if_stale() {
                        screen.insert_text(&text);
                        screen.completion_closed = false;
                        screen.completion_accepted = false;
                        screen.update_completions(runner.as_ref());
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
                    if let Some(next_prompt) = screen.queued_prompts.pop_front() {
                        spawn_turn(
                            &mut screen,
                            &mut runner,
                            next_prompt.text,
                            next_prompt.displayed,
                            &permissions,
                            &event_tx,
                            &mut active,
                            &mut cancel,
                        );
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
                                        screen.completion_closed = false;
                                        screen.completion_accepted = false;
                                        screen.update_completions(runner.as_ref());
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
                            screen.insert_text(&text);
                            screen.completion_closed = false;
                            screen.completion_accepted = false;
                            screen.update_completions(runner.as_ref());
                        }
                        Some(Ok(Event::Mouse(mouse))) => {
                            match mouse.kind {
                                MouseEventKind::ScrollUp => {
                                    if screen.panel == Some(Panel::Ctf) {
                                        if screen.ctf_detail_view {
                                            screen.ctf_detail_scroll = screen.ctf_detail_scroll.saturating_sub(3);
                                        } else if screen.ctf_selected > 0 {
                                            screen.ctf_selected = screen.ctf_selected.saturating_sub(1);
                                        }
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
                                    if screen.panel == Some(Panel::Ctf) {
                                        if screen.ctf_detail_view {
                                            screen.ctf_detail_scroll = screen.ctf_detail_scroll.saturating_add(3);
                                        } else {
                                            let len = screen.ctf_challenges_count();
                                            if len > 0 && screen.ctf_selected + 1 < len {
                                                screen.ctf_selected += 1;
                                            }
                                        }
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
    if let Err(e) = result {
        eprintln!("[error] {e}");
        std::process::exit(1);
    }
    std::process::exit(0);
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
            });
        }
    }
    Ok(())
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
    if control
        && key.code == KeyCode::Char('d')
        && screen.input.is_empty()
        && screen.approval.is_none()
        && screen.form.is_none()
    {
        return Ok(true);
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
    if screen.approval.is_none() && key.code == KeyCode::Esc {
        if screen.form.take().is_some() || screen.picker.take().is_some() {
            screen.delete_pending = None;
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
            if let Some(r) = runner.as_ref() {
                screen.settings = Some(CliSettingsState::from_runner(r));
            } else {
                screen.settings = Some(CliSettingsState::new(
                    &Config::default(),
                    &ProvidersConfig::default(),
                ));
            }
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
    if screen.panel == Some(Panel::Ctf) {
        let len = screen.ctf_challenges_count();
        match key.code {
            KeyCode::Up | KeyCode::Char('8') => {
                if screen.ctf_detail_view {
                    screen.ctf_detail_scroll = screen.ctf_detail_scroll.saturating_sub(1);
                } else if screen.ctf_selected > 0 {
                    screen.ctf_selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('2') => {
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
            KeyCode::Enter if !screen.ctf_detail_view && len > 0 && screen.ctf_selected < len => {
                screen.ctf_detail_view = true;
                screen.ctf_detail_scroll = 0;
            }
            _ => {}
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
    if is_esc {
        if screen.approval.is_none() && (screen.form.is_some() || screen.picker.is_some()) {
            screen.form = None;
            screen.picker = None;
            screen.delete_pending = None;
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
        let count = form.inputs.len();
        let save = (control && key.code == KeyCode::Char('s'))
            || (key.code == KeyCode::Enter
                && key.modifiers.is_empty()
                && form.selected + 1 >= count);
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
                KeyCode::BackTab => form.selected = (form.selected + count - 1) % count,
                KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    form.selected = (form.selected + count - 1) % count
                }
                KeyCode::Tab => form.selected = (form.selected + 1) % count,
                KeyCode::Enter if key.modifiers.is_empty() => {
                    form.selected = (form.selected + 1) % count
                }
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
            if let Some(owner) = runner.as_mut() {
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
        if text.eq_ignore_ascii_case("/quit") {
            return Ok(true);
        }
        if text.eq_ignore_ascii_case("/cancel") {
            screen.input = composer();
            screen.completions.clear();
            screen.reply(PermissionDecision::Deny);
            if let Some(tx) = cancel.take() {
                let _ = tx.send(());
            }
            screen.status = "Cancelling".into();
            return Ok(false);
        }
        if text.starts_with('/') {
            // `/bg` 命令族 busy 时可用（shell/list/kill/tail 不触碰 runner；
            // run 需要 provider 上下文，返回忙碌提示）。
            let bg_args = text
                .strip_prefix("/bg")
                .unwrap_or_default()
                .trim()
                .to_string();
            if text.eq_ignore_ascii_case("/bg") || text.to_ascii_lowercase().starts_with("/bg ") {
                screen.input = composer();
                screen.completions.clear();
                match cli_commands::parse_bg(&bg_args) {
                    Ok(cli_commands::CliJobs::Run { .. }) => {
                        screen.message(
                            "后台任务",
                            "AI 正在运行，请先 /cancel 或等待完成后再启动后台子代理",
                            MUTED,
                        );
                    }
                    Ok(jobs) => {
                        return apply_action(
                            screen,
                            CliAction::Jobs(jobs),
                            runner,
                            permissions,
                            events,
                            active,
                            cancel,
                        )
                    }
                    Err(error) => screen.message("Error", &error.to_string(), ERROR),
                }
                return Ok(false);
            }

            let lower = text.to_ascii_lowercase();
            if lower == "/subagents stop" || lower.starts_with("/subagents stop ") {
                screen.input = composer();
                screen.completions.clear();
                let target = lower.strip_prefix("/subagents stop").unwrap_or("").trim();
                let message = match cli_commands::stop_subagents(
                    &screen.background,
                    &screen.subagents,
                    target,
                ) {
                    Ok(msg) => msg,
                    Err(err) => err.to_string(),
                };
                screen.message("Subagents", &message, ACCENT);
                return Ok(false);
            }
            return Ok(false);
        }

        if has_alt_or_ctrl {
            // Alt+Enter / Ctrl+Enter: 即时导向（立刻打断当前生成并读取新指示）
            screen.input = composer();
            screen.completions.clear();
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
                screen.todo_closed = false;
            }
            screen.message(&title, &text, MUTED);
        }
        CliAction::TodoVisibility(open) => {
            screen.todo_closed = !open;
            if open {
                screen.status = "任务清单已展开".into();
            } else {
                screen.status = "任务清单已收起（输入 /todo open 重新展开）".into();
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
        }
        CliAction::Form(form) => {
            screen.status.clear();
            screen.form = Some(FormState::new(form));
        }
        CliAction::Picker(picker) => {
            screen.picker = Some(picker);
            screen.picker_selected = 0;
            screen.delete_pending = None;
        }
        CliAction::Task(task) => {
            if screen.busy || active.is_some() {
                return Ok(false);
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
            if let Some(owner) = runner.as_ref() {
                screen.settings = Some(CliSettingsState::from_runner(owner));
            } else {
                screen.settings = Some(CliSettingsState::new(
                    &Config::default(),
                    &ProvidersConfig::default(),
                ));
            }
            screen.panel = Some(Panel::Settings);
        }
        CliAction::SettingsTab(tab) => {
            if let Some(owner) = runner.as_ref() {
                let mut settings = CliSettingsState::from_runner(owner);
                settings.tab = tab;
                screen.settings = Some(settings);
            } else {
                let mut settings =
                    CliSettingsState::new(&Config::default(), &ProvidersConfig::default());
                settings.tab = tab;
                screen.settings = Some(settings);
            }
            screen.panel = Some(Panel::Settings);
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
    async fn busy_commands_cancel_or_quit_without_owner_and_never_spawn_another_turn() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        screen.busy = true;
        let permissions = Arc::new(PermissionBroker::deny_all());
        let (events, _rx) = mpsc::unbounded_channel();
        let (tx, mut rx) = oneshot::channel();
        let mut cancel = Some(tx);
        let mut active = None;
        for text in ["/new", "/model", "/compact"] {
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
            assert_eq!(screen.input.lines().join("\n"), text);
            assert!(active.is_none());
            assert!(cancel.is_some());
        }
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
        assert!(render(&mut screen, 100, 25).contains("Press d again"));
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
        assert!(screen.todo_closed);

        let snapshot_closed = render(&mut screen, 100, 30);
        assert!(
            !snapshot_closed.contains("todo close"),
            "Closed panel should not show header hint: {snapshot_closed}"
        );
        assert!(
            snapshot_closed.contains("todo open"),
            "Footer should remind how to reopen: {snapshot_closed}"
        );

        // Reopen panel via /todo open
        screen.insert_text("/todo open");
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!screen.todo_closed);

        let snapshot_reopened = render(&mut screen, 100, 30);
        assert!(
            snapshot_reopened.contains("todo close"),
            "Reopened panel should show again: {snapshot_reopened}"
        );

        // Clear tasks via /todo clear
        screen.insert_text("/todo clear");
        input_key(&mut screen, &mut runner, KeyCode::Enter, KeyModifiers::NONE);

        let snapshot_cleared = render(&mut screen, 100, 30);
        assert!(
            !snapshot_cleared.contains("todo close"),
            "Cleared list should remove panel: {snapshot_cleared}"
        );

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
        // 欢迎页提示使用 cyber update
        assert!(
            clean_text.contains("检测到新版本V0.9.9，可运行cyberupdate进行更新"),
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
        if let Some(next_prompt) = screen.queued_prompts.pop_front() {
            spawn_turn(
                &mut screen,
                &mut runner,
                next_prompt.text,
                next_prompt.displayed,
                &permissions,
                &events,
                &mut active,
                &mut cancel,
            );
        }

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
        assert_eq!(
            screen.settings.as_ref().unwrap().tab,
            SettingsTab::StorageSystem
        );

        // Direct keys 1-7
        let direct_keys = [
            ('1', SettingsTab::AgentModel),
            ('2', SettingsTab::UiWorkflow),
            ('3', SettingsTab::Subagents),
            ('4', SettingsTab::ToolsMcp),
            ('5', SettingsTab::Providers),
            ('6', SettingsTab::EnvMemory),
            ('7', SettingsTab::StorageSystem),
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
        settings_key_with_runner(
            &mut screen,
            &mut runner_opt,
            KeyCode::Right,
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

        // Row 5: Max steps increment
        screen.settings.as_mut().unwrap().selected_row = 5;
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
            KeyCode::Right,
            KeyModifiers::NONE,
        );
        assert_ne!(
            orig_theme,
            screen.settings.as_ref().unwrap().config_draft.ui.theme
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
        // 按 3 次 Right 增加到 8 次
        for _ in 0..3 {
            settings_key_with_runner(
                &mut screen,
                &mut runner_opt,
                KeyCode::Right,
                KeyModifiers::NONE,
            );
        }
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
        // 按 2 次 Right 增加到 5 秒
        for _ in 0..2 {
            settings_key_with_runner(
                &mut screen,
                &mut runner_opt,
                KeyCode::Right,
                KeyModifiers::NONE,
            );
        }
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
        assert_eq!(screen.panel, None, "保存后面板应关闭");
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
            KeyCode::Char(' '),
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

        assert_eq!(screen.panel, None);
        assert!(screen.settings.is_none());
        assert!(screen.status.contains("已保存并立即生效"));

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
        let rendered = render(&mut screen, 80, 24).replace(' ', "");
        assert!(rendered.contains("recon_ports"), "{rendered}");
        assert!(rendered.contains("⏱运行中"), "{rendered}");
        assert!(rendered.contains("vuln_scan"), "{rendered}");
        assert!(rendered.contains("✓完成"), "{rendered}");
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
        assert!(screen.selection.is_none());

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
}
