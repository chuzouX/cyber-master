//! Chat 模式状态：对话条目历史 + tui-textarea 输入框 + 流式缓冲。
//!
//! `ChatState` 持有 Chat 模式全部可变状态。`submit()` 把输入框文本转为一条 user
//! 条目并切到 streaming 态（清空输入框、置 `streaming=true`、返回待发送文本）。
//! 流式 token 由 App 的 `handle_agent_event` 写入 `streaming_buffer`；
//! ToolCall/ToolResult 事件先 flush buffer 为 assistant 条目再 push；
//! Done/Error 时 `finalize_stream` 把残留 buffer 定稿为 assistant 条目并退出 streaming 态。
//!
//! 跨轮历史（`history()`）只取 User/Assistant 文本，剥离 ToolCall/ToolResult
//! （工具链仅在单次 spawn 内部维护，避免历史膨胀 + provider 翻译复杂度）。

use cyber_agent::Message;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde::{Deserialize, Serialize};
use tui_textarea::TextArea;

use crate::theme::Theme;

/// 一条对话条目（user / assistant / 工具调用 / 工具结果 / system）。
///
/// `ToolCall` 带 `pending` 标记：结果未到时显示 `▶`，结果到达后由紧随的 `ToolResult`
/// 条目展示输出。工具链（ToolCall+ToolResult）不入跨轮历史。
///
/// `Serialize/Deserialize` 用于历史持久化（`~/.cyber/history/{cwd_hash}.json`）。
///
/// 用 **adjacently tagged**（`tag` + `content`）而非 internally tagged：后者无法
/// 序列化 newtype 变体（`User(String)` 等，payload 非 map）。adjacent 表示形如
/// `{"kind":"User","data":"你好"}` / `{"kind":"ToolCall","data":{...}}`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum ChatEntry {
    User(String),
    Assistant(String),
    /// 工具调用（agent 即将执行）。`arguments` 为 JSON 字符串。
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// 工具执行结果（含输出或错误）。
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    System(String),
}

/// Chat 模式状态。
pub struct ChatState {
    /// 已完成的对话条目（按时间序）。
    pub entries: Vec<ChatEntry>,
    /// 多行输入框。
    pub input: TextArea<'static>,
    /// 是否正在流式生成（true 时禁用提交）。
    pub streaming: bool,
    /// 当前流式响应的累积 buffer；Done 时转为 assistant 条目。
    pub streaming_buffer: String,
    /// 已完成条目的渲染行缓存（不含流式 tail 与空态引导）。
    /// 仅当 `entries.len()` 变化或 `cache_dirty`（如 theme 切换）时重建，
    /// 避免每帧重新 tokenize 全部条目。流式 tail 由 view 每帧追加（量小）。
    cached_history: Vec<Line<'static>>,
    /// 缓存对应的 `entries.len()`，不匹配时触发重建。
    cached_entries_len: usize,
    /// 强制重建标记（theme 切换等 entries.len() 不变但内容样式需刷新的场景）。
    cache_dirty: bool,
}

impl ChatState {
    pub fn new() -> Self {
        let mut input = TextArea::default();
        // 占位提示文本（样式在 render 时按当前 theme 应用，因 theme 可被 Settings 实时切换）
        input.set_placeholder_text("输入消息，Enter 发送，Shift+Enter 换行…");
        Self {
            entries: Vec::new(),
            input,
            streaming: false,
            streaming_buffer: String::new(),
            cached_history: Vec::new(),
            cached_entries_len: 0,
            cache_dirty: true,
        }
    }

    /// 在 draw 前（`&mut self` 上下文）调用：若 `entries.len()` 变化或缓存被标脏
    ///（如 theme 切换），则重建已完成条目的渲染行缓存。流式 tail 不入缓存，由 view
    /// 每帧追加。render 以 `&self` 经 `cached_history()` 只读复用，避免每帧重建。
    pub fn prepare_render(&mut self, theme: &Theme) {
        if self.cache_dirty || self.cached_entries_len != self.entries.len() {
            self.cached_history = render_entries(&self.entries, theme);
            self.cached_entries_len = self.entries.len();
            self.cache_dirty = false;
        }
    }

    /// 标记缓存需要重建（entries.len() 不变但样式需刷新时调用，如 theme 切换）。
    pub fn invalidate_cache(&mut self) {
        self.cache_dirty = true;
    }

