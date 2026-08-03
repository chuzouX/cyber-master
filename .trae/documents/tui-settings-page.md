# TUI 设置页面实现方案

## Context（为什么做）

P1 骨架已完成（启动/配置/Welcome/三模式占位/6 主题），但用户无法在 TUI 内查看或修改配置——改主题/鼠标/provider 必须手动编辑 `~/.cyber/config.toml`。本方案在 TUI 中加入**可编辑 + 持久化**的设置页面，让用户在终端内调整配置并回写磁盘，主题/鼠标即时生效。这是 P1→P2 过渡期的实用增强，也为后续 Chat/Workflow 模式提供运行时配置入口。

进入方式：全局热键 `s` + Welcome 第 4 项。

## 关键设计决策（已拍板）

1. **保存机制**：右侧面板底部「保存设置」按钮行 + `Enter` 触发。**不用 `Ctrl+S`**——`docs/DESIGN.md:489` §9.2 已把 `Ctrl+S` 预留给"保存会话"，避免日后撞键。
2. **Esc = 双击丢弃 + 真回退**：进入 Settings 时快照 `config_at_entry`；编辑即时改 `self.config` + live-apply；首次 Esc（dirty 时）弹 toast 不退出，二次 Esc 回退到 `config_at_entry` 并重新 live-apply 撤销视觉改动后退出。消除"改了看似保留、重启回弹"的矛盾。
3. **保存范围**：把合并后的 Config 写回 `~/.cyber/config.toml`，写前 `.bak` 备份 + 原子 `tmp`→`rename`。若 `cwd/.cyber/config.toml` 存在，顶部显示横幅"保存仅写全局；被项目覆盖的字段重启后回弹"。已知损失：`toml::to_string_pretty` 会丢掉默认配置的行内注释（toml crate 不保注释），UI 加一句说明。
4. **「生效」列**：诚实标注每个字段生效时机（即时/重启/P2/P3/P4/P5/—），避免"改了没反应"的误导。死配置字段（frame_rate/animations/extra_path/log_level）标 ReadOnly。
5. **Live-apply 仅 theme + mouse**：`theme` 改后 `self.theme = Theme::resolve(...)` 重新解析；`mouse` 改后 `execute!(Enable/DisableMouseCapture)`。`default_provider` 即时生效但无需特殊处理（标题栏每帧读 `config.agent.default_provider`）。
6. **`default_mode` 接线 main.rs**：启动初始模式读 `config.ui.default_mode`（有项目时），让"重启生效"标注诚实。`log_level` 不接线（tracing 在 `load_app_context` 之前初始化，chicken-and-egg），标 ReadOnly + 说明"由 RUST_LOG / --log-level 控制"。
7. **`default_provider` 选项动态化**：枚举选项来自 `providers.providers.keys()`，不写死三家。
8. **鼠标 cleanup 修正**：`run()` 退出时**无条件** `DisableMouseCapture`（当前用启动时快照值，中途开启鼠标后退出可能泄漏）。
9. **新增 Action 最小化**：仅 `Left` / `Right` / `OpenSettings`。无 `Save`（按钮行 Enter）、无 `BackTab`（Tab 段循环回绕即可）。
10. **Providers 段只读**：列出 name/kind/base_url/model，`api_key` 脱敏（`${ENV}` 引用原样显示，明文 key 显示前 4 位 + `****`）。

## 字段表（SECTIONS 定义）

| 段 | 字段 | kind | 生效 | live | 备注 |
|---|---|---|---|---|---|
| UI | theme | Enum[6 主题] | 即时 | Theme | 重解析 |
| UI | default_mode | Enum[chat/workflow/dashboard] | 重启 | — | main.rs 接线 |
| UI | mouse | Bool | 即时 | Mouse | capture toggle |
| UI | animations | ReadOnly | — | — | P6 |
| UI | frame_rate | ReadOnly | — | — | loop poll 驱动 |
| Agent | default_provider | Enum[providers.keys] | 即时 | — | 标题栏重绘 |
| Agent | auto_tool_call | Bool | P2 | — | |
| Agent | max_steps | Number{1,200,1} | P2 | — | u32 |
| Workflow | max_parallel_nodes | Number{1,64,1} | P4 | — | u32 |
| Workflow | default_timeout_secs | Number{1,86400,60} | P4 | — | u64 |
| Workflow | checkpoint | Bool | P4 | — | |
| Tools | prefer_docker | Bool | P3 | — | |
| Tools | extra_path | ReadOnly | — | — | 列表 |
| Storage | history_retention_days | Number{1,3650,1} | P5 | — | u32 |
| Storage | log_level | ReadOnly | — | — | RUST_LOG/--log-level 控制 |
| Providers | (整段) | ReadOnly | — | — | api_key 脱敏 |

`FieldDef { label, kind, effect:&'static str, live:LiveApply, get:fn(&Config)->String, set:fn(&mut Config,String) }`，`static SECTIONS: &[SectionDef]`。fn 指针在 static 中合法（Rust 1.96 const-stable）。

## 键位（Settings 模式）

