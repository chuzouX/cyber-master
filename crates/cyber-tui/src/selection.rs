use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// 逻辑内容坐标：指向具体逻辑行与 Unicode 字符偏移量。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentCoord {
    /// 逻辑消息行索引（对应 `source[line_idx]`）。
    pub line_idx: usize,
    /// 该逻辑行内的 Unicode 字符偏移量（`chars()` 索引，杜绝 UTF-8 字节截断）。
    pub char_offset: usize,
}

impl ContentCoord {
    pub const fn new(line_idx: usize, char_offset: usize) -> Self {
        Self {
            line_idx,
            char_offset,
        }
    }
}

/// 文本选区状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextSelection {
    /// 选区固定起点（可持久保留在滚出视口的内容坐标上）。
    pub anchor: ContentCoord,
    /// 选区活动终点（随鼠标拖拽或 Shift 点击更新）。
    pub cursor: ContentCoord,
    /// 是否正在进行拖拽选择。
    pub selecting: bool,
}

impl TextSelection {
    /// 以指定坐标初始化选区。
    pub fn new(coord: ContentCoord) -> Self {
        Self {
            anchor: coord,
            cursor: coord,
            selecting: true,
        }
    }

    /// 返回规范化的选区闭开区间 `(start, end)`，保证 `start <= end`。
    pub fn range(&self) -> (ContentCoord, ContentCoord) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// 判断选区是否为空（起点与终点相同）。
    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }

    /// 保持 anchor 不动，扩展选区终点至指定坐标。
    pub fn extend_to(&mut self, coord: ContentCoord) {
        self.cursor = coord;
    }
}

/// 默认文本选区高亮样式：深蓝底高亮白字粗体。
pub fn default_selection_style() -> Style {
    Style::default()
        .bg(Color::Rgb(38, 79, 120))
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

/// 根据选区跨行提取纯文本，中间行以 `\n` 连接。
#[allow(clippy::needless_range_loop)]
pub fn extract_text(source_lines: &[Line<'_>], sel: &TextSelection) -> String {
    if sel.is_empty() || source_lines.is_empty() {
        return String::new();
    }
    let (start, end) = sel.range();
    if start.line_idx >= source_lines.len() {
        return String::new();
    }
    let end_line_idx = end.line_idx.min(source_lines.len() - 1);
    let mut result_lines = Vec::with_capacity(end_line_idx - start.line_idx + 1);

    for line_idx in start.line_idx..=end_line_idx {
        let line = &source_lines[line_idx];
        let line_chars: Vec<char> = line.spans.iter().flat_map(|s| s.content.chars()).collect();
        let line_len = line_chars.len();

        if start.line_idx == end.line_idx {
            let from = start.char_offset.min(line_len);
            let to = end.char_offset.min(line_len);
            if from < to {
                result_lines.push(line_chars[from..to].iter().collect::<String>());
            } else {
                result_lines.push(String::new());
            }
        } else if line_idx == start.line_idx {
            let from = start.char_offset.min(line_len);
            result_lines.push(line_chars[from..].iter().collect::<String>());
        } else if line_idx == end_line_idx {
            let to = end.char_offset.min(line_len);
            result_lines.push(line_chars[..to].iter().collect::<String>());
        } else {
            result_lines.push(line_chars.iter().collect::<String>());
        }
    }

    result_lines.join("\n")
}

/// 计算可见折行与当前选区的交集字符区间，将落入区间的 Span 精准切分并赋予选区样式。
pub fn apply_selection_to_row(
    row: &Line<'static>,
    row_char_start: usize,
    row_char_end: usize,
    line_idx: usize,
    sel: &TextSelection,
    sel_style: Style,
) -> Line<'static> {
    apply_selection_to_row_full(
        row,
        row_char_start,
        row_char_end,
        line_idx,
        sel,
        sel_style,
        0,
        None,
    )
}

/// 支持指定左侧 padding 与 source_line 的完整选区高亮应用函数。
#[allow(clippy::too_many_arguments)]
pub fn apply_selection_to_row_full(
    row: &Line<'static>,
    row_char_start: usize,
    row_char_end: usize,
    line_idx: usize,
    sel: &TextSelection,
    sel_style: Style,
    padding: usize,
    source_line: Option<&Line<'static>>,
) -> Line<'static> {
    if sel.is_empty() {
        return row.clone();
    }
    let (sel_start, sel_end) = sel.range();
    if line_idx < sel_start.line_idx || line_idx > sel_end.line_idx {
        return row.clone();
    }

    let sel_char_start = if line_idx == sel_start.line_idx {
        sel_start.char_offset
    } else {
        0
    };
    let sel_char_end = if line_idx == sel_end.line_idx {
        sel_end.char_offset
    } else {
        usize::MAX
    };

    let intersect_start = row_char_start.max(sel_char_start);
    let intersect_end = row_char_end.min(sel_char_end);
    if intersect_start >= intersect_end {
        return row.clone();
    }

    // 计算逻辑字符区间在当前折行内容中的相对字符偏移
    let (row_start, row_end) = if let Some(src) = source_line {
        let mut r_start = 0;
        let mut r_end = 0;
        let mut idx = 0;
        for span in &src.spans {
            for ch in span.content.chars() {
                let w = if ch == '\t' { 4 } else { 1 };
                if idx >= row_char_start && idx < intersect_start {
                    r_start += w;
                }
                if idx >= row_char_start && idx < intersect_end {
                    r_end += w;
                }
                idx += 1;
                if idx >= intersect_end {
                    break;
                }
            }
            if idx >= intersect_end {
                break;
            }
        }
        (r_start, r_end)
    } else {
        (
            intersect_start - row_char_start,
            intersect_end - row_char_start,
        )
    };

    if row_start >= row_end {
        return row.clone();
    }

    let mut new_spans = Vec::with_capacity(row.spans.len() + 2);
    let mut span_idx_start = 0;

    // 保留前置 padding span
    if padding > 0 && !row.spans.is_empty() {
        let first = &row.spans[0];
        if first.content.chars().count() == padding && first.content.chars().all(|c| c == ' ') {
            new_spans.push(first.clone());
            span_idx_start = 1;
        }
    }

    let mut current_pos = 0;
    for span in &row.spans[span_idx_start..] {
        let span_chars: Vec<char> = span.content.chars().collect();
        let span_len = span_chars.len();
        let span_end = current_pos + span_len;

        if span_end <= row_start || current_pos >= row_end {
            // 完全在选区前或选区后
            new_spans.push(span.clone());
        } else {
            // 与选区存在交集
            let overlap_from = row_start.saturating_sub(current_pos).min(span_len);
            let overlap_to = row_end.saturating_sub(current_pos).min(span_len);

            if overlap_from > 0 {
                let prefix: String = span_chars[..overlap_from].iter().collect();
                new_spans.push(Span::styled(prefix, span.style));
            }
            if overlap_from < overlap_to {
                let selected: String = span_chars[overlap_from..overlap_to].iter().collect();
                new_spans.push(Span::styled(selected, span.style.patch(sel_style)));
            }
            if overlap_to < span_len {
                let suffix: String = span_chars[overlap_to..].iter().collect();
                new_spans.push(Span::styled(suffix, span.style));
            }
        }
        current_pos = span_end;
    }

    Line {
        spans: new_spans,
        style: row.style,
        alignment: row.alignment,
    }
}

