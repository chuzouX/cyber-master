//! `/sessions` 面板渲染：列出当前 cwd 的所有 session，支持选中标记 + 待删除提示。
//!
//! 纯函数渲染，状态全部在 [`crate::app::SessionsPanelState`]（选中 / 待删除 / 列表快照）
//! 与 [`crate::history::SessionIndex`]（current 标记）。按键处理在 `app.rs::handle_sessions_key`。

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};

use crate::app::SessionsPanelState;
use crate::history::SessionIndex;
use crate::theme::Theme;

/// 面板中 id 的最大显示宽度（超出截断，加 `…`）。
const ID_DISPLAY_LEN: usize = 10;

/// 渲染 `/sessions` 面板。
///
/// - `state.list`：进入面板时快照的 session 列表（导航/删除均操作此快照）。
/// - `state.selected`：当前选中行。
/// - `state.pending_delete`：待删除确认行（同项二次 `d` 执行删除）。
/// - `idx.current`：当前激活 session id（用 `★` 标记）。
pub fn render(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &SessionsPanelState,
    idx: &SessionIndex,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(
            Line::from(" 会话管理 / Sessions ")
                .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 列表区 + 底部 hint
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).split(inner);
    let list_area = chunks[0];
    let hint_area = chunks[1];

    let mut lines: Vec<Line> = Vec::new();

    if state.list.is_empty() {
        lines.push(
            Line::from("（无会话）")
                .style(Style::default().fg(theme.muted))
                .alignment(Alignment::Center),
        );
    } else {
        // 表头
        lines.push(
            Line::from(vec![
                Span::styled(
                    "标题",
                    Style::default().fg(theme.muted).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  · 消息  · id"),
            ])
            .style(Style::default().fg(theme.muted)),
        );
        lines.push(Line::from(""));
        for (i, meta) in state.list.iter().enumerate() {
            let selected = i == state.selected;
            let pending = state.pending_delete == Some(i);
            let is_current = meta.id == idx.current;
            let marker = if selected { "▸ " } else { "  " };
            let delete_tag = if pending { "  [待删除!]" } else { "" };
            let star = if is_current { " ★当前" } else { "" };
            let row_style = if selected {
                Style::default().bg(theme.sel_bg)
            } else {
                Style::default().bg(theme.bg)
            };
            let title_color = if pending { theme.accent } else { theme.title };
            let id_short = truncate_id(&meta.id, ID_DISPLAY_LEN);

            lines.push(
                Line::from(vec![
                    Span::raw(marker),
                    Span::styled(
                        format!("{}{}", meta.title, star),
                        Style::default().fg(title_color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  · {} 条  · [{}]", meta.message_count, id_short),
                        Style::default().fg(theme.muted),
                    ),
                    Span::styled(
                        delete_tag.to_string(),
                        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                    ),
                ])
                .style(row_style),
            );
        }
    }

    let visible_rows = list_area.height as usize;
    let selected_row = if state.list.is_empty() { 0 } else { 2 + state.selected };
    let max_scroll = lines.len().saturating_sub(visible_rows);
    let scroll = selected_row
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(max_scroll)
        .min(u16::MAX as usize) as u16;

    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(theme.bg).fg(theme.fg))
            .scroll((scroll, 0)),
        list_area,
    );

    // 底部 hint：待删除态切换提示文案
    let hint = if let Some(i) = state.pending_delete {
        let name = state
            .list
            .get(i)
            .map(|m| m.title.as_str())
            .unwrap_or("?");
        format!(" 再按 d 确认删除「{name}」· 其他键取消")
    } else {
        " ↑↓ 选择  Enter 切换  n 新建  d 删除  Esc 返回".to_string()
    };
    let hint_style = if state.pending_delete.is_some() {
        Style::default().fg(theme.accent)
    } else {
        Style::default().fg(theme.muted)
    };
    frame.render_widget(
        Paragraph::new(Line::from(hint)).style(hint_style),
        hint_area,
    );
}

/// 把 id 截断到 `max` 字符（超出加 `…`）。短于 max 原样返回。
fn truncate_id(id: &str, max: usize) -> String {
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= max {
        id.to_string()
    } else {
        let head: String = chars.iter().take(max - 1).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::SessionMeta;
    use ratatui::{backend::TestBackend, Terminal};

    fn meta(id: &str, title: &str, count: usize) -> SessionMeta {
        SessionMeta {
            id: id.into(),
            title: title.into(),
            created_at: 0,
            updated_at: 0,
            message_count: count,
        }
    }

    #[test]
    fn render_sessions_panel_does_not_panic() {
        let state = SessionsPanelState {
            selected: 1,
            pending_delete: None,
            list: vec![
                meta("abc123", "第一个会话", 3),
                meta("def456", "第二个会话", 10),
            ],
        };
        let idx = SessionIndex {
            current: "def456".into(),
            sessions: state.list.clone(),
        };
        let mut terminal = Terminal::new(TestBackend::new(70, 20)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &Theme::resolve("cyberpunk"), &state, &idx))
            .unwrap();
    }

    #[test]
    fn render_sessions_panel_with_pending_delete() {
        let state = SessionsPanelState {
            selected: 0,
            pending_delete: Some(0),
            list: vec![meta("abc123", "待删", 1)],
        };
        let idx = SessionIndex {
            current: "abc123".into(),
            sessions: state.list.clone(),
        };
        let mut terminal = Terminal::new(TestBackend::new(70, 16)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &Theme::resolve("cyberpunk"), &state, &idx))
            .unwrap();
    }

    #[test]
    fn render_empty_list_shows_placeholder() {
        let state = SessionsPanelState {
            selected: 0,
            pending_delete: None,
            list: vec![],
        };
        let idx = SessionIndex::default();
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &Theme::resolve("cyberpunk"), &state, &idx))
            .unwrap();
    }

    #[test]
    fn truncate_id_short_returns_as_is() {
        assert_eq!(truncate_id("abc", 10), "abc");
    }

    #[test]
    fn truncate_id_long_truncates_with_ellipsis() {
        let out = truncate_id("abcdefghijklmnop", 8);
        assert_eq!(out.chars().count(), 8);
        assert!(out.ends_with('…'));
        assert!(out.starts_with("abcdefg"));
    }
}
