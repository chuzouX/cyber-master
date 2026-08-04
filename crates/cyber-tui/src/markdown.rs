//! 轻量 Markdown → ratatui `Line<'static>` 渲染器。
//!
//! 覆盖 LLM 输出常见子集：标题(`#`)、代码块(``` ``` ```)、行内代码(`` ` ``)、
//! 粗体(`**`)、粗斜体(`***`)、斜体(`*`)、下划线(`<u>`)、删除线(`~~`)、链接(`[t](u)`)、
//! 列表(`-`/`*`/`+`/`数字.`)、引用(`>`)、分隔线(`---`)、数学公式（行内 `$...$`、
//! 块级 `$$...$$`）。非完整 CommonMark：不处理嵌套列表、表格、HTML（`<u>`/`<ins>` 例外，
//! 作下划线语法糖）。
//!
//! 流式 buffer 可能为不完整 markdown（未闭合 `**` 或未闭合 ``` ``` ```），每帧重新解析
//!（量小）；未闭合格式降级为纯文本，不会 panic。
//!
//! 不引入外部 markdown crate：TUI 只需 span 级样式，手写解析器可精确映射到主题色
//! 且避免依赖膨胀（与项目自实现 FNV hash / SSE 行缓冲一致）。

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::theme::Theme;

/// Markdown 渲染所需的颜色集，由 `Theme` 派生（不新增 Theme 字段，保持主题结构稳定）。
struct MdColors {
    fg: Color,
    code_fg: Color,
    header: Color,
    link: Color,
    quote: Color,
    list: Color,
    hr: Color,
    /// 数学公式色（行内 `$...$` 与块级 `$$...$$`）。TUI 无法渲染 LaTeX，
    /// 用斜体 + accent 色标记原始公式文本，与代码块（DIM）视觉区分。
    math: Color,
}

impl MdColors {
    fn from_theme(t: &Theme) -> Self {
        Self {
            fg: t.fg,
            code_fg: t.title,
            header: t.accent,
            link: t.accent,
            quote: t.muted,
            list: t.accent,
            hr: t.muted,
            math: t.accent,
        }
    }
}

/// 把一段 markdown 文本渲染为带样式的 `Line<'static>` 列表。
///
/// 空文本返回空 vec（由调用方决定是否补占位行）。块级与行内均不 panic；未闭合的
/// 标记降级为纯文本。
pub fn render(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let md = MdColors::from_theme(theme);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_code = false;
    let mut code_lines: Vec<String> = Vec::new();
    let mut in_math = false;
    let mut math_lines: Vec<String> = Vec::new();

    for raw in text.lines() {
        if in_code {
            if is_fence_line(raw) {
                flush_code_block(&mut out, &md, &code_lines);
                in_code = false;
                code_lines.clear();
            } else {
                code_lines.push(raw.to_string());
            }
            continue;
        }
        // 块级数学：独占一行的 $$ 起止（单行 $$x^2$$ 由行内解析处理）
        if in_math {
            if raw.trim() == "$$" {
                flush_math_block(&mut out, &md, &math_lines);
                in_math = false;
                math_lines.clear();
            } else {
                math_lines.push(raw.to_string());
            }
            continue;
        }
        if raw.trim() == "$$" {
            in_math = true;
            math_lines.clear();
            continue;
        }
        if is_fence_line(raw) {
            in_code = true;
            code_lines.clear();
            continue;
        }
        out.push(render_block_line(raw, &md));
    }
    // 未闭合代码块 / 数学块（流式中常见）：把已累积的行渲染出来
    if in_code {
        flush_code_block(&mut out, &md, &code_lines);
    }
    if in_math {
        flush_math_block(&mut out, &md, &math_lines);
    }
    out
}

/// 渲染块级数学公式：每行缩进 2 空格 + math 色 + 斜体（TUI 无法渲染 LaTeX，
/// 原样展示公式文本，仅作视觉标记）。
fn flush_math_block(out: &mut Vec<Line<'static>>, md: &MdColors, math_lines: &[String]) {
    let style = Style::default().fg(md.math).add_modifier(Modifier::ITALIC);
    for ml in math_lines {
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(ml.clone(), style),
        ]));
    }
}

