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
                break 'outer;
            }
            used += w;
            text.push(ch);
        }
        if !text.is_empty() {
            out.push(Span::styled(text, span.style));
        }
    }
    if truncated {
        out.push(Span::styled("…", last_style));
    }
    out
}
