//! 模型思考交互式提问模式 (AskUser / Clarify) UI 交互组件。
//!
//! 提供固定渲染在输入框正上方的结构化提问面板、选项光标聚焦、
//! 数字快捷键 (1-9)、多选题勾选、自定义文本编辑与回车提交。

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use cyber_agent::{QuestionAnswer, QuestionItem, QuestionRequest, QuestionResponse};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use tui_textarea::TextArea;

/// 交互处理结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionUiResult {
    /// 状态更新，继续保持提问交互界面
    Continue,
    /// 用户确认完成全部问题作答，提交结果
    Submit(QuestionResponse),
    /// 用户跳过或取消提问交互
    Cancel,
}

/// 提问组件交互状态机
pub struct QuestionUiState {
    /// 当前提问请求数据
    pub request: QuestionRequest,
    /// 当前处于第几题 (0..questions.len())
    pub active_idx: usize,
    /// 每道题选中的选项下标集合 (question_idx -> Set<option_idx>)
    pub selections: HashMap<usize, HashSet<usize>>,
    /// 每道题光标停留的行下标 (0..options.len() 为选项，options.len() 为自定义输入行)
    pub focused: HashMap<usize, usize>,
    /// 每道题用户手输的自定义补充文本
    pub custom_inputs: HashMap<usize, String>,
    /// 是否正处于自定义文本编辑模式
    pub custom_editing: bool,
    /// 自定义文本输入 textarea
    pub custom_textarea: TextArea<'static>,
}

impl QuestionUiState {
    /// 基于提问请求构造初始状态，自动预选每道题的推荐选项
    pub fn new(request: QuestionRequest) -> Self {
        let mut selections = HashMap::new();
        let mut focused = HashMap::new();
        let mut custom_inputs = HashMap::new();

        for (q_idx, q) in request.questions.iter().enumerate() {
            custom_inputs.insert(q_idx, String::new());

            let mut selected_set = HashSet::new();
            let mut first_rec = None;

            for (opt_idx, opt) in q.options.iter().enumerate() {
                if opt.recommended {
                    if first_rec.is_none() {
                        first_rec = Some(opt_idx);
                    }
                    if q.multi {
                        selected_set.insert(opt_idx);
                    }
                }
            }

            // 单选时若有推荐项，预选首个推荐项
            if !q.multi {
                if let Some(rec) = first_rec {
                    selected_set.insert(rec);
                }
            }

            focused.insert(q_idx, first_rec.unwrap_or(0));
            selections.insert(q_idx, selected_set);
        }

        let mut custom_textarea = TextArea::default();
        custom_textarea.set_placeholder_text("输入自定义补充说明，按 Enter 保存...");
        custom_textarea.set_cursor_line_style(Style::default());

        Self {
            request,
            active_idx: 0,
            selections,
            focused,
            custom_inputs,
            custom_editing: false,
            custom_textarea,
        }
    }

    /// 当前激活的题目对象
    pub fn current_question(&self) -> Option<&QuestionItem> {
        self.request.questions.get(self.active_idx)
    }

    /// 当前题目选项数量
    pub fn current_options_len(&self) -> usize {
        self.current_question().map_or(0, |q| q.options.len())
    }

    /// 当前题目是否允许多选
    pub fn current_is_multi(&self) -> bool {
        self.current_question().is_some_and(|q| q.multi)
    }

    /// 当前题目光标行
    pub fn current_focused(&self) -> usize {
        *self.focused.get(&self.active_idx).unwrap_or(&0)
    }

    /// 设置当前题目光标行
    pub fn set_focused(&mut self, row: usize) {
        self.focused.insert(self.active_idx, row);
    }

    /// 单选题设置唯一选中
    pub fn set_single_selection(&mut self, q_idx: usize, opt_idx: usize) {
        let mut set = HashSet::new();
        set.insert(opt_idx);
        self.selections.insert(q_idx, set);
    }

    /// 多选题切换选中状态
    pub fn toggle_selection(&mut self, q_idx: usize, opt_idx: usize) {
        let set = self.selections.entry(q_idx).or_default();
        if set.contains(&opt_idx) {
            set.remove(&opt_idx);
        } else {
            set.insert(opt_idx);
        }
    }

    /// 进入自定义输入编辑模式
    pub fn start_custom_editing(&mut self) {
        self.custom_editing = true;
        let existing = self
            .custom_inputs
            .get(&self.active_idx)
            .cloned()
            .unwrap_or_default();
        self.custom_textarea = TextArea::default();
        self.custom_textarea.set_cursor_line_style(Style::default());
        if !existing.is_empty() {
            self.custom_textarea.insert_str(&existing);
        }
    }

