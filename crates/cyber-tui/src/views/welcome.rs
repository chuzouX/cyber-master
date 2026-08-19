//! Welcome 启动页：无 `.cyber.md` 时进入，引导用户选择入口。

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};

use crate::theme::Theme;

const OPTIONS: &[&str] = &[
    "新建项目  (New Project)",
    "打开工作流 (Open Workflow)",
    "进入聊天  (Enter Chat)",
    "设置      (Settings)",
    "关于      (About)",
];

/// 渲染 Welcome 页。
///
/// - `selected`：当前选中项索引（0..OPTIONS.len()）
/// - `toast`：一次性提示（如占位功能说明），有值时显示在底部
pub fn render(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    selected: usize,
    toast: Option<&str>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(
            Line::from(concat!(" Cyber Master · v", env!("CARGO_PKG_VERSION"), " "))
                .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Keep the landing content centered and readable on wide terminals.
    let content_height = 3 + 1 + (OPTIONS.len() as u16 * 3) + 1 + 1
        + if toast.is_some() { 2 } else { 0 };
    let content_area = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(content_height),
        Constraint::Min(0),
    ])
    .split(inner)[1];
    let content_width = content_area.width.min(76);
    let centered = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(content_width),
        Constraint::Min(0),
    ])
    .split(content_area)[1];

    let mut constraints = vec![
        Constraint::Length(3), // brand + subtitle
        Constraint::Length(1), // spacer
    ];
    constraints.extend((0..OPTIONS.len()).map(|_| Constraint::Length(3)));
    constraints.push(Constraint::Length(1)); // spacer
    constraints.push(Constraint::Length(1)); // hint
    if toast.is_some() {
        constraints.push(Constraint::Length(2));
    }
    let rows = Layout::vertical(constraints).split(centered);
    let mut row = 0usize;

    let accent = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(theme.muted);

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("◈  CYBER MASTER", accent),
                Span::styled("  SECURITY WORKSPACE", muted),
            ]),
            Line::from("Select a workspace to begin"),
        ])
        .alignment(Alignment::Center),
        rows[row],
    );
    row += 2;

    for (idx, opt) in OPTIONS.iter().enumerate() {
        let is_selected = idx == selected;
        let item_style = if is_selected {
            Style::default()
                .fg(theme.sel_fg)
                .bg(theme.sel_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };
        let marker = if is_selected { "▸  " } else { "   " };
        let item_block = if is_selected {
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .style(Style::default().bg(theme.sel_bg))
        } else {
            Block::default().borders(Borders::NONE)
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(marker, item_style),
                Span::styled(*opt, item_style),
            ]))
            .block(item_block)
            .alignment(Alignment::Left),
            rows[row],
        );
        row += 1;
    }

    row += 1;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("↑↓", accent),
            Span::styled(" navigate   ", muted),
            Span::styled("Enter", accent),
            Span::styled(" select   ", muted),
            Span::styled("s", accent),
            Span::styled(" settings   ", muted),
            Span::styled("q", accent),
            Span::styled(" quit", muted),
        ]))
        .alignment(Alignment::Center),
        rows[row],
    );
    row += 1;

    if let Some(message) = toast {
        frame.render_widget(
            Paragraph::new(Line::from(message).style(Style::default().fg(theme.accent)))
                .alignment(Alignment::Center),
            rows[row],
        );
    }
}
