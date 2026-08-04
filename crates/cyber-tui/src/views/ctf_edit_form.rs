//! CTF 题目编辑表单模态层：编辑单道 CTF 题目的字段。
//!
//! 从 CTF 面板列表视图按 `e` 进入，作为顶层 `Mode::CtfEditForm` 渲染。
//! 字段：name / category（←→ 循环）/ status（←→ 循环）/ description / target /
//! flag / tags（空格分隔）/ key_points / is_global（←→ 循环）+ 保存 / 取消按钮。
//!
//! 文本字段用单个复用 `TextArea`：Enter 进入编辑（load 值）→ 输入 → Enter 提交 / Esc 取消。

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};
use tui_textarea::TextArea;

use cyber_core::{current_time_str, CtfCategory, CtfChallenge, CtfStatus};

use crate::theme::Theme;

/// category 选项（←→ 循环）。
const CATEGORIES: &[CtfCategory] = &[
    CtfCategory::Misc,
    CtfCategory::Web,
    CtfCategory::Reverse,
    CtfCategory::Pwn,
    CtfCategory::Crypto,
];

/// status 选项（←→ 循环）。
const STATUSES: &[CtfStatus] = &[CtfStatus::InProgress, CtfStatus::Solved];

/// is_global 选项（←→ 循环）。
const GLOBAL_OPTIONS: &[bool] = &[false, true];

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
/// 0=name 1=category 2=status 3=description 4=target 5=flag 6=tags 7=key_points 8=is_global 9=保存 10=取消
const FIELDS: &[FieldDef] = &[
    FieldDef { label: "名称 name", kind: FieldKind::Text },
    FieldDef { label: "分类 category", kind: FieldKind::Enum },
    FieldDef { label: "状态 status", kind: FieldKind::Enum },
    FieldDef { label: "描述 description", kind: FieldKind::Text },
    FieldDef { label: "靶机 target", kind: FieldKind::Text },
    FieldDef { label: "Flag", kind: FieldKind::Text },
    FieldDef { label: "标签 tags (空格分隔)", kind: FieldKind::Text },
    FieldDef { label: "关键知识点 key_points", kind: FieldKind::Text },
    FieldDef { label: "范围 scope", kind: FieldKind::Enum },
    FieldDef { label: "保存", kind: FieldKind::Button },
    FieldDef { label: "取消", kind: FieldKind::Button },
];
const IDX_CATEGORY: usize = 1;
const IDX_STATUS: usize = 2;
const IDX_GLOBAL: usize = 8;
const IDX_SAVE: usize = 9;
const IDX_CANCEL: usize = 10;

/// 表单按键的副作用意图，由 App 解释执行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtfEditFormAction {
    None,
    Save,
    Cancel,
}

/// CTF 题目编辑表单状态。
pub struct CtfEditFormState {
    pub name: String,
    pub category_idx: usize,
    pub status_idx: usize,
    pub description: String,
    pub target: String,
    pub flag: String,
    pub tags: String,
    pub key_points: String,
    pub global_idx: usize,
    /// 编辑目标的题目 ID（用于回写）。
    pub challenge_id: String,
    pub focused: usize,
    pub editing: bool,
    pub textarea: TextArea<'static>,
}

impl CtfEditFormState {
    /// 从现有题目装载编辑表单。
    pub fn from_challenge(c: &CtfChallenge) -> Self {
        let category_idx = CATEGORIES
            .iter()
            .position(|&cat| cat == c.category)
            .unwrap_or(0);
        let status_idx = STATUSES
            .iter()
            .position(|&s| s == c.status)
            .unwrap_or(0);
        let global_idx = GLOBAL_OPTIONS
            .iter()
            .position(|&g| g == c.is_global)
            .unwrap_or(0);
        let mut textarea = TextArea::default();
        textarea.set_placeholder_text("输入…");
        Self {
            name: c.name.clone(),
            category_idx,
            status_idx,
            description: c.description.clone(),
            target: c.target.clone().unwrap_or_default(),
            flag: c.flag.clone().unwrap_or_default(),
            tags: c.tags.join(" "),
            key_points: c.key_points.clone().unwrap_or_default(),
            global_idx,
            challenge_id: c.id.clone(),
            focused: 0,
            editing: false,
            textarea,
        }
    }

    pub fn category(&self) -> CtfCategory {
        CATEGORIES[self.category_idx]
    }

    pub fn status(&self) -> CtfStatus {
        STATUSES[self.status_idx]
    }

    pub fn is_global(&self) -> bool {
        GLOBAL_OPTIONS[self.global_idx]
    }

