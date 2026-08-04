//! 设置页：查看/编辑配置 + 持久化入口。
//!
//! 编辑模型：字段表 `SECTIONS` 用 fn 指针访问 `Config` 字段；bool 切换 / enum 循环 /
//! number ±step。`theme` 与 `mouse` 改动由 App live-apply（见 `app.rs`）。
//! 保存由右侧面板底部「保存设置」行 + `Enter` 触发（不用 `Ctrl+S`，避开 §9.2 会话保存冲突）。
//! Esc 双击回退到进入时的快照（见 `app.rs` 的 `config_at_entry`）。

use cyber_core::{Config, ProvidersConfig};
use cyber_mcp::{McpRegistry, McpServersConfig, McpTransport};
use cyber_skills::{Skill, SkillRegistry, SkillSource};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};

use crate::theme::Theme;

const THEMES: &[&str] = &[
    "catppuccin",
    "cyberpunk",
    "dracula",
    "gruvbox",
    "nord",
    "tokyo-night",
];
const MODES: &[&str] = &["chat", "workflow", "dashboard"];
const LOG_LEVELS: &[&str] = &["error", "warn", "info", "debug", "trace"];

/// 字段编辑类型。
enum FieldKind {
    Bool,
    /// 固定选项枚举。
    Enum(&'static [&'static str]),
    /// 选项来自 `providers.keys()`（运行时动态，排序后循环）。
    ProviderEnum,
    Number { min: u64, max: u64, step: u64 },
    ReadOnly,
}

/// 编辑后是否需要 App 立即应用副作用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveApply {
    None,
    Theme,
    Mouse,
}

struct FieldDef {
    label: &'static str,
    kind: FieldKind,
    /// 生效时机标注（即时/重启/P2/…/—），仅用于展示。
    effect: &'static str,
    live: LiveApply,
    get: fn(&Config) -> String,
    set: fn(&mut Config, String),
}

struct SectionDef {
    name: &'static str,
    editable: bool,
    fields: &'static [FieldDef],
}

// ---- get/set 自由函数（fn 指针可入 static）----

fn get_theme(c: &Config) -> String {
    c.ui.theme.clone()
}
fn set_theme(c: &mut Config, v: String) {
    c.ui.theme = v;
}
fn get_default_mode(c: &Config) -> String {
    c.ui.default_mode.clone()
}
fn set_default_mode(c: &mut Config, v: String) {
    c.ui.default_mode = v;
}
fn get_mouse(c: &Config) -> String {
    c.ui.mouse.to_string()
}
fn set_mouse(c: &mut Config, v: String) {
    c.ui.mouse = v == "true";
}
fn get_animations(c: &Config) -> String {
    c.ui.animations.to_string()
}
fn get_frame_rate(c: &Config) -> String {
    c.ui.frame_rate.to_string()
}

fn get_default_provider(c: &Config) -> String {
    c.agent.default_provider.clone()
}
fn set_default_provider(c: &mut Config, v: String) {
    c.agent.default_provider = v;
}
fn get_auto_tool_call(c: &Config) -> String {
    c.agent.auto_tool_call.to_string()
}
fn set_auto_tool_call(c: &mut Config, v: String) {
    c.agent.auto_tool_call = v == "true";
}
fn get_max_steps(c: &Config) -> String {
    c.agent.max_steps.to_string()
}
fn set_max_steps(c: &mut Config, v: String) {
    c.agent.max_steps = v.parse().unwrap_or(50);
}

fn get_max_parallel_nodes(c: &Config) -> String {
    c.workflow.max_parallel_nodes.to_string()
}
fn set_max_parallel_nodes(c: &mut Config, v: String) {
    c.workflow.max_parallel_nodes = v.parse().unwrap_or(8);
}
fn get_default_timeout_secs(c: &Config) -> String {
    c.workflow.default_timeout_secs.to_string()
}
fn set_default_timeout_secs(c: &mut Config, v: String) {
    c.workflow.default_timeout_secs = v.parse().unwrap_or(1800);
}
fn get_checkpoint(c: &Config) -> String {
    c.workflow.checkpoint.to_string()
}
fn set_checkpoint(c: &mut Config, v: String) {
    c.workflow.checkpoint = v == "true";
}

fn get_prefer_docker(c: &Config) -> String {
    c.tools.prefer_docker.to_string()
}
fn set_prefer_docker(c: &mut Config, v: String) {
    c.tools.prefer_docker = v == "true";
}
fn get_extra_path(c: &Config) -> String {
    if c.tools.extra_path.is_empty() {
        "(空)".into()
    } else {
        c.tools.extra_path.join(", ")
    }
}

fn get_history_retention_days(c: &Config) -> String {
    c.storage.history_retention_days.to_string()
}
fn set_history_retention_days(c: &mut Config, v: String) {
    c.storage.history_retention_days = v.parse().unwrap_or(90);
}
fn get_log_level(c: &Config) -> String {
    c.storage.log_level.clone()
}

fn noop_set(_: &mut Config, _: String) {}

const UI_FIELDS: &[FieldDef] = &[
    FieldDef {
        label: "主题 theme",
        kind: FieldKind::Enum(THEMES),
        effect: "即时",
        live: LiveApply::Theme,
        get: get_theme,
        set: set_theme,
    },
    FieldDef {
        label: "默认模式 default_mode",
        kind: FieldKind::Enum(MODES),
        effect: "重启",
        live: LiveApply::None,
        get: get_default_mode,
        set: set_default_mode,
    },
    FieldDef {
        label: "鼠标 mouse",
        kind: FieldKind::Bool,
        effect: "即时",
        live: LiveApply::Mouse,
        get: get_mouse,
        set: set_mouse,
    },
    FieldDef {
        label: "动画 animations",
        kind: FieldKind::ReadOnly,
        effect: "P6",
        live: LiveApply::None,
        get: get_animations,
        set: noop_set,
    },
    FieldDef {
        label: "帧率 frame_rate",
        kind: FieldKind::ReadOnly,
        effect: "—",
        live: LiveApply::None,
        get: get_frame_rate,
        set: noop_set,
    },
];