/// 渲染代码块：每行加 `│ ` 左侧标记 + code_fg 色，清晰界定块边界（TUI 无满宽背景，
/// 用标记线比部分行背景更整洁）。
fn flush_code_block(out: &mut Vec<Line<'static>>, md: &MdColors, code_lines: &[String]) {
    let bar = Style::default().fg(md.code_fg).add_modifier(Modifier::DIM);
    let body = Style::default().fg(md.code_fg);
    for cl in code_lines {
        out.push(Line::from(vec![
            Span::styled("│ ", bar),
            Span::styled(cl.clone(), body),
        ]));
    }
}

/// 渲染一个非代码块行：识别标题 / 引用 / 列表 / 分隔线，其余按段落行（含行内格式）。
fn render_block_line(raw: &str, md: &MdColors) -> Line<'static> {
    let trimmed = raw.trim_start();
    let indent_len = raw.len() - trimmed.len();
    let indent: String = " ".repeat(indent_len);

    // 分隔线：--- / *** / ___（≥3 个相同字符，允许空白）
    if is_hr(trimmed) {
        return Line::from(Span::styled(
            "─".repeat(30),
            Style::default().fg(md.hr),
        ));
    }

    // 标题：# .. ######
    if let Some((level, content)) = parse_header(trimmed) {
        let style = Style::default()
            .fg(md.header)
            .add_modifier(Modifier::BOLD);
        return Line::from(Span::styled(
            format!("{} {content}", "#".repeat(level)),
            style,
        ));
    }

    // 引用：> ...
    if let Some(rest) = trimmed.strip_prefix('>') {
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        let mut spans = vec![Span::styled("│ ", Style::default().fg(md.quote))];
        spans.extend(parse_rich(rest, md));
        return Line::from(spans);
    }

    // 列表项：- / * / + / 数字.
    if let Some((marker, rest)) = parse_list(trimmed) {
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        let mut spans = vec![Span::raw(indent.clone())];
        spans.push(Span::styled(
            marker.to_string(),
            Style::default().fg(md.list),
        ));
        spans.push(Span::raw(" "));
        spans.extend(parse_rich(rest, md));
        return Line::from(spans);
    }

    // 空行 / 段落行
    if trimmed.is_empty() {
        return Line::from("");
    }
    let mut spans = vec![Span::raw(indent)];
    spans.extend(parse_rich(trimmed, md));
    Line::from(spans)
}

// ── 行内格式解析 ────────────────────────────────────────────────────────────
// 扫描字符串，按优先级匹配标记（见 `try_match`）：行内代码 > 粗斜体 > 粗体 > 斜体 >
// 下划线 > 删除线 > 显示数学 > 行内数学 > 链接。
// 匹配失败（未找到闭合）时该字符按普通文本处理，继续向后扫描——保证未闭合格式降级
// 为纯文本而非吞字符。粗体/斜体/下划线内部递归调用 `parse_rich`，支持有限嵌套（如 **`code`**）。

fn parse_rich(text: &str, md: &MdColors) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut pos = 0; // 当前扫描字节偏移
    let mut text_start = 0; // 待落盘的普通文本起始偏移

    while pos < text.len() {
        if let Some((consumed, inline)) = try_match(&text[pos..], md) {
            if pos > text_start {
                spans.push(Span::styled(
                    text[text_start..pos].to_string(),
                    Style::default().fg(md.fg),
                ));
            }
            spans.extend(inline);
            pos += consumed;
            text_start = pos;
        } else {
            // 无标记命中：前进一个 UTF-8 字符
            let ch_len = text[pos..].chars().next().unwrap().len_utf8();
            pos += ch_len;
        }
    }
    if text_start < text.len() {
        spans.push(Span::styled(
            text[text_start..].to_string(),
            Style::default().fg(md.fg),
        ));
    }
    spans
}

