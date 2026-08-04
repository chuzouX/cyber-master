//! Chat 视图：消息历史区 + 流式 buffer + tui-textarea 输入框。
//!
//! P2.2 重写：历史区遍历 `ChatEntry`（User/Assistant/ToolCall/ToolResult/System），
//! 工具调用与结果以 `▶`/`→`/`✗` 标记渲染，错误结果用红色高亮。Assistant 条目（含
//! 流式 buffer）经 `markdown` 模块渲染（标题/代码块/行内代码/粗斜体/链接/列表/引用），
//! 标签 `[assistant]` 独占一行，Markdown 内容随后铺开。布局：
//! ```text
//! ┌─ Chat ────────────────────┐
//! │ [user] ...                 │  ← 历史 + 流式 buffer（Min）
//! │ [assistant]                │  ← assistant 标签独占一行
//! │ # 标题 / **粗体** / `code`  │  ← Markdown 渲染
//! │   ▶ [tool] list_dir({...}) │  ← 工具调用
//! │     → a.txt                │  ← 工具结果（✗ 红色表示错误）
//! │ [assistant]                │  ← streaming 进行中消息
//! │ 收到：…▌                    │  ← 末行带光标（buffer 作 Markdown 渲染）
//! ├─────────────────────────────┤
//! │ 输入 / 生成中…              │  ← textarea（Length 3）
//! ├─────────────────────────────┤
//! │ Enter 发送  Shift+Enter 换行 │  ← hint（Length 1）
//! └─────────────────────────────┘
//! ```
//! `render_placeholder` 保留供 Workflow/Dashboard 占位页复用。

use cyber_core::{PriceConfig, ProjectContext};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
    Frame,
};

use crate::app::UsageStats;
use crate::chat::ChatState;
use crate::theme::Theme;

