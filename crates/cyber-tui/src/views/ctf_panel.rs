//! CTF 题目面板渲染：列表视图 + 详情视图。
//!
//! 纯函数渲染，状态全部在 App。列表视图显示所有题目的摘要行，
//! 详情视图显示选中题目的完整信息（描述/靶机/Flag/标签/时间/Writeup/关键知识点）。

use std::cell::Cell;

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthChar;

use cyber_core::CtfChallenge;

use crate::theme::Theme;

/// CTF 面板默认宽度（字符）。
pub const CTF_PANEL_WIDTH: u16 = 52;

/// 渲染 CTF 题目面板。
///
/// - `challenges`：题目列表
/// - `selected`：当前选中索引
/// - `detail_view`：false=列表视图, true=详情视图
/// - `detail_scroll`：详情视图的垂直滚动偏移
/// - `focused`：面板是否聚焦（影响边框颜色）
pub fn render(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    challenges: &[CtfChallenge],
    selected: usize,
    detail_view: bool,
    detail_scroll: usize,
    focused: bool,
    list_scroll: &Cell<usize>,
) {
    let border_color = if focused { theme.accent } else { theme.border };
    let title = if detail_view {
        " 题目详情 "
    } else {
        " 题目面板 "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(
            Line::from(title)
                .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .padding(Padding::new(1, 1, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 内容区 + 底部 hint
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
    let content_area = chunks[0];
    let hint_area = chunks[1];

    if detail_view {
        render_detail(frame, content_area, hint_area, theme, challenges, selected, detail_scroll);
    } else {
        render_list(frame, content_area, hint_area, theme, challenges, selected, focused, list_scroll);
    }
}

fn render_list(
    frame: &mut Frame,
    content_area: Rect,
    hint_area: Rect,
    theme: &Theme,
    challenges: &[CtfChallenge],
    selected: usize,
    _focused: bool,
    list_scroll: &Cell<usize>,
) {
    if challenges.is_empty() {
        let empty = Paragraph::new("（无题目）")
            .style(Style::default().fg(theme.muted))
            .alignment(Alignment::Center);
        frame.render_widget(empty, content_area);
    } else {
        let mut lines: Vec<Line> = Vec::new();
        // 记录每道题目的起始行号，用于自动滚动跟随选中项
        let mut item_starts: Vec<usize> = Vec::with_capacity(challenges.len());
        for (i, c) in challenges.iter().enumerate() {
            item_starts.push(lines.len());
            let is_selected = i == selected;
            let marker = if is_selected { "▸ " } else { "  " };
            let row_style = if is_selected {
                Style::default().bg(theme.sel_bg).fg(theme.sel_fg)
            } else {
                Style::default().fg(theme.fg)
            };

            // 第 1 行：序号 [分类]名称 状态（名称超长时自动换行）
            let status_str = if c.is_solved() { "已完成" } else { "进行中" };
            let status_color = if c.is_solved() {
                theme.accent
            } else {
                theme.muted
            };
            // 固定前缀宽度：marker(2) + "N. "(3) + "[分类]"(6) + " "(1) = 12
            // 后缀："  "(2) + 状态(最多 6) = 8
            let name_max = (content_area.width as usize).saturating_sub(20);
            let name_parts = wrap_text_by_width(&c.name, name_max);
            let global_marker = if c.is_global { "★" } else { " " };
            lines.push(Line::from(vec![
                Span::raw(marker),
                Span::styled(
                    format!("{}. ", i + 1),
                    Style::default().fg(theme.muted),
                ),
                Span::styled(
                    format!("[{}]", c.category),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    name_parts[0].clone(),
                    row_style.add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(status_str, Style::default().fg(status_color)),
                Span::styled(global_marker, Style::default().fg(theme.accent)),
            ]));
            // 名称换行续行（缩进对齐名称位置）
            for extra in &name_parts[1..] {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        extra.clone(),
                        row_style.add_modifier(Modifier::BOLD),
                    ),
                ]));
            }

            // 第 2 行：开始时间 结束时间 用时 WP标记
            let end_str = c.end_time.as_deref().unwrap_or("--");
            let dur = c.duration_str().unwrap_or_else(|| "--".into());
            let wp = if c.has_writeup() { "已写WP" } else { "" };
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    &c.start_time,
                    Style::default().fg(theme.muted),
                ),
                Span::raw("  "),
                Span::styled(end_str, Style::default().fg(theme.muted)),
                Span::raw("  "),
                Span::styled(dur, Style::default().fg(theme.muted)),
                Span::raw("  "),
                Span::styled(wp, Style::default().fg(theme.accent)),
            ]));

            lines.push(Line::raw(""));
        }

        // 粘性滚动：仅在选中项即将溢出视口时才调整滚动偏移
        let visible_h = content_area.height as usize;
        let total_lines = lines.len();
        let max_scroll = total_lines.saturating_sub(visible_h);
        let prev = list_scroll.get().min(max_scroll);

        let sel_start = item_starts.get(selected).copied().unwrap_or(0);
        let sel_end = item_starts.get(selected + 1).copied().unwrap_or(total_lines);

        let scroll = if total_lines <= visible_h {
            0
        } else if sel_start < prev {
            // 选中项在视口上方 → 上滚到其起始行
            sel_start
        } else if sel_end > prev + visible_h {
            // 选中项在视口下方 → 下滚到刚好露出其末行
            sel_end.saturating_sub(visible_h).min(max_scroll)
        } else {
            // 选中项仍在视口内 → 保持不动
            prev
        };
        list_scroll.set(scroll);

        frame.render_widget(
            Paragraph::new(lines).scroll((scroll as u16, 0)),
            content_area,
        );
    }

    let hint = if challenges.is_empty() {
        "Ctrl+T 关闭面板"
    } else {
        "↑↓/PgUpPgDn选择 Enter查看 d删除 s状态 g全局 G全部全局 e编辑 Esc关闭"
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(theme.muted)),
        hint_area,
    );
}