/// 在切片起始处尝试匹配一个行内标记。返回 `(消费字节数, 该标记生成的 spans)`。
///
/// 匹配优先级（前缀冲突时长者先）：行内代码 > 粗斜体 `***` > 粗体 `**` > 斜体 `*` >
/// 下划线 `<u>`/`<ins>` > 删除线 `~~` > 显示数学 `$$` > 行内数学 `$` > 链接 `[`。
fn try_match(s: &str, md: &MdColors) -> Option<(usize, Vec<Span<'static>>)> {
    // 行内代码：`...`（内部不解析其他标记）
    if let Some(rest) = s.strip_prefix('`') {
        if let Some(end) = rest.find('`') {
            let code = &rest[..end];
            return Some((
                end + 2, // 1 开 + end 内容 + 1 闭
                vec![Span::styled(
                    code.to_string(),
                    Style::default().fg(md.code_fg).add_modifier(Modifier::DIM),
                )],
            ));
        }
    }
    // 粗斜体：***...***（先于 ** 匹配，否则 ** 会吞掉前两个 *）
    if let Some(rest) = s.strip_prefix("***") {
        if let Some(end) = rest.find("***") {
            let inner = &rest[..end];
            let mut inner_spans = parse_rich(inner, md);
            for sp in &mut inner_spans {
                sp.style = sp.style.add_modifier(Modifier::BOLD | Modifier::ITALIC);
            }
            return Some((end + 6, inner_spans)); // 3 开 + end 内容 + 3 闭
        }
    }
    // 粗体：**...**（先于斜体匹配，避免 * 吞掉 **）
    if let Some(rest) = s.strip_prefix("**") {
        if let Some(end) = rest.find("**") {
            let inner = &rest[..end];
            let mut inner_spans = parse_rich(inner, md);
            for sp in &mut inner_spans {
                sp.style = sp.style.add_modifier(Modifier::BOLD);
            }
            return Some((end + 4, inner_spans)); // 2 开 + end 内容 + 2 闭
        }
    }
    // 斜体：*...*（跳过成对的 **，找下一个单独的 *）
    if let Some(rest) = s.strip_prefix('*') {
        let bytes = s.as_bytes();
        let mut search = 0; // rest 内偏移
        while search < rest.len() {
            if let Some(rel) = rest[search..].find('*') {
                let abs_in_rest = search + rel;
                let abs_in_s = abs_in_rest + 1; // 补回被 strip 的开头 *
                // 跳过 **（由粗体分支处理）
                if abs_in_s + 1 < s.len() && bytes[abs_in_s + 1] == b'*' {
                    search = abs_in_rest + 2;
                    continue;
                }
                let inner = &rest[..abs_in_rest];
                let mut inner_spans = parse_rich(inner, md);
                for sp in &mut inner_spans {
                    sp.style = sp.style.add_modifier(Modifier::ITALIC);
                }
                return Some((abs_in_rest + 2, inner_spans)); // 1 开 + 内容 + 1 闭
            } else {
                break;
            }
        }
    }
    // 下划线：<u>...</u> / <ins>...</ins>（Markdown 无原生下划线语法，用 HTML 糖）
    if let Some(rest) = s.strip_prefix("<u>") {
        if let Some(end) = rest.find("</u>") {
            let inner = &rest[..end];
            let mut inner_spans = parse_rich(inner, md);
            for sp in &mut inner_spans {
                sp.style = sp.style.add_modifier(Modifier::UNDERLINED);
            }
            return Some((end + 7, inner_spans)); // 3 开 + end 内容 + 4 闭
        }
    }
    if let Some(rest) = s.strip_prefix("<ins>") {
        if let Some(end) = rest.find("</ins>") {
            let inner = &rest[..end];
            let mut inner_spans = parse_rich(inner, md);
            for sp in &mut inner_spans {
                sp.style = sp.style.add_modifier(Modifier::UNDERLINED);
            }
            return Some((end + 11, inner_spans)); // 5 开 + end 内容 + 6 闭
        }
    }
    // 删除线：~~...~~
    if let Some(rest) = s.strip_prefix("~~") {
        if let Some(end) = rest.find("~~") {
            let inner = &rest[..end];
            let mut inner_spans = parse_rich(inner, md);
            for sp in &mut inner_spans {
                sp.style = sp.style.add_modifier(Modifier::CROSSED_OUT);
            }
            return Some((end + 4, inner_spans)); // 2 开 + end 内容 + 2 闭
        }
    }
    // 显示数学（单行 $$...$$）：先于 $ 匹配。内容原样展示（不递归解析，避免 ^/_ 误触发）
    if let Some(rest) = s.strip_prefix("$$") {
        if let Some(end) = rest.find("$$") {
            let inner = &rest[..end];
            if !inner.is_empty() {
                return Some((
                    end + 4, // 2 开 + end 内容 + 2 闭
                    vec![Span::styled(
                        inner.to_string(),
                        Style::default().fg(md.math).add_modifier(Modifier::ITALIC),
                    )],
                ));
            }
        }
    }
    // 行内数学：$...$（开头非空格、闭合前非空格、内容非空；避免货币 $5 误触发）
    if let Some(rest) = s.strip_prefix('$') {
        if !rest.is_empty() && !rest.starts_with(' ') {
            if let Some(end) = rest.find('$') {
                let inner = &rest[..end];
                if !inner.is_empty() && !inner.ends_with(' ') {
                    return Some((
                        end + 2, // 1 开 + end 内容 + 1 闭
                        vec![Span::styled(
                            inner.to_string(),
                            Style::default().fg(md.math).add_modifier(Modifier::ITALIC),
                        )],
                    ));
                }
            }
        }
    }
    // 链接：[text](url)
    if s.starts_with('[') {
        if let Some(text_end) = s.find(']') {
            if s[text_end..].starts_with("](") {
                if let Some(url_end) = s[text_end + 2..].find(')') {
                    let link_text = &s[1..text_end];
                    let text_spans = parse_rich(link_text, md);
                    let link_spans: Vec<Span<'static>> = text_spans
                        .into_iter()
                        .map(|mut sp| {
                            sp.style = Style::default()
                                .fg(md.link)
                                .add_modifier(Modifier::UNDERLINED);
                            sp
                        })
                        .collect();
                    return Some((text_end + 2 + url_end + 1, link_spans));
                }
            }
        }
    }
    None
}

