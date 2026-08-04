//! MCP server 表单模态层：新增 / 编辑单个 MCP server 配置。
//!
//! 从 Settings MCP 段（`a`/`e`）进入，作为顶层 `Mode::McpForm` 渲染。
//! 字段：name / transport（←→ 循环）/ command / args / env / url / headers / timeout_secs
//! + 保存 / 取消按钮。
//!
//! transport 决定字段可见性：stdio 显示 command/args/env；http/sse 显示 url/headers。
//! 文本字段用单个复用 `TextArea`：Enter 进入编辑（load 值）→ 输入 → Enter 提交 / Esc 取消。
//! `args` 每行一个参数；`env`/`headers` 每行 `KEY=VALUE`。

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};
use tui_textarea::TextArea;

use cyber_mcp::{McpServerSpec, McpTransport};

use crate::theme::Theme;

/// transport 选项（←→ 循环）。
const TRANSPORTS: &[McpTransport] = &[
    McpTransport::Stdio,
    McpTransport::Http,
    McpTransport::Sse,
];

/// 字段类型。
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
/// 0=name 1=transport 2=command 3=args 4=env 5=url 6=headers 7=timeout 8=保存 9=取消
const FIELDS: &[FieldDef] = &[
    FieldDef { label: "名称 name", kind: FieldKind::Text },
    FieldDef { label: "传输 transport", kind: FieldKind::Enum },
    FieldDef { label: "命令 command (stdio)", kind: FieldKind::Text },
    FieldDef { label: "参数 args (每行一个, stdio)", kind: FieldKind::Text },
    FieldDef { label: "环境变量 env (每行 KEY=VALUE, stdio)", kind: FieldKind::Text },
    FieldDef { label: "URL url (http/sse)", kind: FieldKind::Text },
    FieldDef { label: "请求头 headers (每行 KEY=VALUE, http/sse)", kind: FieldKind::Text },
    FieldDef { label: "超时 timeout_secs", kind: FieldKind::Text },
    FieldDef { label: "保存", kind: FieldKind::Button },
    FieldDef { label: "取消", kind: FieldKind::Button },
];
const IDX_TRANSPORT: usize = 1;
const IDX_SAVE: usize = 8;
const IDX_CANCEL: usize = 9;

/// 表单按键的副作用意图，由 App 解释执行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpFormAction {
    None,
    Save,
    Cancel,
}

/// MCP server 表单状态。
pub struct McpFormState {
    pub name: String,
    pub transport_idx: usize,
    pub command: String,
    /// 每行一个参数。
    pub args: String,
    /// 每行 KEY=VALUE。
    pub env: String,
    pub url: String,
    /// 每行 KEY=VALUE。
    pub headers: String,
    pub timeout_secs: String,
    /// `Some` = 编辑现有（值为原始 name）；`None` = 新增。
    pub original_name: Option<String>,
    pub focused: usize,
    pub editing: bool,
    pub textarea: TextArea<'static>,
}

impl McpFormState {
    /// 新增模式：默认值（stdio / 空 command / timeout=5）。
    pub fn empty() -> Self {
        let mut textarea = TextArea::default();
        textarea.set_placeholder_text("输入…");
        Self {
            name: String::new(),
            transport_idx: 0,
            command: String::new(),
            args: String::new(),
            env: String::new(),
            url: String::new(),
            headers: String::new(),
            timeout_secs: "5".into(),
            original_name: None,
            focused: 0,
            editing: false,
            textarea,
        }
    }

    /// 编辑模式：从现有 spec 装载。
    pub fn from_spec(spec: &McpServerSpec) -> Self {
        let transport_idx = TRANSPORTS
            .iter()
            .position(|t| *t == spec.transport)
            .unwrap_or(0);
        let mut s = Self {
            name: spec.name.clone(),
            transport_idx,
            command: spec.command.clone().unwrap_or_default(),
            args: spec.args.join("\n"),
            env: serialize_map(&spec.env),
            url: spec.url.clone().unwrap_or_default(),
            headers: serialize_map(&spec.headers),
            timeout_secs: spec.timeout_secs.to_string(),
            original_name: Some(spec.name.clone()),
            ..Self::empty()
        };
        // 编辑模式焦点先停在 transport（name 一般不改）
        s.focused = 1;
        s
    }

