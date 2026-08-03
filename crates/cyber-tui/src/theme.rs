//! 主题：基础颜色对，按 `config.ui.theme` 字符串匹配预设。
//!
//! P1 只提供 5 个内置主题的最小颜色集（bg/fg/accent/muted/border/选中色/标题色），
//! 完整主题引擎（语法高亮、图表配色、动画过渡）留待 P7 打磨阶段。

use ratatui::style::Color;

/// 一组主题颜色。所有字段为 `Color`，便于直接喂给 `Style`。
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub bg: Color,
    pub fg: Color,
    pub accent: Color,
    pub muted: Color,
    pub border: Color,
    pub sel_bg: Color,
    pub sel_fg: Color,
    pub title: Color,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

const TOKYO_NIGHT: Theme = Theme {
    bg: rgb(26, 27, 38),
    fg: rgb(192, 202, 245),
    accent: rgb(122, 162, 247),
    muted: rgb(86, 95, 137),
    border: rgb(51, 70, 124),
    sel_bg: rgb(40, 52, 87),
    sel_fg: rgb(192, 202, 245),
    title: rgb(125, 207, 255),
};

const CATPPUCCIN: Theme = Theme {
    bg: rgb(30, 30, 46),
    fg: rgb(205, 214, 244),
    accent: rgb(203, 166, 247),
    muted: rgb(108, 112, 134),
    border: rgb(69, 71, 90),
    sel_bg: rgb(49, 50, 68),
    sel_fg: rgb(205, 214, 244),
    title: rgb(245, 194, 231),
};

const DRACULA: Theme = Theme {
    bg: rgb(40, 42, 54),
    fg: rgb(248, 248, 242),
    accent: rgb(189, 147, 249),
    muted: rgb(98, 114, 164),
    border: rgb(68, 71, 90),
    sel_bg: rgb(68, 71, 90),
    sel_fg: rgb(248, 248, 242),
    title: rgb(255, 121, 198),
};

const GRUVBOX: Theme = Theme {
    bg: rgb(40, 40, 40),
    fg: rgb(235, 219, 178),
    accent: rgb(250, 189, 47),
    muted: rgb(146, 131, 116),
    border: rgb(80, 73, 69),
    sel_bg: rgb(80, 73, 69),
    sel_fg: rgb(235, 219, 178),
    title: rgb(254, 128, 25),
};

const NORD: Theme = Theme {
    bg: rgb(46, 52, 64),
    fg: rgb(216, 222, 233),
    accent: rgb(136, 192, 208),
    muted: rgb(76, 86, 106),
    border: rgb(59, 66, 82),
    sel_bg: rgb(59, 66, 82),
    sel_fg: rgb(216, 222, 233),
    title: rgb(129, 161, 193),
};

/// 赛博朋克主题：深紫黑底 + 霓虹粉 accent + 霓虹青 title/选中色。
///
/// 灵感来自 ratatui.rs "Built with Ratatui" 项目群的暗底霓虹 TUI 美学
///（scope-tui 示波器、rebels-in-the-sky 太空海盗、binsider 二进制分析等）。
const CYBERPUNK: Theme = Theme {
    bg: rgb(13, 2, 33),       // #0D0221 深紫黑底
    fg: rgb(244, 244, 248),   // #F4F4F8 冷白
    accent: rgb(255, 42, 109), // #FF2A6D 霓虹粉（标题栏底，深色字保证对比）
    muted: rgb(109, 109, 153), // #6D6D99 暗紫灰
    border: rgb(61, 31, 92),   // #3D1F5C 暗紫边框
    sel_bg: rgb(26, 11, 46),   // #1A0B2E 深紫选中底
    sel_fg: rgb(5, 217, 232),  // #05D9E8 霓虹青选中字
    title: rgb(5, 217, 232),   // #05D9E8 霓虹青标题
};

impl Theme {
    /// 按主题名解析；未知名回退到 `cyberpunk`（与 `UiConfig::default` 一致）。
    pub fn resolve(name: &str) -> Theme {
        match name {
            "catppuccin" => CATPPUCCIN,
            "cyberpunk" => CYBERPUNK,
            "dracula" => DRACULA,
            "gruvbox" => GRUVBOX,
            "nord" => NORD,
            "tokyo-night" => TOKYO_NIGHT,
            _ => CYBERPUNK, // 未知值回退到默认主题 cyberpunk（与 UiConfig::default 一致）
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_known_themes() {
        // 不同主题 bg 颜色不同，借此断言命中正确预设（而非被回退）
        assert_eq!(Theme::resolve("tokyo-night").bg, rgb(26, 27, 38));
        assert_eq!(Theme::resolve("catppuccin").bg, rgb(30, 30, 46));
        assert_eq!(Theme::resolve("cyberpunk").bg, rgb(13, 2, 33));
        assert_eq!(Theme::resolve("cyberpunk").accent, rgb(255, 42, 109));
        assert_eq!(Theme::resolve("dracula").bg, rgb(40, 42, 54));
        assert_eq!(Theme::resolve("gruvbox").bg, rgb(40, 40, 40));
        assert_eq!(Theme::resolve("nord").bg, rgb(46, 52, 64));
    }

    #[test]
    fn resolve_unknown_falls_back_to_default() {
        // 未知主题名回退到默认主题 cyberpunk（与 UiConfig::default 一致）
        let fallback = Theme::resolve("does-not-exist");
        assert_eq!(fallback.bg, CYBERPUNK.bg);
        assert_eq!(fallback.title, CYBERPUNK.title);
        // tokyo-night 不应被错误回退到 cyberpunk
        assert_ne!(Theme::resolve("tokyo-night").bg, CYBERPUNK.bg);
    }
}
