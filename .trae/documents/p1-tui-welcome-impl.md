# P1 剩余实现：TUI 主循环 + Welcome 启动页

## Context

P1 骨架阶段验收标准是"能启动、能读配置、能渲染空界面"。当前已完成工程结构与配置层（[cyber-core](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-core/src) + [loader.rs](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-core/src/loader.rs)），但 [cyber-app/src/main.rs](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-app/src/main.rs) 只做了 `println` 打印配置，[cyber-tui/src/lib.rs](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-tui/src/lib.rs) 仍是空占位。

本次目标：补齐 P1 剩余 4 项——clap 参数解析、启动状态机装配、ratatui TUI 主循环、Welcome 启动页，达到"能渲染空界面"。完成后更新 [docs/PROGRESS.md](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/docs/PROGRESS.md)。

设计依据：[DESIGN.md §2.3 启动状态机](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/docs/DESIGN.md#L143)、[§9 UI/UX](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/docs/DESIGN.md#L473)、[§10 状态管理](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/docs/DESIGN.md#L506)。

## 范围边界

**本次做**（P1 验收最小集）：
- clap 解析 `--cwd` / `--log-level`
- 启动状态机：`load_app_context` → 有 `project` 进 Chat 占位 / 无 `project` 进 Welcome
- ratatui 主循环（alternate screen + 事件轮询 + 渲染 + 终端恢复，panic 安全）
- Welcome 页：三选项 `↑↓` 导航 + `Enter` 确认 + 选中高亮
- Chat / Workflow / Dashboard 三个占位页（显示项目信息 + "P2/P4/P5 实现"提示）
- `Tab` 在 Chat/Workflow/Dashboard 间循环；`Esc` 无项目时回 Welcome；`q`/`Ctrl+C` 退出
- 基础主题：5 预设颜色对（按 `config.ui.theme` 字符串匹配）
- 全局标题栏（模式·项目·provider·状态）+ 状态栏（键位提示）

**本次不做**（留给 P2+）：
- tokio 事件总线 / agent 流式（P1 用同步 `event::poll` 阻塞循环即可）
- tui-textarea 输入框、命令面板、帮助模态、动画
- 真实 Chat 功能、Workflow DAG 画布、Dashboard 数据

## 依赖版本（核实于 docs.rs 2026-08）

- `ratatui = "0.30"`（提供 `init()` / `restore()` 便捷函数，内置 panic hook 自动恢复终端）
- `crossterm = "0.28"`（与 ratatui 0.30 配对，`event::poll`/`event::read` API 稳定）

## 实现步骤

### 1. 依赖声明

**根 [Cargo.toml](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/Cargo.toml) `[workspace.dependencies]`** 增：
```toml
ratatui = "0.30"
crossterm = "0.28"
```

**[crates/cyber-tui/Cargo.toml](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-tui/Cargo.toml)** 改 `[dependencies]`：
```toml
cyber-core = { workspace = true }
ratatui = { workspace = true }
crossterm = { workspace = true }
color-eyre = { workspace = true }
tracing = { workspace = true }
```

**[crates/cyber-app/Cargo.toml](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-app/Cargo.toml)** 增：
```toml
cyber-tui = { workspace = true }
clap = { workspace = true }
```

### 2. cyber-tui 模块拆分

新建 `crates/cyber-tui/src/` 下：

- **`theme.rs`**：`Theme` 结构体（字段 `bg/fg/accent/muted/border/sel_bg/sel_fg/title` 均为 `ratatui::style::Color`）+ 5 个 `const` 预设（tokyo-night / catppuccin / dracula / gruvbox / nord）+ `Theme::resolve(name: &str) -> Theme`（未知名回退 tokyo-night）。配单测 `resolve_known` / `resolve_unknown_falls_back`。

- **`event.rs`**：`Action` 枚举（`Quit / Up / Down / Enter / Tab / Esc / Other`）+ `pub fn next_action(timeout: Duration) -> io::Result<Option<Action>>`。内部 `event::poll(timeout)` → `event::read()`，仅匹配 `Event::Key(k)` 且 `k.kind == KeyEventKind::Press`（Windows 过滤 Release/Repeat），映射到 `Action`。

- **`app.rs`**：
  - `#[derive(Clone,Copy)] pub enum Mode { Welcome, Chat, Workflow, Dashboard }`
  - `pub struct App { config: Config, project: Option<ProjectContext>, mode: Mode, selected: usize, toast: Option<String>, should_quit: bool }`
  - `impl App { pub fn new(config, project, initial_mode) -> Self; pub fn run(self) -> color_eyre::Result<()>; fn main_loop(&mut self, terminal) -> io::Result<()>; fn handle_action(&mut self, Action); fn render(&self, frame: &mut Frame); }`
  - `run()`：`ratatui::init()` → 依 `config.ui.mouse` 决定是否 `execute!(EnableMouseCapture)` → `main_loop` → `DisableMouseCapture` → `ratatui::restore()`（即使 `main_loop` 返回 Err 也先 restore，用 `let result = ...; restore(); result` 保证）
  - `main_loop`：`loop { terminal.draw(|f| self.render(f))?; if let Some(a) = next_action(250ms)? { self.handle_action(a) } if self.should_quit { break } }`
  - `handle_action`：全局 `Quit/Ctrl+C`→退出；`Tab` 在 Chat/Workflow/Dashboard 循环（Welcome 下 Tab 无效）；`Esc` 当 `project.is_none()` 且 mode≠Welcome → 回 Welcome；Welcome 下 `Up/Down` 调 `selected`（3 项循环），`Enter` 按选中项：进聊天→Chat，新建项目/打开工作流→设 toast "（P1 占位：后续阶段实现）"
  - `render`：`Layout::vertical([Length(1), Min(0), Length(1)])` 拆标题栏/主区/状态栏；标题栏显示 `mode · project名?· provider · first_run标记`；主区按 `mode` 分发到 views 函数；状态栏显示键位提示

- **`views/mod.rs`**：`pub mod welcome; pub mod chat;` + `pub use ...`
- **`views/welcome.rs`**：`pub fn render(frame, area, theme, selected, toast)`——居中块，标题 "Cyber Master v0.1.0"，三行 `List`（新建项目/打开工作流/进入聊天）选中行 `sel_bg/sel_fg` 高亮 + `▸` 前缀，底部 hint `↑↓ 导航 Enter 确认 q 退出`；toast 有值时在底部一行显示
- **`views/chat.rs`**：`pub fn render(frame, area, theme, project: Option<&ProjectContext>, provider: &str)`——`Paragraph` 显示 "Chat Mode（P2 实现流式对话）"，下方列项目信息（project/scope/owner/rules 条数）+ provider；Workflow/Dashboard 占位复用此文件同构函数 `render_placeholder(title, stage, ...)`

- **`lib.rs`**：`pub mod app/event/theme/views; pub use app::{App, Mode}; pub use theme::Theme;`

### 3. cyber-app 入口改造

**[crates/cyber-app/src/main.rs](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-app/src/main.rs)** 重写：
```rust
use clap::Parser;
use cyber_core::load_app_context;
use cyber_tui::{App, Mode};

#[derive(Parser)]
#[command(name = "cyber", version, about = "网络安全智能体终端")]
struct Cli {
    #[arg(long)] cwd: Option<PathBuf>,
    #[arg(long)] log_level: Option<String>,
}

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    // tracing 初始化（沿用现有 EnvFilter 逻辑，cli.log_level 覆盖默认 info）
    let cwd = cli.cwd.unwrap_or_else(|| std::env::current_dir().unwrap());
    let ctx = load_app_context(&cwd)?;
    let initial = if ctx.project.is_some() { Mode::Chat } else { Mode::Welcome };
    App::new(ctx.config, ctx.project, initial).run()?;
    Ok(())
}
```
（tracing 初始化保留现有 `EnvFilter::try_from_default_env`，`log_level` 提供时注入。）

### 4. 复用现有代码

- 启动状态机的"加载配置"环节直接复用 `cyber_core::load_app_context`（已含首次初始化 + 三层合并 + `.cyber.md` 解析 + tracing 日志）
- 项目上下文信息渲染复用 `ProjectContext{frontmatter, rules()}`
- 主题名来自 `Config::ui.theme`，默认值来自 `UiConfig::default()`

## 验证

1. **编译**：`cargo build -p cyber-app` 通过；`cargo clippy --workspace -- -D warnings` 无警告
2. **单测**：`cargo test -p cyber-tui`（theme resolve + action 映射）
3. **端到端手动验证**（TUI 视觉为主）：
   - 在无 `.cyber.md` 的临时目录运行：`cargo run -p cyber_app -- --cwd <tempdir>` → 渲染 Welcome，`↑↓` 移动高亮，`Enter` 进 Chat 占位，`Tab` 循环 Chat/Workflow/Dashboard，`Esc` 回 Welcome，`q` 退出且终端正常恢复
   - 在有 `.cyber.md` 的项目根运行 `cargo run -p cyber_app` → 直接进 Chat 占位页显示项目信息
   - 强制 panic 验证（临时加 `panic!()`）：`ratatui::init()` 的 panic hook 恢复终端后打印 panic
   - `RUST_LOG=cyber_core=debug cargo run` 确认配置加载日志正常输出
4. **更新 [docs/PROGRESS.md](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/docs/PROGRESS.md)**：1.3/1.4/1.5 勾选完成，P1 完成度 → 100%，变更日志加一条

## 风险与注意

- Windows 终端按键会发 Release/Repeat 事件 → 必须用 `KeyEventKind::Press` 过滤，否则 `q` 退出会触发两次
- `ratatui::restore()` 必须在 `main_loop` 出错时也执行，用 `let r = ...; restore(); r` 模式而非 `?` 提前返回
- `config.ui.mouse` 为 false 时不启用 `EnableMouseCapture`，避免吞终端选区