    pub fn is_edit(&self) -> bool {
        self.original_name.is_some()
    }

    pub fn transport(&self) -> McpTransport {
        TRANSPORTS[self.transport_idx]
    }

    /// 当前字段在当前 transport 下是否可见（不可见字段跳过导航）。
    fn is_field_visible(&self, idx: usize) -> bool {
        let is_stdio = self.transport() == McpTransport::Stdio;
        match idx {
            2..=4 => is_stdio,
            5 | 6 => !is_stdio,
            _ => true,
        }
    }

    /// 下一个可见字段索引（循环）。
    fn next_visible(&self) -> usize {
        let n = FIELDS.len();
        for i in 1..=n {
            let idx = (self.focused + i) % n;
            if self.is_field_visible(idx) {
                return idx;
            }
        }
        self.focused
    }

    /// 上一个可见字段索引（循环）。
    fn prev_visible(&self) -> usize {
        let n = FIELDS.len();
        for i in 1..=n {
            let idx = (self.focused + n - i) % n;
            if self.is_field_visible(idx) {
                return idx;
            }
        }
        self.focused
    }

    /// 校验并构造 `McpServerSpec`。失败返回错误文案（App 弹 toast）。
    /// `existing_names` 用于重名校验（编辑模式排除自身原名）。
    pub fn into_spec(
        &self,
        existing_names: &[&str],
    ) -> Result<McpServerSpec, String> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err("名称不能为空".into());
        }
        let original = self.original_name.as_deref().unwrap_or("");
        if name != original && existing_names.contains(&name.as_str()) {
            return Err(format!("名称 '{name}' 已存在"));
        }
        let transport = self.transport();
        let (command, args, env, url, headers) = match transport {
            McpTransport::Stdio => {
                let cmd = self.command.trim().to_string();
                if cmd.is_empty() {
                    return Err("stdio 模式 command 不能为空".into());
                }
                let args = parse_lines(&self.args);
                let env = parse_map(&self.env)
                    .map_err(|e| format!("env 解析失败: {e}"))?;
                (Some(cmd), args, env, None, Default::default())
            }
            McpTransport::Http | McpTransport::Sse => {
                let url = self.url.trim().to_string();
                if url.is_empty() {
                    return Err("http/sse 模式 url 不能为空".into());
                }
                let headers = parse_map(&self.headers)
                    .map_err(|e| format!("headers 解析失败: {e}"))?;
                (None, Vec::new(), Default::default(), Some(url), headers)
            }
        };
        let timeout_secs: u64 = self
            .timeout_secs
            .trim()
            .parse()
            .map_err(|_| "timeout_secs 必须是数字".to_string())?;
        Ok(McpServerSpec {
            name,
            transport,
            command,
            args,
            env,
            url,
            headers,
            timeout_secs,
        })
    }

    fn get_field(&self, idx: usize) -> String {
        match idx {
            0 => self.name.clone(),
            1 => format!("{}", self.transport()),
            2 => self.command.clone(),
            3 => self.args.clone(),
            4 => self.env.clone(),
            5 => self.url.clone(),
            6 => self.headers.clone(),
            7 => self.timeout_secs.clone(),
            _ => String::new(),
        }
    }

    fn set_field(&mut self, idx: usize, val: String) {
        match idx {
            0 => self.name = val,
            2 => self.command = val,
            3 => self.args = val,
            4 => self.env = val,
            5 => self.url = val,
            6 => self.headers = val,
            7 => self.timeout_secs = val,
            _ => {}
        }
    }

    fn is_text_field(idx: usize) -> bool {
        matches!(idx, 0 | 2 | 3 | 4 | 5 | 6 | 7)
    }

    fn start_editing(&mut self, idx: usize) {
        let val = self.get_field(idx);
        self.textarea.clear();
        self.textarea.insert_str(&val);
        self.editing = true;
    }

    /// 处理一个按键，返回副作用意图。
    pub fn handle_key(&mut self, k: KeyEvent) -> McpFormAction {
        if self.editing {
            return self.handle_editing_key(k);
        }
        match k.code {
            KeyCode::Up => {
                self.focused = self.prev_visible();
                McpFormAction::None
            }
            KeyCode::Down => {
                self.focused = self.next_visible();
                McpFormAction::None
            }
            KeyCode::Left => {
                if self.focused == IDX_TRANSPORT {
                    self.transport_idx =
                        (self.transport_idx + TRANSPORTS.len() - 1) % TRANSPORTS.len();
                    // 切换 transport 后若当前焦点不可见，移到可见字段
                    if !self.is_field_visible(self.focused) {
                        self.focused = self.next_visible();
                    }
                }
                McpFormAction::None
            }
            KeyCode::Right => {
                if self.focused == IDX_TRANSPORT {
                    self.transport_idx = (self.transport_idx + 1) % TRANSPORTS.len();
                    if !self.is_field_visible(self.focused) {
                        self.focused = self.next_visible();
                    }
                }
                McpFormAction::None
            }
            KeyCode::Enter => {
                match self.focused {
                    IDX_SAVE => McpFormAction::Save,
                    IDX_CANCEL => McpFormAction::Cancel,
                    IDX_TRANSPORT => McpFormAction::None,
                    idx if Self::is_text_field(idx) && self.is_field_visible(idx) => {
                        self.start_editing(idx);
                        McpFormAction::None
                    }
                    _ => McpFormAction::None,
                }
            }
            KeyCode::Esc => McpFormAction::Cancel,
            _ => McpFormAction::None,
        }
    }

    fn handle_editing_key(&mut self, k: KeyEvent) -> McpFormAction {
        match k.code {
            KeyCode::Enter => {
                let val = self.textarea.lines().join("\n");
                self.set_field(self.focused, val);
                self.editing = false;
                McpFormAction::None
            }
            KeyCode::Esc => {
                self.editing = false; // 丢弃改动
                McpFormAction::None
            }
            _ => {
                self.textarea.input(k);
                McpFormAction::None
            }
        }
    }

    /// draw 前 `&mut self` 应用 textarea 样式（绕过 render `&self` 限制）。
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
pub fn render_form(frame: &mut Frame, area: Rect, theme: &Theme, state: &McpFormState) {
    let modal = centered_rect(72, 82, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(
            Line::from(if state.is_edit() {
                format!(" 编辑 MCP Server: {} ", state.original_name.as_deref().unwrap_or(""))
            } else {
                " 添加 MCP Server ".to_string()
            })
            .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let chunks = Layout::vertical([
        Constraint::Min(0),    // 字段列表
        Constraint::Length(3), // 编辑器 / hint
        Constraint::Length(1), // 按钮行
    ])
    .split(inner);

    render_fields(frame, chunks[0], theme, state);
    render_editor(frame, chunks[1], theme, state);
    render_buttons(frame, chunks[2], theme, state);
}

fn render_fields(frame: &mut Frame, area: Rect, theme: &Theme, state: &McpFormState) {
    let mut lines: Vec<Line> = Vec::new();
    for (i, f) in FIELDS.iter().enumerate() {
        if f.kind == FieldKind::Button {
            continue; // 按钮单独渲染
        }
        if !state.is_field_visible(i) {
            continue; // transport 不适用的字段不显示
        }
        let selected = i == state.focused && !state.editing;
        let marker = if selected { "▸ " } else { "  " };
        let value: String = if i == IDX_TRANSPORT {
            format!("{}  ←→", state.transport())
        } else {
            state.get_field(i)
        };
        let editing_marker = if selected && state.editing { " [编辑中]" } else { "" };
        let row_style = if selected {
            Style::default().bg(theme.sel_bg)
        } else {
            Style::default().bg(theme.bg)
        };
        // 多行字段（args/env/headers）只显示首行 + 计数
        let display_value = if matches!(i, 3 | 4 | 6) && value.contains('\n') {
            let line_count = value.lines().count();
            let first = value.lines().next().unwrap_or("");
            if first.is_empty() {
                format!("({line_count} 行){editing_marker}")
            } else {
                format!("{first} … ({line_count} 行){editing_marker}")
            }
        } else {
            format!("{value}{editing_marker}")
        };
        lines.push(
            Line::from(vec![
                Span::raw(marker.to_string()),
                Span::styled(f.label.to_string(), Style::default().fg(theme.fg)),
                Span::raw(" : "),
                Span::styled(
                    display_value,
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

fn render_editor(frame: &mut Frame, area: Rect, theme: &Theme, state: &McpFormState) {
    if state.editing {
        frame.render_widget(&state.textarea, area);
        return;
    }
    let hint = " Enter 编辑字段 · ←→ 切 transport · Esc 取消";
    frame.render_widget(
        Paragraph::new(Line::from(hint)).style(Style::default().fg(theme.muted)),
        area,
    );
}

fn render_buttons(frame: &mut Frame, area: Rect, theme: &Theme, state: &McpFormState) {
    let buttons = [(IDX_SAVE, "保存"), (IDX_CANCEL, "取消")];
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::raw(" "));
    for (idx, label) in buttons {
        let active = state.focused == idx && !state.editing;
        let marker = if active { "▸[" } else { " [" };
        let close = "] ";
        let style = if active {
            Style::default()
                .bg(theme.sel_bg)
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
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

// ---- 辅助函数 ----

/// HashMap → "KEY=VALUE\n" 拼接（排序键以稳定显示顺序）。
fn serialize_map(map: &std::collections::HashMap<String, String>) -> String {
    let mut entries: Vec<(&String, &String)> = map.iter().collect();
    entries.sort_by_key(|(k, _)| *k);
    entries
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 按行拆分（去空行）。
fn parse_lines(s: &str) -> Vec<String> {
    s.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// "KEY=VALUE\nKEY2=VALUE2" → HashMap。失败返回错误行信息。
fn parse_map(s: &str) -> Result<std::collections::HashMap<String, String>, String> {
    let mut map = std::collections::HashMap::new();
    for (i, line) in s.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((k, v)) = trimmed.split_once('=') else {
            return Err(format!("第 {} 行缺少 '=': {trimmed}", i + 1));
        };
        let k = k.trim().to_string();
        if k.is_empty() {
            return Err(format!("第 {} 行 KEY 为空", i + 1));
        }
        map.insert(k, v.trim().to_string());
    }
    Ok(map)
}

/// 居中算子区域（percent_x 宽，percent_y 高）。
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let pop_h = area.height.saturating_mul(percent_y) / 100;
    let pop_w = area.width.saturating_mul(percent_x) / 100;
    let y = area.y + (area.height.saturating_sub(pop_h)) / 2;
    let x = area.x + (area.width.saturating_sub(pop_w)) / 2;
    Rect::new(x, y, pop_w, pop_h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn empty_defaults() {
        let s = McpFormState::empty();
        assert!(!s.is_edit());
        assert_eq!(s.transport(), McpTransport::Stdio);
        assert_eq!(s.timeout_secs, "5");
        assert_eq!(s.focused, 0);
    }

    #[test]
    fn from_spec_loads_fields_and_is_edit() {
        let spec = McpServerSpec {
            name: "filesystem".into(),
            transport: McpTransport::Stdio,
            command: Some("npx".into()),
            args: vec!["-y".into(), "@modelcontextprotocol/server-filesystem".into()],
            env: [("FOO".into(), "bar".into())].into_iter().collect(),
            url: None,
            headers: Default::default(),
            timeout_secs: 10,
        };
        let s = McpFormState::from_spec(&spec);
        assert!(s.is_edit());
        assert_eq!(s.original_name.as_deref(), Some("filesystem"));
        assert_eq!(s.name, "filesystem");
        assert_eq!(s.command, "npx");
        assert_eq!(s.args, "-y\n@modelcontextprotocol/server-filesystem");
        assert!(s.env.contains("FOO=bar"));
        assert_eq!(s.timeout_secs, "10");
    }

    #[test]
    fn transport_cycle_via_left_right() {
        let mut s = McpFormState::empty();
        s.focused = IDX_TRANSPORT;
        s.handle_key(key(KeyCode::Right));
        assert_eq!(s.transport(), McpTransport::Http);
        s.handle_key(key(KeyCode::Right));
        assert_eq!(s.transport(), McpTransport::Sse);
        s.handle_key(key(KeyCode::Right));
        assert_eq!(s.transport(), McpTransport::Stdio);
        s.handle_key(key(KeyCode::Left));
        assert_eq!(s.transport(), McpTransport::Sse);
    }

    #[test]
    fn into_spec_validates_empty_name() {
        let s = McpFormState::empty();
        let err = s.into_spec(&[]).unwrap_err();
        assert!(err.contains("名称"));
    }

    #[test]
    fn into_spec_validates_empty_command_stdio() {
        let mut s = McpFormState::empty();
        s.name = "foo".into();
        let err = s.into_spec(&[]).unwrap_err();
        assert!(err.contains("command"));
    }

    #[test]
    fn into_spec_validates_empty_url_http() {
        let mut s = McpFormState::empty();
        s.name = "foo".into();
        s.transport_idx = 1; // http
        let err = s.into_spec(&[]).unwrap_err();
        assert!(err.contains("url"));
    }

    #[test]
    fn into_spec_rejects_duplicate_name_on_add() {
        let mut s = McpFormState::empty();
        s.name = "existing".into();
        s.command = "npx".into();
        let err = s.into_spec(&["existing"]).unwrap_err();
        assert!(err.contains("已存在"));
    }

    #[test]
    fn into_spec_allows_keep_name_on_edit() {
        let mut s = McpFormState::empty();
        s.name = "existing".into();
        s.command = "npx".into();
        s.original_name = Some("existing".into());
        let spec = s.into_spec(&["existing"]).unwrap();
        assert_eq!(spec.name, "existing");
    }

    #[test]
    fn into_spec_rejects_bad_timeout() {
        let mut s = McpFormState::empty();
        s.name = "foo".into();
        s.command = "npx".into();
        s.timeout_secs = "abc".into();
        let err = s.into_spec(&[]).unwrap_err();
        assert!(err.contains("timeout_secs"));
    }

    #[test]
    fn into_spec_stdio_builds_correctly() {
        let mut s = McpFormState::empty();
        s.name = "fs".into();
        s.command = "npx".into();
        s.args = "-y\n@modelcontextprotocol/server-filesystem\n.".into();
        s.env = "FOO=bar\nBAZ=qux".into();
        s.timeout_secs = "15".into();
        let spec = s.into_spec(&[]).unwrap();
        assert_eq!(spec.transport, McpTransport::Stdio);
        assert_eq!(spec.command.as_deref(), Some("npx"));
        assert_eq!(spec.args, vec!["-y", "@modelcontextprotocol/server-filesystem", "."]);
        assert_eq!(spec.env.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(spec.env.get("BAZ"), Some(&"qux".to_string()));
        assert!(spec.url.is_none());
        assert_eq!(spec.timeout_secs, 15);
    }

    #[test]
    fn into_spec_http_builds_correctly() {
        let mut s = McpFormState::empty();
        s.name = "remote".into();
        s.transport_idx = 1; // http
        s.url = "https://scanner.internal/mcp".into();
        s.headers = "Authorization=Bearer token".into();
        s.timeout_secs = "10".into();
        let spec = s.into_spec(&[]).unwrap();
        assert_eq!(spec.transport, McpTransport::Http);
        assert_eq!(spec.url.as_deref(), Some("https://scanner.internal/mcp"));
        assert_eq!(
            spec.headers.get("Authorization"),
            Some(&"Bearer token".to_string())
        );
        assert!(spec.command.is_none());
        assert!(spec.args.is_empty());
    }

    #[test]
    fn enter_text_field_starts_editing_enter_commits() {
        let mut s = McpFormState::empty();
        s.focused = 0; // name
        assert_eq!(s.handle_key(key(KeyCode::Enter)), McpFormAction::None);
        assert!(s.editing);
        s.handle_key(key(KeyCode::Char('z')));
        s.handle_key(key(KeyCode::Char('z')));
        s.handle_key(key(KeyCode::Enter));
        assert!(!s.editing);
        assert_eq!(s.name, "zz");
    }

    #[test]
    fn editing_esc_discards() {
        let mut s = McpFormState::empty();
        s.focused = 2; // command
        s.command = "old".into();
        s.handle_key(key(KeyCode::Enter)); // 进入编辑，load "old"
        s.handle_key(key(KeyCode::Char('X')));
        s.handle_key(key(KeyCode::Esc)); // 丢弃
        assert!(!s.editing);
        assert_eq!(s.command, "old", "Esc 应丢弃编辑");
    }

    #[test]
    fn save_button_returns_save_action() {
        let mut s = McpFormState::empty();
        s.focused = IDX_SAVE;
        assert_eq!(s.handle_key(key(KeyCode::Enter)), McpFormAction::Save);
    }

    #[test]
    fn cancel_button_and_esc_return_cancel() {
        let mut s = McpFormState::empty();
        s.focused = IDX_CANCEL;
        assert_eq!(s.handle_key(key(KeyCode::Enter)), McpFormAction::Cancel);
        s.focused = 0;
        assert_eq!(s.handle_key(key(KeyCode::Esc)), McpFormAction::Cancel);
    }

    #[test]
    fn stdio_fields_hidden_on_http() {
        let mut s = McpFormState::empty();
        s.transport_idx = 1; // http
        assert!(!s.is_field_visible(2)); // command
        assert!(!s.is_field_visible(3)); // args
        assert!(!s.is_field_visible(4)); // env
        assert!(s.is_field_visible(5)); // url
        assert!(s.is_field_visible(6)); // headers
    }

    #[test]
    fn http_fields_hidden_on_stdio() {
        let s = McpFormState::empty();
        assert!(s.is_field_visible(2)); // command
        assert!(s.is_field_visible(3)); // args
        assert!(s.is_field_visible(4)); // env
        assert!(!s.is_field_visible(5)); // url
        assert!(!s.is_field_visible(6)); // headers
    }

    #[test]
    fn navigation_skips_hidden_fields() {
        let mut s = McpFormState::empty();
        s.transport_idx = 1; // http
        s.focused = 1; // transport
        // Down 应跳过 command/args/env（2/3/4）直达 url（5）
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.focused, 5);
    }

    #[test]
    fn parse_map_rejects_missing_equals() {
        let result = parse_map("FOO=bar\nBADLINE");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("缺少"));
    }

    #[test]
    fn parse_map_skips_empty_lines() {
        let result = parse_map("FOO=bar\n\nBAZ=qux\n  ").unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(result.get("BAZ"), Some(&"qux".to_string()));
    }

    #[test]
    fn serialize_parse_roundtrip() {
        let mut map = std::collections::HashMap::new();
        map.insert("KEY1".into(), "val1".into());
        map.insert("KEY2".into(), "val2".into());
        let s = serialize_map(&map);
        let parsed = parse_map(&s).unwrap();
        assert_eq!(parsed, map);
    }

    #[test]
    fn render_form_does_not_panic() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut s = McpFormState::from_spec(&McpServerSpec {
            name: "test".into(),
            transport: McpTransport::Stdio,
            command: Some("npx".into()),
            args: vec!["-y".into(), "server".into()],
            env: Default::default(),
            url: None,
            headers: Default::default(),
            timeout_secs: 5,
        });
        s.prepare_render(&crate::theme::Theme::resolve("cyberpunk"));
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_form(f, f.area(), &crate::theme::Theme::resolve("cyberpunk"), &s))
            .unwrap();
    }

    #[test]
    fn render_form_http_does_not_panic() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut s = McpFormState::empty();
        s.name = "remote".into();
        s.transport_idx = 1; // http
        s.url = "https://example.com/mcp".into();
        s.prepare_render(&crate::theme::Theme::resolve("cyberpunk"));
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_form(f, f.area(), &crate::theme::Theme::resolve("cyberpunk"), &s))
            .unwrap();
    }
}
