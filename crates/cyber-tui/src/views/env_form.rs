//! Env var 表单模态层：新增 / 编辑单条环境变量。
//!
//! 从 Settings Env 段（`a`/`e`）进入，作为顶层 `Mode::EnvForm` 渲染。
//! 字段：key / value / sensitive（←→ 切换） + 保存 / 取消按钮。
//! 文本字段用单个复用 `TextArea`：Enter 进入编辑 → 输入 → Enter 提交 / Esc 取消。

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};
use tui_textarea::TextArea;

use cyber_core::EnvVar;

use crate::theme::Theme;

/// 字段类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    Text,
    Bool,
    Button,
}

struct FieldDef {
    label: &'static str,
    kind: FieldKind,
}

/// 字段顺序即焦点导航顺序（Up/Down 循环）。
/// 0=key 1=value 2=sensitive 3=保存 4=取消
const FIELDS: &[FieldDef] = &[
    FieldDef { label: "名称 key", kind: FieldKind::Text },
    FieldDef { label: "值 value", kind: FieldKind::Text },
    FieldDef { label: "敏感内容 sensitive", kind: FieldKind::Bool },
    FieldDef { label: "保存", kind: FieldKind::Button },
    FieldDef { label: "取消", kind: FieldKind::Button },
];
const IDX_KEY: usize = 0;
const IDX_VALUE: usize = 1;
const IDX_SENSITIVE: usize = 2;
const IDX_SAVE: usize = 3;
const IDX_CANCEL: usize = 4;

/// 表单按键的副作用意图，由 App 解释执行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvFormAction {
    None,
    Save,
    Cancel,
}

/// Env var 表单状态。
pub struct EnvFormState {
    pub key: String,
    pub value: String,
    pub sensitive: bool,
    /// 当前焦点字段索引。
    pub focused: usize,
    /// 是否正在文本编辑（Enter 进入 / Enter 提交 / Esc 取消）。
    pub editing: bool,
    /// 编辑模式：true = 编辑已有（保留 index），false = 新增。
    pub edit_index: Option<usize>,
    pub textarea: TextArea<'static>,
}

impl Default for EnvFormState {
    fn default() -> Self {
        let mut ta = TextArea::default();
        ta.set_cursor_line_style(Style::default());
        Self {
            key: String::new(),
            value: String::new(),
            sensitive: false,
            focused: 0,
            editing: false,
            edit_index: None,
            textarea: ta,
        }
    }
}

impl EnvFormState {
    /// 新增模式。
    pub fn for_add() -> Self {
        Self::default()
    }

    /// 编辑模式：从已有 EnvVar 装载。
    pub fn for_edit(idx: usize, var: &EnvVar) -> Self {
        Self {
            key: var.key.clone(),
            value: var.value.clone(),
            sensitive: var.sensitive,
            edit_index: Some(idx),
            ..Self::default()
        }
    }

    fn is_text_field(idx: usize) -> bool {
        matches!(idx, IDX_KEY | IDX_VALUE)
    }

    fn get_field(&self, idx: usize) -> String {
        match idx {
            IDX_KEY => self.key.clone(),
            IDX_VALUE => {
                if self.sensitive {
                    mask_value(&self.value)
                } else {
                    self.value.clone()
                }
            }
            IDX_SENSITIVE => self.sensitive.to_string(),
            _ => String::new(),
        }
    }

    fn start_editing(&mut self, idx: usize) {
        let val = if idx == IDX_VALUE && self.sensitive {
            // 编辑敏感值时显示明文
            self.value.clone()
        } else {
            self.get_field(idx)
        };
        self.textarea = TextArea::default();
        self.textarea.set_cursor_line_style(Style::default());
        self.textarea.insert_str(&val);
        self.editing = true;
    }

    /// 处理按键，返回副作用意图。
    pub fn handle_key(&mut self, k: KeyEvent) -> EnvFormAction {
        if self.editing {
            return self.handle_editing_key(k);
        }
        match k.code {
            KeyCode::Esc => EnvFormAction::Cancel,
            KeyCode::Up => {
                if self.focused == 0 {
                    self.focused = FIELDS.len() - 1;
                } else {
                    self.focused -= 1;
                }
                EnvFormAction::None
            }
            KeyCode::Down => {
                self.focused = (self.focused + 1) % FIELDS.len();
                EnvFormAction::None
            }
            KeyCode::Left | KeyCode::Right if self.focused == IDX_SENSITIVE => {
                self.sensitive = !self.sensitive;
                EnvFormAction::None
            }
            KeyCode::Enter => match self.focused {
                IDX_SAVE => EnvFormAction::Save,
                IDX_CANCEL => EnvFormAction::Cancel,
                IDX_SENSITIVE => {
                    self.sensitive = !self.sensitive;
                    EnvFormAction::None
                }
                idx if Self::is_text_field(idx) => {
                    self.start_editing(idx);
                    EnvFormAction::None
                }
                _ => EnvFormAction::None,
            },
            _ => EnvFormAction::None,
        }
    }

