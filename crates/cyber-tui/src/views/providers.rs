//! Provider 表单模态层：新增 / 编辑单个 LLM 服务商。
//!
//! 从 Settings（`a`/`e`）或 Chat（`/provider add|edit`）进入，作为顶层 `Mode::ProviderForm`
//! 渲染。字段：name / kind / base_url / api_key / model / max_tokens / temperature +
//! 三个按钮：拉取模型 / 保存 / 取消。
//!
//! 文本字段用单个复用 `TextArea`：Enter 进入编辑（load 值）→ 输入 → Enter 提交 / Esc 取消编辑。
//! `kind` 用 ←→ 循环 `PROVIDER_KINDS`。「拉取模型」异步 GET `{base}/models`，结果经 mpsc
//! 回传 App → `deliver_fetch`，弹出 picker 选中后回填 model 字段。

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};
use tui_textarea::TextArea;

use cyber_core::{ProviderConfig, ProviderConfig as _Cfg, ProvidersConfig, PROVIDER_KINDS};

use crate::theme::Theme;

/// 字段类型（决定编辑方式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    Text,
    Enum,
    Button,
}

struct FieldDef {
    label: &'static str,
    kind: FieldKind,
}

/// 字段顺序即焦点导航顺序（Up/Down 循环）。
/// 0-6 为数据字段，7=拉取模型，8=保存，9=取消。
const FIELDS: &[FieldDef] = &[
    FieldDef { label: "名称 name", kind: FieldKind::Text },
    FieldDef { label: "类型 kind", kind: FieldKind::Enum },
    FieldDef { label: "base_url", kind: FieldKind::Text },
    FieldDef { label: "api_key", kind: FieldKind::Text },
    FieldDef { label: "model", kind: FieldKind::Text },
    FieldDef { label: "max_tokens", kind: FieldKind::Text },
    FieldDef { label: "temperature", kind: FieldKind::Text },
    FieldDef { label: "拉取模型", kind: FieldKind::Button },
    FieldDef { label: "保存", kind: FieldKind::Button },
    FieldDef { label: "取消", kind: FieldKind::Button },
];
const IDX_KIND: usize = 1;
const IDX_FETCH: usize = 7;
const IDX_SAVE: usize = 8;
const IDX_CANCEL: usize = 9;

/// 表单按键的副作用意图，由 App 解释执行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormAction {
    None,
    Save,
    Cancel,
    Fetch,
    Toast(String),
}

/// Provider 表单状态。
pub struct ProviderFormState {
    pub name: String,
    pub kind_idx: usize,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_tokens: String,
    pub temperature: String,
    /// `Some` = 编辑现有（值为原始 name）；`None` = 新增。
    pub original_name: Option<String>,
    pub focused: usize,
    pub editing: bool,
    pub textarea: TextArea<'static>,
    pub fetching: bool,
    pub fetch_id: u64,
    pub fetch_error: Option<String>,
    pub fetched_models: Vec<String>,
    pub picker_open: bool,
    pub picker_selected: usize,
}

impl ProviderFormState {
    /// 新增模式：默认值（openai / 空 url / 4096 / 0.7）。
    pub fn empty() -> Self {
        let mut textarea = TextArea::default();
        textarea.set_placeholder_text("输入…");
        Self {
            name: String::new(),
            kind_idx: 0,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            max_tokens: "4096".into(),
            temperature: "0.7".into(),
            original_name: None,
            focused: 0,
            editing: false,
            textarea,
            fetching: false,
            fetch_id: 0,
            fetch_error: None,
            fetched_models: Vec::new(),
            picker_open: false,
            picker_selected: 0,
        }
    }

    /// 编辑模式：从现有 provider 装载。
    pub fn from_provider(name: &str, cfg: &ProviderConfig) -> Self {
        let kind_idx = PROVIDER_KINDS
            .iter()
            .position(|k| *k == cfg.kind)
            .unwrap_or(0);
        let mut s = Self {
            name: name.to_string(),
            kind_idx,
            base_url: cfg.base_url.clone(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens.to_string(),
            temperature: cfg.temperature.to_string(),
            original_name: Some(name.to_string()),
            ..Self::empty()
        };
        // 编辑模式焦点先停在 base_url（name 一般不改）
        s.focused = 2;
        s
    }

    pub fn is_edit(&self) -> bool {
        self.original_name.is_some()
    }

    pub fn kind(&self) -> &'static str {
        PROVIDER_KINDS[self.kind_idx]
    }