    /// 已完成条目的渲染行（只读）。调用前须先经 `prepare_render` 刷新。
    pub fn cached_history(&self) -> &[Line<'static>] {
        &self.cached_history
    }

    /// 尝试提交输入：流式期或空输入返回 `None`；否则返回 `(待发送文本, 上下文历史)`，
    /// push user 条目（用于显示）、清空输入、置 streaming。
    ///
    /// 返回的 `history` **不含当前输入**——因为 agent 的 `run_stream` 会在内部把
    /// `user_input` append 到 history。若 history 含当前输入会导致重复。
    pub fn submit(&mut self) -> Option<(String, Vec<Message>)> {
        if self.streaming {
            return None;
        }
        let text: String = self.input.lines().join("\n");
        if text.trim().is_empty() {
            return None;
        }
        self.input.clear();
        // history 必须在 push 当前 user 条目之前取（run_stream 会再 append user_input）
        let history = self.history();
        self.entries.push(ChatEntry::User(text.clone()));
        self.streaming = true;
        self.streaming_buffer.clear();
        Some((text, history))
    }

    /// 把当前 `streaming_buffer` 定稿为一条 assistant 条目（仅 buffer 非空时 push），
    /// **不**改变 streaming 态。用于 ToolCall 到达前先把已累积的 assistant 文本落盘。
    pub fn flush_streaming_to_assistant(&mut self) {
        if !self.streaming_buffer.is_empty() {
            let content = std::mem::take(&mut self.streaming_buffer);
            self.entries.push(ChatEntry::Assistant(content));
        }
    }

    /// 收到一次工具调用事件：先 flush buffer（若有 assistant 前导文本），再 push ToolCall。
    pub fn push_tool_call(&mut self, id: String, name: String, arguments: String) {
        self.flush_streaming_to_assistant();
        self.entries.push(ChatEntry::ToolCall { id, name, arguments });
    }

    /// 收到工具结果事件：push ToolResult（紧随对应 ToolCall，无需 flush）。
    pub fn push_tool_result(&mut self, id: String, name: String, output: String, is_error: bool) {
        self.entries
            .push(ChatEntry::ToolResult { id, name, output, is_error });
    }

    /// 把当前 `streaming_buffer` 定稿为一条 assistant 条目，退出 streaming 态。
    /// buffer 为空时不 push（避免空 assistant 条目；工具链已有 ToolCall/ToolResult 记录）。
    pub fn finalize_stream(&mut self) {
        self.flush_streaming_to_assistant();
        self.streaming = false;
    }

    /// 取消流式（Esc）：丢弃 buffer，不追加 assistant 条目，退出 streaming 态。
    pub fn cancel_stream(&mut self) {
        self.streaming_buffer.clear();
        self.streaming = false;
    }

    /// 清空全部对话条目（`/clear` 命令）。不改变 streaming 态（流式期不允许清空）。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.streaming_buffer.clear();
    }

    /// 取走输入框文本（不清屏、不 push 条目、不改 streaming），用于斜杠命令。
    /// 返回的文本已去尾换行。
    pub fn take_input(&mut self) -> String {
        let text: String = self.input.lines().join("\n");
        self.input.clear();
        text
    }

    /// 把已完成的 User/Assistant 条目转为 agent `Message`（作为下一次请求的 history 上下文）。
    /// 剥离 ToolCall/ToolResult（工具链仅在单次 spawn 内部维护）。
    pub fn history(&self) -> Vec<Message> {
        self.entries
            .iter()
            .filter_map(|e| match e {
                ChatEntry::User(c) => Some(Message::user(c)),
                ChatEntry::Assistant(c) => Some(Message::assistant(c)),
                _ => None,
            })
            .collect()
    }
}

impl Default for ChatState {
    fn default() -> Self {
        Self::new()
    }
}

// ── 条目 → 渲染行 ───────────────────────────────────────────────────────────
// 以下函数把已完成 `ChatEntry` 转为带样式的 `Line<'static>`（含条目间空行）。
// 供 `ChatState::prepare_render` 缓存复用，`views::chat` 经 `cached_history()` 读取，
// 避免每帧重新 tokenize 全部条目。流式 tail（光标行）由 view 现场构建，不入此缓存。