/// 渲染 Chat 主视图。
#[allow(clippy::too_many_arguments)]
pub fn render(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &ChatState,
    project: Option<&ProjectContext>,
    provider: &str,
    usage: &UsageStats,
    price: Option<&PriceConfig>,
) {
    let scrolled = !state.is_following_bottom();
    let title = if scrolled {
        format!(" Chat · provider={provider} · ↑已滚动 ")
    } else {
        format!(" Chat · provider={provider} ")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(
            Line::from(title)
                .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 历史（弹性） / 输入框（3 行含边框） / usage 状态栏（1 行） / hint（1 行）
    let chunks = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);

    render_history(frame, chunks[0], theme, state, project);
    render_input(frame, chunks[1], state);
    render_usage_bar(frame, chunks[2], theme, usage, price);
    render_hint(frame, chunks[3], theme, state);
    // 斜杠补全菜单：浮于输入框上方（覆盖历史区底部），最后绘制以叠加在最上层
    if state.slash_menu.open && !state.slash_menu.filtered.is_empty() {
        render_slash_menu(frame, chunks[1], theme, &state.slash_menu);
    }
}

/// 渲染消息历史区：已完成的 ChatEntry（user/assistant/tool/system）+ 流式中的 buffer。
///
/// 滚动性能（跟手性）关键：非空会话走 **预折行缓存 + 可见窗口** 路径——
/// `state.wrapped_lines(theme, width)` 返回按可视宽度拆好的单行 `Line`（key 命中即 O(1)
/// 复用，仅内容/宽度/theme 变化才 O(N) 重建），render 只 clone 可见窗口（O(visible)）
/// 并以 **无 Wrap、无 scroll** 的 `Paragraph` 渲染。避免旧行为每帧 `line_count` + `Wrap`
/// 重算（O(N)）与全量 `extend_from_slice` clone —— 长历史滚动卡顿主因。
///
/// 空会话（无 entries 且非流式）渲染引导文本（量小，直接 Paragraph+Wrap，不参与滚动）。
fn render_history(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &ChatState,
    project: Option<&ProjectContext>,
) {
    // 用 Padding 让内容不贴边
    let padded = Block::default().padding(Padding::new(1, 1, 0, 0));
    let inner = padded.inner(area);
    frame.render_widget(padded, area);

    // 空会话引导：直接渲染（量小，无需缓存/滚动）
    if state.entries.is_empty() && !state.streaming {
        let mut lines: Vec<Line> = Vec::new();
        lines.push(
            Line::from("Chat Mode（对话交互式）")
                .style(Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        );
        match project {
            Some(p) => {
                lines.push(Line::from(format!(
                    "项目：{} · scope：{} · 护栏 {} 条",
                    p.frontmatter.project.as_deref().unwrap_or("<未指定>"),
                    p.frontmatter.scope.as_deref().unwrap_or("<未指定>"),
                    p.rules().len(),
                ))
                .style(Style::default().fg(theme.muted)));
            }
            None => {
                lines.push(
                    Line::from("无项目上下文（当前目录未检测到 .cyber.md）")
                        .style(Style::default().fg(theme.muted)),
                );
            }
        }
        lines.push(Line::from(""));
        lines.push(
            Line::from("在下方输入框输入消息，Enter 发送。")
                .style(Style::default().fg(theme.muted)),
        );
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        state.set_scroll_metrics(0, inner.height as usize);
        return;
    }

    // 非空：预折行缓存 + 可见窗口
    let wrapped = state.wrapped_lines(theme, inner.width);
    let total = wrapped.len();
    let visible = inner.height as usize;
    let max_scroll = total.saturating_sub(visible);
    state.set_scroll_metrics(total, visible);
    let offset = state.resolved_scroll_offset(max_scroll);
    let end = (offset + visible).min(total);
    // 仅 clone 可见窗口（O(visible)），无 Wrap 无 scroll → ratatui 渲染 O(visible)
    let window: Vec<Line<'static>> = if offset < end {
        wrapped[offset..end].to_vec()
    } else {
        Vec::new()
    };
    drop(wrapped); // 释放 RefCell 借用
    frame.render_widget(Paragraph::new(window), inner);
}

/// 渲染输入框。
///
/// textarea 的边框/样式（含 streaming 态切换）由 `App::style_chat_input` 在 draw 前
/// 以 `&mut self` 应用（`set_block`/`set_style` 需 `&mut`，而 render 是 `&self`）。
/// 此处仅渲染已配置好的 textarea。
fn render_input(frame: &mut Frame, area: Rect, state: &ChatState) {
    frame.render_widget(&state.input, area);
}

/// 渲染 usage 状态栏：缓存命中率 + token 计数 + 成本（如果配置了价格）。
///
/// 空会话（无任何 usage）显示灰色占位，避免空白行。
fn render_usage_bar(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    usage: &UsageStats,
    price: Option<&PriceConfig>,
) {
    let total_in = usage.cache_hit + usage.cache_miss;
    if total_in == 0 && usage.completion == 0 {
        // 无数据：显示淡色占位
        frame.render_widget(
            Paragraph::new(Line::from("")),
            area,
        );
        return;
    }

    let hit_pct = usage.hit_rate() * 100.0;
    let mut spans: Vec<Span> = vec![
        Span::styled(
            format!(" cache {hit_pct:.1}% "),
            Style::default()
                .fg(if hit_pct >= 80.0 {
                    theme.accent
                } else {
                    theme.muted
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("│"),
        Span::styled(
            format!(" ↑{} ↓{} ", fmt_tokens(total_in), fmt_tokens(usage.completion)),
            Style::default().fg(theme.muted),
        ),
    ];

    if let Some(p) = price {
        let cost = usage.cost(p);
        spans.push(Span::raw("│"));
        spans.push(Span::styled(
            format!(" ${cost:.4} "),
            Style::default().fg(theme.accent),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        area,
    );
}

/// 格式化 token 计数：< 1000 原样，≥ 1000 用 k 单位（1.2k）。
fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        format!("{n}")
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// 渲染底部 hint 行。
fn render_hint(frame: &mut Frame, area: Rect, theme: &Theme, state: &ChatState) {
    let hint = if state.streaming {
        " 生成中… Esc 取消 · Tab 切换模式 · Ctrl+, 设置 · Ctrl+C 退出"
    } else {
        " Enter 发送 · Shift+Enter 换行 · / 命令 · PgUp/PgDn 滚动 · Tab 切换 · Ctrl+, 设置 · Esc 返回 · Ctrl+C 退出"
    };
    frame.render_widget(
        Paragraph::new(Line::from(hint)).style(Style::default().fg(theme.muted)),
        area,
    );
}

/// 渲染斜杠命令补全菜单：浮于输入框正上方，覆盖历史区底部。
///
/// 高度按过滤结果数（最多 8 项 + 边框）计算，位置贴齐输入框上沿。先 `Clear` 擦除
/// 背景历史内容再绘制带边框的 `List`，选中行整行高亮（`sel_bg`/`sel_fg`）。
/// `input_area` 为输入框区域，菜单以其上沿为底向上展开。
fn render_slash_menu(
    frame: &mut Frame,
    input_area: Rect,
    theme: &Theme,
    menu: &crate::chat::SlashMenu,
) {
    let count = menu.filtered.len();
    let h = (count.min(8) as u16).saturating_add(2); // +2 上下边框
    let menu_area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(h),
        width: input_area.width,
        height: h,
    };
    // 擦除菜单覆盖区的历史内容，避免透出
    frame.render_widget(Clear, menu_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(
            Line::from(" 命令(↑↓选择 Enter 补全 Esc 关闭) ")
                .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg));
    let items: Vec<ListItem<'static>> = menu
        .filtered
        .iter()
        .map(|spec| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    spec.usage.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(spec.desc.to_string(), Style::default().fg(theme.muted)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(theme.sel_bg)
                .fg(theme.sel_fg)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut list_state = ListState::default();
    if count > 0 {
        list_state.select(Some(menu.selected.min(count - 1)));
    }
    frame.render_stateful_widget(list, menu_area, &mut list_state);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::UsageStats;
    use crate::chat::{ChatEntry, ChatState};
    use ratatui::{backend::TestBackend, Terminal};

    fn empty_usage() -> UsageStats {
        UsageStats::default()
    }

    #[test]
    fn empty_chat_renders_without_panic() {
        let theme = Theme::resolve("cyberpunk");
        let state = ChatState::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock", &empty_usage(), None))
            .unwrap();
    }

    #[test]
    fn chat_with_entries_renders() {
        let theme = Theme::resolve("cyberpunk");
        let mut state = ChatState::new();
        state.entries.push(ChatEntry::User("你好".into()));
        state.entries.push(ChatEntry::Assistant("收到：你好".into()));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &state, None, "openai", &empty_usage(), None))
            .unwrap();
    }

    #[test]
    fn chat_streaming_with_buffer_renders() {
        let theme = Theme::resolve("nord");
        let mut state = ChatState::new();
        state.streaming = true;
        state.streaming_buffer = "收到：hi".into();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &state, None, "ollama", &empty_usage(), None))
            .unwrap();
    }

    #[test]
    fn chat_streaming_empty_buffer_renders_cursor() {
        let theme = Theme::resolve("dracula");
        let mut state = ChatState::new();
        state.streaming = true;
        state.streaming_buffer.clear();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock", &empty_usage(), None))
            .unwrap();
    }

    #[test]
    fn chat_multiline_assistant_renders() {
        let theme = Theme::resolve("cyberpunk");
        let mut state = ChatState::new();
        state
            .entries
            .push(ChatEntry::Assistant("第一行\n第二行\n第三行".into()));
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock", &empty_usage(), None))
            .unwrap();
    }

    #[test]
    fn chat_tool_call_and_result_renders() {
        let theme = Theme::resolve("cyberpunk");
        let mut state = ChatState::new();
        state.entries.push(ChatEntry::User("列出当前目录".into()));
        state.entries.push(ChatEntry::Assistant("好的，我来查看。".into()));
        state.entries.push(ChatEntry::ToolCall {
            id: "c1".into(),
            name: "list_dir".into(),
            arguments: "{\"path\":\".\"}".into(),
        });
        state.entries.push(ChatEntry::ToolResult {
            id: "c1".into(),
            name: "list_dir".into(),
            output: "a.txt\nb.txt\nc.txt".into(),
            is_error: false,
        });
        state
            .entries
            .push(ChatEntry::Assistant("当前目录有 3 个文件。".into()));
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock", &empty_usage(), None))
            .unwrap();
    }

    #[test]
    fn chat_tool_error_result_renders() {
        let theme = Theme::resolve("cyberpunk");
        let mut state = ChatState::new();
        state.entries.push(ChatEntry::ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: "{\"path\":\"/nonexistent\"}".into(),
        });
        state.entries.push(ChatEntry::ToolResult {
            id: "c1".into(),
            name: "read_file".into(),
            output: "文件不存在: /nonexistent".into(),
            is_error: true,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock", &empty_usage(), None))
            .unwrap();
    }

    #[test]
    fn chat_tool_call_empty_args_renders() {
        let theme = Theme::resolve("cyberpunk");
        let mut state = ChatState::new();
        state.entries.push(ChatEntry::ToolCall {
            id: "c1".into(),
            name: "list_dir".into(),
            arguments: "{}".into(),
        });
        state.entries.push(ChatEntry::ToolResult {
            id: "c1".into(),
            name: "list_dir".into(),
            output: "".into(),
            is_error: false,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock", &empty_usage(), None))
            .unwrap();
    }

    #[test]
    fn chat_slash_menu_renders_without_panic() {
        let theme = Theme::resolve("cyberpunk");
        let mut state = ChatState::new();
        state.input.insert_str("/mo");
        state.update_slash_menu();
        assert!(state.slash_menu.open);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock", &empty_usage(), None))
            .unwrap();
    }

    #[test]
    fn chat_scrolled_state_renders_without_panic() {
        let theme = Theme::resolve("cyberpunk");
        let mut state = ChatState::new();
        // 多条目使历史超过可视高度
        for i in 0..30 {
            state.entries.push(ChatEntry::User(format!("消息 {i}")));
            state
                .entries
                .push(ChatEntry::Assistant(format!("回复 {i}").repeat(5)));
        }
        // 模拟首帧回写度量后上滚
        state.set_scroll_metrics(200, 15);
        state.scroll_history(-50);
        assert!(!state.is_following_bottom());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock", &empty_usage(), None))
            .unwrap();
    }

    #[test]
    fn chat_slash_menu_full_list_renders() {
        let theme = Theme::resolve("nord");
        let mut state = ChatState::new();
        state.input.insert_str("/");
        state.update_slash_menu();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock", &empty_usage(), None))
            .unwrap();
    }

    #[test]
    fn usage_bar_renders_with_data() {
        let theme = Theme::resolve("cyberpunk");
        let usage = UsageStats {
            cache_hit: 9000,
            cache_miss: 1000,
            completion: 500,
        };
        let price = PriceConfig {
            input_per_m: Some(0.14),
            output_per_m: Some(0.28),
            cache_hit_per_m: Some(0.014),
        };
        let state = ChatState::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock", &usage, Some(&price)))
            .unwrap();
    }

    #[test]
    fn usage_bar_renders_without_price() {
        let theme = Theme::resolve("cyberpunk");
        let usage = UsageStats {
            cache_hit: 500,
            cache_miss: 500,
            completion: 200,
        };
        let state = ChatState::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock", &usage, None))
            .unwrap();
    }
}