    /// 当前表单值的快照（用于 fetch，即使未校验通过也能拉取）。
    pub fn to_provider_config_snapshot(&self) -> ProviderConfig {
        ProviderConfig {
            kind: self.kind().to_string(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            max_tokens: self.max_tokens.trim().parse().unwrap_or(4096),
            temperature: self.temperature.trim().parse().unwrap_or(0.7),
        }
    }

    /// 校验并构造 `(name, ProviderConfig)`。失败返回错误文案（App 弹 toast）。
    pub fn into_provider(
        &self,
        existing: &ProvidersConfig,
    ) -> Result<(String, ProviderConfig), String> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err("名称不能为空".into());
        }
        let original = self.original_name.as_deref().unwrap_or("");
        if name != original && existing.providers.contains_key(&name) {
            return Err(format!("名称 '{name}' 已存在"));
        }
        let base_url = self.base_url.trim().to_string();
        if base_url.is_empty() {
            return Err("base_url 不能为空".into());
        }
        let max_tokens: u32 = self
            .max_tokens
            .trim()
            .parse()
            .map_err(|_| "max_tokens 必须是数字".to_string())?;
        let temperature: f32 = self
            .temperature
            .trim()
            .parse()
            .map_err(|_| "temperature 必须是数字".to_string())?;
        Ok((
            name,
            ProviderConfig {
                kind: self.kind().to_string(),
                base_url,
                api_key: self.api_key.trim().to_string(),
                model: self.model.trim().to_string(),
                max_tokens,
                temperature,
            },
        ))
    }

    fn get_field(&self, idx: usize) -> String {
        match idx {
            0 => self.name.clone(),
            1 => self.kind().to_string(),
            2 => self.base_url.clone(),
            3 => self.api_key.clone(),
            4 => self.model.clone(),
            5 => self.max_tokens.clone(),
            6 => self.temperature.clone(),
            _ => String::new(),
        }
    }

    fn set_field(&mut self, idx: usize, val: String) {
        match idx {
            0 => self.name = val,
            2 => self.base_url = val,
            3 => self.api_key = val,
            4 => self.model = val,
            5 => self.max_tokens = val,
            6 => self.temperature = val,
            _ => {}
        }
    }

    fn is_text_field(idx: usize) -> bool {
        matches!(idx, 0 | 2 | 3 | 4 | 5 | 6)
    }

    fn start_editing(&mut self, idx: usize) {
        let val = self.get_field(idx);
        self.textarea.clear();
        self.textarea.insert_str(&val);
        self.editing = true;
    }

    /// 处理一个按键，返回副作用意图。`existing` 保留供未来实时校验（当前校验在 `into_provider`）。
    pub fn handle_key(&mut self, k: KeyEvent, _existing: &ProvidersConfig) -> FormAction {
        // Ctrl+C / Ctrl+Q 不在此处理（App 层统一作退出）
        if self.picker_open {
            return self.handle_picker_key(k);
        }
        if self.editing {
            return self.handle_editing_key(k);
        }
        match k.code {
            KeyCode::Up => {
                self.focused = (self.focused + FIELDS.len() - 1) % FIELDS.len();
                FormAction::None
            }
            KeyCode::Down => {
                self.focused = (self.focused + 1) % FIELDS.len();
                FormAction::None
            }
            KeyCode::Left => {
                if self.focused == IDX_KIND {
                    self.kind_idx =
                        (self.kind_idx + PROVIDER_KINDS.len() - 1) % PROVIDER_KINDS.len();
                }
                FormAction::None
            }
            KeyCode::Right => {
                if self.focused == IDX_KIND {
                    self.kind_idx = (self.kind_idx + 1) % PROVIDER_KINDS.len();
                }
                FormAction::None
            }
            KeyCode::Enter => {
                match self.focused {
                    IDX_FETCH => {
                        if self.fetching {
                            FormAction::None
                        } else {
                            FormAction::Fetch
                        }
                    }
                    IDX_SAVE => FormAction::Save,
                    IDX_CANCEL => FormAction::Cancel,
                    IDX_KIND => FormAction::None,
                    idx if Self::is_text_field(idx) => {
                        self.start_editing(idx);
                        FormAction::None
                    }
                    _ => FormAction::None,
                }
            }
            KeyCode::Esc => FormAction::Cancel,
            _ => FormAction::None,
        }
    }

    fn handle_editing_key(&mut self, k: KeyEvent) -> FormAction {
        match k.code {
            KeyCode::Enter => {
                let val = self.textarea.lines().join("\n");
                self.set_field(self.focused, val);
                self.editing = false;
                FormAction::None
            }
            KeyCode::Esc => {
                self.editing = false; // 丢弃改动
                FormAction::None
            }
            _ => {
                self.textarea.input(k);
                FormAction::None
            }
        }
    }

    fn handle_picker_key(&mut self, k: KeyEvent) -> FormAction {
        if self.fetched_models.is_empty() {
            self.picker_open = false;
            return FormAction::None;
        }
        let n = self.fetched_models.len();
        match k.code {
            KeyCode::Up => {
                self.picker_selected = (self.picker_selected + n - 1) % n;
                FormAction::None
            }
            KeyCode::Down => {
                self.picker_selected = (self.picker_selected + 1) % n;
                FormAction::None
            }
            KeyCode::Enter => {
                let m = self.fetched_models[self.picker_selected].clone();
                self.model = m;
                self.picker_open = false;
                FormAction::None
            }
            KeyCode::Esc => {
                self.picker_open = false;
                FormAction::None
            }
            _ => FormAction::None,
        }
    }

    /// 发起拉取：bump fetch_id（防 stale）+ 置 fetching。返回 fetch_id 供 App spawn 任务。
    pub fn start_fetch(&mut self) -> u64 {
        self.fetch_id = self.fetch_id.wrapping_add(1);
        self.fetching = true;
        self.fetch_error = None;
        self.fetched_models.clear();
        self.picker_open = false;
        self.fetch_id
    }

    /// 接收拉取结果。fetch_id 不匹配（已发起新一轮或 form 重开）则丢弃。
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
                    self.fetched_models = models;
                    self.picker_selected = 0;
                    self.picker_open = true;
                }
            }
            Err(e) => {
                self.fetch_error = Some(e);
            }
        }
    }

    /// draw 前 `&mut self` 应用 textarea 样式（绕过 render `&self` 限制，同 ChatState 模式）。
    pub fn prepare_render(&mut self, theme: &Theme) {
        self.textarea.set_block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border))
                .title(
                    Line::from(format!(" {} ", FIELDS[self.focused].label))
                        .style(Style::default().fg(theme.title)),
                ),
        );
        self.textarea
            .set_style(Style::default().fg(theme.fg).bg(theme.bg));
        self.textarea
            .set_placeholder_style(Style::default().fg(theme.muted));
    }
}