    /// 将表单值应用到一个题目（返回新题目副本）。
    pub fn apply_to(&self, mut c: CtfChallenge) -> CtfChallenge {
        c.name = self.name.clone();
        c.category = self.category();
        let was_solved = c.is_solved();
        let now_solved = self.status() == CtfStatus::Solved;
        c.status = self.status();
        c.description = self.description.clone();
        c.target = if self.target.is_empty() {
            None
        } else {
            Some(self.target.clone())
        };
        c.flag = if self.flag.is_empty() {
            None
        } else {
            Some(self.flag.clone())
        };
        c.tags = self
            .tags
            .split_whitespace()
            .map(String::from)
            .collect();
        c.key_points = if self.key_points.is_empty() {
            None
        } else {
            Some(self.key_points.clone())
        };
        c.is_global = self.is_global();
        // 状态变化时维护 end_time
        if !was_solved && now_solved {
            c.end_time = Some(current_time_str());
        } else if was_solved && !now_solved {
            c.end_time = None;
        }
        c
    }

    fn is_text_field(idx: usize) -> bool {
        matches!(
            idx,
            0 | 3 | 4 | 5 | 6 | 7
        )
    }

    fn get_field(&self, idx: usize) -> String {
        match idx {
            0 => self.name.clone(),
            1 => self.category().label().to_string(),
            2 => self.status().label().to_string(),
            3 => self.description.clone(),
            4 => self.target.clone(),
            5 => self.flag.clone(),
            6 => self.tags.clone(),
            7 => self.key_points.clone(),
            8 => {
                if self.is_global() {
                    "全局 ★".to_string()
                } else {
                    "仅本 session".to_string()
                }
            }
            _ => String::new(),
        }
    }

    fn set_field(&mut self, idx: usize, val: String) {
        match idx {
            0 => self.name = val,
            3 => self.description = val,
            4 => self.target = val,
            5 => self.flag = val,
            6 => self.tags = val,
            7 => self.key_points = val,
            _ => {}
        }
    }

    fn start_editing(&mut self, idx: usize) {
        let val = self.get_field(idx);
        self.textarea.clear();
        self.textarea.insert_str(&val);
        self.editing = true;
    }

    /// 处理一个按键，返回副作用意图。
    pub fn handle_key(&mut self, k: KeyEvent) -> CtfEditFormAction {
        if self.editing {
            return self.handle_editing_key(k);
        }
        match k.code {
            KeyCode::Up => {
                if self.focused == 0 {
                    self.focused = FIELDS.len() - 1;
                } else {
                    self.focused -= 1;
                }
                CtfEditFormAction::None
            }
            KeyCode::Down => {
                self.focused = (self.focused + 1) % FIELDS.len();
                CtfEditFormAction::None
            }
            KeyCode::Left => {
                match self.focused {
                    IDX_CATEGORY => {
                        self.category_idx =
                            (self.category_idx + CATEGORIES.len() - 1) % CATEGORIES.len();
                    }
                    IDX_STATUS => {
                        self.status_idx =
                            (self.status_idx + STATUSES.len() - 1) % STATUSES.len();
                    }
                    IDX_GLOBAL => {
                        self.global_idx =
                            (self.global_idx + GLOBAL_OPTIONS.len() - 1) % GLOBAL_OPTIONS.len();
                    }
                    _ => {}
                }
                CtfEditFormAction::None
            }
            KeyCode::Right => {
                match self.focused {
                    IDX_CATEGORY => {
                        self.category_idx = (self.category_idx + 1) % CATEGORIES.len();
                    }
                    IDX_STATUS => {
                        self.status_idx = (self.status_idx + 1) % STATUSES.len();
                    }
                    IDX_GLOBAL => {
                        self.global_idx = (self.global_idx + 1) % GLOBAL_OPTIONS.len();
                    }
                    _ => {}
                }
                CtfEditFormAction::None
            }
            KeyCode::Enter => {
                match self.focused {
                    IDX_SAVE => CtfEditFormAction::Save,
                    IDX_CANCEL => CtfEditFormAction::Cancel,
                    idx if Self::is_text_field(idx) => {
                        self.start_editing(idx);
                        CtfEditFormAction::None
                    }
                    _ => CtfEditFormAction::None,
                }
            }
            KeyCode::Esc => CtfEditFormAction::Cancel,
            _ => CtfEditFormAction::None,
        }
    }

