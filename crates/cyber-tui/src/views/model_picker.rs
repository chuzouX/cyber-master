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

use cyber_core::{ModelConfig, ProvidersConfig};

use crate::app::ModelPickerState;
use crate::theme::Theme;

/// 渲染 `/model` 面板。
pub fn render(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &ModelPickerState,
    providers: &ProvidersConfig,
    default_provider: &str,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(
            Line::from(" 模型选择 / Model Picker ")
                .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .padding(Padding::new(1, 1, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 双栏 + 底部 hint
    let chunks = Layout::vertical([
        Constraint::Min(0),   // 双栏
        Constraint::Length(2), // hint / 状态
    ])
    .split(inner);
    let body = chunks[0];
    let hint_area = chunks[1];

    let panes = Layout::horizontal([
        Constraint::Percentage(40),
        Constraint::Percentage(60),
    ])
    .split(body);
    let provider_pane = panes[0];
    let model_pane = panes[1];

    render_providers(frame, provider_pane, theme, state, providers, default_provider);

    // 取当前选中 provider 的 models map，传给 model 栏以显示 alias
    let names = providers.sorted_names();
    let model_configs: Option<&HashMap<String, ModelConfig>> = if state.provider_selected < names.len() {
        providers.providers.get(&names[state.provider_selected]).map(|p| &p.models)
    } else {
        None
    };
    render_models(frame, model_pane, theme, state, model_configs);
    render_hint(frame, hint_area, theme, state);
}

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
            Line::from(" Providers ")
                .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
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
            let star = if is_default { " ★默认" } else { "" };
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
            lines.push(
                Line::from(vec![
                    Span::raw(marker),
                    Span::styled(
                        format!("{name}{star}"),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("\n  [{}] {}", cfg.kind, model_label), {
                        let mut s = Style::default().fg(theme.muted);
                        if selected {
                            s = s.bg(theme.sel_bg);
                        }
                        s
                    }),
                ])
                .style(row_style),
            );
            lines.push(Line::from(""));
        }
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg)),
        inner,
    );
}

fn render_models(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    state: &ModelPickerState,
    model_configs: Option<&HashMap<String, ModelConfig>>,
) {
    let focused = state.focus_models;
    let border_fg = if focused { theme.accent } else { theme.border };
    let title = if state.fetching {
        " Models (拉取中…) "
    } else {
        " Models "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_fg))
        .title(
            Line::from(title)
                .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    if let Some(err) = &state.fetch_error {
        lines.push(
            Line::from(format!(" ⚠ {err}"))
                .style(Style::default().fg(theme.accent)),
        );
    } else if state.fetching {
        lines.push(
            Line::from(" ⏳ 正在拉取模型列表…")
                .style(Style::default().fg(theme.muted)),
        );
    } else if state.models.is_empty() {
        lines.push(
            Line::from("（无模型，按 Tab 切到 Providers 栏选择 provider 后自动拉取）")
                .style(Style::default().fg(theme.muted))
                .alignment(Alignment::Center),
        );
    } else {
        for (i, m) in state.models.iter().enumerate() {
            let selected = i == state.model_selected;
            let marker = if selected { "▸ " } else { "  " };
            let style = if selected {
                Style::default().bg(theme.sel_bg).fg(theme.sel_fg).add_modifier(Modifier::BOLD)
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
            lines.push(Line::from(format!("{marker}{label}")).style(style));
        }
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg)),
        inner,
    );
}

fn render_hint(frame: &mut Frame, area: Rect, theme: &Theme, state: &ModelPickerState) {
    let hint = if state.focus_models {
        " ↑↓ 选模型  Tab 切到 Providers  Enter 确认  Esc 返回"
    } else {
        " ↑↓ 选 provider（自动拉取模型）  Tab 切到 Models  Esc 返回"
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
    fn render_model_picker_does_not_panic() {
        let state = ModelPickerState {
            provider_selected: 0,
            model_selected: 0,
            models: vec!["gpt-4o".into(), "gpt-4o-mini".into()],
            fetching: false,
            fetch_id: 0,
            fetch_error: None,
            focus_models: false,
        };
        let providers = make_providers();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &Theme::resolve("cyberpunk"), &state, &providers, "openai"))
            .unwrap();
    }

    #[test]
    fn render_model_picker_fetching() {
        let state = ModelPickerState {
            provider_selected: 1,
            model_selected: 0,
            models: vec![],
            fetching: true,
            fetch_id: 1,
            fetch_error: None,
            focus_models: true,
        };
        let providers = make_providers();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &Theme::resolve("cyberpunk"), &state, &providers, "ollama"))
            .unwrap();
    }

    #[test]
    fn render_model_picker_error() {
        let state = ModelPickerState {
            provider_selected: 0,
            model_selected: 0,
            models: vec![],
            fetching: false,
            fetch_id: 1,
            fetch_error: Some("timeout".into()),
            focus_models: true,
        };
        let providers = make_providers();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &Theme::resolve("cyberpunk"), &state, &providers, "openai"))
            .unwrap();
    }

    #[test]
    fn render_model_picker_no_providers() {
        let state = ModelPickerState::default();
        let providers = ProvidersConfig::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &Theme::resolve("cyberpunk"), &state, &providers, ""))
            .unwrap();
    }
}