/// 渲染表单模态层（居中）。
pub fn render_form(frame: &mut Frame, area: Rect, theme: &Theme, state: &ProviderFormState) {
    let modal = centered_rect(72, 82, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(
            Line::from(if state.is_edit() {
                format!(" 编辑 Provider: {} ", state.original_name.as_deref().unwrap_or(""))
            } else {
                " 添加 Provider ".to_string()
            })
            .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let chunks = Layout::vertical([
        Constraint::Min(0),   // 字段列表
        Constraint::Length(3), // 编辑器 / picker / hint
        Constraint::Length(1), // 状态行
        Constraint::Length(1), // 按钮行
    ])
    .split(inner);

    render_fields(frame, chunks[0], theme, state);
    render_editor(frame, chunks[1], theme, state);
    render_status(frame, chunks[2], theme, state);
    render_buttons(frame, chunks[3], theme, state);
}

fn render_fields(frame: &mut Frame, area: Rect, theme: &Theme, state: &ProviderFormState) {
    let mut lines: Vec<Line> = Vec::new();
    for (i, f) in FIELDS.iter().enumerate() {
        if f.kind == FieldKind::Button {
            continue; // 按钮单独渲染
        }
        let selected = i == state.focused && !state.editing && !state.picker_open;
        let marker = if selected { "▸ " } else { "  " };
        let value: String = if i == IDX_KIND {
            format!("{}  ←→", state.kind())
        } else if i == 3 {
            // api_key 脱敏显示
            mask_key(&state.api_key)
        } else {
            state.get_field(i)
        };
        let editing_marker = if selected && state.editing { " [编辑中]" } else { "" };
        let row_style = if selected {
            Style::default().bg(theme.sel_bg)
        } else {
            Style::default().bg(theme.bg)
        };
        lines.push(
            Line::from(vec![
                Span::raw(marker.to_string()),
                Span::styled(f.label.to_string(), Style::default().fg(theme.fg)),
                Span::raw(" : "),
                Span::styled(
                    format!("{value}{editing_marker}"),
                    Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
                ),
            ])
            .style(row_style),
        );
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg)),
        area,
    );
}

