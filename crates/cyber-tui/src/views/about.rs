//! 关于页面：展示 Cyber Master 版本、简介、核心能力与快捷键。

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};

use crate::theme::Theme;

/// 渲染关于页面。
pub fn render(frame: &mut Frame, area: Rect, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(
            Line::from(" 关于 / About ")
                .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .padding(Padding::new(4, 4, 2, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 内容自适应居中：上方弹性 + 内容 + 下方弹性
    let content_height = 27u16;
    let outer = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(content_height),
        Constraint::Min(0),
    ])
    .split(inner);
    let content_area = outer[1];

    let rows = Layout::vertical([
        Constraint::Length(1), // 应用名
        Constraint::Length(1), // 版本
        Constraint::Length(1), // 空
        Constraint::Length(1), // 简介
        Constraint::Length(1), // 空
        Constraint::Length(1), // 核心能力标题
        Constraint::Length(1), // 能力 1
        Constraint::Length(1), // 能力 2
        Constraint::Length(1), // 能力 3
        Constraint::Length(1), // 能力 4
        Constraint::Length(1), // 空
        Constraint::Length(1), // 快捷键标题
        Constraint::Length(1), // 快捷键 1
        Constraint::Length(1), // 快捷键 2
        Constraint::Length(1), // 快捷键 3
        Constraint::Length(1), // 快捷键 4
        Constraint::Length(1), // 快捷键 5
        Constraint::Length(1), // 空
        Constraint::Length(1), // 仓库
        Constraint::Length(1), // 协议
        Constraint::Length(1), // 空
        Constraint::Length(1), // 作者
        Constraint::Length(1), // 博客
        Constraint::Length(1), // 主页
        Constraint::Length(1), // GitHub
        Constraint::Length(1), // 空
        Constraint::Length(1), // hint
    ])
    .split(content_area);

    let mut i = 0usize;
    let accent = Style::default().fg(theme.accent).add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(theme.muted);
    let title = Style::default().fg(theme.title).add_modifier(Modifier::BOLD);

    // 应用名
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Cyber Master", accent),
            Span::raw("  ·  网络安全智能体终端"),
        ]))
        .alignment(Alignment::Center),
        rows[i],
    );
    i += 1;

    // 版本
    frame.render_widget(
        Paragraph::new(Line::from(format!(
            "版本 v{}  ·  Edition 2021",
            env!("CARGO_PKG_VERSION"),
        )))
        .style(muted)
        .alignment(Alignment::Center),
        rows[i],
    );
    i += 1;

    // 空
    frame.render_widget(Paragraph::new(""), rows[i]);
    i += 1;

    // 简介
    frame.render_widget(
        Paragraph::new(Line::from(
            "对话式安全智能体终端：集成多 LLM 流式对话、工作流 DAG、MCP/Skill 工具体系与 CTF 协作面板。",
        ))
        .style(Style::default().fg(theme.fg))
        .alignment(Alignment::Center),
        rows[i],
    );
    i += 1;

    // 空
    frame.render_widget(Paragraph::new(""), rows[i]);
    i += 1;

    // 核心能力
    frame.render_widget(
        Paragraph::new(Line::from("核心能力")).style(title).alignment(Alignment::Center),
        rows[i],
    );
    i += 1;
    let features = [
        ("多 Provider 流式对话", "DeepSeek 思考链 · 上下文自动压缩 · 历史持久化"),
        ("统一工具表", "内置工具 + MCP（stdio/HTTP/SSE）+ Skill 渐进式披露"),
        ("工作流 DAG", "节点编排 · 并行执行 · tokio mpsc 流式资产传递"),
        ("CTF 协作面板", "题目管理 · writeup 归档 · 会话隔离 · 全局/会话作用域"),
    ];
    for (name, desc) in features {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("▸ {name}  "), accent),
                Span::styled(desc, muted),
            ]))
            .alignment(Alignment::Center),
            rows[i],
        );
        i += 1;
    }

    // 空
    frame.render_widget(Paragraph::new(""), rows[i]);
    i += 1;

    // 快捷键
    frame.render_widget(
        Paragraph::new(Line::from("常用快捷键")).style(title).alignment(Alignment::Center),
        rows[i],
    );
    i += 1;
    let keys = [
        ("Ctrl+L", "日志查看器"),
        ("Ctrl+T", "CTF 面板"),
        ("Ctrl+O", "展开/折叠最近条目"),
        ("F9", "切换鼠标捕获 / 选区模式"),
        ("s", "进入设置"),
    ];
    for (k, v) in keys {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("  {k:<8}"), accent),
                Span::styled(v, muted),
            ]))
            .alignment(Alignment::Center),
            rows[i],
        );
        i += 1;
    }

    // 空
    frame.render_widget(Paragraph::new(""), rows[i]);
    i += 1;

    // 仓库
    let repo = env!("CARGO_PKG_REPOSITORY");
    let repo_line = if repo.is_empty() {
        Line::from(vec![Span::styled("仓库  https://github.com/chuzouX/cyber-master", muted)])
    } else {
        Line::from(vec![
            Span::styled("仓库  ", muted),
            Span::raw(repo),
        ])
    };
    frame.render_widget(
        Paragraph::new(repo_line)
            .style(Style::default().fg(theme.fg))
            .alignment(Alignment::Center),
        rows[i],
    );
    i += 1;

    // 协议
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("协议  ", muted),
            Span::raw(env!("CARGO_PKG_LICENSE")),
        ]))
        .style(Style::default().fg(theme.fg))
        .alignment(Alignment::Center),
        rows[i],
    );
    i += 1;

    // 空
    frame.render_widget(Paragraph::new(""), rows[i]);
    i += 1;

    // 作者
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("作者  ", muted),
            Span::raw("chuzouX"),
        ]))
        .style(Style::default().fg(theme.fg))
        .alignment(Alignment::Center),
        rows[i],
    );
    i += 1;

    // 博客
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("博客  ", muted),
            Span::raw("https://chuzoux.top/"),
        ]))
        .style(Style::default().fg(theme.fg))
        .alignment(Alignment::Center),
        rows[i],
    );
    i += 1;

    // 主页
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("主页  ", muted),
            Span::raw("https://space.chuzoux.top/"),
        ]))
        .style(Style::default().fg(theme.fg))
        .alignment(Alignment::Center),
        rows[i],
    );
    i += 1;

    // GitHub
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("GitHub  ", muted),
            Span::raw("https://github.com/chuzouX"),
        ]))
        .style(Style::default().fg(theme.fg))
        .alignment(Alignment::Center),
        rows[i],
    );
    i += 1;

    // 空
    frame.render_widget(Paragraph::new(""), rows[i]);
    i += 1;

    // hint
    frame.render_widget(
        Paragraph::new(Line::from("Esc 返回   q 退出")).style(muted).alignment(Alignment::Center),
        rows[i],
    );
}