    fn handle_editing_key(&mut self, k: KeyEvent) -> EnvFormAction {
        match k.code {
            KeyCode::Esc => {
                self.editing = false;
                EnvFormAction::None
            }
            KeyCode::Enter => {
                let val = self.textarea.lines().join("\n");
                match self.focused {
                    IDX_KEY => self.key = val,
                    IDX_VALUE => self.value = val,
                    _ => {}
                }
                self.editing = false;
                EnvFormAction::None
            }
            _ => {
                self.textarea.input(k);
                EnvFormAction::None
            }
        }
    }

    /// 提交保存：构造 EnvVar 返回。
    pub fn build_var(&self) -> EnvVar {
        EnvVar {
            key: self.key.trim().to_string(),
            value: self.value.clone(),
            sensitive: self.sensitive,
        }
    }

    /// 校验：key 非空。
    pub fn is_valid(&self) -> bool {
        !self.key.trim().is_empty()
    }
}

/// 脱敏：保留前 2 + 后 3，中间 `****`。≤5 字符全掩码。
fn mask_value(val: &str) -> String {
    if val.len() <= 5 {
        "****".into()
    } else {
        let prefix: String = val.chars().take(2).collect();
        let suffix: String = val.chars().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect();
        format!("{prefix}****{suffix}")
    }
}