fn render_editor(frame: &mut Frame, area: Rect, theme: &Theme, state: &ProviderFormState) {
    if state.editing {
        frame.render_widget(&state.textarea, area);
        return;
    }
    if state.picker_open && !state.fetched_models.is_empty() {
        let mut lines: Vec<Line> = Vec::new();
        lines.push(
            Line::from(" 选择模型 (Enter 选中 / Esc 关闭)")
                .style(Style::default().fg(theme.muted)),
        );
        for (i, m) in state.fetched_models.iter().enumerate() {
            let selected = i == state.picker_selected;
            let marker = if selected { "▸ " } else { "  " };
            let style = if selected {
                Style::default().bg(theme.sel_bg).fg(theme.sel_fg)
            } else {
                Style::default().fg(theme.fg)
            };
            lines.push(Line::from(format!("{marker}{m}")).style(style));
        }
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(theme.bg)),
            area,
        );
        return;
    }
    let hint = if state.fetching {
        " 拉取中…"
    } else {
        " Enter 编辑字段 · ←→ 切换 kind · Esc 取消"
    };
    frame.render_widget(
        Paragraph::new(Line::from(hint)).style(Style::default().fg(theme.muted)),
        area,
    );
}

fn render_status(frame: &mut Frame, area: Rect, theme: &Theme, state: &ProviderFormState) {
    let line = if let Some(err) = &state.fetch_error {
        Line::from(format!(" ⚠ {err}")).style(Style::default().fg(theme.accent))
    } else if state.fetching {
        Line::from(" ⏳ 正在拉取模型列表…").style(Style::default().fg(theme.muted))
    } else {
        Line::from("")
    };
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme.bg)),
        area,
    );
}

fn render_buttons(frame: &mut Frame, area: Rect, theme: &Theme, state: &ProviderFormState) {
    let buttons = [(IDX_FETCH, "拉取模型"), (IDX_SAVE, "保存"), (IDX_CANCEL, "取消")];
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::raw(" "));
    for (idx, label) in buttons {
        let active = state.focused == idx && !state.editing && !state.picker_open;
        let marker = if active { "▸[" } else { " [" };
        let close = "] ";
        let style = if active {
            Style::default().bg(theme.sel_bg).fg(theme.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };
        spans.push(Span::styled(format!("{marker}{label}{close}"), style));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.bg)),
        area,
    );
}

/// 脱敏 api_key：空 → (未设置)；`${ENV}` 原样；明文 → (已设置)。
/// 与 settings.rs 的 mask_key 同语义（复制以避免跨模块 pub 可见性扩散）。
fn mask_key(k: &str) -> String {
    if k.is_empty() {
        "(未设置)".into()
    } else if k.starts_with("${") {
        k.into()
    } else {
        "(已设置)".into()
    }
}

/// 居中算子区域（percent_x 宽，percent_y 高）。
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let pop_h = area.height.saturating_mul(percent_y) / 100;
    let pop_w = area.width.saturating_mul(percent_x) / 100;
    let y = area.y + (area.height.saturating_sub(pop_h)) / 2;
    let x = area.x + (area.width.saturating_sub(pop_w)) / 2;
    Rect::new(x, y, pop_w, pop_h)
}