const AGENT_FIELDS: &[FieldDef] = &[
    FieldDef {
        label: "默认 provider",
        kind: FieldKind::ProviderEnum,
        effect: "即时",
        live: LiveApply::None,
        get: get_default_provider,
        set: set_default_provider,
    },
    FieldDef {
        label: "自动工具调用 auto_tool_call",
        kind: FieldKind::Bool,
        effect: "P2",
        live: LiveApply::None,
        get: get_auto_tool_call,
        set: set_auto_tool_call,
    },
    FieldDef {
        label: "最大步数 max_steps",
        kind: FieldKind::Number {
            min: 1,
            max: 200,
            step: 1,
        },
        effect: "P2",
        live: LiveApply::None,
        get: get_max_steps,
        set: set_max_steps,
    },
];

const WORKFLOW_FIELDS: &[FieldDef] = &[
    FieldDef {
        label: "最大并行节点",
        kind: FieldKind::Number {
            min: 1,
            max: 64,
            step: 1,
        },
        effect: "P4",
        live: LiveApply::None,
        get: get_max_parallel_nodes,
        set: set_max_parallel_nodes,
    },
    FieldDef {
        label: "默认超时(秒)",
        kind: FieldKind::Number {
            min: 1,
            max: 86400,
            step: 60,
        },
        effect: "P4",
        live: LiveApply::None,
        get: get_default_timeout_secs,
        set: set_default_timeout_secs,
    },
    FieldDef {
        label: "断点续跑 checkpoint",
        kind: FieldKind::Bool,
        effect: "P4",
        live: LiveApply::None,
        get: get_checkpoint,
        set: set_checkpoint,
    },
];

const TOOLS_FIELDS: &[FieldDef] = &[
    FieldDef {
        label: "优先 docker",
        kind: FieldKind::Bool,
        effect: "P3",
        live: LiveApply::None,
        get: get_prefer_docker,
        set: set_prefer_docker,
    },
    FieldDef {
        label: "额外 PATH",
        kind: FieldKind::ReadOnly,
        effect: "—",
        live: LiveApply::None,
        get: get_extra_path,
        set: noop_set,
    },
];

const STORAGE_FIELDS: &[FieldDef] = &[
    FieldDef {
        label: "历史保留天数",
        kind: FieldKind::Number {
            min: 1,
            max: 3650,
            step: 1,
        },
        effect: "P5",
        live: LiveApply::None,
        get: get_history_retention_days,
        set: set_history_retention_days,
    },
    FieldDef {
        label: "日志级别 log_level",
        kind: FieldKind::ReadOnly,
        effect: "—",
        live: LiveApply::None,
        get: get_log_level,
        set: noop_set,
    },
];

static SECTIONS: &[SectionDef] = &[
    SectionDef {
        name: "UI",
        editable: true,
        fields: UI_FIELDS,
    },
    SectionDef {
        name: "Agent",
        editable: true,
        fields: AGENT_FIELDS,
    },
    SectionDef {
        name: "Workflow",
        editable: true,
        fields: WORKFLOW_FIELDS,
    },
    SectionDef {
        name: "Tools",
        editable: true,
        fields: TOOLS_FIELDS,
    },
    SectionDef {
        name: "Storage",
        editable: true,
        fields: STORAGE_FIELDS,
    },
    SectionDef {
        name: "Providers",
        editable: false,
        fields: &[],
    },
    SectionDef {
        name: "MCP",
        editable: false,
        fields: &[],
    },
    SectionDef {
        name: "Skills",
        editable: false,
        fields: &[],
    },
];

/// 设置页视图状态。App 持有，跨次进入保留位置。
#[derive(Debug, Clone, Default)]
pub struct SettingsState {
    pub section: usize,
    pub selected: usize,
    pub dirty: bool,
    /// 首次 Esc（dirty 时）置 true，二次 Esc 触发回退。
    pub pending_discard: bool,
    /// Providers 段有未保存改动（与 `dirty` 分离：dirty 跟踪 config，dirty_providers 跟踪 providers）。
    pub dirty_providers: bool,
    /// Providers 段 cursor（在排序后的 provider 名列表中）。
    pub provider_selected: usize,
    /// 双击 `d` 删除确认：首次 `d` 置 `Some(idx)`，二次 `d` 执行删除，任一其他键清除。
    pub pending_delete_idx: Option<usize>,
    /// MCP 段有未保存改动（与 dirty/dirty_providers 分离）。
    pub dirty_mcp: bool,
    /// MCP 段 cursor（在 servers 列表中）。
    pub mcp_selected: usize,
    /// MCP 段双击 `d` 删除确认。
    pub mcp_pending_delete_idx: Option<usize>,
    /// Skills 段 cursor（在 skills 列表中）。
    pub skills_selected: usize,
    /// Providers 段 cursor 是否停在保存按钮行。
    pub provider_on_save: bool,
    /// MCP 段 cursor 是否停在保存按钮行。
    pub mcp_on_save: bool,
}