    fn handle_editing_key(&mut self, k: KeyEvent) -> CtfEditFormAction {
        match k.code {
            KeyCode::Enter => {
                let val = self.textarea.lines().join("\n");
                self.set_field(self.focused, val);
                self.editing = false;
                CtfEditFormAction::None
            }
            KeyCode::Esc => {
                self.editing = false;
                CtfEditFormAction::None
            }
            _ => {
                self.textarea.input(k);
                CtfEditFormAction::None
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
pub fn render_form(frame: &mut Frame, area: Rect, theme: &Theme, state: &CtfEditFormState) {
    let modal = centered_rect(72, 82, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(
            Line::from(format!(" 编辑题目: {} ", state.name))
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

fn render_fields(frame: &mut Frame, area: Rect, theme: &Theme, state: &CtfEditFormState) {
    let mut lines: Vec<Line> = Vec::new();
    for (i, f) in FIELDS.iter().enumerate() {
        if f.kind == FieldKind::Button {
            continue;
        }
        let selected = i == state.focused && !state.editing;
        let marker = if selected { "▸ " } else { "  " };
        let value: String = match i {
            IDX_CATEGORY => format!("{}  ←→", state.category().label()),
            IDX_STATUS => format!("{}  ←→", state.status().label()),
            IDX_GLOBAL => {
                if state.is_global() {
                    "全局 ★  ←→".to_string()
                } else {
                    "仅本 session  ←→".to_string()
                }
            }
            _ => state.get_field(i),
        };
        let editing_marker = if selected && state.editing { " [编辑中]" } else { "" };
        let row_style = if selected {
            Style::default().bg(theme.sel_bg)
        } else {
            Style::default().bg(theme.bg)
        };
        // 多行字段（description/key_points）只显示首行 + 计数
        let display_value = if matches!(i, 3 | 7) && value.contains('\n') {
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

fn render_editor(frame: &mut Frame, area: Rect, theme: &Theme, state: &CtfEditFormState) {
    if state.editing {
        frame.render_widget(&state.textarea, area);
        return;
    }
    let hint = " Enter 进入编辑 · Esc 取消 ";
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(theme.muted)),
        area,
    );
}

fn render_buttons(frame: &mut Frame, area: Rect, theme: &Theme, state: &CtfEditFormState) {
    let save_marker = if state.focused == IDX_SAVE && !state.editing {
        "▸ [保存]"
    } else {
        "  [保存]"
    };
    let cancel_marker = if state.focused == IDX_CANCEL && !state.editing {
        "▸ [取消]"
    } else {
        "  [取消]"
    };
    let line = Line::from(vec![
        Span::styled(
            save_marker,
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw("    "),
        Span::styled(cancel_marker, Style::default().fg(theme.fg)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// 居中矩形（width/height 为百分比）。
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::vertical([Constraint::Percentage(percent_y)]).split(area);
    Layout::horizontal([Constraint::Percentage(percent_x)]).split(popup_layout[0])[0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn make_challenge() -> CtfChallenge {
        let mut c = CtfChallenge::new("test-challenge".into(), CtfCategory::Web);
        c.description = "A test".into();
        c.target = Some("nc 1.2.3.4 1234".into());
        c.flag = Some("flag{test}".into());
        c.tags = vec!["web".into(), "sql".into()];
        c.key_points = Some("SQL injection".into());
        c
    }

    #[test]
    fn render_form_no_panic() {
        let theme = Theme::resolve("cyberpunk");
        let c = make_challenge();
        let state = CtfEditFormState::from_challenge(&c);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render_form(f, f.area(), &theme, &state))
            .unwrap();
    }

    #[test]
    fn apply_to_roundtrip() {
        let c = make_challenge();
        let state = CtfEditFormState::from_challenge(&c);
        let applied = state.apply_to(c.clone());
        assert_eq!(applied.name, c.name);
        assert_eq!(applied.category, c.category);
        assert_eq!(applied.description, c.description);
        assert_eq!(applied.target, c.target);
        assert_eq!(applied.flag, c.flag);
        assert_eq!(applied.tags, c.tags);
        assert_eq!(applied.key_points, c.key_points);
    }

    #[test]
    fn enter_on_text_field_starts_editing() {
        let c = make_challenge();
        let mut s = CtfEditFormState::from_challenge(&c);
        // focused=0 (name) is text → Enter starts editing
        assert_eq!(s.handle_key(key(KeyCode::Enter)), CtfEditFormAction::None);
        assert!(s.editing);
    }

    #[test]
    fn save_button_returns_save() {
        let c = make_challenge();
        let mut s = CtfEditFormState::from_challenge(&c);
        s.focused = IDX_SAVE;
        assert_eq!(s.handle_key(key(KeyCode::Enter)), CtfEditFormAction::Save);
    }

    #[test]
    fn cancel_button_returns_cancel() {
        let c = make_challenge();
        let mut s = CtfEditFormState::from_challenge(&c);
        s.focused = IDX_CANCEL;
        assert_eq!(s.handle_key(key(KeyCode::Enter)), CtfEditFormAction::Cancel);
    }

    #[test]
    fn esc_returns_cancel() {
        let c = make_challenge();
        let mut s = CtfEditFormState::from_challenge(&c);
        assert_eq!(s.handle_key(key(KeyCode::Esc)), CtfEditFormAction::Cancel);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }
}
