//! TUI 视图：每个模式一个渲染函数（P1 占位级）。
//!
//! P1 不引入 `View` trait，所有视图为纯函数 `fn render(frame, area, theme, ...)`，
//! 状态全部集中在 [`crate::app::App`]，便于 P2+ 演进为 trait + 状态机。

pub mod about;
pub mod chat;
pub mod ctf_edit_form;
pub mod ctf_panel;
pub mod env_form;
pub mod mcp_form;
pub mod mcp_panel;
pub mod memory_rule_form;
pub mod model_picker;
pub mod providers;
pub mod sessions;
pub mod settings;
pub mod welcome;

use ratatui::{style::Style, text::Span};

/// 按显示宽度截断并追加 `…`（CJK 宽字符按 2 列计）。
///
/// 视图内的文本行/面板标题一律用它约束宽度，避免内容侵入边框列。
pub(crate) fn clip_cells_ellipsis(text: &str, width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let target = width.saturating_sub(1);
    let mut used = 0;
    let mut result: String = text
        .chars()
        .take_while(|ch| {
            used += if *ch == '\t' {
                4
            } else {
                ch.width().unwrap_or(0)
            };
            used <= target
        })
        .collect();
    result.push('…');
    result
}

/// 依次写入 `spans`，累计视觉宽度不超过 `budget`；超出部分截断并补 `…`。
pub(crate) fn clipped_spans(spans: Vec<Span<'static>>, budget: usize) -> Vec<Span<'static>> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let total: usize = spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    if total <= budget {
        return spans;
    }
    if budget == 0 {
        return Vec::new();
    }
    let limit = budget.saturating_sub(1);
    let mut used = 0usize;
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut last_style = Style::default();
    let mut truncated = false;
    'outer: for span in spans {
        last_style = span.style;
        let mut text = String::new();
        for ch in span.content.chars() {
            let w = if ch == '\t' {
                4
            } else {
                ch.width().unwrap_or(0)
            };
            if used + w > limit {
                truncated = true;
                break;
            }
            used += w;
            text.push(ch);
        }
        // 预算耗尽发生在某个 span 内部时，必须保留该 span 已放下的前缀，
        // 否则整段（如超长描述）会连同其后所有 span 一起消失，只剩「…」。
        if !text.is_empty() {
            out.push(Span::styled(text, span.style));
        }
        if truncated {
            break 'outer;
        }
    }
    if truncated {
        out.push(Span::styled("…", last_style));
    }
    out
}

/// 任务清单可视窗口：`items[start .. start + len]` 为窗口内条目，其余条数分别计入
/// `hidden_above` / `hidden_below`（用于折叠提示文案）。
pub(crate) struct TodoWindow {
    /// 窗口首条在 `items` 中的下标
    pub start: usize,
    /// 窗口内条数
    pub len: usize,
    /// 窗口上方被折叠的条数（== `start`）
    pub hidden_above: usize,
    /// 窗口下方被折叠的条数
    pub hidden_below: usize,
}

/// 「当前进度」行下标：第一个 `InProgress`；无进行中时取最后一条 `Completed`/`Failed`
/// （进度指针）；全为 `Pending`（或空）时取 0。
pub(crate) fn todo_focus_index(items: &[cyber_core::TodoItem]) -> usize {
    use cyber_core::TodoStatus;
    if let Some(i) = items
        .iter()
        .position(|t| t.status == TodoStatus::InProgress)
    {
        return i;
    }
    if let Some(i) = items
        .iter()
        .rposition(|t| matches!(t.status, TodoStatus::Completed | TodoStatus::Failed))
    {
        return i;
    }
    0
}