/// 把已完成条目序列渲染为 `Line<'static>`（每条目后附一空行作分隔）。
pub fn render_entries(entries: &[ChatEntry], theme: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in entries {
        match entry {
            ChatEntry::User(content) => {
                push_role_lines(&mut lines, "[user]", theme.accent, content, theme);
            }
            ChatEntry::Assistant(content) => {
                push_role_lines(&mut lines, "[assistant]", theme.title, content, theme);
            }
            ChatEntry::System(content) => {
                push_role_lines(&mut lines, "[system]", theme.muted, content, theme);
            }
            ChatEntry::ToolCall {
                id: _,
                name,
                arguments,
            } => {
                push_tool_call(&mut lines, theme, name, arguments);
            }
            ChatEntry::ToolResult {
                id: _,
                name: _,
                output,
                is_error,
            } => {
                push_tool_result(&mut lines, theme, output, *is_error);
            }
        }
        lines.push(Line::from("")); // 条目间空行
    }
    lines
}

/// 把一条 user/assistant/system 文本按行展开为带标签的 `Line`。
/// 首行带 `label`（如 `[user]`），续行缩进对齐；空内容仍占一行标签。
fn push_role_lines(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    label_fg: Color,
    content: &str,
    theme: &Theme,
) {
    for (i, text_line) in content.lines().enumerate() {
        let prefix = if i == 0 { label } else { "        " };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{prefix} "),
                Style::default().fg(label_fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(text_line.to_string(), Style::default().fg(theme.fg)),
        ]));
    }
    // 空内容消息至少占一行
    if content.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            format!("{label} "),
            Style::default().fg(label_fg).add_modifier(Modifier::BOLD),
        )]));
    }
}

