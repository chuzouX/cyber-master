//! Welcome 启动页：无 `.cyber.md` 时进入，引导用户选择入口。

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};

use crate::theme::Theme;

const OPTIONS: &[&str] = &[
    "新建项目  (New Project)",
    "打开工作流 (Open Workflow)",
    "进入聊天  (Enter Chat)",
    "设置      (Settings)",
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
            Line::from(" Cyber Master · v0.1.0 ")
                .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .padding(Padding::vertical(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 上下居中：上方弹性留白 + 内容块 + 下方弹性留白
    let content_height = 2 /*副标题+空行*/ + OPTIONS.len() as u16 + 2 /*空行+hint*/ + if toast.is_some() { 2 } else { 0 };
    let outer = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(content_height),
        Constraint::Min(0),
    ])
    .split(inner);
    let content_area = outer[1];

    let mut constraints = vec![Constraint::Length(1), Constraint::Length(1)];
    for _ in OPTIONS {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1)); // 空行
    constraints.push(Constraint::Length(1)); // hint
    if toast.is_some() {
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(1));
    }
    let rows = Layout::vertical(constraints).split(content_area);
    let mut i = 0usize;

    frame.render_widget(
        Paragraph::new(Line::from(
            "欢迎使用 Cyber Master — 选择一个入口开始（未检测到 .cyber.md）",
        ))
        .style(Style::default().fg(theme.muted).add_modifier(Modifier::ITALIC))
        .alignment(Alignment::Center),
        rows[i],
    );
    i += 1;
    frame.render_widget(Paragraph::new(""), rows[i]);
    i += 1;

    for (idx, opt) in OPTIONS.iter().enumerate() {
        let style = if idx == selected {
            Style::default()
                .fg(theme.sel_fg)
                .bg(theme.sel_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };
        let prefix = if idx == selected { "▸ " } else { "  " };
        frame.render_widget(
            Paragraph::new(Line::from(format!("{prefix}{opt}")).style(style))
                .alignment(Alignment::Center),
            rows[i],
        );
        i += 1;
    }

    frame.render_widget(Paragraph::new(""), rows[i]);
    i += 1;
    frame.render_widget(
        Paragraph::new(Line::from("↑/↓ 导航   Enter 确认   s 设置   q 退出"))
            .style(Style::default().fg(theme.muted))
            .alignment(Alignment::Center),
        rows[i],
    );
    i += 1;

    if let Some(t) = toast {
        frame.render_widget(Paragraph::new(""), rows[i]);
        i += 1;
        frame.render_widget(
            Paragraph::new(Line::from(t).style(Style::default().fg(theme.accent)))
                .alignment(Alignment::Center),
            rows[i],
        );
    }
}