/// Providers 段在 SECTIONS 中的索引。
pub const PROVIDERS_SECTION_IDX: usize = 5;
/// MCP 段在 SECTIONS 中的索引。
pub const MCP_SECTION_IDX: usize = 6;
/// Skills 段在 SECTIONS 中的索引。
pub const SKILLS_SECTION_IDX: usize = 7;

impl SettingsState {
    pub fn new() -> Self {
        Self::default()
    }

    fn cur_section(&self) -> &'static SectionDef {
        &SECTIONS[self.section]
    }

    fn cur_field(&self) -> Option<&'static FieldDef> {
        self.cur_section().fields.get(self.selected)
    }

    /// 当前是否停在「保存设置」行（仅可编辑段有意义）。
    pub fn on_save_row(&self) -> bool {
        let s = self.cur_section();
        s.editable && self.selected >= s.fields.len()
    }

    /// 段是否可编辑（Providers 段只读，由 App 侧特殊分派 a/e/d）。
    pub fn is_editable(&self) -> bool {
        self.cur_section().editable
    }

    /// 当前是否在 Providers 段（特殊交互段）。
    pub fn on_providers_section(&self) -> bool {
        self.section == PROVIDERS_SECTION_IDX
    }

    /// 当前是否在 MCP 段。
    pub fn on_mcp_section(&self) -> bool {
        self.section == MCP_SECTION_IDX
    }

    /// 当前是否在 Skills 段。
    pub fn on_skills_section(&self) -> bool {
        self.section == SKILLS_SECTION_IDX
    }

    /// Providers 段是否停在保存按钮行。
    pub fn on_provider_save_row(&self) -> bool {
        self.on_providers_section() && self.provider_on_save
    }

    /// MCP 段是否停在保存按钮行。
    pub fn on_mcp_save_row(&self) -> bool {
        self.on_mcp_section() && self.mcp_on_save
    }

    /// Providers 段 cursor 下移（clamp，空列表 no-op）。
    /// 下移超过最后一项时移到保存按钮行。
    pub fn next_provider(&mut self, len: usize) {
        if self.provider_on_save {
            return; // 已在保存行，不再下移
        }
        if len == 0 {
            self.provider_on_save = true;
            self.provider_selected = 0;
            return;
        }
        if self.provider_selected + 1 >= len {
            // 超过最后一项 → 移到保存行
            self.provider_on_save = true;
        } else {
            self.provider_selected += 1;
        }
    }

    /// Providers 段 cursor 上移（clamp，空列表 no-op）。
    /// 在保存行时上移回到最后一项。
    pub fn prev_provider(&mut self, len: usize) {
        if self.provider_on_save {
            self.provider_on_save = false;
            if len > 0 {
                self.provider_selected = len - 1;
            } else {
                self.provider_selected = 0;
            }
            return;
        }
        if len == 0 {
            self.provider_selected = 0;
            return;
        }
        self.provider_selected = self.provider_selected.saturating_sub(1);
    }

    /// MCP 段 cursor 下移。
    pub fn next_mcp(&mut self, len: usize) {
        if self.mcp_on_save {
            return;
        }
        if len == 0 {
            self.mcp_on_save = true;
            self.mcp_selected = 0;
            return;
        }
        if self.mcp_selected + 1 >= len {
            self.mcp_on_save = true;
        } else {
            self.mcp_selected += 1;
        }
    }

    /// MCP 段 cursor 上移。
    pub fn prev_mcp(&mut self, len: usize) {
        if self.mcp_on_save {
            self.mcp_on_save = false;
            if len > 0 {
                self.mcp_selected = len - 1;
            } else {
                self.mcp_selected = 0;
            }
            return;
        }
        if len == 0 {
            self.mcp_selected = 0;
            return;
        }
        self.mcp_selected = self.mcp_selected.saturating_sub(1);
    }

    /// Skills 段 cursor 下移。
    pub fn next_skill(&mut self, len: usize) {
        if len == 0 {
            self.skills_selected = 0;
            return;
        }
        self.skills_selected = (self.skills_selected + 1).min(len - 1);
    }

    /// Skills 段 cursor 上移。
    pub fn prev_skill(&mut self, len: usize) {
        if len == 0 {
            self.skills_selected = 0;
            return;
        }
        self.skills_selected = self.skills_selected.saturating_sub(1);
    }

    pub fn next_field(&mut self) {
        let s = self.cur_section();
        if !s.editable {
            return;
        }
        let rows = s.fields.len() + 1; // +保存行
        self.selected = (self.selected + 1) % rows;
    }

    pub fn prev_field(&mut self) {
        let s = self.cur_section();
        if !s.editable {
            return;
        }
        let rows = s.fields.len() + 1;
        self.selected = (self.selected + rows - 1) % rows;
    }

    pub fn next_section(&mut self) {
        self.section = (self.section + 1) % SECTIONS.len();
        self.selected = 0;
        // 切段时重置保存按钮焦点
        self.provider_on_save = false;
        self.mcp_on_save = false;
    }

    /// 应用编辑（`forward`=正向下/右，否则反向上/左）。返回需 live-apply 的类型。
    /// 在保存行或只读段调用为空操作。
    pub fn apply_edit(
        &mut self,
        config: &mut Config,
        providers: &ProvidersConfig,
        forward: bool,
    ) -> LiveApply {
        let Some(field) = self.cur_field() else {
            return LiveApply::None;
        };
        let cur = (field.get)(config);
        let new: String = match &field.kind {
            FieldKind::Bool => toggle_bool(&cur),
            FieldKind::Enum(opts) => cycle_enum(&cur, opts, forward),
            FieldKind::ProviderEnum => {
                let mut opts: Vec<&str> = providers.providers.keys().map(|s| s.as_str()).collect();
                opts.sort_unstable();
                cycle_enum(&cur, &opts, forward)
            }
            FieldKind::Number { min, max, step } => adjust_number(&cur, *step, forward, *min, *max),
            FieldKind::ReadOnly => cur.clone(),
        };
        if new != cur {
            (field.set)(config, new);
            self.dirty = true;
        }
        field.live
    }
}