/// 渲染一次工具调用：`  ▶ [tool] name(arguments)`。
/// `▶` 与工具名用 accent 高亮，arguments 用 muted；空参 `{}` 显示为 `()`。
fn push_tool_call(lines: &mut Vec<Line<'static>>, theme: &Theme, name: &str, arguments: &str) {
    let mut spans = vec![
        Span::styled(
            "  ▶ ",
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("[tool] {name}"),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let args_display = if arguments.is_empty() || arguments == "{}" {
        "()".to_string()
    } else {
        format!("({arguments})")
    };
    spans.push(Span::styled(
        args_display,
        Style::default().fg(theme.muted),
    ));
    lines.push(Line::from(spans));
}

/// 渲染工具结果：成功 `    → output`（muted/fg），错误 `    ✗ output`（红色）。
/// 多行 output 逐行展开，续行缩进对齐首行内容；空 output 仍显示标记。
fn push_tool_result(
    lines: &mut Vec<Line<'static>>,
    theme: &Theme,
    output: &str,
    is_error: bool,
) {
    let (marker, marker_color, content_color) = if is_error {
        ("✗", Color::Red, Color::Red)
    } else {
        ("→", theme.muted, theme.fg)
    };
    let out_lines: Vec<&str> = output.lines().collect();
    if out_lines.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            format!("    {marker} "),
            Style::default()
                .fg(marker_color)
                .add_modifier(Modifier::BOLD),
        )]));
    } else {
        for (i, line) in out_lines.iter().enumerate() {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("    {marker} "),
                        Style::default()
                            .fg(marker_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(line.to_string(), Style::default().fg(content_color)),
                ]));
            } else {
                // 续行缩进对齐首行内容（6 空格 = "    {marker} " 宽度）
                lines.push(Line::from(vec![
                    Span::styled("      ", Style::default()),
                    Span::styled(line.to_string(), Style::default().fg(content_color)),
                ]));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyber_agent::Role;

    #[test]
    fn submit_returns_text_history_and_pushes_user_entry() {
        let mut s = ChatState::new();
        s.input.insert_str("你好");
        let (text, history) = s.submit().expect("非空非流式应返回 Some");
        assert_eq!(text, "你好");
        assert!(history.is_empty(), "首次提交 history 应为空（不含当前输入）");
        assert_eq!(s.entries.len(), 1);
        assert!(matches!(s.entries[0], ChatEntry::User(ref c) if c == "你好"));
        assert!(s.streaming, "submit 后应进入 streaming 态");
        assert!(s.input.lines().iter().all(|l| l.is_empty()), "输入框应清空");
    }

    #[test]
    fn submit_during_streaming_returns_none() {
        let mut s = ChatState::new();
        s.input.insert_str("x");
        s.submit();
        s.input.insert_str("y");
        let second = s.submit();
        assert!(second.is_none(), "流式期 submit 应返回 None");
    }

    #[test]
    fn submit_empty_returns_none() {
        let mut s = ChatState::new();
        assert!(s.submit().is_none());
        assert!(s.entries.is_empty());
        assert!(!s.streaming);
    }

    #[test]
    fn submit_whitespace_only_returns_none() {
        let mut s = ChatState::new();
        s.input.insert_str("   \n  ");
        assert!(s.submit().is_none());
        assert!(s.entries.is_empty());
    }

    #[test]
    fn submit_history_excludes_current_input() {
        // 已有一条 user + 一条 assistant 条目后，再次 submit
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("q1".into()));
        s.entries.push(ChatEntry::Assistant("a1".into()));
        s.input.insert_str("q2");
        let (text, history) = s.submit().unwrap();
        assert_eq!(text, "q2");
        assert_eq!(history.len(), 2, "history 应含 2 条先前消息，不含当前 q2");
        assert_eq!(history[0].content, "q1");
        assert_eq!(history[1].content, "a1");
        assert_eq!(s.entries.len(), 3, "entries 应含 3 条（含刚 push 的 q2）");
    }

    #[test]
    fn finalize_stream_appends_assistant_and_clears_buffer() {
        let mut s = ChatState::new();
        s.streaming = true;
        s.streaming_buffer.push_str("收到：hi");
        s.finalize_stream();
        assert!(!s.streaming);
        assert_eq!(s.entries.len(), 1);
        assert!(matches!(s.entries[0], ChatEntry::Assistant(ref c) if c == "收到：hi"));
        assert!(s.streaming_buffer.is_empty());
    }

    #[test]
    fn finalize_empty_buffer_no_entry() {
        // 空缓冲 finalize 不 push assistant 条目（避免空条目），但仍退出 streaming
        let mut s = ChatState::new();
        s.streaming = true;
        s.finalize_stream();
        assert!(!s.streaming);
        assert!(s.entries.is_empty(), "空 buffer 不应 push 条目");
    }

    #[test]
    fn cancel_stream_drops_buffer_without_entry() {
        let mut s = ChatState::new();
        s.streaming = true;
        s.streaming_buffer.push_str("部分");
        s.cancel_stream();
        assert!(!s.streaming);
        assert!(s.entries.is_empty());
        assert!(s.streaming_buffer.is_empty());
    }

    #[test]
    fn history_excludes_streaming_buffer() {
        let mut s = ChatState::new();
        s.input.insert_str("a");
        s.submit();
        s.streaming_buffer.push_str("部分");
        let h = s.history();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].role, Role::User);
        assert_eq!(h[0].content, "a");
    }

    #[test]
    fn history_strips_tool_entries() {
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("q".into()));
        s.entries.push(ChatEntry::ToolCall {
            id: "1".into(),
            name: "list_dir".into(),
            arguments: "{}".into(),
        });
        s.entries.push(ChatEntry::ToolResult {
            id: "1".into(),
            name: "list_dir".into(),
            output: "a.txt".into(),
            is_error: false,
        });
        s.entries.push(ChatEntry::Assistant("done".into()));
        let h = s.history();
        assert_eq!(h.len(), 2, "history 应只含 User+Assistant，剥离 Tool 条目");
        assert_eq!(h[0].content, "q");
        assert_eq!(h[1].content, "done");
    }

    #[test]
    fn multiline_input_joins_with_newline() {
        let mut s = ChatState::new();
        s.input.insert_str("第一行");
        s.input.insert_newline();
        s.input.insert_str("第二行");
        let (text, _) = s.submit().unwrap();
        assert_eq!(text, "第一行\n第二行");
    }

    #[test]
    fn push_tool_call_flushes_buffer_first() {
        let mut s = ChatState::new();
        s.streaming = true;
        s.streaming_buffer.push_str("让我看看");
        s.push_tool_call("c1".into(), "list_dir".into(), "{\"path\":\".\"}".into());
        // buffer 应被 flush 为 assistant 条目，再 push tool call
        assert_eq!(s.entries.len(), 2);
        assert!(matches!(s.entries[0], ChatEntry::Assistant(ref c) if c == "让我看看"));
        assert!(matches!(&s.entries[1], ChatEntry::ToolCall { name, .. } if name == "list_dir"));
        assert!(s.streaming_buffer.is_empty());
        assert!(s.streaming, "push_tool_call 不应退出 streaming");
    }

    #[test]
    fn push_tool_result_appends_after_call() {
        let mut s = ChatState::new();
        s.streaming = true;
        s.push_tool_call("c1".into(), "list_dir".into(), "{}".into());
        s.push_tool_result("c1".into(), "list_dir".into(), "a.txt".into(), false);
        assert_eq!(s.entries.len(), 2);
        assert!(matches!(&s.entries[1], ChatEntry::ToolResult { output, is_error, .. } if output == "a.txt" && !is_error));
    }

    #[test]
    fn clear_empties_entries() {
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("a".into()));
        s.entries.push(ChatEntry::Assistant("b".into()));
        s.clear();
        assert!(s.entries.is_empty());
    }

    #[test]
    fn take_input_returns_text_and_clears() {
        let mut s = ChatState::new();
        s.input.insert_str("hello");
        let text = s.take_input();
        assert_eq!(text, "hello");
        assert!(s.input.lines().join("").is_empty());
        assert!(!s.streaming, "take_input 不应改 streaming");
        assert!(s.entries.is_empty(), "take_input 不 push 条目");
    }

    #[test]
    fn prepare_render_caches_entries_lines() {
        let theme = Theme::resolve("cyberpunk");
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("你好".into()));
        s.entries.push(ChatEntry::Assistant("收到".into()));
        // 首次 prepare：缓存为空 + dirty → 重建
        s.prepare_render(&theme);
        let cached = s.cached_history();
        assert!(!cached.is_empty(), "有条目则缓存非空");
        // render_entries 在每条目后附一空行；2 条目 → 至少 4 行（含分隔空行）
        assert!(cached.len() >= 4);
    }

    #[test]
    fn prepare_render_rebuilds_only_on_entry_count_change() {
        let theme = Theme::resolve("cyberpunk");
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("q".into()));
        s.prepare_render(&theme);
        let len_after_first = s.cached_history().len();
        // 无 entries 变化再次 prepare：缓存长度不变（复用，不重建）
        s.prepare_render(&theme);
        assert_eq!(s.cached_history().len(), len_after_first);
        // 新增条目 → 重建，缓存增长
        s.entries.push(ChatEntry::Assistant("a".into()));
        s.prepare_render(&theme);
        assert!(
            s.cached_history().len() > len_after_first,
            "条目增加后缓存应重建并增长"
        );
    }

    #[test]
    fn invalidate_cache_forces_rebuild_on_theme_change() {
        let theme_a = Theme::resolve("cyberpunk");
        let theme_b = Theme::resolve("nord");
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("hi".into()));
        s.prepare_render(&theme_a);
        // entries.len() 不变，仅 theme 变 → 须 invalidate 才会重建
        s.invalidate_cache();
        s.prepare_render(&theme_b);
        // 重建后缓存仍非空（内容样式已用 theme_b，这里仅验证重建发生不 panic）
        assert!(!s.cached_history().is_empty());
    }

    #[test]
    fn chatentry_serde_adjacent_tag_roundtrip() {
        // 验证 adjacently-tagged 表示可正确往返（含 newtype 与 struct 变体）
        let entries = vec![
            ChatEntry::User("你好".into()),
            ChatEntry::Assistant("multi\nline".into()),
            ChatEntry::ToolCall {
                id: "c1".into(),
                name: "list_dir".into(),
                arguments: "{\"path\":\".\"}".into(),
            },
            ChatEntry::ToolResult {
                id: "c1".into(),
                name: "list_dir".into(),
                output: "a.txt".into(),
                is_error: true,
            },
            ChatEntry::System("sys".into()),
        ];
        let json = serde_json::to_string(&entries).expect("序列化应成功");
        // adjacent tag：每个对象含 "kind" 与 "data"
        assert!(json.contains("\"kind\":\"User\""), "json: {json}");
        assert!(json.contains("\"kind\":\"ToolCall\""), "json: {json}");
        assert!(json.contains("\"data\":\"你好\""), "json: {json}");
        let loaded: Vec<ChatEntry> = serde_json::from_str(&json).expect("反序列化应成功");
        assert_eq!(loaded.len(), entries.len());
        assert!(matches!(&loaded[0], ChatEntry::User(c) if c == "你好"));
        assert!(
            matches!(&loaded[3], ChatEntry::ToolResult { is_error, output, .. } if *is_error && output == "a.txt")
        );
    }
}