    /// 退出自定义输入编辑模式并保存文本
    pub fn finish_custom_editing(&mut self) {
        let text = self.custom_textarea.lines().join(" ");
        self.custom_inputs
            .insert(self.active_idx, text.trim().to_string());
        self.custom_editing = false;
    }

    /// 切换到下一题
    pub fn next_question(&mut self) {
        if self.custom_editing {
            self.finish_custom_editing();
        }
        let total = self.request.questions.len();
        if total > 0 {
            self.active_idx = (self.active_idx + 1) % total;
        }
    }

    /// 切换到上一题
    pub fn prev_question(&mut self) {
        if self.custom_editing {
            self.finish_custom_editing();
        }
        let total = self.request.questions.len();
        if total > 0 {
            self.active_idx = if self.active_idx == 0 {
                total - 1
            } else {
                self.active_idx - 1
            };
        }
    }

    /// 计算输入框上方所需的垂直高度
    pub fn height_needed(&self) -> u16 {
        if self.request.questions.is_empty() {
            return 0;
        }
        let q = match self.current_question() {
            Some(q) => q,
            None => return 0,
        };
        let options_len = q.options.len() as u16;
        let cur_f = self.current_focused();
        let has_desc = cur_f < q.options.len()
            && q.options
                .get(cur_f)
                .and_then(|o| o.description.as_deref())
                .is_some();

        // 边框(2) + 题干(1) + 选项(options_len) + 自定义行(1) + 底部提示(1) + 说明展开(has_desc ? 1 : 0)
        let total = options_len + 5 + if has_desc { 1 } else { 0 };
        total.clamp(6, 12)
    }

    /// 收集全部答复数据
    pub fn collect_response(&self) -> QuestionResponse {
        let answers = self
            .request
            .questions
            .iter()
            .enumerate()
            .map(|(q_idx, q)| {
                let selected_indices = self.selections.get(&q_idx);
                let selected = match selected_indices {
                    Some(set) => {
                        let mut labels = Vec::new();
                        for (opt_idx, opt) in q.options.iter().enumerate() {
                            if set.contains(&opt_idx) {
                                labels.push(opt.label.clone());
                            }
                        }
                        labels
                    }
                    None => Vec::new(),
                };

                let custom = self
                    .custom_inputs
                    .get(&q_idx)
                    .cloned()
                    .filter(|s| !s.trim().is_empty());

                QuestionAnswer {
                    id: q.id.clone(),
                    question: q.question.clone(),
                    selected,
                    custom,
                }
            })
            .collect();

        QuestionResponse {
            answers,
            cancelled: false,
        }
    }

    /// 键盘事件分流处理
    pub fn handle_key(&mut self, key: KeyEvent) -> QuestionUiResult {
        if key.code == KeyCode::Esc {
            if self.custom_editing {
                self.custom_editing = false;
                return QuestionUiResult::Continue;
            } else {
                return QuestionUiResult::Cancel;
            }
        }

        // 自定义输入编辑态
        if self.custom_editing {
            match key.code {
                KeyCode::Enter => {
                    self.finish_custom_editing();
                    return QuestionUiResult::Continue;
                }
                KeyCode::Tab => {
                    self.finish_custom_editing();
                    self.next_question();
                    return QuestionUiResult::Continue;
                }
                _ => {
                    self.custom_textarea.input(key);
                    return QuestionUiResult::Continue;
                }
            }
        }

        let total_questions = self.request.questions.len();
        let total_opts = self.current_options_len();
        let total_rows = total_opts + 1; // 选项 + 自定义行

        match key.code {
            // 快速数字键 1..=9 选中
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let num = c.to_digit(10).unwrap() as usize;
                if num >= 1 && num <= total_opts {
                    let opt_idx = num - 1;
                    if self.current_is_multi() {
                        self.toggle_selection(self.active_idx, opt_idx);
                        self.set_focused(opt_idx);
                    } else {
                        self.set_single_selection(self.active_idx, opt_idx);
                        self.set_focused(opt_idx);
                        // 单选题且还有后续题目，自动跳至下一题
                        if self.active_idx + 1 < total_questions {
                            self.active_idx += 1;
                        }
                    }
                }
                QuestionUiResult::Continue
            }

            // 上下移动光标
            KeyCode::Up | KeyCode::Char('k') => {
                let cur = self.current_focused();
                let prev = if cur == 0 { total_rows - 1 } else { cur - 1 };
                self.set_focused(prev);
                QuestionUiResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let cur = self.current_focused();
                let next = (cur + 1) % total_rows;
                self.set_focused(next);
                QuestionUiResult::Continue
            }

            // 空格键勾选
            KeyCode::Char(' ') => {
                let cur = self.current_focused();
                if cur < total_opts {
                    if self.current_is_multi() {
                        self.toggle_selection(self.active_idx, cur);
                    } else {
                        self.set_single_selection(self.active_idx, cur);
                    }
                } else {
                    // 自定义输入行
                    self.start_custom_editing();
                }
                QuestionUiResult::Continue
            }

            // 题目切换
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.next_question();
                QuestionUiResult::Continue
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.prev_question();
                QuestionUiResult::Continue
            }

