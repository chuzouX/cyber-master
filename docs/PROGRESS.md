# Cyber Master — 实施进度跟踪

> 本文件实时反映各阶段实施进度。每完成一项即更新对应勾选状态与说明。
> 设计依据见 [DESIGN.md](./DESIGN.md)，路线图对应 [§13](./DESIGN.md#13-开发路线图)。

**最近更新**：2026-08-04（Settings 设置页实现）

---

## 总览

| 阶段 | 状态 | 完成度 | 说明 |
| --- | :---: | :---: | --- |
| [P1 骨架](#p1-骨架) | ✅ 完成 | 100% | workspace + 配置层 + 启动状态机 + ratatui 主循环 + Welcome 页 |
| [P2 Chat](#p2-chat) | ⚪ 未开始 | 0% | LLM provider、流式对话、工具调用 |
| [P3 MCP+Skill](#p3-mcpskill) | ⚪ 未开始 | 0% | MCP 客户端、Skill 加载、统一工具表 |
| [P4 Workflow 引擎](#p4-workflow-引擎) | ⚪ 未开始 | 0% | DAG 定义、画布编辑、执行器、并行调度 |
| [P5 监控+日志](#p5-监控日志) | ⚪ 未开始 | 0% | Dashboard、节点日志、日志分析、断点续跑 |
| [P6 安全工具](#p6-安全工具) | ⚪ 未开始 | 0% | cyber-tools 封装、工具发现、docker 兜底 |
| [P7 打磨](#p7-打磨) | ⚪ 未开始 | 0% | 主题、动画、报告导出、文档、CI |

图例：✅ 完成 / 🟡 进行中 / ⚪ 未开始

---

## P1 骨架

> 验收标准：能启动、能读配置、能渲染空界面

### 1.1 工程结构 ✅

- [x] Cargo workspace 9 crate 骨架（cyber-app / cyber-core / cyber-tui / cyber-agent / cyber-workflow / cyber-mcp / cyber-skills / cyber-tools / cyber-storage）
- [x] 依赖方向约束：app → tui/agent/workflow → core → storage，禁止反向

### 1.2 配置层 ✅

- [x] `cyber-core::paths` — `~/.cyber` 全局路径缓存 + 项目级 `.cyber/` / `.cyber.md` 路径定位
- [x] `cyber-core::error` — 统一 `CoreError` 枚举（NoHomeDir / Init / FileRead / FileEncoding / Toml / TomlSer / Yaml / Config）
- [x] `cyber-core::fsutil` — `read_utf8` 统一读取（区分 FileRead / FileEncoding 并附文件路径）
- [x] `cyber-core::config` — 强类型 `Config`（ui/agent/workflow/tools/storage 五段）
- [x] `cyber-core::providers` — `ProvidersConfig` + 三家默认模板（OpenAI / Anthropic / Ollama）
- [x] `cyber-core::project` — `.cyber.md` frontmatter + 正文解析（serde_yaml + rules 护栏注入）
- [x] `cyber-core::init` — `ensure_global_init` 首次启动创建 `~/.cyber` 目录结构与默认配置
- [x] `cyber-core::loader` — `load_app_context` 三层配置加载（全局 → 项目级覆盖 deep merge → `.cyber.md`）
- [x] 单测覆盖：merge_tables / BOM 容忍 / 首次初始化 / 重复初始化跳过

### 1.3 启动流程状态机 ✅

- [x] 配置加载（`load_app_context` 已就绪）
- [x] CLI 参数解析（clap）：`--cwd` / `--log-level`（见 [main.rs](../crates/cyber-app/src/main.rs)）
- [x] 启动状态机装配：有项目上下文 → Chat；无 → Welcome（`App::new` + main 路由）
- [ ] Provider / MCP / Skill 注册表初始化（P1 仅占位，留 P2/P3 接入；当前 Chat 仅显示 provider 名）

### 1.4 ratatui TUI 主循环 ✅

- [x] `cyber-tui` app 状态机（`Mode` 枚举 + `App` 结构体，见 [app.rs](../crates/cyber-tui/src/app.rs)）
- [x] event loop（crossterm `event::poll` → `Action` 归约，见 [event.rs](../crates/cyber-tui/src/event.rs)）
- [x] render 入口（`ratatui::init/restore` + panic hook 自动恢复终端）
- [x] 主题加载（6 预设：tokyo-night/catppuccin/dracula/gruvbox/nord/cyberpunk，见 [theme.rs](../crates/cyber-tui/src/theme.rs)）
- [x] 鼠标捕获开关（依 `config.ui.mouse` 条件 `EnableMouseCapture`）
- [x] 全局标题栏 + 状态栏布局（`Layout::vertical([1, Min, 1])`）

### 1.5 Welcome 启动页 ✅

- [x] Welcome 视图：引导三选项（新建项目 / 打开工作流 / 进入聊天，见 [welcome.rs](../crates/cyber-tui/src/views/welcome.rs)）
- [x] 键盘导航（↑↓ 循环移动选中 + Enter 确认 + 选中行 `▸` 高亮）
- [x] 无 `.cyber.md` 时自动进入 Welcome（main 状态机路由）
- [x] Chat / Workflow / Dashboard 三个占位页（[chat.rs](../crates/cyber-tui/src/views/chat.rs)，标注 P2/P4/P5 实现）
- [x] `Tab` 在 Chat/Workflow/Dashboard 循环；`Esc` 无项目时回 Welcome；`q`/`Ctrl+C` 退出

### 1.6 P1 质量保证 ✅

- [x] Windows 兼容性审查（code-review skill + 双 sub-agent 交叉验证 + 实测）：BOM / TOCTOU / 中间状态均 false positive
- [x] 修复 2 个 minor：编码错误友好化 + 创建错误附 stage+path 上下文
- [x] 关键路径 tracing 日志：paths / init / loader / fsutil / project 全覆盖
- [x] `cyber-app` main 初始化 tracing subscriber（默认 info，RUST_LOG 覆盖）
- [x] 端到端验证：first_run / 三层配置合并 / `.cyber.md` 解析 / 非 UTF-8 warn 输出
- [x] TUI 验证：`cargo build` / `cargo clippy --workspace -D warnings` 干净 / `cargo test`（core 8 + tui 5 全过）/ clap `--help` 正常 / 非交互首帧渲染 Welcome（标题+三选项+Welcome，7970B 输出，stderr 空，无 panic）
- [x] 修复 cyber-core 既有 clippy `derivable_impls`（`Config` / `ToolsConfig` 手动 Default → derive，等价替换）

### 1.7 Settings 设置页 ✅

> P1 增量：配置查看 / 编辑 / 持久化模态层。设计见 [DESIGN.md §9.4](./DESIGN.md#94-设置页settings-模态层)。

- [x] `Mode::Settings` 模态层：全局 `s` + Welcome 第 4 项进入，`Esc` 返回 `prev_mode`（[app.rs](../crates/cyber-tui/src/app.rs)）
- [x] 设置视图：左侧 6 段侧边栏 + 右侧字段表，Providers 段只读（[settings.rs](../crates/cyber-tui/src/views/settings.rs)）
- [x] 字段编辑模型：fn 指针 `SECTIONS` + `FieldKind`（Bool / Enum / ProviderEnum / Number / ReadOnly）派发
- [x] live-apply：`theme` 重解析 + `mouse` 切换捕获即时生效；其余字段标生效时机（即时/重启/P2–P5/—）
- [x] 持久化：`save_config` 原子写（`.tmp` → `.bak` 备份 → rename）回写 `~/.cyber/config.toml`（[loader.rs](../crates/cyber-core/src/loader.rs)）
- [x] Esc 双击回退：dirty 时首次提示 + 二次用 `config_at_entry` 快照回滚并 live-apply 复位
- [x] dirty 标记：标题 `Settings *` + 保存行 + 底部提示三处可见
- [x] 项目级覆盖警告横幅：检测到 `.cyber/config.toml` 时提示保存仅写全局
- [x] Action 扩展：新增 `OpenSettings` / `Left` / `Right`，键位映射 `s`/`←`/`→`（[event.rs](../crates/cyber-tui/src/event.rs)）
- [x] Welcome 第 4 项「设置 (Settings)」+ 状态栏键位提示同步更新（[welcome.rs](../crates/cyber-tui/src/views/welcome.rs)）
- [x] 单测覆盖：settings 10 个（导航/编辑/枚举循环/Provider 排序/只读/脱敏）+ app 8 个（进入/退出/双击回退/Tab/保存/渲染）；workspace 合计 core 10 + tui 28 = 38 全过，clippy 干净

---

## P2 Chat

> 验收标准：对话模式可用

- [ ] `Provider` trait 抽象（OpenAI / Anthropic / Ollama 三实现）
- [ ] 流式响应（SSE / chunked）
- [ ] 工具调用（tool-calling 协议）
- [ ] 斜杠命令（`/clear` `/mode` `/analyze-logs` 等）
- [ ] 上下文注入（系统提示词 + `.cyber.md` rules + 会话历史）
- [ ] agent loop（max_steps 限制）
- [ ] ChatView 渲染（消息流 + 输入框 + 状态栏）

---

## P3 MCP+Skill

> 验收标准：MCP/Skill 可在 chat 调用

- [ ] MCP 客户端（stdio / SSE / Streamable HTTP）
- [ ] 启动时按 `mcp/servers.toml` 拉起 + 健康检查
- [ ] 工具发现 + schema 缓存
- [ ] Skill 加载（`~/.cyber/skills/` + 项目级）
- [ ] 统一工具表（内置 / MCP / Skill 同质暴露给 agent）

---

## P4 Workflow 引擎

> 验收标准：可编排可运行工作流

- [ ] DAG 数据模型 + 节点类型定义
- [ ] 画布编辑器（节点图自实现：坐标系 / 命中测试 / 拖拽 / 贝塞尔连线）
- [ ] 执行器（tokio::mpsc 流式资产传递，非批处理）
- [ ] 并行调度（max_parallel_nodes）
- [ ] 断点续跑（checkpoint）
- [ ] 节点类型：recon / classify / scan / mcp-tool / report 等

---

## P5 监控+日志

> 验收标准：实时可观测

- [ ] Dashboard 视图（工作流列表 + 全局状态）
- [ ] 工作流详情视图（Overview / Nodes / Logs / Stats / Assets）
- [ ] 节点级日志缓冲（环形 5000 行 + 溢出落盘）
- [ ] 日志持久化（`~/.cyber/logs/YYYY-MM-DD.log` 滚动）
- [ ] `/analyze-logs` LLM 归类/根因建议
- [ ] 断点续跑 UI 恢复

---

## P6 安全工具

> 验收标准：内置工具链可用

- [ ] `cyber-tools` 工具封装（subfinder / nmap / nuclei / httpx 等）
- [ ] 工具发现与版本检测
- [ ] docker 兜底（`prefer_docker` 配置项）
- [ ] 工具调用统一接口（注入统一工具表）

---

## P7 打磨

> 验收标准：发布 v0.1

- [ ] 主题（catppuccin / tokyo-night / dracula / gruvbox / nord）
- [ ] 动画（依 `config.ui.animations`）
- [ ] 报告导出（Markdown > HTML > JSON > PDF，Handlebars 模板）
- [ ] 文档完善
- [ ] CI（lint / test / cross-build）

---

## 遗留与待办

| 项 | 说明 | 状态 |
| --- | --- | --- |
| `~/.cyber` 测试残留 | `c:\Users\chuzo\.cyber` 为测试期间生成的默认配置模板；沙箱 allowlist 限制无法程序化删除 | ⚠️ 待用户决定保留或手动清理 |

---

## 变更日志

- **2026-08-04**：初始化 PROGRESS.md。P1 配置层（1.1 / 1.2 / 1.6）完成；启动状态机（1.3）部分完成；TUI 主循环（1.4）与 Welcome 页（1.5）未开始。
- **2026-08-04**：P1 收口。完成 1.3 启动状态机（clap + 路由）、1.4 ratatui 主循环（`ratatui::init/restore` + 同步事件循环 + 5 主题 + 鼠标开关 + 标题/状态栏）、1.5 Welcome 页（三选项导航 + Chat/Workflow/Dashboard 占位 + Tab/Esc/q 键位）。新增 `cyber-tui` 四模块（app/event/theme/views）+ 重写 `cyber-app` main。clippy 全 workspace 干净，单测全过，首帧渲染验证通过。**P1 完成度 → 100%**。
- **2026-08-04**：新增 `cyberpunk` 主题（深紫黑底 #0D0221 + 霓虹粉 accent #FF2A6D + 霓虹青 title/选中 #05D9E8），灵感来自 ratatui.rs "Built with Ratatui" 项目群的暗底霓虹 TUI 美学。`Theme::resolve` + 单测 + 配置注释 + DESIGN.md §2.4/§9.3 同步更新。
- **2026-08-04**：默认主题由 `tokyo-night` 改为 `cyberpunk`。改动 `UiConfig::default` + `default_config.toml` 模板 + `Theme::resolve` 未知回退（拆分 `tokyo-night` 显式臂与 `_` 回退臂，避免 tokyo-night 被错误回退）+ DESIGN.md §2.4 示例 + 单测。
- **2026-08-04**：实现 Settings 设置页（§1.7）。新增 `Mode::Settings` 模态层 + `views::settings`（fn 指针 `SECTIONS` + `FieldKind` 编辑模型 + `LiveApply` 即时应用）+ `save_config` 原子写持久化（`.tmp`/`.bak`）+ Esc 双击回退（`config_at_entry` 快照）+ 项目级覆盖警告横幅。扩展 `Action`（`OpenSettings`/`Left`/`Right`）与键位映射，Welcome 第 4 项 + 状态栏提示同步。单测 settings 10 + app 8，workspace 合计 38 全过，clippy 干净。DESIGN.md §9.2 键位表 + §9.4 设置页小节（含字段生效时机表）同步。