/// 任务清单可视窗口：优先让「当前进度」行可见，且滚动量最小（窗口尾部贴住焦点行，
/// 即上方保留尽可能多的已完成上下文）。
///
/// - `rows` 为面板内区可用行数；`rows == 0` 或 `items` 为空 → 空窗口（`len == 0`）。
/// - `items.len() <= rows` → 全量显示，`hidden_above == hidden_below == 0`（与改动前一致）。
/// - 否则预留**最后 1 行**给折叠提示（可见 `rows - 1` 条），`start` 先取
///   `focus.saturating_sub(visible - 1)`，再夹紧到 `items.len() - visible`；
///   `visible == 0`（`rows == 1`，只剩提示行）时 `start = focus`。
pub(crate) fn todo_visible_window(items: &[cyber_core::TodoItem], rows: usize) -> TodoWindow {
    let total = items.len();
    if total == 0 || rows == 0 {
        return TodoWindow {
            start: 0,
            len: 0,
            hidden_above: 0,
            hidden_below: total,
        };
    }
    if total <= rows {
        return TodoWindow {
            start: 0,
            len: total,
            hidden_above: 0,
            hidden_below: 0,
        };
    }
    let visible = rows - 1;
    let focus = todo_focus_index(items);
    let start = if visible == 0 {
        focus.min(total)
    } else {
        focus.saturating_sub(visible - 1).min(total - visible)
    };
    TodoWindow {
        start,
        len: visible,
        hidden_above: start,
        hidden_below: total - start - visible,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyber_core::{TodoItem, TodoStatus};

    fn item(id: &str, status: TodoStatus) -> TodoItem {
        TodoItem::new(id, "t", status)
    }

    #[test]
    fn todo_focus_index_prefers_first_in_progress() {
        use TodoStatus::{Completed, InProgress, Pending};
        let a = vec![
            item("1", Completed),
            item("2", InProgress),
            item("3", Pending),
        ];
        assert_eq!(todo_focus_index(&a), 1);
        let b = vec![
            item("1", Completed),
            item("2", InProgress),
            item("3", Pending),
            item("4", InProgress),
        ];
        assert_eq!(todo_focus_index(&b), 1);
    }

    #[test]
    fn todo_focus_index_falls_back_to_last_finished() {
        use TodoStatus::{Completed, Failed, Pending};
        let a = vec![
            item("1", Pending),
            item("2", Completed),
            item("3", Completed),
            item("4", Pending),
        ];
        assert_eq!(todo_focus_index(&a), 2);
        let b = vec![item("1", Completed), item("2", Failed), item("3", Pending)];
        assert_eq!(todo_focus_index(&b), 1);
        let c = vec![item("1", Pending), item("2", Pending)];
        assert_eq!(todo_focus_index(&c), 0);
    }

    #[test]
    fn todo_visible_window_shows_all_when_fits() {
        use TodoStatus::Pending;
        let items = vec![item("1", Pending), item("2", Pending), item("3", Pending)];
        let w = todo_visible_window(&items, 5);
        assert_eq!(
            (w.start, w.len, w.hidden_above, w.hidden_below),
            (0, 3, 0, 0)
        );
    }

    #[test]
    fn todo_visible_window_follows_focus_row() {
        use TodoStatus::{Completed, InProgress, Pending};
        // 10 条，指定下标为 InProgress，其余按给定状态填充
        let build = |focus: usize, tail: &[TodoStatus]| -> Vec<TodoItem> {
            (0..10)
                .map(|i| {
                    let status = if i == focus {
                        InProgress
                    } else {
                        tail.get(i).copied().unwrap_or(Completed)
                    };
                    item(&(i + 1).to_string(), status)
                })
                .collect()
        };

        let w = todo_visible_window(&build(6, &[]), 5);
        assert_eq!(
            (w.start, w.len, w.hidden_above, w.hidden_below),
            (3, 4, 3, 3)
        );

        let w = todo_visible_window(&build(9, &[]), 5);
        assert_eq!(
            (w.start, w.len, w.hidden_above, w.hidden_below),
            (6, 4, 6, 0)
        );

        // 尾部三条为 Pending，验证焦点在第 1 条时窗口从 0 开始
        let mut tail = vec![Completed; 10];
        tail[7] = Pending;
        tail[8] = Pending;
        tail[9] = Pending;
        let w = todo_visible_window(&build(0, &tail), 5);
        assert_eq!(
            (w.start, w.len, w.hidden_above, w.hidden_below),
            (0, 4, 0, 6)
        );
    }

    #[test]
    fn todo_visible_window_edges() {
        use TodoStatus::Pending;
        let items: Vec<TodoItem> = (1..=4).map(|i| item(&i.to_string(), Pending)).collect();

        let w = todo_visible_window(&items, 1);
        assert_eq!(
            (w.start, w.len, w.hidden_above, w.hidden_below),
            (0, 0, 0, 4)
        );

        let w = todo_visible_window(&items, 0);
        assert_eq!(
            (w.start, w.len, w.hidden_above, w.hidden_below),
            (0, 0, 0, 4)
        );

        let empty: Vec<TodoItem> = Vec::new();
        let w = todo_visible_window(&empty, 5);
        assert_eq!(
            (w.start, w.len, w.hidden_above, w.hidden_below),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn clipped_spans_keeps_truncated_span_prefix() {
        use unicode_width::UnicodeWidthStr;
        // 首个长字段（工具描述）超出预算时必须保留其可见前缀：
        // 旧实现 break 出整个循环，把该字段与后续字段（命令行）一起丢掉，只剩「…」。
        let out = clipped_spans(
            vec![
                Span::raw("▶ "),
                Span::raw("[fenjing_crack] "),
                Span::raw("Fenjing 攻击指定表单参数:数据中注入点写 PAYLOAD,自动检测 WAF"),
                Span::raw("  · cd /d D:\\CTF && python -m fenjing crack"),
            ],
            60,
        );
        let text: String = out.iter().map(|s| s.content.to_string()).collect();
        assert!(
            text.starts_with("▶ [fenjing_crack] Fenjing"),
            "截断字段的已放下前缀必须保留: {text}"
        );
        assert!(text.ends_with('…'), "截断必须补省略号: {text}");
        assert_eq!(
            UnicodeWidthStr::width(text.as_str()),
            60,
            "可见宽度必须恰好等于预算: {text}"
        );
    }

    #[test]
    fn clipped_spans_returns_input_when_it_fits() {
        let spans = vec![Span::raw("ab"), Span::raw("cd")];
        let out = clipped_spans(spans, 4);
        assert_eq!(out.len(), 2);
        let text: String = out.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "abcd");
    }

    #[test]
    fn clipped_spans_truncates_at_span_boundary() {
        use unicode_width::UnicodeWidthStr;
        // 第一个 span 恰好用尽预算：后续 span 整体剔除并补省略号，不产生半个宽字符。
        let out = clipped_spans(vec![Span::raw("abcd"), Span::raw("中文内容")], 4);
        let text: String = out.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "abc…");
        assert_eq!(UnicodeWidthStr::width(text.as_str()), 4);
    }
}