            // 自定义输入快捷键 'c' 或 'e'
            KeyCode::Char('c') | KeyCode::Char('e') => {
                self.set_focused(total_opts);
                self.start_custom_editing();
                QuestionUiResult::Continue
            }

            // 回车键确认 / 下一题 / 提交
            KeyCode::Enter => {
                let cur = self.current_focused();
                if cur < total_opts && !self.current_is_multi() {
                    // 单选题回车未勾选时补全选中
                    self.set_single_selection(self.active_idx, cur);
                } else if cur == total_opts {
                    let has_custom = self
                        .custom_inputs
                        .get(&self.active_idx)
                        .is_some_and(|s| !s.trim().is_empty());
                    if !has_custom {
                        // 尚未填写自定义内容时按 Enter 激活输入
                        self.start_custom_editing();
                        return QuestionUiResult::Continue;
                    }
                }

                // 若有后续题目，跳至下一题；若是最后一题，提交
                if self.active_idx + 1 < total_questions {
                    self.active_idx += 1;
                    QuestionUiResult::Continue
                } else {
                    let resp = self.collect_response();
                    QuestionUiResult::Submit(resp)
                }
            }

            _ => QuestionUiResult::Continue,
        }
    }

    /// 鼠标点击处理
    pub fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> QuestionUiResult {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return QuestionUiResult::Continue;
        }

        if mouse.column < area.x
            || mouse.column >= area.right()
            || mouse.row < area.y
            || mouse.row >= area.bottom()
        {
            return QuestionUiResult::Continue;
        }

        let rel_y = mouse.row.saturating_sub(area.y);
        let total_opts = self.current_options_len();

        // 标题行/Tab 切换区域
        if rel_y == 0 && self.request.questions.len() > 1 {
            self.next_question();
            return QuestionUiResult::Continue;
        }

        // 选项行起点为 y=2 (0: 边框, 1: 题干)
        if rel_y >= 2 {
            let row_idx = (rel_y - 2) as usize;
            if row_idx < total_opts {
                self.set_focused(row_idx);
                if self.current_is_multi() {
                    self.toggle_selection(self.active_idx, row_idx);
                } else {
                    self.set_single_selection(self.active_idx, row_idx);
                }
                return QuestionUiResult::Continue;
            } else if row_idx == total_opts {
                self.set_focused(total_opts);
                self.start_custom_editing();
                return QuestionUiResult::Continue;
            }
        }

        // 底部确认栏点击直接提交
        if rel_y == area.height.saturating_sub(1) {
            let resp = self.collect_response();
            return QuestionUiResult::Submit(resp);
        }

        QuestionUiResult::Continue
    }
}

