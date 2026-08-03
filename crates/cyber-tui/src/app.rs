//! TUI 应用核心：模式状态机 + 主循环 + 渲染分发。
//!
//! P1 采用同步阻塞事件循环（`event::poll`）；P2+ 接入 agent 流式后升级为
//! tokio 事件总线（见 DESIGN §10.2）。终端初始化与恢复交由 `ratatui::init/restore`
//! 处理，其内置 panic hook 会在 panic 时自动恢复终端。
//!
//! Settings 是"用 Mode 模拟的模态层"：全局 `s` 或 Welcome 第 4 项进入，`Esc` 返回
//! `prev_mode`；编辑即时改 `config` + live-apply（theme/mouse），保存由设置页内
//! 「保存设置」行 + `Enter` 触发（`save_config` 回写 `~/.cyber/config.toml`）。

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::Paragraph,
    DefaultTerminal, Frame,
};
use tracing::info;

use cyber_core::{save_config, Config, ProjectContext, ProvidersConfig};

use crate::event::{next_action, Action};
use crate::theme::Theme;
use crate::views;
use crate::views::settings::{LiveApply, SettingsState};

/// 顶层模式 / 屏幕。Welcome 为启动入口屏，Settings 为模态设置层，其余三个对应 DESIGN §9。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Welcome,
    Chat,
    Workflow,
    Dashboard,
    Settings,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Welcome => "Welcome",
            Mode::Chat => "Chat",
            Mode::Workflow => "Workflow",
            Mode::Dashboard => "Dashboard",
            Mode::Settings => "Settings",
        }
    }
}

pub struct App {
    config: Config,
    providers: ProvidersConfig,
    project: Option<ProjectContext>,
    theme: Theme,
    mode: Mode,
    selected: usize,
    toast: Option<String>,
    first_run: bool,
    should_quit: bool,
    // Settings 模态层状态
    settings: SettingsState,
    prev_mode: Mode,
    /// 进入 Settings 时的配置快照，供 Esc 双击回退。
    config_at_entry: Config,
    config_file: PathBuf,
    has_project_config: bool,
}