// 抑制未使用导入警告（`_Cfg` 别名保留供未来扩展）。
#[allow(unused_imports)]
use _Cfg as _ProviderCfgAlias;

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn empty_defaults() {
        let s = ProviderFormState::empty();
        assert!(!s.is_edit());
        assert_eq!(s.kind(), "openai");
        assert_eq!(s.max_tokens, "4096");
        assert_eq!(s.temperature, "0.7");
        assert_eq!(s.focused, 0);
    }

    #[test]
    fn from_provider_loads_fields_and_is_edit() {
        let cfg = ProviderConfig {
            kind: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            api_key: "${ANTHROPIC_API_KEY}".into(),
            model: "claude-sonnet-4-5".into(),
            max_tokens: 8192,
            temperature: 0.3,
        };
        let s = ProviderFormState::from_provider("anthropic", &cfg);
        assert!(s.is_edit());
        assert_eq!(s.original_name.as_deref(), Some("anthropic"));
        assert_eq!(s.kind(), "anthropic");
        assert_eq!(s.base_url, "https://api.anthropic.com");
        assert_eq!(s.max_tokens, "8192");
    }

    #[test]
    fn kind_cycle_via_left_right() {
        let mut s = ProviderFormState::empty();
        s.focused = IDX_KIND;
        s.handle_key(key(KeyCode::Right), &ProvidersConfig::default());
        assert_eq!(s.kind(), "anthropic");
        s.handle_key(key(KeyCode::Left), &ProvidersConfig::default());
        assert_eq!(s.kind(), "openai");
        // 回绕
        s.handle_key(key(KeyCode::Left), &ProvidersConfig::default());
        assert_eq!(s.kind(), "openai-compatible");
    }

    #[test]
    fn into_provider_validates_empty_name() {
        let s = ProviderFormState::empty();
        let err = s.into_provider(&ProvidersConfig::default()).unwrap_err();
        assert!(err.contains("名称"));
    }

    #[test]
    fn into_provider_validates_empty_base_url() {
        let mut s = ProviderFormState::empty();
        s.name = "foo".into();
        let err = s.into_provider(&ProvidersConfig::default()).unwrap_err();
        assert!(err.contains("base_url"));
    }

    #[test]
    fn into_provider_rejects_duplicate_name_on_add() {
        let mut s = ProviderFormState::empty();
        s.name = "openai".into();
        s.base_url = "https://x".into();
        let existing = ProvidersConfig::default_template();
        let err = s.into_provider(&existing).unwrap_err();
        assert!(err.contains("已存在"));
    }

    #[test]
    fn into_provider_allows_keep_name_on_edit() {
        let cfg = ProviderConfig {
            kind: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            ..Default::default()
        };
        let s = ProviderFormState::from_provider("openai", &cfg);
        // 同名编辑应通过
        let (name, _) = s.into_provider(&ProvidersConfig::default_template()).unwrap();
        assert_eq!(name, "openai");
    }

    #[test]
    fn into_provider_rejects_bad_number() {
        let mut s = ProviderFormState::empty();
        s.name = "foo".into();
        s.base_url = "https://x".into();
        s.max_tokens = "abc".into();
        let err = s.into_provider(&ProvidersConfig::default()).unwrap_err();
        assert!(err.contains("max_tokens"));
    }

    #[test]
    fn enter_text_field_starts_editing_enter_commits() {
        let mut s = ProviderFormState::empty();
        s.focused = 0; // name
        assert_eq!(s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default()), FormAction::None);
        assert!(s.editing);
        // 输入字符
        s.handle_key(key(KeyCode::Char('z')), &ProvidersConfig::default());
        s.handle_key(key(KeyCode::Char('z')), &ProvidersConfig::default());
        // Enter 提交
        s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default());
        assert!(!s.editing);
        assert_eq!(s.name, "zz");
    }

    #[test]
    fn editing_esc_discards() {
        let mut s = ProviderFormState::empty();
        s.focused = 2; // base_url
        s.base_url = "old".into();
        s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default()); // 进入编辑，load "old"
        s.handle_key(key(KeyCode::Char('X')), &ProvidersConfig::default());
        s.handle_key(key(KeyCode::Esc), &ProvidersConfig::default()); // 丢弃
        assert!(!s.editing);
        assert_eq!(s.base_url, "old", "Esc 应丢弃编辑");
    }

    #[test]
    fn save_button_returns_save_action() {
        let mut s = ProviderFormState::empty();
        s.focused = IDX_SAVE;
        assert_eq!(s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default()), FormAction::Save);
    }

    #[test]
    fn cancel_button_and_esc_return_cancel() {
        let mut s = ProviderFormState::empty();
        s.focused = IDX_CANCEL;
        assert_eq!(s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default()), FormAction::Cancel);
        assert_eq!(s.handle_key(key(KeyCode::Esc), &ProvidersConfig::default()), FormAction::Cancel);
    }

    #[test]
    fn fetch_button_returns_fetch_action() {
        let mut s = ProviderFormState::empty();
        s.focused = IDX_FETCH;
        assert_eq!(s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default()), FormAction::Fetch);
    }

    #[test]
    fn fetch_button_noop_while_fetching() {
        let mut s = ProviderFormState::empty();
        s.focused = IDX_FETCH;
        s.fetching = true;
        assert_eq!(s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default()), FormAction::None);
    }

    #[test]
    fn start_fetch_bumps_id_and_sets_fetching() {
        let mut s = ProviderFormState::empty();
        let id1 = s.start_fetch();
        assert!(s.fetching);
        let id2 = s.start_fetch();
        assert_ne!(id1, id2);
    }

    #[test]
    fn deliver_fetch_stale_id_ignored() {
        let mut s = ProviderFormState::empty();
        let id = s.start_fetch();
        s.deliver_fetch(id.wrapping_sub(1), Ok(vec!["m".into()]));
        assert!(s.fetching, "stale 结果应被忽略");
        assert!(s.fetched_models.is_empty());
    }

    #[test]
    fn deliver_fetch_success_opens_picker() {
        let mut s = ProviderFormState::empty();
        let id = s.start_fetch();
        s.deliver_fetch(id, Ok(vec!["gpt-4o".into(), "gpt-4o-mini".into()]));
        assert!(!s.fetching);
        assert_eq!(s.fetched_models.len(), 2);
        assert!(s.picker_open);
        assert_eq!(s.picker_selected, 0);
    }

    #[test]
    fn deliver_fetch_error_stores_message() {
        let mut s = ProviderFormState::empty();
        let id = s.start_fetch();
        s.deliver_fetch(id, Err("timeout".into()));
        assert!(!s.fetching);
        assert_eq!(s.fetch_error.as_deref(), Some("timeout"));
        assert!(!s.picker_open);
    }

    #[test]
    fn picker_enter_selects_model() {
        let mut s = ProviderFormState::empty();
        let id = s.start_fetch();
        s.deliver_fetch(id, Ok(vec!["a".into(), "b".into()]));
        assert!(s.picker_open);
        s.handle_key(key(KeyCode::Down), &ProvidersConfig::default()); // → b
        s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default()); // 选中 b
        assert!(!s.picker_open);
        assert_eq!(s.model, "b");
    }

    #[test]
    fn to_provider_config_snapshot_parses_numbers() {
        let mut s = ProviderFormState::empty();
        s.base_url = "https://x".into();
        s.max_tokens = "2048".into();
        s.temperature = "0.1".into();
        let cfg = s.to_provider_config_snapshot();
        assert_eq!(cfg.max_tokens, 2048);
        assert!((cfg.temperature - 0.1).abs() < 1e-6);
    }

    #[test]
    fn render_form_does_not_panic() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut s = ProviderFormState::from_provider(
            "openai",
            &ProviderConfig {
                kind: "openai".into(),
                base_url: "https://api.openai.com/v1".into(),
                api_key: "${OPENAI_API_KEY}".into(),
                model: "gpt-4o".into(),
                ..Default::default()
            },
        );
        s.prepare_render(&crate::theme::Theme::resolve("cyberpunk"));
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_form(f, f.area(), &crate::theme::Theme::resolve("cyberpunk"), &s))
            .unwrap();
    }
}