/// 安全地将文本写入系统剪贴板（容错静默处理无桌面环境或锁竞争）。
pub fn set_clipboard_text(text: &str) -> bool {
    let mut cb_res = arboard::Clipboard::new();
    for _ in 0..5 {
        if cb_res.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
        cb_res = arboard::Clipboard::new();
    }
    if let Ok(mut cb) = cb_res {
        cb.set_text(text.to_string()).is_ok()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn test_coord_ordering_and_range() {
        let c1 = ContentCoord::new(1, 5);
        let c2 = ContentCoord::new(1, 10);
        let c3 = ContentCoord::new(2, 0);

        assert!(c1 < c2);
        assert!(c2 < c3);

        let sel_forward = TextSelection {
            anchor: c1,
            cursor: c2,
            selecting: true,
        };
        assert_eq!(sel_forward.range(), (c1, c2));
        assert!(!sel_forward.is_empty());

        let sel_backward = TextSelection {
            anchor: c2,
            cursor: c1,
            selecting: true,
        };
        assert_eq!(sel_backward.range(), (c1, c2));

        let sel_empty = TextSelection::new(c1);
        assert!(sel_empty.is_empty());
        assert_eq!(sel_empty.range(), (c1, c1));
    }

    #[test]
    fn test_extract_text_single_and_multi_line() {
        let lines = vec![
            Line::from("Hello, World!"),
            Line::from("Second line of text."),
            Line::from("Third and final line."),
        ];

        // 单行切片
        let sel1 = TextSelection {
            anchor: ContentCoord::new(0, 7),
            cursor: ContentCoord::new(0, 12),
            selecting: false,
        };
        assert_eq!(extract_text(&lines, &sel1), "World");

        // 跨两行
        let sel2 = TextSelection {
            anchor: ContentCoord::new(0, 7),
            cursor: ContentCoord::new(1, 6),
            selecting: false,
        };
        assert_eq!(extract_text(&lines, &sel2), "World!\nSecond");

        // 跨三行
        let sel3 = TextSelection {
            anchor: ContentCoord::new(0, 7),
            cursor: ContentCoord::new(2, 5),
            selecting: false,
        };
        assert_eq!(
            extract_text(&lines, &sel3),
            "World!\nSecond line of text.\nThird"
        );
    }

    #[test]
    fn test_apply_selection_to_row_slicing() {
        let style_default = Style::default().fg(Color::Gray);
        let row = Line::from(vec![
            Span::styled("Hello ", style_default),
            Span::styled("Cyber Master", style_default),
            Span::styled("!", style_default),
        ]);
        let sel_style = Style::default().bg(Color::Blue);

        // 选中 "lo Cyber" -> 位于第 3 到第 11 字符
        let sel = TextSelection {
            anchor: ContentCoord::new(0, 3),
            cursor: ContentCoord::new(0, 11),
            selecting: false,
        };

        let result = apply_selection_to_row(&row, 0, 19, 0, &sel, sel_style);
        let text: String = result.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Hello Cyber Master!");

        // 检查 span 样式切分
        // 0..3: "Hel" (style_default)
        // 3..6: "lo " (sel_style)
        // 6..11: "Cyber" (sel_style)
        // 11..18: " Master" (style_default)
        // 18..19: "!" (style_default)
        assert_eq!(result.spans[0].content, "Hel");
        assert_eq!(result.spans[0].style, style_default);

        assert_eq!(result.spans[1].content, "lo ");
        assert_eq!(result.spans[1].style, style_default.patch(sel_style));

        assert_eq!(result.spans[2].content, "Cyber");
        assert_eq!(result.spans[2].style, style_default.patch(sel_style));

        assert_eq!(result.spans[3].content, " Master");
        assert_eq!(result.spans[3].style, style_default);
    }
}