const WELCOME_OPTIONS: usize = 4;

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        providers: ProvidersConfig,
        project: Option<ProjectContext>,
        initial: Mode,
        first_run: bool,
        config_file: PathBuf,
        has_project_config: bool,
    ) -> Self {
        let theme = Theme::resolve(&config.ui.theme);
        Self {
            config_at_entry: config.clone(),
            config,
            providers,
            project,
            theme,
            mode: initial,
            selected: 0,
            toast: None,
            first_run,
            should_quit: false,
            settings: SettingsState::new(),
            prev_mode: initial,
            config_file,
            has_project_config,
        }
    }

    /// 启动 TUI 主循环。终端初始化与恢复由 `ratatui::init/restore` 负责。
    pub fn run(mut self) -> color_eyre::Result<()> {
        let mut terminal: DefaultTerminal = ratatui::init();
        if self.config.ui.mouse {
            execute!(io::stdout(), EnableMouseCapture)?;
        }
        // 即使 main_loop 出错也先恢复终端，避免终端卡在 alternate screen。
        let result = self.main_loop(&mut terminal);
        // 无条件禁用鼠标捕获（幂等），避免中途开启鼠标后退出泄漏。
        let _ = execute!(io::stdout(), DisableMouseCapture);
        ratatui::restore();
        result?;
        info!(mode = ?self.mode, "TUI 退出");
        Ok(())
    }

    fn main_loop(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let tick = Duration::from_millis(250);
        loop {
            terminal.draw(|f| self.render(f))?;
            if let Some(action) = next_action(tick)? {
                self.handle_action(action);
            }
            if self.should_quit {
                break;
            }
        }
        Ok(())
    }

    fn handle_action(&mut self, a: Action) {
        // 任意有效输入清除一次性 toast（Other 除外，避免无谓清除）
        if a != Action::Other && self.toast.is_some() {
            self.toast = None;
        }
        // Settings 下：除 Esc/Quit/Other 外的任何动作取消"待丢弃"状态
        if self.mode == Mode::Settings && !matches!(a, Action::Esc | Action::Quit | Action::Other) {
            self.settings.pending_discard = false;
        }
        match a {
            Action::Quit => self.should_quit = true,
            Action::OpenSettings => {
                // 已在 Settings 时 no-op；否则记 prev_mode + 快照后进入
                if self.mode != Mode::Settings {
                    self.prev_mode = self.mode;
                    self.config_at_entry = self.config.clone();
                    self.settings.pending_discard = false;
                    self.mode = Mode::Settings;
                }
            }
            Action::Tab => {
                if self.mode == Mode::Settings {
                    self.settings.next_section();
                } else {
                    // Welcome 下 Tab 无效；其余三模式循环
                    self.mode = match self.mode {
                        Mode::Chat => Mode::Workflow,
                        Mode::Workflow => Mode::Dashboard,
                        Mode::Dashboard => Mode::Chat,
                        Mode::Welcome | Mode::Settings => self.mode,
                    };
                }
            }
            Action::Esc => {
                if self.mode == Mode::Settings {
                    self.exit_settings();
                } else if self.project.is_none() && self.mode != Mode::Welcome {
                    // 仅在无项目上下文时允许从占位模式返回 Welcome
                    self.mode = Mode::Welcome;
                }
            }
            Action::Up => {
                if self.mode == Mode::Settings {
                    self.settings.prev_field();
                } else if self.mode == Mode::Welcome {
                    self.selected = (self.selected + WELCOME_OPTIONS - 1) % WELCOME_OPTIONS;
                }
            }
            Action::Down => {
                if self.mode == Mode::Settings {
                    self.settings.next_field();
                } else if self.mode == Mode::Welcome {
                    self.selected = (self.selected + 1) % WELCOME_OPTIONS;
                }
            }
            Action::Left => {
                if self.mode == Mode::Settings {
                    let live = self.settings.apply_edit(&mut self.config, &self.providers, false);
                    self.apply_live(live);
                }
            }
            Action::Right => {
                if self.mode == Mode::Settings {
                    let live = self.settings.apply_edit(&mut self.config, &self.providers, true);
                    self.apply_live(live);
                }
            }
            Action::Enter => {
                if self.mode == Mode::Settings {
                    if self.settings.on_save_row() {
                        self.save_settings();
                    } else {
                        let live =
                            self.settings.apply_edit(&mut self.config, &self.providers, true);
                        self.apply_live(live);
                    }
                } else if self.mode == Mode::Welcome {
                    match self.selected {
                        2 => self.mode = Mode::Chat, // 进入聊天
                        3 => {
                            // 进入设置
                            self.prev_mode = Mode::Welcome;
                            self.config_at_entry = self.config.clone();
                            self.settings.pending_discard = false;
                            self.mode = Mode::Settings;
                        }
                        0 => self.toast =
                            Some("（P1 占位：新建项目向导将在后续阶段实现）".into()),
                        1 => self.toast = Some("（P1 占位：工作流编辑器将在 P4 实现）".into()),
                        _ => {}
                    }
                }
                // Chat/Workflow/Dashboard 占位态：Enter 无操作
            }
            Action::Other => {}
        }
    }

    /// 编辑副作用即时应用（theme 重解析 / mouse 捕获切换）。
    fn apply_live(&mut self, live: LiveApply) {
        match live {
            LiveApply::None => {}
            LiveApply::Theme => {
                self.theme = Theme::resolve(&self.config.ui.theme);
            }
            LiveApply::Mouse => {
                let res = if self.config.ui.mouse {
                    execute!(io::stdout(), EnableMouseCapture)
                } else {
                    execute!(io::stdout(), DisableMouseCapture)
                };
                if let Err(e) = res {
                    tracing::warn!(error = %e, "切换鼠标捕获失败");
                }
            }
        }
    }

    /// 保存配置到 `~/.cyber/config.toml`。
    fn save_settings(&mut self) {
        match save_config(&self.config, &self.config_file) {
            Ok(()) => {
                self.settings.dirty = false;
                self.settings.pending_discard = false;
                self.config_at_entry = self.config.clone();
                self.toast = Some("配置已保存".into());
            }
            Err(e) => {
                self.toast = Some(format!("保存失败: {e}"));
            }
        }
    }

    /// 退出设置：dirty 时首次 Esc 提示，二次 Esc 回退到 `config_at_entry` 后返回 `prev_mode`。
    fn exit_settings(&mut self) {
        if self.settings.dirty {
            if !self.settings.pending_discard {
                self.settings.pending_discard = true;
                self.toast = Some("再按 Esc 丢弃改动，或选择「保存设置」".into());
                return;
            }
            // 二次 Esc：回退到进入时的快照
            self.config = self.config_at_entry.clone();
            self.apply_live(LiveApply::Theme);
            self.apply_live(LiveApply::Mouse);
            self.settings.dirty = false;
            self.settings.pending_discard = false;
            self.toast = Some("已丢弃改动".into());
        }
        self.mode = self.prev_mode;
    }

    fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let chunks = Layout::vertical([
            Constraint::Length(1), // 标题栏
            Constraint::Min(0),    // 主区
            Constraint::Length(1), // 状态栏
        ])
        .split(area);
        self.render_title_bar(frame, chunks[0]);
        self.render_main(frame, chunks[1]);
        self.render_status_bar(frame, chunks[2]);
    }

    fn render_title_bar(&self, frame: &mut Frame, area: Rect) {
        let project_name = self
            .project
            .as_ref()
            .and_then(|p| p.frontmatter.project.clone())
            .unwrap_or_else(|| "<无项目>".into());
        let first_run = if self.first_run { " · first-run" } else { "" };
        let title = Line::from(format!(
            " {} │ {} │ provider={} │ theme={}{} ",
            self.mode.label(),
            project_name,
            self.config.agent.default_provider,
            self.config.ui.theme,
            first_run,
        ));
        frame.render_widget(
            Paragraph::new(title).style(
                Style::default()
                    .bg(self.theme.accent)
                    .fg(self.theme.bg)
                    .add_modifier(Modifier::BOLD),
            ),
            area,
        );
    }

    fn render_main(&self, frame: &mut Frame, area: Rect) {
        match self.mode {
            Mode::Welcome => views::welcome::render(
                frame,
                area,
                &self.theme,
                self.selected,
                self.toast.as_deref(),
            ),
            Mode::Chat => views::chat::render(
                frame,
                area,
                &self.theme,
                self.project.as_ref(),
                &self.config.agent.default_provider,
            ),
            Mode::Workflow => views::chat::render_placeholder(
                frame,
                area,
                &self.theme,
                "Workflow Mode",
                "P4 阶段实现（DAG 画布 / 节点编辑 / 并行执行）",
                self.project.as_ref(),
            ),
            Mode::Dashboard => views::chat::render_placeholder(
                frame,
                area,
                &self.theme,
                "Dashboard Mode",
                "P5 阶段实现（工作流监控 / 节点日志 / 资产统计）",
                self.project.as_ref(),
            ),
            Mode::Settings => views::settings::render(
                frame,
                area,
                &self.theme,
                &self.config,
                &self.providers,
                &self.settings,
                self.has_project_config,
            ),
        }
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let hint = match self.mode {
            Mode::Welcome => " ↑/↓ 导航   Enter 确认   s 设置   q 退出",
            Mode::Settings => " ↑↓ 行  Tab 段  Enter 编辑/保存  ←→ 调整  Esc 返回  q 退出",
            _ => " Tab 切换模式   s 设置   Esc 返回 Welcome   q 退出",
        };
        frame.render_widget(
            Paragraph::new(Line::from(hint))
                .style(Style::default().bg(self.theme.muted).fg(self.theme.bg)),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyber_core::{Config, ProvidersConfig};

    fn temp_config_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cyber_app_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    fn make_app(initial: Mode, config_file: PathBuf) -> App {
        App::new(
            Config::default(),
            ProvidersConfig::default(),
            None,
            initial,
            false,
            config_file,
            false,
        )
    }

    #[test]
    fn open_settings_records_prev_mode() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_action(Action::OpenSettings);
        assert_eq!(app.mode, Mode::Settings);
        assert_eq!(app.prev_mode, Mode::Chat);
        // 已在 Settings 时 OpenSettings 为 no-op（不改 prev_mode）
        app.prev_mode = Mode::Workflow;
        app.handle_action(Action::OpenSettings);
        assert_eq!(app.mode, Mode::Settings);
        assert_eq!(app.prev_mode, Mode::Workflow, "重复 OpenSettings 不应改 prev_mode");
    }

    #[test]
    fn theme_edit_live_applies_and_marks_dirty() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_action(Action::OpenSettings);
        // selected=0 → theme；Right 正向循环：cyberpunk → dracula
        app.handle_action(Action::Right);
        assert_eq!(app.config.ui.theme, "dracula");
        assert_eq!(
            app.theme.bg,
            Theme::resolve("dracula").bg,
            "theme 应即时重新解析"
        );
        assert!(app.settings.dirty);
    }

    #[test]
    fn esc_double_press_rolls_back() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_action(Action::OpenSettings);
        app.handle_action(Action::Right); // 改 theme → dirty
        assert!(app.settings.dirty);
        // 首次 Esc：提示，不退出
        app.handle_action(Action::Esc);
        assert_eq!(app.mode, Mode::Settings, "首次 Esc 不应退出");
        assert!(app.settings.pending_discard);
        // 二次 Esc：回退 + 退出
        app.handle_action(Action::Esc);
        assert_eq!(app.mode, Mode::Chat, "二次 Esc 应返回 prev_mode");
        assert_eq!(app.config.ui.theme, "cyberpunk", "回退应恢复原值");
        assert!(!app.settings.dirty);
        assert_eq!(app.theme.bg, Theme::resolve("cyberpunk").bg, "theme 应回滚");
    }

    #[test]
    fn esc_not_dirty_exits_immediately() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_action(Action::OpenSettings);
        app.handle_action(Action::Esc);
        assert_eq!(app.mode, Mode::Chat, "无改动时 Esc 应直接返回");
    }

    #[test]
    fn save_persists_and_clears_dirty() {
        let path = temp_config_path();
        let mut app = make_app(Mode::Chat, path.clone());
        app.handle_action(Action::OpenSettings);
        app.handle_action(Action::Right); // theme → dracula, dirty
        assert!(app.settings.dirty);
        // 导航到保存行（UI 段 5 字段，Down 5 次到 selected=5）
        for _ in 0..5 {
            app.handle_action(Action::Down);
        }
        assert!(app.settings.on_save_row());
        app.handle_action(Action::Enter); // 保存
        assert!(!app.settings.dirty, "保存后 dirty 应清");
        assert!(path.exists(), "配置文件应已写入");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("theme = \"dracula\""),
            "磁盘应写入新主题值，实际: {raw}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn welcome_item_3_enters_settings() {
        let mut app = make_app(Mode::Welcome, temp_config_path());
        // selected 0→3
        for _ in 0..3 {
            app.handle_action(Action::Down);
        }
        assert_eq!(app.selected, 3);
        app.handle_action(Action::Enter);
        assert_eq!(app.mode, Mode::Settings);
        assert_eq!(app.prev_mode, Mode::Welcome);
    }

    #[test]
    fn tab_in_settings_switches_section() {
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.handle_action(Action::OpenSettings);
        assert_eq!(app.settings.section, 0);
        app.handle_action(Action::Tab);
        assert_eq!(app.settings.section, 1, "Tab 应切到下一段");
        assert_eq!(app.settings.selected, 0, "切段应重置 selected");
    }

    #[test]
    fn settings_and_providers_render_without_panic() {
        use ratatui::{backend::TestBackend, Terminal};
        // UI 段（默认）渲染
        let mut app = make_app(Mode::Chat, temp_config_path());
        app.mode = Mode::Settings;
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();

        // Providers 段 + 项目级覆盖横幅渲染（has_project_config=true）
        let mut app2 = App::new(
            Config::default(),
            ProvidersConfig::default_template(),
            None,
            Mode::Chat,
            false,
            temp_config_path(),
            true,
        );
        app2.mode = Mode::Settings;
        app2.settings.section = 5; // Providers
        let mut t2 = Terminal::new(TestBackend::new(90, 24)).unwrap();
        t2.draw(|f| app2.render(f)).unwrap();
    }

    #[test]
    fn welcome_with_four_options_renders() {
        use ratatui::{backend::TestBackend, Terminal};
        let app = make_app(Mode::Welcome, temp_config_path());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
    }
}