fn render_detail(
    frame: &mut Frame,
    content_area: Rect,
    hint_area: Rect,
    theme: &Theme,
    challenges: &[CtfChallenge],
    selected: usize,
    scroll: usize,
) {
    let Some(c) = challenges.get(selected) else {
        frame.render_widget(
            Paragraph::new("（无题目）").style(Style::default().fg(theme.muted)),
            content_area,
        );
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    let w = content_area.width as usize;

    // 标题行
    lines.push(Line::from(vec![
        Span::styled(
            format!("[{}]", c.category),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            c.name.clone(),
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::raw(""));

    // 题目描述
    lines.push(Span::styled(
        "题目描述",
        Style::default().fg(theme.muted).add_modifier(Modifier::BOLD),
    ).into());
    for l in wrap_text_by_width(&c.description, w) {
        lines.push(Line::from(l).style(Style::default().fg(theme.fg)));
    }
    lines.push(Line::raw(""));

    // 靶机
    if let Some(target) = &c.target {
        lines.push(Span::styled(
            "靶机",
            Style::default().fg(theme.muted).add_modifier(Modifier::BOLD),
        ).into());
        for l in wrap_text_by_width(target, w) {
            lines.push(Line::from(l).style(Style::default().fg(theme.fg)));
        }
        lines.push(Line::raw(""));
    }

    // Flag
    if let Some(flag) = &c.flag {
        lines.push(Span::styled(
            "Flag",
            Style::default().fg(theme.muted).add_modifier(Modifier::BOLD),
        ).into());
        for l in wrap_text_by_width(flag, w) {
            lines.push(Line::from(l).style(Style::default().fg(theme.accent)));
        }
        lines.push(Line::raw(""));
    }

    // 标签
    if !c.tags.is_empty() {
        lines.push(Span::styled(
            "Tag",
            Style::default().fg(theme.muted).add_modifier(Modifier::BOLD),
        ).into());
        for l in wrap_text_by_width(&c.tags.join("、"), w) {
            lines.push(Line::from(l).style(Style::default().fg(theme.fg)));
        }
        lines.push(Line::raw(""));
    }

    // 时间
    let end_str = c.end_time.as_deref().unwrap_or("--");
    let dur = c.duration_str().unwrap_or_else(|| "--".into());
    lines.push(Line::from(format!(
        "{} - {}  {}",
        c.start_time, end_str, dur
    ))
    .style(Style::default().fg(theme.muted)));
    lines.push(Line::raw(""));

    // 分割线
    let sep = "─".repeat(w);
    lines.push(Line::from(sep).style(Style::default().fg(theme.border)));

    // Writeup（Markdown 渲染）
    lines.push(Span::styled(
        "Writeup",
        Style::default().fg(theme.muted).add_modifier(Modifier::BOLD),
    ).into());
    if let Some(wp) = &c.writeup {
        let wp_lines = crate::markdown::render(wp, theme);
        if wp_lines.is_empty() {
            lines.push(Line::from("（空）").style(Style::default().fg(theme.muted)));
        } else {
            lines.extend(wp_lines);
        }
    } else {
        lines.push(Line::from("（未撰写）").style(Style::default().fg(theme.muted)));
    }
    lines.push(Line::raw(""));

    // 分割线
    let sep = "─".repeat(w);
    lines.push(Line::from(sep).style(Style::default().fg(theme.border)));

    // 关键知识点
    lines.push(Span::styled(
        "关键知识点 / 卡点",
        Style::default().fg(theme.muted).add_modifier(Modifier::BOLD),
    ).into());
    if let Some(kp) = &c.key_points {
        for line in kp.lines() {
            for l in wrap_text_by_width(line, w) {
                lines.push(Line::from(l).style(Style::default().fg(theme.fg)));
            }
        }
    } else {
        lines.push(Line::from("（无）").style(Style::default().fg(theme.muted)));
    }

    let para = Paragraph::new(lines).scroll((scroll as u16, 0));
    frame.render_widget(para, content_area);

    let hint = if c.is_solved() && !c.has_writeup() {
        "w写WP Esc返回 Shift+Esc回对话"
    } else {
        "Esc返回 Shift+Esc回对话"
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(theme.muted)),
        hint_area,
    );
}

/// 按显示宽度换行文本（支持中文等宽字符）。
/// 返回非空 Vec：每项为一行内容。
fn wrap_text_by_width(s: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![s.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + w > max_width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += w;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyber_core::{CtfCategory, CtfStatus};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn make_challenge(name: &str, category: CtfCategory, solved: bool) -> CtfChallenge {
        let mut c = CtfChallenge::new(name.into(), category);
        if solved {
            c.status = CtfStatus::Solved;
            c.flag = Some("flag{test}".into());
            c.end_time = Some("15:00".into());
            c.start_time = "14:00".into();
        }
        c
    }

    #[test]
    fn render_empty_list_no_panic() {
        let theme = Theme::resolve("cyberpunk");
        let scroll = Cell::new(0);
        let mut terminal = Terminal::new(TestBackend::new(42, 20)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &[], 0, false, 0, true, &scroll))
            .unwrap();
    }

    #[test]
    fn render_list_with_challenges() {
        let theme = Theme::resolve("cyberpunk");
        let challenges = vec![
            make_challenge("web1", CtfCategory::Web, false),
            make_challenge("pwn1", CtfCategory::Pwn, true),
        ];
        let scroll = Cell::new(0);
        let mut terminal = Terminal::new(TestBackend::new(42, 20)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &challenges, 0, false, 0, true, &scroll))
            .unwrap();
    }

    #[test]
    fn render_detail_view() {
        let theme = Theme::resolve("cyberpunk");
        let mut c = make_challenge("crypto1", CtfCategory::Crypto, true);
        c.description = "RSA challenge".into();
        c.target = Some("nc 1.2.3.4 1234".into());
        c.tags = vec!["RSA".into(), "crypto".into()];
        c.key_points = Some("RSA common modulus".into());
        c.writeup = Some("Step 1: ...".into());
        let scroll = Cell::new(0);
        let mut terminal = Terminal::new(TestBackend::new(42, 30)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &[c], 0, true, 0, true, &scroll))
            .unwrap();
    }
}
