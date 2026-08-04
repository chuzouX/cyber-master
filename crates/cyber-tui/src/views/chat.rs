//! Chat 视图：消息历史区 + 流式 buffer + tui-textarea 输入框。
//!
//! P2.2 重写：历史区遍历 `ChatEntry`（User/Assistant/ToolCall/ToolResult/System），
//! 工具调用与结果以 `▶`/`→`/`✗` 标记渲染，错误结果用红色高亮。流式 buffer 仍作为
//! 末尾进行中 assistant 消息（带 `▌` 光标）。布局：
//! ```text
//! ┌─ Chat ────────────────────┐
//! │ [user] ...                 │  ← 历史 + 流式 buffer（Min）
//! │ [assistant] ...            │
//! │   ▶ [tool] list_dir({...}) │  ← 工具调用
//! │     → a.txt                │  ← 工具结果（✗ 红色表示错误）
//! │ [assistant] 收到：…▌        │  ← streaming 时追加进行中消息（带光标）
//! ├─────────────────────────────┤
//! │ 输入 / 生成中…              │  ← textarea（Length 3）
//! ├─────────────────────────────┤
//! │ Enter 发送  Shift+Enter 换行 │  ← hint（Length 1）
//! └─────────────────────────────┘
//! ```
//! `render_placeholder` 保留供 Workflow/Dashboard 占位页复用。

use cyber_core::ProjectContext;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph, Wrap},
    Frame,
};

use crate::chat::ChatState;
use crate::theme::Theme;

/// 渲染 Chat 主视图。
pub fn render(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &ChatState,
    project: Option<&ProjectContext>,
    provider: &str,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(
            Line::from(format!(" Chat · provider={provider} "))
                .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 历史（弹性） / 输入框（3 行含边框） / hint（1 行）
    let chunks = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(inner);

    render_history(frame, chunks[0], theme, state, project);
    render_input(frame, chunks[1], state);
    render_hint(frame, chunks[2], theme, state);
}

/// 渲染消息历史区：已完成的 ChatEntry（user/assistant/tool/system）+ 流式中的 buffer。
fn render_history(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &ChatState,
    project: Option<&ProjectContext>,
) {
    let mut lines: Vec<Line> = Vec::new();

    // 空会话时显示项目上下文摘要作为引导
    if state.entries.is_empty() && !state.streaming {
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
    } else {
        // 复用缓存（已完成条目行，由 ChatState::prepare_render 在 draw 前维护）。
        // 若缓存未就绪（直接调用 render 而未先 prepare_render，如单元测试），回退现场构建，
        // 避免空渲染。生产主循环每帧先经 style_chat_input→prepare_render，缓存恒就绪。
        let cached = state.cached_history();
        if cached.is_empty() && !state.entries.is_empty() {
            lines.extend(crate::chat::render_entries(&state.entries, theme));
        } else {
            lines.extend_from_slice(cached);
        }
        // 流式进行中：把 buffer 作为末尾进行中 assistant 消息（带 ▌ 光标）
        if state.streaming {
            push_streaming_tail(&mut lines, theme, &state.streaming_buffer);
        }
    }

    let history = Paragraph::new(lines).wrap(Wrap { trim: false });
    // 用 Padding 让内容不贴边
    let padded = Block::default().padding(Padding::new(1, 1, 0, 0));
    let inner = padded.inner(area);
    frame.render_widget(padded, area);
    // 自动滚动到底部：line_count 含 wrap 折行后的实际行数，与可视高度取差即底部偏移，
    // 保证流式新内容始终可见（否则超屏后顶部之外的内容不可见）。
    let total = history.line_count(inner.width);
    let visible = inner.height as usize;
    let scroll = total.saturating_sub(visible).min(u16::MAX as usize) as u16;
    frame.render_widget(history.scroll((scroll, 0)), inner);
}

/// 渲染流式 tail：把 `streaming_buffer` 作为进行中的 assistant 消息（末行带 ▌ 光标）。
/// 空 buffer 显示等待光标。每帧现场构建（量小，不入缓存）。
fn push_streaming_tail(lines: &mut Vec<Line>, theme: &Theme, buffer: &str) {
    let prefix = "[assistant]";
    let buf_lines: Vec<&str> = buffer.lines().collect();
    if buf_lines.is_empty() {
        // 还没收到 token，显示等待光标
        lines.push(Line::from(vec![
            Span::styled(
                format!("{prefix} "),
                Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
            ),
            Span::styled("▌", Style::default().fg(theme.accent)),
        ]));
    } else {
        for (i, text_line) in buf_lines.iter().enumerate() {
            let prefix_str = if i == 0 { prefix } else { "        " };
            let is_last = i == buf_lines.len() - 1;
            let mut spans = vec![Span::styled(
                format!("{prefix_str} "),
                Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
            )];
            spans.push(Span::styled(
                text_line.to_string(),
                Style::default().fg(theme.fg),
            ));
            if is_last {
                spans.push(Span::styled("▌", Style::default().fg(theme.accent)));
            }
            lines.push(Line::from(spans));
        }
    }
}

/// 渲染输入框。
///
/// textarea 的边框/样式（含 streaming 态切换）由 `App::style_chat_input` 在 draw 前
/// 以 `&mut self` 应用（`set_block`/`set_style` 需 `&mut`，而 render 是 `&self`）。
/// 此处仅渲染已配置好的 textarea。
fn render_input(frame: &mut Frame, area: Rect, state: &ChatState) {
    frame.render_widget(&state.input, area);
}

/// 渲染底部 hint 行。
fn render_hint(frame: &mut Frame, area: Rect, theme: &Theme, state: &ChatState) {
    let hint = if state.streaming {
        " 生成中… Esc 取消 · Tab 切换模式 · Ctrl+, 设置 · Ctrl+C 退出"
    } else {
        " Enter 发送 · Shift+Enter 换行 · Tab 切换模式 · Ctrl+, 设置 · Esc 返回 · Ctrl+C 退出"
    };
    frame.render_widget(
        Paragraph::new(Line::from(hint)).style(Style::default().fg(theme.muted)),
        area,
    );
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
    use crate::chat::{ChatEntry, ChatState};
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn empty_chat_renders_without_panic() {
        let theme = Theme::resolve("cyberpunk");
        let state = ChatState::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock"))
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
            .draw(|f| render(f, f.area(), &theme, &state, None, "openai"))
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
            .draw(|f| render(f, f.area(), &theme, &state, None, "ollama"))
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
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock"))
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
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock"))
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
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock"))
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
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock"))
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
            .draw(|f| render(f, f.area(), &theme, &state, None, "mock"))
            .unwrap();
    }
}