- `s`（全局）/ Welcome `Enter` on item 3 → 进入 Settings（记 `prev_mode`）
- `↑`/`↓`：上一/下一行（字段 + 保存按钮，回绕）
- `Tab`：下一段（回绕）
- `Enter`：字段行 → 编辑（bool 切换 / enum 正向循环）；保存按钮行 → 保存
- `←`/`→`：enum 反/正向循环；number −/+step；bool 切换；ReadOnly/保存按钮 → no-op
- `Esc`：dirty → 首次 toast「再按 Esc 丢弃，或选保存」+ 置 `pending_discard`；二次 → 回退 `config_at_entry` + 重 apply theme/mouse + 退出回 `prev_mode`。非 dirty → 直接退出
- `q`：退出（全局）

## 实施步骤（有序）

1. **crates/cyber-core/src/loader.rs**：加 `pub fn save_config(config: &Config, path: &Path) -> Result<()>`——备份旧文件 → `toml::to_string_pretty` → 写 `path.tmp` → `fs::rename` 覆盖。错误用 `CoreError::TomlSer`/`Io`。加 round-trip 单测（默认/非默认/含 Vec/UTF-8 字符串）。
2. **crates/cyber-core/src/lib.rs**：`pub use loader::save_config;`。
3. **crates/cyber-tui/src/event.rs**：加 `Action::{Left, Right, OpenSettings}`；`key_to_action`：`s`→OpenSettings、Left/Right 箭头→Left/Right。注意 `Ctrl+s` 不映射（保留给未来会话保存）。加单测。
4. **crates/cyber-tui/src/views/settings.rs（新）**：定义 `FieldKind`/`LiveApply`/`FieldDef`/`SectionDef`/`SECTIONS`/`SettingsState{section,selected,dirty,pending_discard}` + 纯函数 `toggle_bool`/`cycle_enum`/`adjust_number` + `render(frame,area,theme,config,providers,&SettingsState,has_project_config)`。布局：`Layout::horizontal` 左侧段列表（~22）+ 右侧字段行（`label : value  生效`）+ 保存按钮行 + 顶部横幅（has_project_config 时）。Providers 段 api_key 脱敏。窄终端右侧 `Min(40)`。
5. **crates/cyber-tui/src/views/mod.rs**：`pub mod settings;`
6. **crates/cyber-tui/src/app.rs**：
   - `Mode::Settings` + `label()`；
   - `App` 加字段 `providers`/`config_file:PathBuf`/`settings:SettingsState`/`prev_mode:Mode`/`config_at_entry:Config`/`has_project_config:bool`；
   - `App::new(config, providers, project, initial, first_run, config_file, has_project_config)`；
   - `handle_action`：OpenSettings（非 Settings 时记 prev_mode+快照 config_at_entry+进 Settings）、Tab（Settings→下一段 / 否则循环模式）、Up/Down（Settings→行导航 / Welcome 导航）、Left/Right（Settings→apply_edit+live_apply）、Enter（Settings→编辑或保存；Welcome 4 项 WELCOME_OPTIONS=4）、Esc（Settings→双击回退 / 否则现有逻辑）、Quit；
   - `live_apply(&mut self, LiveApply)`；
   - `render_main` 加 Settings 分支；`render_status_bar` 加 Settings hint；`run()` cleanup 无条件 `DisableMouseCapture`。
7. **crates/cyber-app/src/main.rs**：`initial_mode` 读 `config.ui.default_mode`（有项目时）；算 `has_project_config = cwd.join(".cyber").join("config.toml").exists()`；传 `ctx.providers`/`ctx.paths.config_file.clone()`/`has_project_config` 给 `App::new`。
8. **docs/PROGRESS.md / DESIGN.md**：记 Settings 实现 + 字段生效时机表 + `s` 键 P2 需门控备忘。

依赖顺序：1→2；3、4 并行；5 随 4；6 依赖 1-5；7 依赖 6；8 末尾。

## 验证

- `cargo test --workspace`：cyber-core（save_config round-trip + 原子性 + 现有 8）、cyber-tui（toggle_bool/cycle_enum/adjust_number 边界 + SettingsState 导航 + event 新映射 + 现有 5）、app handle_action 集成（进/出 Settings、theme live-apply、双击 Esc 回退、Save 写盘+dirty 清、Welcome item 3）。
- `cargo clippy --workspace --all-targets -- -D warnings` 干净。
- 手验（真实终端 `cargo run -p cyber-app`）：进/出 Settings；6 主题即时切换；mouse 切换；保存重启值保留；双击 Esc 回退；项目级 `.cyber/config.toml` 存在时横幅；api_key 脱敏；窄终端布局。

## 风险/备忘

- `s` 键在 P2 Chat 文本输入态需门控（在 `handle_action` 拦截，不塞进 `key_to_action`）。
- 保存丢行内注释（toml crate 限制），换 `toml_edit` 保注释列为后续增强。
- 保存范围：项目级覆盖烘进全局，重启后项目级仍覆盖——横幅已提示。
- `config_at_entry` clone 在 P2 tokio 化后需重新审视（DESIGN §10.1 用 `Arc<Config>`）。
- Settings 是"用 Mode 模拟的模态层"，未来引入真正 overlay 层时迁移。