/// 渲染表单模态层。
pub fn render_form(frame: &mut Frame, area: Rect, theme: &Theme, state: &EnvFormState) {
    let title = if state.edit_index.is_some() {
        " 编辑环境变量 "
    } else {
        " 新增环境变量 "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Line::from(title).style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)))
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).split(inner);
    let form_area = chunks[0];
    let hint_area = chunks[1];

    let mut lines: Vec<Line> = Vec::new();
    for (i, f) in FIELDS.iter().enumerate() {
        if f.kind == FieldKind::Button {
            continue;
        }
        let selected = i == state.focused && !state.editing;
        let marker = if selected { "▸ " } else { "  " };
        let value: String = if i == IDX_SENSITIVE {
            if state.sensitive {
                "是  ←→".to_string()
            } else {
                "否  ←→".to_string()
            }
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

    // 按钮行
    lines.push(Line::from(""));
    let save_marker = if state.focused == IDX_SAVE { "▸ " } else { "  " };
    let cancel_marker = if state.focused == IDX_CANCEL { "▸ " } else { "  " };
    let save_style = if state.focused == IDX_SAVE {
        Style::default().bg(theme.sel_bg).fg(theme.sel_fg)
    } else {
        Style::default().fg(theme.fg)
    };
    let cancel_style = if state.focused == IDX_CANCEL {
        Style::default().bg(theme.sel_bg).fg(theme.sel_fg)
    } else {
        Style::default().fg(theme.fg)
    };
    lines.push(
        Line::from(vec![
            Span::styled(format!("{save_marker}保存"), save_style.add_modifier(Modifier::BOLD)),
            Span::raw("    "),
            Span::styled(format!("{cancel_marker}取消"), cancel_style.add_modifier(Modifier::BOLD)),
        ]),
    );

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg)),
        form_area,
    );

    // 底部提示
    let hint = if state.editing {
        " Enter 提交 · Esc 取消编辑"
    } else {
        " Enter 编辑字段 · ←→ 切换 sensitive · Esc 取消"
    };
    frame.render_widget(
        Paragraph::new(Line::from(hint)).style(Style::default().fg(theme.muted)),
        hint_area,
    );

    // 编辑中覆盖渲染 textarea
    if state.editing {
        let ta_area = Rect {
            x: form_area.x + 4,
            y: form_area.y + (state.focused as u16),
            width: form_area.width.saturating_sub(6),
            height: 1,
        };
        let mut ta = state.textarea.clone();
        ta.set_block(Block::default());
        ta.set_style(Style::default().fg(theme.title).bg(theme.sel_bg));
        frame.render_widget(&ta, ta_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn for_add_defaults() {
        let s = EnvFormState::for_add();
        assert!(s.key.is_empty());
        assert!(s.value.is_empty());
        assert!(!s.sensitive);
        assert!(s.edit_index.is_none());
    }

    #[test]
    fn for_edit_loads_values() {
        let var = EnvVar {
            key: "API_KEY".into(),
            value: "secret123".into(),
            sensitive: true,
        };
        let s = EnvFormState::for_edit(2, &var);
        assert_eq!(s.key, "API_KEY");
        assert_eq!(s.value, "secret123");
        assert!(s.sensitive);
        assert_eq!(s.edit_index, Some(2));
    }

    #[test]
    fn sensitive_toggles_via_left_right() {
        let mut s = EnvFormState::for_add();
        s.focused = IDX_SENSITIVE;
        assert!(!s.sensitive);
        s.handle_key(key(KeyCode::Right));
        assert!(s.sensitive);
        s.handle_key(key(KeyCode::Left));
        assert!(!s.sensitive);
    }

    #[test]
    fn sensitive_toggles_via_enter() {
        let mut s = EnvFormState::for_add();
        s.focused = IDX_SENSITIVE;
        s.handle_key(key(KeyCode::Enter));
        assert!(s.sensitive);
    }

    #[test]
    fn text_field_enter_starts_editing() {
        let mut s = EnvFormState::for_add();
        s.focused = IDX_KEY;
        s.handle_key(key(KeyCode::Enter));
        assert!(s.editing);
    }

    #[test]
    fn editing_enter_commits_value() {
        let mut s = EnvFormState::for_add();
        s.focused = IDX_KEY;
        s.handle_key(key(KeyCode::Enter));
        for ch in "MY_VAR".chars() {
            s.handle_key(key(KeyCode::Char(ch)));
        }
        s.handle_key(key(KeyCode::Enter));
        assert!(!s.editing);
        assert_eq!(s.key, "MY_VAR");
    }

    #[test]
    fn editing_esc_cancels() {
        let mut s = EnvFormState::for_add();
        s.focused = IDX_KEY;
        s.handle_key(key(KeyCode::Enter));
        s.handle_key(key(KeyCode::Esc));
        assert!(!s.editing);
        assert!(s.key.is_empty());
    }

    #[test]
    fn save_button_returns_save_action() {
        let mut s = EnvFormState::for_add();
        s.focused = IDX_SAVE;
        assert_eq!(s.handle_key(key(KeyCode::Enter)), EnvFormAction::Save);
    }

    #[test]
    fn cancel_button_returns_cancel_action() {
        let mut s = EnvFormState::for_add();
        s.focused = IDX_CANCEL;
        assert_eq!(s.handle_key(key(KeyCode::Enter)), EnvFormAction::Cancel);
    }

    #[test]
    fn esc_returns_cancel() {
        let mut s = EnvFormState::for_add();
        assert_eq!(s.handle_key(key(KeyCode::Esc)), EnvFormAction::Cancel);
    }

    #[test]
    fn build_var_trims_key() {
        let mut s = EnvFormState::for_add();
        s.key = "  API_KEY  ".into();
        s.value = "secret".into();
        s.sensitive = true;
        let v = s.build_var();
        assert_eq!(v.key, "API_KEY");
        assert_eq!(v.value, "secret");
        assert!(v.sensitive);
    }

    #[test]
    fn is_valid_requires_non_empty_key() {
        let mut s = EnvFormState::for_add();
        assert!(!s.is_valid());
        s.key = "  ".into();
        assert!(!s.is_valid());
        s.key = "KEY".into();
        assert!(s.is_valid());
    }

    #[test]
    fn up_down_cycles_focus() {
        let mut s = EnvFormState::for_add();
        assert_eq!(s.focused, IDX_KEY);
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.focused, IDX_VALUE);
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.focused, IDX_SENSITIVE);
        s.handle_key(key(KeyCode::Up));
        assert_eq!(s.focused, IDX_VALUE);
        // 从首项 Up 回绕到末项（取消按钮）
        s.focused = IDX_KEY;
        s.handle_key(key(KeyCode::Up));
        assert_eq!(s.focused, IDX_CANCEL);
    }

    #[test]
    fn mask_value_short_string() {
        assert_eq!(mask_value("abc"), "****");
        assert_eq!(mask_value(""), "****");
    }

    #[test]
    fn mask_value_long_string() {
        assert_eq!(mask_value("sk-abcdef123"), "sk****123");
        assert_eq!(mask_value("secretkey"), "se****key");
    }
}
