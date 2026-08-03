//! Chat 占位页 + 通用占位页（Workflow / Dashboard）。
//!
//! P1 仅渲染静态文本展示项目上下文与后续阶段说明；P2 起接入真实 chat。

use cyber_core::ProjectContext;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};

use crate::theme::Theme;

/// Chat 占位页：显示项目上下文 + provider + P2 提示。
pub fn render(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    project: Option<&ProjectContext>,
    provider: &str,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(
            Line::from(" Chat Mode ")
                .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(
        Line::from("Chat Mode（对话交互式）")
            .style(Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
    );
    lines.push(Line::from("状态：P2 阶段实现（LLM provider / 流式对话 / 工具调用 / 斜杠命令）")
        .style(Style::default().fg(theme.muted)));
    lines.push(Line::from(""));
    lines.push(
        Line::from(format!("默认 provider : {provider}"))
            .style(Style::default().fg(theme.fg)),
    );
    lines.push(Line::from(""));

    match project {
        Some(p) => {
            lines.push(
                Line::from("项目上下文（来自 .cyber.md）")
                    .style(Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
            );
            lines.push(Line::from(format!(
                "  project : {}",
                p.frontmatter.project.as_deref().unwrap_or("<未指定>")
            )));
            lines.push(Line::from(format!(
                "  scope   : {}",
                p.frontmatter.scope.as_deref().unwrap_or("<未指定>")
            )));
            lines.push(Line::from(format!(
                "  owner   : {}",
                p.frontmatter.owner.as_deref().unwrap_or("<未指定>")
            )));
            lines.push(Line::from(format!(
                "  rules   : {} 条安全护栏",
                p.rules().len()
            )));
        }
        None => {
            lines.push(
                Line::from("项目上下文：无（当前目录未检测到 .cyber.md）")
                    .style(Style::default().fg(theme.muted)),
            );
        }
    }
    lines.push(Line::from(""));
    lines.push(
        Line::from("Tab 切换模式   Esc 返回 Welcome   q 退出")
            .style(Style::default().fg(theme.muted)),
    );

    frame.render_widget(Paragraph::new(lines), inner);
}

/// 通用占位页（Workflow / Dashboard）。
pub fn render_placeholder(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    title: &str,
    stage: &str,
    project: Option<&ProjectContext>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(
            Line::from(format!(" {title} "))
                .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(
        Line::from(title.to_string())
            .style(Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
    );
    lines.push(
        Line::from(format!("状态：{stage}")).style(Style::default().fg(theme.muted)),
    );
    lines.push(Line::from(""));
    if let Some(p) = project {
        lines.push(
            Line::from(format!(
                "当前项目：{}",
                p.frontmatter.project.as_deref().unwrap_or("<未指定>")
            ))
            .style(Style::default().fg(theme.fg)),
        );
    } else {
        lines.push(
            Line::from("项目上下文：无").style(Style::default().fg(theme.muted)),
        );
    }
    lines.push(Line::from(""));
    lines.push(
        Line::from("Tab 切换模式   Esc 返回 Welcome   q 退出")
            .style(Style::default().fg(theme.muted)),
    );

    frame.render_widget(Paragraph::new(lines), inner);
}