// ── 块级判定辅助 ────────────────────────────────────────────────────────────

fn is_hr(s: &str) -> bool {
    let t = s.trim();
    if t.len() < 3 {
        return false;
    }
    let first = t.chars().next().unwrap();
    if first != '-' && first != '*' && first != '_' {
        return false;
    }
    t.chars().all(|c| c == first)
}

fn parse_header(s: &str) -> Option<(usize, &str)> {
    let mut n = 0;
    for c in s.chars() {
        if c == '#' {
            n += 1;
        } else {
            break;
        }
    }
    if (1..=6).contains(&n) && s.as_bytes().get(n) == Some(&b' ') {
        Some((n, s[n + 1..].trim()))
    } else {
        None
    }
}

fn parse_list(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    // 无序列表：- / * / + 后接空格
    if bytes.len() >= 2 && matches!(bytes[0], b'-' | b'*' | b'+') && bytes[1] == b' ' {
        return Some((&s[0..1], &s[2..]));
    }
    // 有序列表：数字 + '.' + 空格
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1] == b' ' {
        return Some((&s[..i + 1], &s[i + 2..]));
    }
    None
}

fn is_fence_line(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::resolve("cyberpunk")
    }

    #[test]
    fn empty_text_returns_empty() {
        assert!(render("", &theme()).is_empty());
    }

    #[test]
    fn plain_text_yields_one_line() {
        let lines = render("hello world", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn header_renders_one_line() {
        let lines = render("# Title", &theme());
        assert_eq!(lines.len(), 1, "单行标题应一行");
    }

    #[test]
    fn level_six_header_recognized() {
        // 不应 panic；###### 仍为合法标题
        let lines = render("###### deep", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn seven_hashes_not_header() {
        // 7 个 # 不合法（>6），应按段落行渲染（不 panic）
        let lines = render("####### not header", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn code_block_fenced_renders_content_without_delimiters() {
        let text = "```rust\nlet x = 1;\nlet y = 2;\n```\n";
        let lines = render(text, &theme());
        // 围栏分隔行被消费，只保留 2 行代码
        assert_eq!(lines.len(), 2, "代码块应只含 2 行内容（分隔行已隐藏）");
    }

    #[test]
    fn code_block_tilde_fence_also_works() {
        let text = "~~~\ncode\n~~~\n";
        let lines = render(text, &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn unclosed_code_block_flushes_in_streaming() {
        // 流式未闭合围栏：已累积的代码行仍应渲染
        let text = "```python\ndef f():\n    pass\n";
        let lines = render(text, &theme());
        assert_eq!(lines.len(), 2, "未闭合代码块应渲染已累积 2 行");
    }

    #[test]
    fn horizontal_rule_renders() {
        let lines = render("---", &theme());
        assert_eq!(lines.len(), 1);
        let lines2 = render("***", &theme());
        assert_eq!(lines2.len(), 1);
    }

    #[test]
    fn blockquote_renders_with_bar_marker() {
        let lines = render("> quoted text", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn unordered_list_renders_marker() {
        for marker in ["- item", "* item", "+ item"] {
            let lines = render(marker, &theme());
            assert_eq!(lines.len(), 1, "{marker} 应渲染为单行列表项");
        }
    }

    #[test]
    fn ordered_list_renders_marker() {
        let lines = render("1. first", &theme());
        assert_eq!(lines.len(), 1);
        let lines = render("42. answer", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn inline_code_does_not_parse_inner_formatting() {
        // `a **b** c` 内部 ** 不应触发粗体
        let lines = render("`a **b** c`", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn bold_renders_without_panicking() {
        let lines = render("**important**", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn italic_renders_and_does_not_conflict_with_bold() {
        // **bold** 与 *italic* 混合
        let lines = render("**bold** and *italic*", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn strikethrough_renders() {
        let lines = render("~~deleted~~", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn link_renders_text_with_link_style() {
        let lines = render("[example](https://example.com)", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn unclosed_bold_degrades_to_plain_text() {
        // 流式未闭合 **：应降级为纯文本，不吞字符
        let lines = render("**unfinished bold", &theme());
        assert_eq!(lines.len(), 1, "未闭合粗体应按段落行渲染");
    }

    #[test]
    fn unclosed_inline_code_degrades_to_plain_text() {
        let lines = render("some `unclosed code", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn multiline_markdown_renders_multiple_lines() {
        let text = "# Header\n\ntext\n\n- item";
        let lines = render(text, &theme());
        // 5 个逻辑行：标题 / 空 / 段落 / 空 / 列表
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn nested_bold_around_code_renders() {
        let lines = render("**`code`**", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn link_without_url_degrades() {
        // [text] 后无 ](url) 应按纯文本处理
        let lines = render("[text only", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn utf8_content_renders_without_panicking() {
        let lines = render("**加粗** 和 `代码` 与 [链接](https://例.com)", &theme());
        assert_eq!(lines.len(), 1);
    }

    // ── 新增：粗斜体 / 下划线 / 数学公式 ────────────────────────────────────

    #[test]
    fn bold_italic_renders() {
        // ***bi*** 应作为粗斜体整体匹配（不被 ** + * 拆开）
        use ratatui::style::Modifier;
        let lines = render("***bold italic***", &theme());
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        // 应存在携带 BOLD|ITALIC 的 span
        assert!(
            spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD | Modifier::ITALIC)),
            "应存在粗斜体 span"
        );
    }

    #[test]
    fn bold_italic_mixed_with_plain() {
        let lines = render("前 ***bi*** 后", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn underline_html_u_renders() {
        use ratatui::style::Modifier;
        let lines = render("<u>underlined</u>", &theme());
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        assert!(
            spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::UNDERLINED)),
            "应存在下划线 span"
        );
    }

    #[test]
    fn underline_html_ins_renders() {
        let lines = render("<ins>ins</ins> text", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn unclosed_underline_degrades_to_plain() {
        let lines = render("<u>no close", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn inline_math_renders() {
        use ratatui::style::Modifier;
        let lines = render("公式 $x^2 + y^2$ 如上", &theme());
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        assert!(
            spans.iter().any(|s| &*s.content == "x^2 + y^2"
                && s.style.add_modifier.contains(Modifier::ITALIC)),
            "应存在斜体数学 span"
        );
    }

    #[test]
    fn inline_math_does_not_swallow_currency() {
        // "$5 仅此"：开头 $ 后无第二个 $ 闭合 → 降级为纯文本，不吞字符
        let lines = render("价格 $5 仅此", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn inline_math_requires_non_space_close() {
        // $ x $（首尾空格）不应匹配为数学
        let lines = render("$ x $", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn display_math_single_line_renders() {
        let lines = render("$$x^2 + y^2 = z^2$$", &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn block_math_multiline_renders() {
        let text = "$$\nx^2 + y^2\n= z^2\n$$";
        let lines = render(text, &theme());
        // 2 行公式内容（$$ 起止行被消费）
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn unclosed_block_math_flushes_in_streaming() {
        let text = "$$\nx^2\n";
        let lines = render(text, &theme());
        assert_eq!(lines.len(), 1, "未闭合块级数学应渲染已累积 1 行");
    }

    #[test]
    fn math_content_not_parsed_as_markdown() {
        // $a * b$ 内部 * 不应触发斜体（数学内容原样展示）
        let lines = render("$a * b$", &theme());
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        // 应有一个 span 内容为 "a * b"（含 *）
        assert!(spans.iter().any(|s| &*s.content == "a * b"), "数学内容应原样含 *");
    }
}
