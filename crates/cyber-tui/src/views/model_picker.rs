//! `/model` 面板渲染：双栏选择 provider + model。
//!
//! 左栏列出所有 provider（标★默认），右栏展示当前选中 provider 的模型列表
//! （异步拉取）。Tab/Enter 切换焦点栏；在模型栏按 Enter 确认选择 → 保存并返回。
//! 状态全部在 [`crate::app::ModelPickerState`]，按键处理在 `app.rs::handle_model_picker_key`。

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};

use std::collections::HashMap;

use cyber_core::{ModelConfig, ProviderConfig, ProvidersConfig};

use crate::app::ModelPickerState;
use crate::theme::Theme;

use super::clip_cells_ellipsis;

/// 渲染 `/model` 面板。
pub fn render(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &ModelPickerState,
    providers: &ProvidersConfig,
    default_provider: &str,
) {
    let title_text = match state.target {
        crate::app::ModelPickerTarget::VisionEngine => " 识图引擎模型选择 / Vision Model Picker ",
        crate::app::ModelPickerTarget::DefaultAgent => " 模型选择 / Model Picker ",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(
            Line::from(title_text).style(
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .padding(Padding::new(1, 1, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 双栏 + 底部 hint
    let chunks = Layout::vertical([
        Constraint::Min(0),    // 双栏
        Constraint::Length(2), // hint / 状态
    ])
    .split(inner);
    let body = chunks[0];
    let hint_area = chunks[1];

    let panes =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).split(body);
    let provider_pane = panes[0];
    let model_pane = panes[1];
    render_providers(
        frame,
        provider_pane,
        theme,
        state,
        providers,
        default_provider,
    );

    // 取当前选中 provider 的 models map，传给 model 栏以显示 alias
    let names = providers.sorted_names();
    let current_provider: Option<(&str, &ProviderConfig)> = if state.provider_selected < names.len()
    {
        let name = &names[state.provider_selected];
        providers.providers.get(name).map(|p| (name.as_str(), p))
    } else {
        None
    };
    render_models(frame, model_pane, theme, state, current_provider);
    render_hint(frame, hint_area, theme, state);
}

/// 每个 provider 项在渲染中的行数：name 行 + kind/model 行 = 2
const PROVIDER_ITEM_LINES: usize = 2;

fn render_providers(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &ModelPickerState,
    providers: &ProvidersConfig,
    default_provider: &str,
) {
    let focused = !state.focus_models;
    let border_fg = if focused { theme.accent } else { theme.border };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_fg))
        .title(
            Line::from(" 服务商 / Providers ").style(
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .style(Style::default().bg(theme.bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let names = providers.sorted_names();
    let mut lines: Vec<Line> = Vec::new();
    if names.is_empty() {
        lines.push(
            Line::from("（无 provider）")
                .style(Style::default().fg(theme.muted))
                .alignment(Alignment::Center),
        );
    } else {
        for (i, name) in names.iter().enumerate() {
            let selected = i == state.provider_selected;
            let is_default = name == default_provider;
            let marker = if selected { "▸ " } else { "  " };
            let cfg = &providers.providers[name];
            let row_style = if selected {
                Style::default().bg(theme.sel_bg).fg(theme.sel_fg)
            } else {
                Style::default().fg(theme.fg)
            };
            // 显示名：per-model alias 优先，否则用 model id
            let display = cfg.model_display_name();
            let model_label = if display != cfg.model {
                format!("{} → {}", display, cfg.model)
            } else {
                cfg.model.clone()
            };

            let max_prov_w = (inner.width as usize).saturating_sub(1);
            let mut badges_w = 2usize;
            if is_default {
                badges_w += 8;
            }
            let name_budget = max_prov_w.saturating_sub(badges_w);
            let clipped_name = clip_cells_ellipsis(name, name_budget);

            let mut header_spans = vec![
                Span::styled(
                    marker,
                    if selected {
                        Style::default()
                            .fg(theme.accent)
                            .bg(theme.sel_bg)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(
                    clipped_name,
                    if selected {
                        Style::default()
                            .fg(theme.sel_fg)
                            .bg(theme.sel_bg)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)
                    },
                ),
            ];
            if is_default {
                let mut gold_style = Style::default()
                    .fg(ratatui::style::Color::Rgb(254, 188, 56))
                    .add_modifier(Modifier::BOLD);
                if selected {
                    gold_style = gold_style.bg(theme.sel_bg);
                }
                header_spans.push(Span::styled("  ★ 默认", gold_style));
            }

            let mut sub_style = Style::default().fg(theme.muted);
            if selected {
                sub_style = sub_style.bg(theme.sel_bg);
            }

            let sub_text = format!("    [{}] {}", cfg.kind, model_label);
            let clipped_sub = clip_cells_ellipsis(&sub_text, max_prov_w);

            lines.push(Line::from(header_spans).style(row_style));
            lines.push(Line::from(vec![Span::styled(clipped_sub, sub_style)]).style(row_style));
        }
    }

    // 粘性滚动：选中项溢出视口时自动调整
    let visible_h = inner.height as usize;
    let total_lines = lines.len();
    let prev = state
        .provider_scroll
        .get()
        .min(total_lines.saturating_sub(visible_h));
    let sel_start = state.provider_selected * PROVIDER_ITEM_LINES;
    let sel_end = (sel_start + PROVIDER_ITEM_LINES).min(total_lines);
    let scroll = if total_lines <= visible_h {
        0
    } else if sel_start < prev {
        sel_start
    } else if sel_end > prev + visible_h {
        sel_end
            .saturating_sub(visible_h)
            .min(total_lines.saturating_sub(visible_h))
    } else {
        prev
    };
    state.provider_scroll.set(scroll);

    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(theme.bg))
            .scroll((scroll as u16, 0)),
        inner,
    );
}

fn render_models(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &ModelPickerState,
    current_provider: Option<(&str, &ProviderConfig)>,
) {
    let model_configs: Option<&HashMap<String, ModelConfig>> =
        current_provider.map(|(_, p)| &p.models);
    let focused = state.focus_models;
    let border_fg = if focused { theme.accent } else { theme.border };
    let prov_title_suffix = current_provider
        .map(|(name, _)| format!(" ({name})"))
        .unwrap_or_default();
    let title = if state.fetching {
        format!(" 模型 / Models{prov_title_suffix} (拉取中…) ")
    } else {
        format!(" 模型 / Models{prov_title_suffix} ")
    };
    // 标题绘制在顶边框上：裁剪防止长 provider 名覆盖右上角边框。
    let title = clip_cells_ellipsis(&title, (area.width as usize).saturating_sub(3));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_fg))
        .title(
            Line::from(title).style(
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .style(Style::default().bg(theme.bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    if let Some(err) = &state.fetch_error {
        lines.push(Line::from(format!(" ⚠ {err}")).style(Style::default().fg(theme.accent)));
    } else if state.fetching {
        lines.push(Line::from(" ⟳ 正在拉取模型列表…").style(Style::default().fg(theme.muted)));
    } else if state.models.is_empty() {
        lines.push(
            Line::from("（无模型 · 左栏选中 provider 后按 Enter 从接口获取）")
                .style(Style::default().fg(theme.muted))
                .alignment(Alignment::Center),
        );
    } else {
        // 只构建可见窗口内的行：长模型列表（数百～数千条）下逐行构建 + 逐行能力查询会让
        // 每帧成本随列表长度线性增长（旧实现在这里每行调两次 `CapabilityStore::load()`）。
        let visible_h = inner.height as usize;
        let total = state.models.len();
        let prev = state
            .model_scroll
            .get()
            .min(total.saturating_sub(visible_h));
        let sel = state.model_selected;
        let scroll = if total <= visible_h {
            0
        } else if sel < prev {
            sel
        } else if sel >= prev + visible_h {
            (sel + 1)
                .saturating_sub(visible_h)
                .min(total.saturating_sub(visible_h))
        } else {
            prev
        };
        state.model_scroll.set(scroll);
        // 能力缓存每帧只读一次（显式 models 配置 → 实测缓存 → 名称规则表）。
        let store = cyber_core::CapabilityStore::load();
        for i in scroll..(scroll + visible_h).min(total) {
            let m = &state.models[i];
            let selected = i == state.model_selected;
            let marker = if selected { "▸ " } else { "  " };
            let style = if selected {
                Style::default()
                    .bg(theme.sel_bg)
                    .fg(theme.sel_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg)
            };
            // 若有 alias 则显示 "alias → model_id"
            let label = if let Some(mc) = model_configs.and_then(|map| map.get(m)) {
                if let Some(alias) = &mc.alias {
                    if !alias.is_empty() {
                        format!("{} → {}", alias, m)
                    } else {
                        m.clone()
                    }
                } else {
                    m.clone()
                }
            } else {
                m.clone()
            };

            let is_probing = state.probing_model.as_deref() == Some(m.as_str());
            let is_active = current_provider
                .map(|(_, p)| p.model == *m)
                .unwrap_or(false);
            let has_vision = current_provider.is_some_and(|(prov_name, _)| {
                cyber_core::resolve_vision_capability(model_configs, prov_name, m, &store)
                    .is_supported()
            });

            let max_model_w = (inner.width as usize).saturating_sub(1);
            let mut badges_w = 2usize; // marker
            if is_probing || has_vision {
                badges_w += 8;
            }
            if is_active {
                badges_w += 8;
            }
            let label_budget = max_model_w.saturating_sub(badges_w);
            let clipped_label = clip_cells_ellipsis(&label, label_budget);
            let mut spans = vec![Span::styled(format!("{marker}{clipped_label}"), style)];

            if is_probing {
                let mut probe_style = Style::default()
                    .fg(ratatui::style::Color::Rgb(254, 188, 56))
                    .add_modifier(Modifier::BOLD);
                if selected {
                    probe_style = probe_style.bg(theme.sel_bg);
                }
                spans.push(Span::styled("  ⟳ 探测中", probe_style));
            } else if has_vision {
                let mut vision_style = Style::default()
                    .fg(ratatui::style::Color::Cyan)
                    .add_modifier(Modifier::BOLD);
                if selected {
                    vision_style = vision_style.bg(theme.sel_bg);
                }
                spans.push(Span::styled("  ◈ 视觉", vision_style));
            }

            if is_active {
                let mut active_style = Style::default()
                    .fg(ratatui::style::Color::Rgb(137, 210, 129))
                    .add_modifier(Modifier::BOLD);
                if selected {
                    active_style = active_style.bg(theme.sel_bg);
                }
                spans.push(Span::styled("  ✓ 当前", active_style));
            }

            lines.push(Line::from(spans));
        }
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg)),
        inner,
    );
}

fn render_hint(frame: &mut Frame, area: Rect, theme: &Theme, state: &ModelPickerState) {
    let confirm_action = match state.target {
        crate::app::ModelPickerTarget::VisionEngine => "Enter 设为识图模型",
        crate::app::ModelPickerTarget::DefaultAgent => "Enter 确认选择",
    };
    let hint = if let Some(probing) = &state.probing_model {
        format!(" ⟳ 正在对模型 [{probing}] 进行识图能力实测中，请稍候...")
    } else if state.fetching {
        format!(" ⟳ 正在从接口拉取模型列表… · {confirm_action} · Esc 关闭 ")
    } else if !state.fetched {
        " Tab/←/→ 切栏 · ↑/↓ 移动 · 左栏选中 provider 后 Enter 从接口获取模型 · r 重新拉取 · Esc 关闭 "
            .to_string()
    } else {
        format!(" Tab/←/→ 切栏 · ↑/↓ 移动 · {confirm_action} · r 重新拉取 · t 探测识图 · Esc 关闭 ")
    };
    frame.render_widget(
        Paragraph::new(Line::from(hint)).style(Style::default().fg(theme.muted)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyber_core::ProviderConfig;
    use ratatui::{backend::TestBackend, Terminal};

    fn make_providers() -> ProvidersConfig {
        let mut cfg = ProvidersConfig::default();
        cfg.upsert(
            "openai",
            ProviderConfig {
                kind: "openai".into(),
                base_url: "https://api.openai.com/v1".into(),
                api_key: "${OPENAI_API_KEY}".into(),
                model: "gpt-4o".into(),
                ..Default::default()
            },
        );
        cfg.upsert(
            "ollama",
            ProviderConfig {
                kind: "ollama".into(),
                base_url: "http://localhost:11434".into(),
                model: "qwen2.5:32b".into(),
                ..Default::default()
            },
        );
        cfg
    }

    #[test]
    fn render_model_picker_window_bounds_long_list() {
        // 长模型列表（数千条）只渲染可见窗口：选中项必须可见，远处条目不得被构建/渲染，
        // 且整帧耗时不随列表长度线性增长（旧实现每行调两次 `CapabilityStore::load()`）。
        const TOTAL: usize = 8000;
        let models: Vec<String> = (0..TOTAL).map(|i| format!("model-{i:04}")).collect();
        let state = ModelPickerState {
            models,
            model_selected: TOTAL - 3,
            focus_models: true,
            fetched: true,
            ..Default::default()
        };
        let providers = make_providers();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();

        let started = std::time::Instant::now();
        terminal
            .draw(|f| {
                render(
                    f,
                    f.area(),
                    &Theme::resolve("cyberpunk"),
                    &state,
                    &providers,
                    "openai",
                )
            })
            .unwrap();
        let elapsed = started.elapsed();

        let buffer = terminal.backend().buffer();
        let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(
            text.contains("model-7997"),
            "选中项必须可见（窗口跟随选中项）: {text}"
        );
        assert!(!text.contains("model-0000"), "窗口外条目不得被渲染: {text}");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "8000 条模型的单帧渲染耗时 {elapsed:?} 过高（疑似逐行读盘/构建全量行）"
        );
        assert!(
            state.model_scroll.get() > 0,
            "选中项在末尾时滚动偏移必须跟随"
        );
    }

    #[test]
    fn render_model_picker_does_not_panic() {
        let state = ModelPickerState {
            models: vec!["gpt-4o".into(), "gpt-4o-mini".into()],
            ..Default::default()
        };
        let providers = make_providers();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    f.area(),
                    &Theme::resolve("cyberpunk"),
                    &state,
                    &providers,
                    "openai",
                )
            })
            .unwrap();
    }

    #[test]
    fn render_model_picker_with_vision_badges_and_probing() {
        let mut providers = make_providers();
        let mc = ModelConfig {
            vision: Some(true),
            ..Default::default()
        };
        if let Some(p) = providers.providers.get_mut("openai") {
            p.models.insert("gpt-4o".into(), mc);
        }

        let openai_idx = providers
            .sorted_names()
            .iter()
            .position(|n| n == "openai")
            .unwrap_or(0);
        let state = ModelPickerState {
            provider_selected: openai_idx,
            models: vec!["gpt-4o".into(), "probing-model".into()],
            probing_model: Some("probing-model".into()),
            focus_models: true,
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    f.area(),
                    &Theme::resolve("cyberpunk"),
                    &state,
                    &providers,
                    "openai",
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered_text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(rendered_text.contains('◈') && rendered_text.contains('视'));
        assert!(rendered_text.contains('⟳') && rendered_text.contains('探'));
        assert!(rendered_text.contains('★') && rendered_text.contains('默'));
        assert!(rendered_text.contains('✓') && rendered_text.contains('当'));
    }

    #[test]
    fn render_model_picker_fetching() {
        let state = ModelPickerState {
            provider_selected: 1,
            fetching: true,
            fetch_id: 1,
            focus_models: true,
            ..Default::default()
        };
        let providers = make_providers();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    f.area(),
                    &Theme::resolve("cyberpunk"),
                    &state,
                    &providers,
                    "ollama",
                )
            })
            .unwrap();
    }

    #[test]
    fn render_model_picker_error() {
        let state = ModelPickerState {
            fetch_id: 1,
            fetch_error: Some("timeout".into()),
            focus_models: true,
            ..Default::default()
        };
        let providers = make_providers();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    f.area(),
                    &Theme::resolve("cyberpunk"),
                    &state,
                    &providers,
                    "openai",
                )
            })
            .unwrap();
    }

    #[test]
    fn render_model_picker_no_providers() {
        let state = ModelPickerState::default();
        let providers = ProvidersConfig::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    f.area(),
                    &Theme::resolve("cyberpunk"),
                    &state,
                    &providers,
                    "",
                )
            })
            .unwrap();
    }
}