// ---- 纯编辑函数（可单测）----

fn toggle_bool(cur: &str) -> String {
    // "true" → "false"，其余 → "true"
    (cur != "true").to_string()
}

fn cycle_enum(current: &str, options: &[&str], forward: bool) -> String {
    if options.is_empty() {
        return current.to_string();
    }
    let Some(idx) = options.iter().position(|o| *o == current) else {
        return options[0].to_string();
    };
    let n = options.len();
    let next = if forward {
        (idx + 1) % n
    } else {
        (idx + n - 1) % n
    };
    options[next].to_string()
}

fn adjust_number(cur: &str, step: u64, forward: bool, min: u64, max: u64) -> String {
    let mut val: u64 = cur.parse().unwrap_or(min);
    val = if forward {
        val.saturating_add(step)
    } else {
        val.saturating_sub(step)
    };
    val.clamp(min, max).to_string()
}

/// 脱敏 api_key：空 → (未设置)；`${ENV}` 引用原样；明文 → (已设置)。
fn mask_key(k: &str) -> String {
    if k.is_empty() {
        "(未设置)".into()
    } else if k.starts_with("${") {
        k.into()
    } else {
        "(已设置)".into()
    }
}

// ---- 渲染 ----

/// 渲染设置页。
#[allow(clippy::too_many_arguments)]
pub fn render(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    config: &Config,
    providers: &ProvidersConfig,
    mcp_config: &McpServersConfig,
    mcp_registry: Option<&McpRegistry>,
    skills: &SkillRegistry,
    state: &SettingsState,
    has_project_config: bool,
    toast: Option<&str>,
) {
    let dirty_marker =
        if state.dirty || state.dirty_providers || state.dirty_mcp { " *" } else { "" };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(
            Line::from(format!(" Settings{dirty_marker} "))
                .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 顶部横幅（项目级覆盖提示）+ 底部 toast：两行固定高度
    let content_area = if has_project_config || toast.is_some() {
        let top_h: u16 = if has_project_config { 1 } else { 0 };
        let bottom_h: u16 = if toast.is_some() { 1 } else { 0 };
        let chunks = Layout::vertical([
            Constraint::Length(top_h),
            Constraint::Min(0),
            Constraint::Length(bottom_h),
        ])
        .split(inner);
        if has_project_config {
            frame.render_widget(
                Paragraph::new(Line::from(
                    " ⚠ 检测到项目级 .cyber/config.toml：保存仅写全局，被覆盖字段重启后回弹"
                        .to_string(),
                ))
                .style(Style::default().fg(theme.accent).bg(theme.bg)),
                chunks[0],
            );
        }
        if let Some(t) = toast {
            frame.render_widget(
                Paragraph::new(Line::from(format!(" {t}")).style(Style::default().fg(theme.accent))),
                chunks[2],
            );
        }
        chunks[1]
    } else {
        inner
    };

    let cols = Layout::horizontal([Constraint::Length(24), Constraint::Min(40)]).split(content_area);
    render_sidebar(frame, cols[0], theme, state);
    render_fields(frame, cols[1], theme, config, providers, mcp_config, mcp_registry, skills, state);
}

fn render_sidebar(frame: &mut Frame, area: Rect, theme: &Theme, state: &SettingsState) {
    let mut lines: Vec<Line> = Vec::new();
    for (i, s) in SECTIONS.iter().enumerate() {
        let marker = if i == state.section { "▸ " } else { "  " };
        let mut line = Line::from(format!("{marker}{}", s.name));
        line = if i == state.section {
            line.style(Style::default().bg(theme.sel_bg).fg(theme.sel_fg).add_modifier(Modifier::BOLD))
        } else {
            line.style(Style::default().fg(theme.fg))
        };
        lines.push(line);
    }
    lines.push(Line::from(""));
    lines.push(
        Line::from("Tab 切段  ↑↓ 行").style(Style::default().fg(theme.muted)),
    );
    frame.render_widget(Paragraph::new(lines).style(Style::default().bg(theme.bg)), area);
}

#[allow(clippy::too_many_arguments)]
fn render_fields(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    config: &Config,
    providers: &ProvidersConfig,
    mcp_config: &McpServersConfig,
    mcp_registry: Option<&McpRegistry>,
    skills: &SkillRegistry,
    state: &SettingsState,
) {
    let section = &SECTIONS[state.section];
    let mut lines: Vec<Line> = Vec::new();
    lines.push(
        Line::from(format!(" {} ", section.name))
            .style(Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
    );
    lines.push(Line::from(""));

    if section.editable {
        for (i, field) in section.fields.iter().enumerate() {
            let selected = i == state.selected && !state.on_save_row();
            let value = display_value(field, config);
            let marker = if selected { "▸ " } else { "  " };
            let row_style = if selected {
                Style::default().bg(theme.sel_bg)
            } else {
                Style::default().bg(theme.bg)
            };
            lines.push(
                Line::from(vec![
                    Span::raw(marker.to_string()),
                    Span::styled(field.label.to_string(), Style::default().fg(theme.fg)),
                    Span::raw(" : "),
                    Span::styled(value, Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
                    Span::raw("   ["),
                    Span::styled(field.effect.to_string(), Style::default().fg(theme.muted)),
                    Span::raw("]"),
                ])
                .style(row_style),
            );
        }
        // 保存按钮行
        let on_save = state.on_save_row();
        let save_marker = if on_save { "▸ " } else { "  " };
        let any_dirty = state.dirty || state.dirty_providers || state.dirty_mcp;
        let save_label = if any_dirty { "保存设置 *" } else { "保存设置" };
        let save_style = if on_save {
            Style::default().bg(theme.sel_bg)
        } else {
            Style::default().bg(theme.bg)
        };
        lines.push(Line::from(""));
        lines.push(
            Line::from(vec![
                Span::raw(save_marker.to_string()),
                Span::styled(save_label.to_string(), Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
                Span::raw("   (Enter 保存)"),
            ])
            .style(save_style),
        );
        lines.push(Line::from(""));
        if any_dirty {
            lines.push(
                Line::from("有未保存改动：Enter 保存 / 再按 Esc 丢弃")
                    .style(Style::default().fg(theme.muted)),
            );
        } else {
            lines.push(Line::from("Enter 编辑字段  ←→ 调整  Esc 返回").style(Style::default().fg(theme.muted)));
        }
    } else if state.on_providers_section() {
        // Providers 段：交互式（a 新增 / e 编辑 / d 删除 / Enter 设默认）
        render_providers_lines(&mut lines, theme, providers, config, state);
        // 保存按钮行
        lines.push(Line::from(""));
        let on_save = state.provider_on_save;
        let save_marker = if on_save { "▸ " } else { "  " };
        let any_dirty = state.dirty || state.dirty_providers || state.dirty_mcp;
        let save_label = if any_dirty { "保存设置 *" } else { "保存设置" };
        let save_style = if on_save {
            Style::default().bg(theme.sel_bg)
        } else {
            Style::default().bg(theme.bg)
        };
        lines.push(
            Line::from(vec![
                Span::raw(save_marker.to_string()),
                Span::styled(save_label.to_string(), Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
                Span::raw("   (Enter 保存)"),
            ])
            .style(save_style),
        );
        lines.push(Line::from(""));
        let hint = if let Some(idx) = state.pending_delete_idx {
            let names = providers.sorted_names();
            let name = names.get(idx).map(|s| s.as_str()).unwrap_or("?");
            format!(" 再按 d 确认删除 '{name}' · 其他键取消")
        } else {
            " a 新增  e 编辑  d 删除  Enter 设默认/保存  ↑↓ 选择".to_string()
        };
        let hint_style = if state.pending_delete_idx.is_some() {
            Style::default().fg(theme.accent)
        } else {
            Style::default().fg(theme.muted)
        };
        lines.push(Line::from(hint).style(hint_style));
    } else if state.on_mcp_section() {
        // MCP 段：交互式（a 新增 / e 编辑 / d 删除 / ↑↓ 选择）
        render_mcp_lines(&mut lines, theme, mcp_config, mcp_registry, state);
        // 保存按钮行
        lines.push(Line::from(""));
        let on_save = state.mcp_on_save;
        let save_marker = if on_save { "▸ " } else { "  " };
        let any_dirty = state.dirty || state.dirty_providers || state.dirty_mcp;
        let save_label = if any_dirty { "保存设置 *" } else { "保存设置" };
        let save_style = if on_save {
            Style::default().bg(theme.sel_bg)
        } else {
            Style::default().bg(theme.bg)
        };
        lines.push(
            Line::from(vec![
                Span::raw(save_marker.to_string()),
                Span::styled(save_label.to_string(), Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
                Span::raw("   (Enter 保存)"),
            ])
            .style(save_style),
        );
        lines.push(Line::from(""));
        let hint = if let Some(idx) = state.mcp_pending_delete_idx {
            let name = mcp_config
                .servers
                .get(idx)
                .map(|s| s.name.as_str())
                .unwrap_or("?");
            format!(" 再按 d 确认删除 '{name}' · 其他键取消")
        } else {
            " a 新增  e 编辑  d 删除  Enter 保存  ↑↓ 选择  (保存后重启生效)".to_string()
        };
        let hint_style = if state.mcp_pending_delete_idx.is_some() {
            Style::default().fg(theme.accent)
        } else {
            Style::default().fg(theme.muted)
        };
        lines.push(Line::from(hint).style(hint_style));
    } else if state.on_skills_section() {
        // Skills 段：只读展示（文件型，TUI 内不可编辑）
        render_skills_lines(&mut lines, theme, skills, state);
        lines.push(Line::from(""));
        lines.push(
            Line::from(" ↑↓ 选择  Skills 为文件型配置（编辑 SKILL.md 后重启生效）")
                .style(Style::default().fg(theme.muted)),
        );
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg).fg(theme.fg)),
        area,
    );
}

fn display_value(field: &FieldDef, config: &Config) -> String {
    let raw = (field.get)(config);
    match field.kind {
        FieldKind::Bool => {
            if raw == "true" {
                "on".into()
            } else {
                "off".into()
            }
        }
        _ => raw,
    }
}

fn render_providers_lines(
    lines: &mut Vec<Line>,
    theme: &Theme,
    providers: &ProvidersConfig,
    config: &Config,
    state: &SettingsState,
) {
    let names = providers.sorted_names();
    if names.is_empty() {
        lines.push(
            Line::from("（无 provider，按 a 新增）").style(Style::default().fg(theme.muted)),
        );
        return;
    }
    for (i, name) in names.iter().enumerate() {
        let p = &providers.providers[name];
        let is_default = name.as_str() == config.agent.default_provider;
        let star = if is_default { " ★默认" } else { "" };
        let selected = i == state.provider_selected && !state.provider_on_save;
        let pending_delete = state.pending_delete_idx == Some(i);
        let marker = if selected { "▸ " } else { "  " };
        let delete_tag = if pending_delete { "  [待删除!]" } else { "" };
        let row_style = if selected {
            Style::default().bg(theme.sel_bg)
        } else {
            Style::default().bg(theme.bg)
        };
        let name_color = if pending_delete { theme.accent } else { theme.title };
        lines.push(
            Line::from(vec![
                Span::raw(marker.to_string()),
                Span::styled(
                    format!("{name}{star}"),
                    Style::default().fg(name_color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  [{}] {} · {}{}", p.kind, p.base_url, p.model_display_name(), delete_tag)),
            ])
            .style(row_style),
        );
        let key_line = format!("    api_key: {}", mask_key(&p.api_key));
        lines.push(
            Line::from(key_line).style(Style::default().fg(theme.muted)),
        );
        // 价格配置行：显示单价（$/M），未配置则标「未设置」。使用 effective_price（per-model 优先）
        let price_line = match p.effective_price() {
            None => "    price: 未设置".to_string(),
            Some(pr) => {
                let fmt = |v: Option<f64>| -> String {
                    v.map(|x| format!("{x}")).unwrap_or_else(|| "-".into())
                };
                format!(
                    "    price $/M: in={} out={} cache={}",
                    fmt(pr.input_per_m),
                    fmt(pr.output_per_m),
                    fmt(pr.cache_hit_per_m),
                )
            }
        };
        lines.push(
            Line::from(price_line).style(Style::default().fg(theme.muted)),
        );
    }
}

/// 渲染 MCP servers 列表（名称 / 传输 / 命令或 URL / 超时 / 连接状态）。
fn render_mcp_lines(
    lines: &mut Vec<Line>,
    theme: &Theme,
    mcp_config: &McpServersConfig,
    mcp_registry: Option<&McpRegistry>,
    state: &SettingsState,
) {
    if mcp_config.servers.is_empty() {
        lines.push(
            Line::from("（无 MCP server，按 a 新增）").style(Style::default().fg(theme.muted)),
        );
        return;
    }
    let connected_names: Vec<&str> = mcp_registry
        .map(|r| r.server_names())
        .unwrap_or_default();
    for (i, spec) in mcp_config.servers.iter().enumerate() {
        let selected = i == state.mcp_selected && !state.mcp_on_save;
        let pending_delete = state.mcp_pending_delete_idx == Some(i);
        let marker = if selected { "▸ " } else { "  " };
        let delete_tag = if pending_delete { "  [待删除!]" } else { "" };
        let row_style = if selected {
            Style::default().bg(theme.sel_bg)
        } else {
            Style::default().bg(theme.bg)
        };
        let name_color = if pending_delete { theme.accent } else { theme.title };
        // 传输摘要：stdio → command + args；http/sse → url
        let transport_summary = match spec.transport {
            McpTransport::Stdio => {
                let cmd = spec.command.as_deref().unwrap_or("(未设置)");
                let args = if spec.args.is_empty() {
                    String::new()
                } else {
                    format!(" {}", spec.args.join(" "))
                };
                format!("{cmd}{args}")
            }
            McpTransport::Http | McpTransport::Sse => {
                spec.url.as_deref().unwrap_or("(未设置 url)").to_string()
            }
        };
        // 连接状态：已连接 / 连接失败 / 未启用（mock 模式）
        let (status_text, status_color) = if mcp_registry.is_none() {
            ("未启用", theme.muted)
        } else if connected_names.contains(&spec.name.as_str()) {
            ("已连接", theme.title)
        } else {
            ("连接失败", theme.accent)
        };
        lines.push(
            Line::from(vec![
                Span::raw(marker.to_string()),
                Span::styled(
                    spec.name.to_string(),
                    Style::default().fg(name_color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    "  [{}] {} · timeout={}s",
                    spec.transport, transport_summary, spec.timeout_secs,
                )),
                Span::raw(format!("  · ")),
                Span::styled(
                    format!("[{}]", status_text),
                    Style::default().fg(status_color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(delete_tag.to_string()),
            ])
            .style(row_style),
        );
        // env / headers 摘要
        if !spec.env.is_empty() {
            let env_keys: Vec<&str> = spec.env.keys().map(|s| s.as_str()).collect();
            lines.push(
                Line::from(format!("    env: {}", env_keys.join(", ")))
                    .style(Style::default().fg(theme.muted)),
            );
        }
        if !spec.headers.is_empty() {
            let header_keys: Vec<&str> = spec.headers.keys().map(|s| s.as_str()).collect();
            lines.push(
                Line::from(format!("    headers: {}", header_keys.join(", ")))
                    .style(Style::default().fg(theme.muted)),
            );
        }
    }
}

/// 渲染 Skills 列表（名称 / 来源 / 描述 / 触发词 / 预批准工具）。
fn render_skills_lines(
    lines: &mut Vec<Line>,
    theme: &Theme,
    skills: &SkillRegistry,
    state: &SettingsState,
) {
    let all: Vec<&Skill> = skills.iter().map(|s| s.as_ref()).collect();
    if all.is_empty() {
        lines.push(
            Line::from("（无 Skill，在 ~/.cyber/skills/<name>/SKILL.md 创建）")
                .style(Style::default().fg(theme.muted)),
        );
        return;
    }
    for (i, skill) in all.iter().enumerate() {
        let selected = i == state.skills_selected;
        let marker = if selected { "▸ " } else { "  " };
        let row_style = if selected {
            Style::default().bg(theme.sel_bg)
        } else {
            Style::default().bg(theme.bg)
        };
        let source_tag = match skill.source {
            SkillSource::Global => "全局",
            SkillSource::Project => "项目",
        };
        let manual_tag = if skill.frontmatter.disable_model_invocation {
            "  [仅显式]"
        } else {
            ""
        };
        lines.push(
            Line::from(vec![
                Span::raw(marker.to_string()),
                Span::styled(
                    skill.name().to_string(),
                    Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  [{source_tag}]{manual_tag}")),
            ])
            .style(row_style),
        );
        // 描述行
        lines.push(
            Line::from(format!("    {}", skill.frontmatter.description))
                .style(Style::default().fg(theme.muted)),
        );
        // 触发词
        if !skill.frontmatter.triggers.is_empty() {
            lines.push(
                Line::from(format!("    触发词: {}", skill.frontmatter.triggers.join(", ")))
                    .style(Style::default().fg(theme.muted)),
            );
        }
        // 预批准工具（Claude Code 风格 allowed-tools）
        if !skill.frontmatter.allowed_tools.is_empty() {
            lines.push(
                Line::from(format!(
                    "    预批准工具: {}",
                    skill.frontmatter.allowed_tools.join(", ")
                ))
                .style(Style::default().fg(theme.muted)),
            );
        }
    }
}

// 让 LOG_LEVELS 不产生未使用警告（保留供未来 log_level 改为可编辑时使用）。
#[allow(dead_code)]
const _LOG_LEVELS_USED: &[&str] = LOG_LEVELS;

#[cfg(test)]
mod tests {
    use super::*;
    use cyber_core::ProvidersConfig;

    #[test]
    fn toggle_bool_swaps() {
        assert_eq!(toggle_bool("true"), "false");
        assert_eq!(toggle_bool("false"), "true");
        assert_eq!(toggle_bool("xyz"), "true");
    }

    #[test]
    fn cycle_enum_wraps_both_directions() {
        let opts = ["a", "b", "c"];
        assert_eq!(cycle_enum("a", &opts, true), "b");
        assert_eq!(cycle_enum("c", &opts, true), "a");
        assert_eq!(cycle_enum("a", &opts, false), "c");
        assert_eq!(cycle_enum("b", &opts, false), "a");
    }

    #[test]
    fn cycle_enum_not_found_and_empty() {
        let opts = ["a", "b"];
        assert_eq!(cycle_enum("zzz", &opts, true), "a");
        let empty: [&str; 0] = [];
        assert_eq!(cycle_enum("x", &empty, true), "x");
    }

    #[test]
    fn adjust_number_clamps_and_saturates() {
        assert_eq!(adjust_number("10", 5, true, 0, 100), "15");
        assert_eq!(adjust_number("10", 5, false, 0, 100), "5");
        assert_eq!(adjust_number("98", 10, true, 0, 100), "100");
        assert_eq!(adjust_number("3", 10, false, 0, 100), "0");
        assert_eq!(adjust_number("notanum", 5, true, 1, 100), "6");
    }

    #[test]
    fn settings_state_navigation_wraps_with_save_row() {
        let mut st = SettingsState::new();
        assert_eq!(SECTIONS[0].fields.len(), 5);
        assert!(!st.on_save_row());
        for _ in 0..5 {
            st.next_field();
        }
        assert!(st.on_save_row(), "5 次 next 后应到保存行");
        st.next_field();
        assert!(!st.on_save_row());
        assert_eq!(st.selected, 0);
    }

    #[test]
    fn settings_state_next_section_resets_selected() {
        let mut st = SettingsState::new();
        st.selected = 3;
        st.next_section();
        assert_eq!(st.section, 1);
        assert_eq!(st.selected, 0);
        // 回绕：SECTIONS.len() 次后回到原段
        for _ in 0..SECTIONS.len() {
            st.next_section();
        }
        assert_eq!(st.section, 1);
    }

    #[test]
    fn apply_edit_theme_cycles_and_marks_dirty() {
        let mut st = SettingsState::new();
        let mut cfg = Config::default();
        assert_eq!(cfg.ui.theme, "cyberpunk");
        let live = st.apply_edit(&mut cfg, &ProvidersConfig::default(), true);
        assert_eq!(live, LiveApply::Theme);
        assert_eq!(cfg.ui.theme, "dracula");
        assert!(st.dirty);
    }

    #[test]
    fn apply_edit_bool_toggles() {
        let mut st = SettingsState::new();
        st.selected = 2; // mouse
        let mut cfg = Config::default();
        let live = st.apply_edit(&mut cfg, &ProvidersConfig::default(), true);
        assert_eq!(live, LiveApply::Mouse);
        assert!(!cfg.ui.mouse);
    }

    #[test]
    fn apply_edit_readonly_no_change() {
        let mut st = SettingsState::new();
        st.selected = 4; // frame_rate
        let mut cfg = Config::default();
        let before = cfg.ui.frame_rate;
        let live = st.apply_edit(&mut cfg, &ProvidersConfig::default(), true);
        assert_eq!(live, LiveApply::None);
        assert_eq!(cfg.ui.frame_rate, before);
        assert!(!st.dirty);
    }

    #[test]
    fn apply_edit_provider_enum_cycles_sorted() {
        let mut st = SettingsState::new();
        st.section = 1; // Agent
        st.selected = 0; // default_provider
        let mut cfg = Config::default();
        let providers = ProvidersConfig::default_template();
        // 排序后 [anthropic, ollama, openai]；openai 正向 → anthropic
        let live = st.apply_edit(&mut cfg, &providers, true);
        assert_eq!(live, LiveApply::None);
        assert_eq!(cfg.agent.default_provider, "anthropic");
    }

    #[test]
    fn apply_edit_on_save_row_is_noop() {
        let mut st = SettingsState::new();
        st.selected = SECTIONS[0].fields.len();
        let mut cfg = Config::default();
        let before = cfg.ui.theme.clone();
        let live = st.apply_edit(&mut cfg, &ProvidersConfig::default(), true);
        assert_eq!(live, LiveApply::None);
        assert_eq!(cfg.ui.theme, before);
        assert!(!st.dirty);
    }

    #[test]
    fn providers_section_is_not_editable() {
        let st = SettingsState {
            section: 5, // Providers
            ..SettingsState::default()
        };
        assert!(!st.is_editable());
        assert!(!st.on_save_row(), "只读段不应停在保存行");
    }

    #[test]
    fn on_providers_section_detects_correctly() {
        let mut st = SettingsState::new();
        assert!(!st.on_providers_section());
        st.section = PROVIDERS_SECTION_IDX;
        assert!(st.on_providers_section());
    }

    #[test]
    fn next_prev_provider_clamp() {
        let mut st = SettingsState::new();
        st.section = PROVIDERS_SECTION_IDX;
        // 3 个 provider
        st.next_provider(3);
        assert_eq!(st.provider_selected, 1);
        st.next_provider(3);
        assert_eq!(st.provider_selected, 2);
        // 再下移 → 到保存行
        st.next_provider(3);
        assert!(st.provider_on_save);
        assert_eq!(st.provider_selected, 2);
        // 保存行再下移 → 停留
        st.next_provider(3);
        assert!(st.provider_on_save);
        // 从保存行上移 → 回到最后一项
        st.prev_provider(3);
        assert!(!st.provider_on_save);
        assert_eq!(st.provider_selected, 2);
        st.prev_provider(3);
        assert_eq!(st.provider_selected, 1);
        st.prev_provider(3);
        assert_eq!(st.provider_selected, 0);
        st.prev_provider(3); // saturating_sub 到 0
        assert_eq!(st.provider_selected, 0);
    }

    #[test]
    fn next_prev_provider_empty_list() {
        let mut st = SettingsState::new();
        st.section = PROVIDERS_SECTION_IDX;
        st.provider_selected = 5;
        // 空列表下移 → 保存行
        st.next_provider(0);
        assert!(st.provider_on_save);
        assert_eq!(st.provider_selected, 0);
        // 从保存行上移 → 回到 0（空列表）
        st.prev_provider(0);
        assert!(!st.provider_on_save);
        assert_eq!(st.provider_selected, 0);
    }

    #[test]
    fn interactive_providers_render_does_not_panic() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut st = SettingsState::new();
        st.section = PROVIDERS_SECTION_IDX;
        st.provider_selected = 1;
        st.pending_delete_idx = Some(0);
        let cfg = Config::default();
        let providers = ProvidersConfig::default_template();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    f.area(),
                    &crate::theme::Theme::resolve("cyberpunk"),
                    &cfg,
                    &providers,
                    &McpServersConfig::default(),
                    None,
                    &SkillRegistry::new(),
                    &st,
                    false,
                    None,
                )
            })
            .unwrap();
    }

    #[test]
    fn empty_providers_render_does_not_panic() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut st = SettingsState::new();
        st.section = PROVIDERS_SECTION_IDX;
        let cfg = Config::default();
        let providers = ProvidersConfig::default(); // 空
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    f.area(),
                    &crate::theme::Theme::resolve("cyberpunk"),
                    &cfg,
                    &providers,
                    &McpServersConfig::default(),
                    None,
                    &SkillRegistry::new(),
                    &st,
                    false,
                    None,
                )
            })
            .unwrap();
    }

    #[test]
    fn mask_key_hides_plaintext() {
        assert_eq!(mask_key(""), "(未设置)");
        assert_eq!(mask_key("${OPENAI_API_KEY}"), "${OPENAI_API_KEY}");
        assert_eq!(mask_key("sk-real-secret-key"), "(已设置)");
    }

    #[test]
    fn providers_render_with_price_config_does_not_panic() {
        use cyber_core::PriceConfig;
        use ratatui::{backend::TestBackend, Terminal};
        let mut st = SettingsState::new();
        st.section = PROVIDERS_SECTION_IDX;
        let cfg = Config::default();
        let mut providers = ProvidersConfig::default_template();
        // 给 openai 配置完整价格、anthropic 配置部分价格
        if let Some(p) = providers.providers.get_mut("openai") {
            p.price = Some(PriceConfig {
                input_per_m: Some(2.5),
                output_per_m: Some(10.0),
                cache_hit_per_m: Some(0.3),
                ..Default::default()
            });
        }
        if let Some(p) = providers.providers.get_mut("anthropic") {
            p.price = Some(PriceConfig {
                input_per_m: Some(3.0),
                output_per_m: None,
                cache_hit_per_m: None,
                ..Default::default()
            });
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    f.area(),
                    &crate::theme::Theme::resolve("cyberpunk"),
                    &cfg,
                    &providers,
                    &McpServersConfig::default(),
                    None,
                    &SkillRegistry::new(),
                    &st,
                    false,
                    None,
                )
            })
            .unwrap();
    }
}
