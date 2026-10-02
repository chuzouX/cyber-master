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
        Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
    },
};
use cyber_agent::{
    estimate_messages_tokens, AgentEvent, ApprovalChoice, PermissionBroker, PermissionDecision,
    PermissionMode, PermissionRequest,
};
use cyber_core::ThinkingIntensity;
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph, Wrap},
    Frame, Terminal,
};
use tokio::sync::{mpsc, oneshot};
use tui_textarea::TextArea;

use crate::chat::{entries_to_messages, ChatEntry, KeyDisposition, PasteDetector};
use crate::cli_commands::{
    self, CliAction, CommandForm, CommandPicker, CompletionItem, PickerKind,
};
use crate::headless::{HeadlessOutcome, SessionRunner};
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

fn restore_terminal() {
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

#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Shortcuts,
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
    Generic,
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
    history_view: WrappedViewport,
    approval_scroll: usize,
    approval_max_scroll: usize,
    approval_nonce: String,
    approval_arguments: Vec<Line<'static>>,
    approval_view: WrappedViewport,
    approval_visible: bool,
    panel: Option<Panel>,
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
    last_window_title: String,
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
            has_run: false,
            last_window_title: String::new(),
        };
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
            self.messages.extend(
                clean(text)
                    .lines()
                    .map(|line| Line::styled(line.to_owned(), style)),
            );
            self.messages.push(Line::styled("", style));
            return;
        }
        self.messages.push(Line::styled(
            clean(label),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        self.messages.extend(clean(text).lines().map(|line| {
            Line::styled(
                line.to_owned(),
                Style::default().fg(if label == "You" { CLI_THEME.fg } else { color }),
            )
        }));
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
        self.tools[index].progress.push_str(&clean(chunk));
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
    }

    fn toggle_tools(&mut self) {
        self.tools_expanded = !self.tools_expanded;
        self.refresh_tools(self.tool_width());
    }

    fn replace_tool_card(&mut self, index: usize, width: u16) {
        let replacement = render_tool_card(&self.tools[index], self.tools_expanded, width);
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

    fn footer(&self) -> String {
        if !self.has_run {
            if self.approval.is_some() {
                return "  permission required · 1/2/3 select · Enter confirm · Esc deny".into();
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
        if self.approval.is_some() {
            footer.push_str(" · permission required · 1/2/3 select · Enter confirm · Esc deny");
            footer.push_str(&format!(" · {} · Ctrl+C to cancel", clean(&self.status)));
        } else if !self.status.is_empty() {
            footer.push_str(&format!(" · {}", clean(&self.status)));
        }
        footer
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

            vec![
                Line::from(vec![
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
                ]),
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
        let sections = Layout::vertical([
            Constraint::Length(header_height),
            Constraint::Min(0),
            Constraint::Length(input_height + 1),
            Constraint::Length(1),
        ])
        .split(area);
        self.draw_header(frame, sections[0]);
        self.history_view.update(&self.messages, sections[1].width);
        self.max_scroll = self
            .history_view
            .rows
            .len()
            .saturating_sub(sections[1].height as usize);
        self.scroll = self.scroll.min(self.max_scroll);
        let start = self.max_scroll - self.scroll;
        frame.render_widget(
            Paragraph::new(self.history_view.window(start, sections[1].height)),
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
        let input_area = sections[2];
        let border = if self.approval.is_some() || self.form.is_some() {
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
        draw_composer(frame, input_area, active_input, &title, border, secret);
        let mut footer = self.footer();
        if self.approval.is_some() {
            footer = "  permission required · 1/2/3 select · Enter confirm · Esc deny".into();
        }
        frame.render_widget(
            Paragraph::new(footer).style(Style::default().fg(DIM)),
            sections[3],
        );
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
            let (title, text) = match panel {
                Panel::Shortcuts => (" Shortcuts ", format!("Enter          Send / select completion\nAlt/Shift+Enter New line\nTab · Up/Down   Complete / choose command\nF2 / Ctrl+P     Cycle mode (manual/auto/unlimited)\nUp / Down       History prompts / scroll line\nMouse Wheel     Scroll chat / approval arguments\nCtrl+O          Toggle tool details\nCtrl+C          Cancel task\nCtrl+D          Exit (empty input)\nPgUp / PgDn     Scroll conversation\n?               Toggle shortcuts (empty input)\n\n{}\n\nEsc closes this panel.", cli_commands::commands().iter().map(|spec| format!("{:<30} {}", spec.usage, spec.desc)).collect::<Vec<_>>().join("\n"))),
            };
            popup(frame, sections[1], title, &text);
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
        let info = Rect::new(
            area.x + logo_width,
            area.y + u16::from(logo_width != 0),
            area.width.saturating_sub(logo_width),
            area.height.saturating_sub(u16::from(logo_width != 0)),
        );
        let mut lines = vec![
            Line::styled(
                format!("Cyber Master V{}", env!("CARGO_PKG_VERSION")),
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            ),
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
        if !self.session_title.is_empty()
            && self.session_title != "新会话"
            && self.session_title != "默认会话"
        {
            lines.push(Line::from(vec![
                Span::styled("Session · ", Style::default().fg(ACCENT)),
                Span::styled(
                    clean(&self.session_title),
                    Style::default().fg(FG).add_modifier(Modifier::BOLD),
                ),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), info);
    }
}

fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

fn single_line(text: &str) -> String {
    clean(text).split_whitespace().collect::<Vec<_>>().join(" ")
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
        ToolCardKind::Read => argument_string(arguments, &["path", "file_path"]),
        ToolCardKind::Edit | ToolCardKind::Write => {
            argument_string(arguments, &["path", "file_path"])
        }
        ToolCardKind::Download => argument_string(arguments, &["output", "path", "url"]),
        ToolCardKind::Shell => argument_string(arguments, &["command"]),
        ToolCardKind::Fetch => argument_string(arguments, &["url", "path"]),
        ToolCardKind::List => argument_string(arguments, &["path"]),
        ToolCardKind::Find => argument_string(arguments, &["name", "pattern", "content", "path"]),
        ToolCardKind::Generic => {
            argument_string(arguments, &["intent", "path", "url", "command", "query"])
        }
    };
    clean(value.unwrap_or_default())
}

fn pretty_tool_arguments(arguments: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| clean(arguments))
}

fn tool_card_body(card: &ToolCard, kind: ToolCardKind, expanded: bool) -> Vec<String> {
    let arguments = pretty_tool_arguments(&card.arguments);
    let body = if card.state == ToolCardState::Pending {
        if !card.progress.is_empty() {
            card.progress.as_str()
        } else if matches!(
            kind,
            ToolCardKind::Shell
                | ToolCardKind::Read
                | ToolCardKind::Download
                | ToolCardKind::Fetch
                | ToolCardKind::List
                | ToolCardKind::Find
        ) {
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
        && matches!(
            kind,
            ToolCardKind::Edit | ToolCardKind::Write | ToolCardKind::Generic
        )
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
    } else {
        MUTED
    };
    Style::default().fg(foreground).bg(bg)
}

fn render_tool_card(card: &ToolCard, expanded: bool, width: u16) -> Vec<Line<'static>> {
    let kind = tool_card_kind(&card.name);
    let parsed_arguments = serde_json::from_str::<serde_json::Value>(&card.arguments).ok();
    let title = tool_card_title(card, kind);
    let detail = tool_card_detail(kind, parsed_arguments.as_ref());
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
    let body = tool_card_body(card, kind, expanded);
    let collapsed_limit = match kind {
        ToolCardKind::Edit => 5,
        ToolCardKind::Download => 2,
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

fn draw_composer(
    frame: &mut Frame,
    area: Rect,
    input: &TextArea<'_>,
    title: &str,
    border: Color,
    secret: bool,
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
        frame.render_widget(
            Paragraph::new(if chrome == 6 {
                format!("╭──{label}{}──╮", "─".repeat(fill as usize))
            } else {
                format!("╭{label}{}╮", "─".repeat(fill as usize))
            })
            .style(style),
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

#[derive(Default)]
struct WrappedViewport {
    width: u16,
    padding: u16,
    source: Vec<Line<'static>>,
    ends: Vec<usize>,
    rows: Vec<Line<'static>>,
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
        self.rows.truncate(self.ends.last().copied().unwrap_or(0));
        let full_width = usize::from(width).max(1);
        let padding = usize::from(self.padding).min(full_width.saturating_sub(1) / 2);
        let width = full_width.saturating_sub(padding * 2).max(1);
        for line in &lines[unchanged..] {
            let mut row = Line::default().style(line.style);
            if padding > 0 {
                row.spans.push(Span::raw(" ".repeat(padding)));
            }
            let mut used = 0;
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
            self.source.push(line.clone());
            self.ends.push(self.rows.len());
        }
    }

    fn window(&self, start: usize, height: u16) -> Vec<Line<'static>> {
        self.rows
            .iter()
            .skip(start)
            .take(height as usize)
            .cloned()
            .collect()
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
    let permissions = Arc::new(broker);
    let (event_tx, mut agent_events) = mpsc::unbounded_channel();
    let mut active: Option<ActiveTurn> = None;
    let mut cancel: Option<oneshot::Sender<()>> = None;
    let _session = TerminalSession::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut events = EventStream::new();
    let mut paste = PasteDetector::new();
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    let result = async {
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if !TERMINAL_ACTIVE.load(Ordering::SeqCst) {
                        color_eyre::eyre::bail!("A task panicked; the terminal was restored. See the diagnostic above.");
                    }
                    if let Some(text) = paste.flush_if_stale() {
                        screen.insert_text(&text);
                        screen.completion_closed = false;
                        screen.completion_accepted = false;
                        screen.update_completions(runner.as_ref());
                    }
                    terminal.draw(|frame| screen.draw(frame))?;
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
                    let (restored, outcome) = finished?;
                    while let Ok(event) = agent_events.try_recv() { screen.event(event); }
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
                            if let Some(text) = paste.flush() { screen.insert_text(&text); }
                            screen.insert_text(&text);
                            screen.completion_closed = false;
                            screen.completion_accepted = false;
                            screen.update_completions(runner.as_ref());
                        }
                        Some(Ok(Event::Mouse(mouse))) => {
                            match mouse.kind {
                                MouseEventKind::ScrollUp => {
                                    if screen.approval.is_some() {
                                        screen.approval_scroll = screen.approval_scroll.saturating_sub(3);
                                    } else {
                                        screen.scroll = screen.scroll.saturating_add(3).min(screen.max_scroll);
                                    }
                                }
                                MouseEventKind::ScrollDown => {
                                    if screen.approval.is_some() {
                                        screen.approval_scroll = screen
                                            .approval_scroll
                                            .saturating_add(3)
                                            .min(screen.approval_max_scroll);
                                    } else {
                                        screen.scroll = screen.scroll.saturating_sub(3);
                                    }
                                }
                                _ => {}
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
    if let Some(active) = active {
        if let Ok((restored, _)) = active.await {
            runner = Some(restored);
        }
    }
    if let Some(owner) = &runner {
        if let Some(mcp) = &owner.registries.mcp {
            mcp.shutdown_all().await;
        }
    }
    result
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
        screen.panel = None;
        return Ok(false);
    }
    if (control && key.code == KeyCode::Char('c')) || key.code == KeyCode::Esc {
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
        } else if !screen.busy {
            if screen.input.is_empty() && control {
                return Ok(true);
            }
            screen.input = composer();
            screen.completions.clear();
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
            screen.scroll = screen.scroll.saturating_add(1).min(screen.max_scroll);
            return Ok(false);
        }
        if key.code == KeyCode::Down {
            screen.scroll = screen.scroll.saturating_sub(1);
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
    if key.code != KeyCode::Enter
        || key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL)
    {
        if key.code == KeyCode::Enter {
            screen.input.insert_newline();
        } else {
            screen.input.input(key);
        }
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
        match text {
            value if value.eq_ignore_ascii_case("/quit") => return Ok(true),
            value if value.eq_ignore_ascii_case("/cancel") => {
                screen.input = composer();
                screen.completions.clear();
                screen.reply(PermissionDecision::Deny);
                if let Some(tx) = cancel.take() {
                    let _ = tx.send(());
                }
                screen.status = "Cancelling".into();
            }
            _ => {}
        }
        return Ok(false);
    }
    screen.input = composer();
    screen.completions.clear();
    if text.starts_with('/') {
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
    let Some(mut owned) = runner.take() else {
        return Ok(false);
    };
    screen.message("You", text, ACCENT);
    screen.prompt_history.push(text.to_owned());
    screen.history_index = None;
    screen.saved_draft.clear();
    screen.scroll = 0;
    screen.has_run = true;
    screen.busy = true;
    screen.thinking_started = Some(std::time::Instant::now());
    screen.status = "Working".into();
    let text = text.to_owned();
    let permissions = permissions.clone();
    let events = events.clone();
    let (tx, rx) = oneshot::channel();
    *cancel = Some(tx);
    let intensity = screen.effort;
    *active = Some(tokio::spawn(async move {
        let outcome = owned
            .run_turn_ui(text, intensity, permissions, events, rx)
            .await;
        (owned, outcome)
    }));
    Ok(false)
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
        CliAction::Output { title, text } => screen.message(&title, &text, MUTED),
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
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "╭");
        assert_eq!(buffer[(39, 0)].symbol(), "╮");
        assert_eq!(buffer[(0, 1)].symbol(), "│");
        assert_eq!(buffer[(0, 2)].symbol(), "╰");
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
    async fn approval_number_selects_scope_and_enter_confirms() {
        let owner = crate::headless::tests::test_runner().await;
        let mut screen = CliScreen::new(&owner);
        let (reply, mut approval_rx) = oneshot::channel();
        screen.approval = Some(PermissionRequest {
            tool: "write_file".into(),
            arguments: serde_json::json!({"path": "report.md"}),
            nonce: "request-code".into(),
            reply,
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
        for text in ["ordinary text", "/new", "/model", "/compact"] {
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

        // 1. General task summary
        screen.turn_summary(
            5000,
            1_700_000_000,
            "done",
            Some("任务完成：调用 [list_dir]，已列出当前工作目录条目"),
        );
        render(&mut screen, 100, 20);
        assert!(history(&screen).contains("※summary：任务完成：调用 [list_dir]"));
        // 2. CTF Solved summary
        let mut ch = cyber_core::CtfChallenge::new("web-sqli".into(), cyber_core::CtfCategory::Web);
        ch.status = cyber_core::CtfStatus::Solved;
        ch.flag = Some("NSSCTF{flag_sqli_success}".into());
        runner.replace_challenges(vec![ch]).unwrap();

        let summary =
            runner.generate_turn_summary("done", "利用 SQL 盲注成功提取 Flag 并提交", &[], None);
        assert!(summary.contains("题目【web-sqli】已解出 (Flag: NSSCTF{flag_sqli_success})"));

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
        let text = render(&mut screen, 120, 24);
        assert!(text.contains("deepseek-v4-1-flash-260910 · low"));
        assert!(!text.contains("Cy >"));
        assert!(!text.contains("ctx"));
        assert!(!text.contains("cache"));

        // 2. Active run (thinking / processing): title shows Cy > 0s > model · effort
        screen.busy = true;
        screen.thinking_started = Some(std::time::Instant::now());
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
}