/// 渲染固定于输入框正上方的需求提问与确认卡片
pub fn render_question_box(
    frame: &mut Frame,
    area: Rect,
    state: &QuestionUiState,
    theme_accent: Option<Color>,
) {
    if area.height < 4 || area.width < 10 || state.request.questions.is_empty() {
        return;
    }

    frame.render_widget(Clear, area);

    let border_color = theme_accent.unwrap_or(Color::Rgb(254, 188, 56));
    let total_q = state.request.questions.len();
    let cur_q_num = state.active_idx + 1;

    let title_line = if total_q > 1 {
        Line::from(vec![
            Span::styled(
                " [?] 需求澄清 / 决策确认 ",
                Style::default()
                    .fg(border_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("(第 {cur_q_num}/{total_q} 题) "),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("· [Tab] 切换下一题", Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(vec![Span::styled(
            " [?] 需求澄清 / 决策确认 ",
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        )])
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(title_line);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let q = match state.current_question() {
        Some(q) => q,
        None => return,
    };

    let mut lines = Vec::new();

    // 1. 题干行
    let mut question_spans = Vec::new();
    if let Some(header) = &q.header {
        question_spans.push(Span::styled(
            format!("[{header}] "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }
    question_spans.push(Span::styled(
        format!("? {}", q.question),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));
    if q.multi {
        question_spans.push(Span::styled(
            " (可多选)",
            Style::default()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::ITALIC),
        ));
    } else {
        question_spans.push(Span::styled(
            " (单选)",
            Style::default().fg(Color::DarkGray),
        ));
    }
    lines.push(Line::from(question_spans));

    // 2. 选项行
    let selections = state.selections.get(&state.active_idx);
    let cur_focused = state.current_focused();

    for (opt_idx, opt) in q.options.iter().enumerate() {
        let is_selected = selections.is_some_and(|s| s.contains(&opt_idx));
        let is_focused = cur_focused == opt_idx;

        let pointer = if is_focused { " ▶ " } else { "   " };

        let mark = if q.multi {
            if is_selected {
                "[✔] "
            } else {
                "[ ] "
            }
        } else {
            if is_selected {
                "● "
            } else {
                "○ "
            }
        };

        let num_str = format!("{}. ", opt_idx + 1);

        let label_style = if is_selected {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else if is_focused {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let pointer_style = if is_focused {
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let mut row_spans = vec![
            Span::styled(pointer, pointer_style),
            Span::styled(
                mark,
                if is_selected {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(num_str, Style::default().fg(Color::DarkGray)),
            Span::styled(&opt.label, label_style),
        ];

        if opt.recommended {
            row_spans.push(Span::styled(
                "  [★ 推荐]",
                Style::default()
                    .fg(Color::Rgb(254, 188, 56))
                    .add_modifier(Modifier::BOLD),
            ));
        }

        lines.push(Line::from(row_spans));

        // 聚焦行下挂展示选项说明 (description)
        if is_focused {
            if let Some(desc) = &opt.description {
                lines.push(Line::from(vec![
                    Span::raw("       "),
                    Span::styled(format!("└─ {desc}"), Style::default().fg(Color::DarkGray)),
                ]));
            }
        }
    }

    // 3. 自定义输入行
    let is_custom_focused = cur_focused == q.options.len();
    let pointer = if is_custom_focused { " ▶ " } else { "   " };
    let pointer_style = if is_custom_focused {
        Style::default()
            .fg(border_color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let custom_spans = if state.custom_editing {
        let cur_text = state.custom_textarea.lines().join(" ");
        vec![
            Span::styled(pointer, pointer_style),
            Span::styled("○ ", Style::default().fg(Color::Cyan)),
            Span::styled(
                format!("[✍ 自定义输入: {cur_text}█] (按 Enter 完成)"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]
    } else {
        let custom_val = state
            .custom_inputs
            .get(&state.active_idx)
            .filter(|s| !s.trim().is_empty());

        if let Some(val) = custom_val {
            vec![
                Span::styled(pointer, pointer_style),
                Span::styled("● ", Style::default().fg(Color::Green)),
                Span::styled(
                    format!("[✍ 自定义输入: \"{val}\"] (按 c 修改)"),
                    Style::default().fg(Color::Green),
                ),
            ]
        } else {
            vec![
                Span::styled(pointer, pointer_style),
                Span::styled("○ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "[✍ 自定义输入 (按 c 编辑)]",
                    if is_custom_focused {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
            ]
        }
    };
    lines.push(Line::from(custom_spans));

    // 4. 底部快捷操作栏（若高度允许）
    if inner.height > lines.len() as u16 {
        lines.push(Line::from(vec![Span::styled(
            "1-9: 快捷选中 · ↑↓: 聚焦 · Space: 勾选 · Tab: 换题 · Enter: 确认 · Esc: 取消",
            Style::default().fg(Color::DarkGray),
        )]));
    }

    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Left), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use cyber_agent::{QuestionItem, QuestionOption};
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Constraint, Layout};
    use tokio::sync::oneshot;

    fn key_press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn make_test_request() -> QuestionRequest {
        let (reply, _) = oneshot::channel();
        QuestionRequest {
            id: "test-req".into(),
            questions: vec![
                QuestionItem {
                    id: "auth".into(),
                    header: Some("认证方案".into()),
                    question: "请选择用户认证方案".into(),
                    options: vec![
                        QuestionOption {
                            label: "JWT Token".into(),
                            description: Some("无状态分布式".into()),
                            recommended: true,
                        },
                        QuestionOption {
                            label: "Session Cookie".into(),
                            description: Some("传统集中式会话".into()),
                            recommended: false,
                        },
                        QuestionOption {
                            label: "OAuth 2.0".into(),
                            description: Some("第三方授权".into()),
                            recommended: false,
                        },
                    ],
                    multi: false,
                },
                QuestionItem {
                    id: "features".into(),
                    header: Some("功能模块".into()),
                    question: "请勾选附加功能".into(),
                    options: vec![
                        QuestionOption {
                            label: "审计日志".into(),
                            description: None,
                            recommended: true,
                        },
                        QuestionOption {
                            label: "速率限制".into(),
                            description: None,
                            recommended: false,
                        },
                    ],
                    multi: true,
                },
            ],
            reply,
        }
    }

    #[test]
    fn test_question_ui_key_navigation_and_selection() {
        let req = make_test_request();
        let mut state = QuestionUiState::new(req);

        // 初始状态验证：自动预选推荐项 JWT (0)，处于第 0 题
        assert_eq!(state.active_idx, 0);
        assert_eq!(state.current_focused(), 0);
        assert!(state.selections.get(&0).unwrap().contains(&0));

        // 按数字键 2 选中第 2 个选项 (Session Cookie, 下标 1)
        // 单选切换后自动跳至下一题 (第 1 题)
        let res = state.handle_key(key_press(KeyCode::Char('2')));
        assert_eq!(res, QuestionUiResult::Continue);
        assert_eq!(state.active_idx, 1);
        assert!(state.selections.get(&0).unwrap().contains(&1));
        assert!(!state.selections.get(&0).unwrap().contains(&0));

        // 第 1 题 (多选)：初始已预选推荐项 审计日志 (0)
        assert!(state.selections.get(&1).unwrap().contains(&0));
        // 按 2 切换勾选 速率限制 (1)
        state.handle_key(key_press(KeyCode::Char('2')));
        assert!(state.selections.get(&1).unwrap().contains(&1));
        assert!(state.selections.get(&1).unwrap().contains(&0));

        // 按上下方向键移动光标
        state.handle_key(key_press(KeyCode::Down));
        // 停在自定义输入行 (下标 2)
        assert_eq!(state.current_focused(), 2);

        // 按 'c' 进入自定义输入编辑模式
        state.handle_key(key_press(KeyCode::Char('c')));
        assert!(state.custom_editing);

        // 输入文本
        state.handle_key(key_press(KeyCode::Char('o')));
        state.handle_key(key_press(KeyCode::Char('k')));
        // 回车保存自定义文本并退出编辑
        state.handle_key(key_press(KeyCode::Enter));
        assert!(!state.custom_editing);
        assert_eq!(state.custom_inputs.get(&1).unwrap(), "ok");

        // 回车提交
        let final_res = state.handle_key(key_press(KeyCode::Enter));
        match final_res {
            QuestionUiResult::Submit(resp) => {
                assert!(!resp.cancelled);
                assert_eq!(resp.answers.len(), 2);
                assert_eq!(resp.answers[0].selected, vec!["Session Cookie"]);
                assert_eq!(resp.answers[1].selected.len(), 2);
                assert_eq!(resp.answers[1].custom, Some("ok".into()));
            }
            other => panic!("expected Submit, got {:?}", other),
        }
    }

    #[test]
    fn test_question_box_layout_above_input() {
        let req = make_test_request();
        let state = QuestionUiState::new(req);

        // 1. 验证尺寸计算
        let h_needed = state.height_needed();
        assert!((6..=12).contains(&h_needed));

        // 2. 模拟终端区域布局：验证提问卡片严格挂在输入框正上方
        let area = Rect::new(0, 0, 100, 30);
        let header_height = 3;
        let todo_height = 0;
        let question_height = state.height_needed();
        let input_height = 4;

        let sections = Layout::vertical([
            Constraint::Length(header_height),
            Constraint::Min(0),
            Constraint::Length(todo_height),
            Constraint::Length(question_height),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);

        let question_box_area = sections[3];
        let input_box_area = sections[4];

        // 提问卡片底部与输入框顶部紧密相连
        assert_eq!(question_box_area.bottom(), input_box_area.top());
        assert_eq!(question_box_area.height, question_height);
        assert_eq!(input_box_area.height, input_height);

        // 3. 验证 Ratatui 实际渲染不 panic
        let backend = TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_question_box(frame, question_box_area, &state, None);
            })
            .unwrap();
    }
}
