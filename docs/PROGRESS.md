# Cyber Master — 实施进度跟踪

> 本文件实时反映各阶段实施进度。每完成一项即更新对应勾选状态与说明。
> 设计依据见 [DESIGN.md](./DESIGN.md)，路线图对应 [§13](./DESIGN.md#13-开发路线图)。

**最近更新**：2026-08-04（P2.2 收尾：对话历史持久化 + 流式重绘优化，P2 Chat 100% 完成）

---

## 总览

| 阶段 | 状态 | 完成度 | 说明 |
| --- | :---: | :---: | --- |
| [P1 骨架](#p1-骨架) | ✅ 完成 | 100% | workspace + 配置层 + 启动状态机 + ratatui 主循环 + Welcome 页 |
| [P2 Chat](#p2-chat) | ✅ 完成 | 100% | 流式对话 + 工具调用 + agent loop + 斜杠命令 + 历史持久化 + 流式重绘优化 + 服务商 CRUD + 模型拉取 |
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
>
> 拆分：P2 核心（流式对话）+ P2.2（工具调用 / agent loop / 斜杠命令 / Mock 双模 / 历史持久化 / 流式重绘优化）全部完成。

### 2.1 Provider trait + 三家流式实现 ✅

- [x] `cyber-agent::Provider` trait（对象安全，`fn stream(...) -> Pin<Box<dyn Stream>>`，不用 async-trait，[provider.rs](../crates/cyber-agent/src/provider.rs)）
- [x] `provider_factory(cfg, mock)` 按 `kind` 分发；未知 kind → Err（mock flag 优先于 kind）
- [x] OpenAI 实现：POST `{base}/chat/completions`，`Authorization: Bearer`，SSE `data:` 行解析 `choices[0].delta.content`，`[DONE]` 终止（[openai.rs](../crates/cyber-agent/src/openai.rs)）
- [x] Anthropic 实现：`x-api-key` + `anthropic-version`，`system` 顶层字段，`content_block_delta`→`delta.text`，`message_stop`→Done（[anthropic.rs](../crates/cyber-agent/src/anthropic.rs)）
- [x] Ollama 实现：NDJSON，`message.content`，`done==true`→Done（[ollama.rs](../crates/cyber-agent/src/ollama.rs)）
- [x] `HttpStream` 通用流驱动：`resp.bytes_stream()` + `LineBuf` 行缓冲 + `futures::stream::unfold` 驱动 yield
- [x] `sse::LineBuf` 字节级行缓冲（按 `\n` 切行、去 `\r\n`、UTF-8 不完整字节不产出），三家各一 `parse_line`（[sse.rs](../crates/cyber-agent/src/sse.rs)）
- [x] `resolve_api_key`（`${ENV_VAR}` 展开）放 cyber-core，`ProviderConfig::resolved_api_key()`（[providers.rs](../crates/cyber-core/src/providers.rs)）
- [x] `AgentError`（Http/Stream/Provider/Io/Json/Core），不动 CoreError（[error.rs](../crates/cyber-agent/src/error.rs)）

### 2.2 上下文注入 + agent 任务 ✅

- [x] `build_system_prompt(project)`：BASE_PROMPT + frontmatter(project/scope/authorization/owner) + rules 护栏段；body 暂不注入（[prompt.rs](../crates/cyber-agent/src/prompt.rs)）
- [x] `run_stream(config, providers, project, user_input, history, tx, gen, mock, cwd)`：建 prompt → factory → 驱动流 → 转发 `(gen, AgentEvent)`（[agent.rs](../crates/cyber-agent/src/agent.rs)）
- [x] 任务内 `?` 失败转 `AgentEvent::Error`；`tx.send` 失败（TUI 退出）静默返回；不在任务内 unwrap（agent 永不 panic TUI）

### 2.3 离线 Mock + 流式往返 ✅

- [x] `MockProvider` 双模（[mock.rs](../crates/cyber-agent/src/mock.rs)）：
  - echo 模式（tools 空）：逐字符（20ms）流式回放 `收到：{最后一条 user}` 再 `Done`
  - tool-loop 模式（tools 非空）：第一步发文本 + `list_dir` 工具调用；第二步（工具结果回灌后）发最终文本 + `Done`
- [x] `--mock` CLI flag + `CYBER_MOCK_PROVIDER=1` env（[main.rs](../crates/cyber-app/src/main.rs)）
- [x] 集成测试 `tests/mock_roundtrip.rs`：echo 往返（无项目 / 带历史 / 带 rules / 未知 provider 报错）+ tool-loop 全链路 + generation 标签验证 = 6 例

### 2.4 TUI 异步化 + ChatView ✅

- [x] `App::run` 改 `async`；`main_loop` 用 `tokio::select!`（crossterm `EventStream` / agent 通道 / tick 三路，[app.rs](../crates/cyber-tui/src/app.rs)）
- [x] `ChatState`：`ChatEntry` 枚举（User/Assistant/ToolCall/ToolResult/System）+ `tui-textarea-2` 输入框 + 流式 buffer（submit/finalize/cancel/history/push_tool_call/push_tool_result，[chat.rs](../crates/cyber-tui/src/chat.rs)）
- [x] ChatView：历史区遍历 `ChatEntry`（`[user]`/`[assistant]` 行 + 工具调用 `▶ [tool] name(args)` + 结果 `→ output`/`✗ error` + 流式 `▌` 光标）+ 输入框 + hint（[views/chat.rs](../crates/cyber-tui/src/views/chat.rs)）
- [x] `ChatAction` 键位：Enter 发送 / Shift+Alt+Enter + Ctrl+J 换行 / Esc 取消或返回 / Tab 切模式 / Ctrl+, 设置 / Ctrl+C+Ctrl+Q 退出 / 字母(含 s/q)打字（[event.rs](../crates/cyber-tui/src/event.rs)）
- [x] `handle_agent_event`：Started/Token/ToolCall/ToolResult/Done/Error；generation 守卫（gen 不匹配忽略 stale 事件）
- [x] `JoinHandle::abort`：cancel / 新提交时 abort 旧任务 + bump generation，彻底隔离 stale 事件
- [x] `style_chat_input`：draw 前 `&mut self` 应用 textarea 边框/样式（绕过 render `&self` 限制）

### 2.5 P2 质量保证 ✅

- [x] `tui-textarea-2` 0.12 用 `crossterm_0_28` feature（禁用默认 crossterm 0.29），与本项目 crossterm 0.28 共享 KeyEvent 类型
- [x] `cargo build --workspace` / `cargo clippy --workspace --all-targets -D warnings` 干净
- [x] `cargo test --workspace`：cyber-agent 24（20 lib + 4 集成）+ cyber-core 16 + cyber-tui 62 = 102 全过
- [x] select! 借用坑：agent_rx 为 `main_loop` 局部 `&mut`，tx 为 `self` 字段，`recv()` 不碰 `self`
- [x] submit 返回 `(text, history)`，history 不含当前输入，避免 run_stream 重复 append

### 2.6 工具调用协议 + 内置工具 ✅

- [x] `Tool` trait（对象安全，`run` 返回 boxed future，与 `Provider` 一致，[tool.rs](../crates/cyber-agent/src/tool.rs)）
- [x] `ToolRegistry`：注册 / 按名查找 / 批量导出 schema / 统一执行；`with_builtins()` 装配四内置工具
- [x] `ToolCtx`（cwd + rules + scope）+ `ToolOutput`（content + is_error）+ `ToolSchema`（name + description + parameters JSON Schema）
- [x] 四内置工具（[tools/](../crates/cyber-agent/src/tools/)）：
  - `read_file` — 读文件（UTF-8，附路径上下文）
  - `write_file` — 写文件（护栏：路径须在 cwd 内）
  - `list_dir` — 列目录
  - `shell` — 执行命令（护栏：拦截 `rm -rf /` / 管道至 shell / 危险模式）
- [x] 安全护栏 `guard.rs`：`check_command`（危险模式黑名单）+ `check_write_path`（cwd 逃逸检测）+ `resolve_under_cwd`
- [x] `StreamEvent::ToolCallDelta`（index + id + name + arguments_fragment）+ `AgentEvent::ToolCall/ToolResult`（完整事件）
- [x] 三家 parser 支持 tool-call delta：OpenAI `delta.tool_calls[].function.arguments` / Anthropic `input_json_delta` / Ollama `tool_calls`（[sse.rs](../crates/cyber-agent/src/sse.rs)）

### 2.7 Agent loop + generation 计数器 ✅

- [x] agent loop（[agent.rs](../crates/cyber-agent/src/agent.rs)）：流式 → 累积 `ToolCallDelta`（按 index 合并）→ 执行工具 → 结果回灌 → 再流式，循环至无工具调用或 `max_steps`
- [x] `accumulate_stream`：驱动流到结束，Delta→Token 事件 + 文本累积，ToolCallDelta→按 index 合并为完整 `ToolCall`
- [x] `max_steps` 限制（默认 25）：超限发 `AgentEvent::Error` 而非无限循环
- [x] generation 计数器：`run_stream` 入参 `gen`，每个事件携带 `(gen, AgentEvent)`；TUI cancel/新提交 bump generation，gen 不匹配的 stale 事件被忽略（[app.rs](../crates/cyber-tui/src/app.rs)）
- [x] `auto_tool_call` 配置开关：关闭时不暴露 tools（mock 走 echo 模式）

### 2.8 斜杠命令 ✅

- [x] `slash` 模块（[slash.rs](../crates/cyber-tui/src/slash.rs)）：`SlashCommand` 枚举 + `parse`（大小写不敏感）+ `HELP_TEXT`
- [x] 7 命令：`/help` `/clear` `/mode <name>` `/model <provider>` `/tools` `/cancel` `/quit`
- [x] Submit 分支拦截：输入以 `/` 开头时不发 agent，转 `handle_slash_command`（[app.rs](../crates/cyber-tui/src/app.rs)）
- [x] 流式期限制：`/clear` `/mode` `/model` 阻止（须先 `/cancel`）；`/cancel` 仅流式期有效；`/help` `/tools` `/quit` 任意时刻可用
- [x] `/tools` 经 `ToolRegistry::with_builtins()` 列出名称 + 描述；`/model` 校验 provider 存在性并列出可用项

### 2.9 Mock 双模 + 集成测试 ✅

- [x] `MockProvider` 双模（[mock.rs](../crates/cyber-agent/src/mock.rs)）：echo（tools 空，逐字符 20ms）/ tool-loop（tools 非空，两步：文本+工具调用 → 最终文本）
- [x] 集成测试 `mock_tool_loop_roundtrip`：验证 Started → Token（前导文本）→ ToolCall(list_dir) → ToolResult → Token（最终文本）→ Done 全链路
- [x] 集成测试 `mock_tool_loop_respects_generation_tag`：验证事件携带正确 gen 标签
- [x] echo 测试改用 `auto_tool_call=false` 保持 echo 路径（避免误入 tool-loop 模式）

### 2.10 P2.2 质量保证 ✅

- [x] `cargo build --workspace` / `cargo clippy --workspace --all-targets -D warnings` 干净
- [x] `cargo test --workspace`：cyber-agent 71（65 lib + 6 集成）+ cyber-core 16 + cyber-tui 82 = **169 全过**
- [x] `#[allow(clippy::too_many_arguments)]` 用于 `run_stream`/`run_inner`（9 参数，与 `App::new` 一致）
- [x] `ChatState::history()` 剥离 ToolCall/ToolResult（工具链仅单次 spawn 内部维护，避免历史膨胀 + provider 翻译复杂度）
- [x] `submit()` 返回 `(text, history)`，history 不含当前输入（run_stream 内部 append，防重复）

### 2.11 对话历史持久化 ✅

- [x] `cyber-tui::history` 模块：`cwd_hash`（FNV-1a 64bit，稳定跨 Rust 版本）+ `load`/`save`（原子写 tmp→rename，history_dir 自动创建）+ `history_file`，[history.rs](../crates/cyber-tui/src/history.rs)
- [x] 存储路径 `~/.cyber/history/{cwd_hash}.json`：按 cwd 隔离，不同项目互不干扰；`Paths.history_dir` 字段 + `init::create_global_layout` 首启建目录
- [x] `ChatEntry` 加 `Serialize/Deserialize`：**adjacently tagged**（`tag="kind"` + `content="data"`，如 `{"kind":"User","data":"你好"}`）——internally tagged 无法序列化 newtype 变体 `User(String)`
- [x] App 集成：启动 `run()` 加载历史 → `chat.entries.extend(saved)`；`save_history()` 在 Done/Error/Esc 取消/`/cancel`/`/clear`/`/quit`/Ctrl+C/退出 8 处调用（退出 catch-all 兜底）
- [x] 失败仅记日志不阻断会话（`tracing::warn`）；损坏文件回退空历史而非 panic
- [x] 测试：history 模块 8 单测（hash 稳定/往返/缺文件/建目录/cwd 隔离/空覆盖/损坏回退/文件名）+ App `save_history_persists_to_cwd_hash_file`、`done_event_persists_history` + ChatEntry serde 往返

### 2.12 流式重绘优化 ✅

- [x] 行缓存：`ChatState.cached_history: Vec<Line<'static>>`，由 `prepare_render(&mut self, theme)` 在 draw 前（`style_chat_input` 的 `&mut self` 上下文）按 `entries.len()` 变化或 `cache_dirty` 重建；render 以 `&self` 经 `cached_history()` 只读复用，避免每帧重新 tokenize 全部条目
- [x] 条目→行转换下沉到 `chat::render_entries`（+ `push_role_lines`/`push_tool_call`/`push_tool_result`），视图与模型复用同一实现；流式 tail（`▌` 光标行）由 view 现场构建（量小，不入缓存）
- [x] theme 切换经 `apply_live(Theme)` 调 `invalidate_cache` 标脏，下帧 `prepare_render` 重建（缓存行内嵌旧颜色须刷新）
- [x] 自动滚动到底部：`Paragraph::line_count(width)`（ratatui `unstable-rendered-line-info` feature）含 wrap 折行实际行数，`scroll = total.saturating_sub(visible)`，保证流式新内容始终可见（否则超屏后底部不可见）
- [x] 缓存未就绪回退：直接调用 `render`（未先 `prepare_render`，如单元测试）时 `cached_history` 为空 → 现场构建，避免空渲染；生产主循环每帧先 `prepare_render`，缓存恒就绪
- [x] 测试：`prepare_render_caches_entries_lines` / `prepare_render_rebuilds_only_on_entry_count_change` / `invalidate_cache_forces_rebuild_on_theme_change`

### 2.13 P2 收尾质量保证 ✅

- [x] `cargo build --workspace` / `cargo clippy --workspace --all-targets -D warnings` 干净
- [x] `cargo test --workspace`：cyber-agent 71（65 lib + 6 集成）+ cyber-core 16 + cyber-tui 96 = **183 全过**（较 2.10 新增 14 测试：history 8 + chat 缓存/serde 4 + app 持久化 2）
- [x] ratatui 启用 `unstable-rendered-line-info` feature（`Paragraph::line_count` 供自动滚动；pin 0.30.2，unstable 标签不影响锁定版本）

### 2.14 服务商 CRUD + 模型拉取 ✅

> 参考 `example/wepclaude` 的 customProviders 逻辑，在 Settings 与 Chat 两路管理 LLM 服务商（新增/编辑/删除/设默认 + 异步拉取模型列表）。

**cyber-core 后端**：
- [x] `PROVIDER_KINDS: &[&str]`（openai/anthropic/ollama/openai-compatible）+ `ProviderConfig::normalize()`（trim + 去尾 `/`），[providers.rs](../crates/cyber-core/src/providers.rs)
- [x] `ProvidersConfig` CRUD：`sorted_names()` / `upsert(name, cfg)` / `remove(name) -> Option<ProviderConfig>`
- [x] `save_providers(&ProvidersConfig, &Path)` 原子写（`.tmp` → `.bak` → rename，镜像 `save_config`），[loader.rs](../crates/cyber-core/src/loader.rs)；`lib.rs` re-export
- [x] 单测：KINDS 长度 / normalize / upsert 覆盖 / remove / sorted_names 顺序 + `save_providers_roundtrip` + `save_providers_creates_bak`

**cyber-agent 模型拉取**：
- [x] `fetch_models(&ProviderConfig) -> Result<Vec<String>>`：`reqwest` 15s 超时，按 kind 试 `{base}/models` 与 `{base}/v1/models`（anthropic 先 v1，其余先 /models，含 ollama fallback），headers 按 kind（anthropic→`x-api-key`+`anthropic-version`；openai/compatible→`Authorization: Bearer`；ollama→无 auth），[models.rs](../crates/cyber-agent/src/models.rs)
- [x] `extract_model_ids(&Value) -> Vec<String>`：端口 JS `extractModelIds`（处理 `data[]`/`models[]`/`id`/字符串数组，去重 trim）
- [x] 单测：`extract_model_ids` 各 payload 形态 + `fetch_endpoints` 顺序（离线，不打真实 HTTP）

**cyber-tui 视图层**：
- [x] `views/providers.rs`（新文件）：`ProviderFormState`（name/kind_idx/base_url/api_key/model/max_tokens/temperature/original_name/focused/editing/textarea/fetching/fetch_id/fetch_error/fetched_models/picker_open/picker_selected）+ `FormAction{None,Save,Cancel,Fetch,Toast}` + `handle_key` + `start_fetch`/`deliver_fetch`（stale 守卫）+ `prepare_render` + `render_form` 居中模态
- [x] `views/settings.rs`：`SettingsState` 扩展 `dirty_providers`/`provider_selected`/`pending_delete_idx`；`render_providers_lines` 交互版（`▸` cursor + `★默认` + `[待删除!]` 标记 + hint `a 新增 e 编辑 d 删除 Enter 设默认`）
- [x] `event.rs`：`Action` 加 `AddProvider`(`a`)/`EditProvider`(`e`)/`DeleteProvider`(`d`)；`key_to_action` 映射
- [x] `slash.rs`：`SlashCommand::Provider(String)` + `parse` `/provider` 分支 + `HELP_TEXT` 追加；单测 `/provider` + 子命令 + 大小写

**cyber-tui app 集成**（[app.rs](../crates/cyber-tui/src/app.rs)）：
- [x] `Mode::ProviderForm` + `AppPaths`（打包 config_file/providers_file/history_dir/cwd，净参 11→10）+ `FetchResult { fetch_id, result }`
- [x] `App` 新字段：`provider_form`/`providers_at_entry`/`paths`/`fetch_tx`；`main_loop` 第 4 路 `select!` 分支 `fetch_rx.recv()` → `handle_fetch_result`
- [x] `handle_provider_form_key`：委托 `form.handle_key`，按 `FormAction` 分派（Save 双轨持久化 / Cancel 回 prev_mode / Fetch spawn 任务 / Toast）
- [x] Settings Providers 段分派：`a`/`e`/`d`（双击确认）/`Enter`（设默认）/`Up`/`Down`（provider_selected 导航）
- [x] `handle_slash_command` 加 `Provider(args)`：`list`(空)/`add`/`use <name>`/`remove <name>`/`edit <name>`，流式期阻止
- [x] 持久化双轨：Settings 入口延迟随「保存设置」写盘（`dirty_providers`）；Chat `/provider` 入口立即 `save_providers`
- [x] Esc 双击回滚扩展 `providers_at_entry` 快照；`save_settings` 补 `save_providers` + 重置 `dirty_providers`
- [x] `default_provider` 防悬空：删除/重命名触及默认时自动回退/改名 + toast
- [x] 重命名同步：form Save 时 `original_name == default_provider && != new_name` → 同步改 `default_provider`

**cyber-app 入口**（[main.rs](../crates/cyber-app/src/main.rs)）：
- [x] `mpsc::unbounded_channel::<FetchResult>()` + 构造 `AppPaths` + `App::new` 新签名传 `AppPaths`+`fetch_tx` + `.run(agent_rx, fetch_rx)`

**验证**：
- [x] `cargo build --workspace` / `cargo clippy --workspace --all-targets -D warnings` 干净
- [x] `cargo test --workspace`：cyber-agent 85（79 lib + 6 集成）+ cyber-core 23 + cyber-tui 127 = **235 全过**（较 2.13 新增 52 测试：core 7 + agent 8 + tui 37）
- [x] DESIGN.md §3.2/§9.2/§9.4 同步 + 新增 §9.5 Provider Form 模态层

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
- **2026-08-04**：P2.2 Chat 核心能力补全（§2.6–2.10）。①工具调用协议：`Tool` trait + `ToolRegistry` + 四内置工具（read_file/write_file/list_dir/shell）+ 安全护栏（命令黑名单 + cwd 逃逸检测）+ 三家 parser 支持 tool-call delta。②agent loop：`accumulate_stream` 累积 ToolCallDelta 按 index 合并 → 执行工具 → 结果回灌 → 再流式，`max_steps` 限制；generation 计数器隔离 cancel 后 stale 事件（`run_stream` 入参 gen，事件携带 `(gen, AgentEvent)`）。③斜杠命令：`slash` 模块（7 命令 + 大小写不敏感解析），Submit 分支拦截 `/` 开头输入。④Mock 双模：echo（tools 空）/ tool-loop（tools 非空，两步驱动 agent loop 全链路）。⑤`ChatEntry` 枚举重设计（User/Assistant/ToolCall/ToolResult/System）+ views/chat 渲染工具调用 `▶`/结果 `→`/`✗`。测试 cyber-agent 71 + core 16 + tui 82 = **169 全过**，clippy 干净。**P2 完成度 → 90%**。
- **2026-08-04**：P2.2 收尾——对话历史持久化 + 流式重绘优化（§2.11–2.13），**P2 完成度 → 100%**。①历史持久化：`cyber-tui::history` 模块（FNV-1a `cwd_hash` + 原子写 `load`/`save`），存 `~/.cyber/history/{cwd_hash}.json` 按 cwd 隔离；`Paths.history_dir` + init 首启建目录；`ChatEntry` 改 **adjacently tagged** serde（internally tagged 无法序列化 newtype 变体）；App 启动加载 + Done/Error/cancel/clear/quit/退出 8 处 `save_history`；失败仅 warn 不阻断。②流式重绘优化：`ChatState.cached_history` 行缓存（`prepare_render` 在 `&mut self` draw 前 hook 按 entries.len/theme 变化重建，render `&self` 只读复用）+ 条目→行转换下沉 `chat::render_entries` + theme 切换 `invalidate_cache` + `Paragraph::line_count` 自动滚动到底部（ratatui `unstable-rendered-line-info` feature）+ 缓存未就绪回退现场构建。测试 cyber-agent 71 + core 16 + tui 96 = **183 全过**（+14），clippy 干净。
- **2026-08-04**：服务商 CRUD + 模型拉取（§2.14），参考 `example/wepclaude` customProviders 逻辑。①cyber-core：`PROVIDER_KINDS` + `ProviderConfig::normalize()` + `ProvidersConfig` CRUD（`sorted_names`/`upsert`/`remove`）+ `save_providers` 原子写。②cyber-agent：`fetch_models` 异步拉取（按 kind 试 `/models`/`/v1/models` + provider 专属 headers）+ `extract_model_ids` 端口 JS。③cyber-tui 视图：`views/providers.rs` 模态表单（`ProviderFormState` + `FormAction` + textarea 编辑 + kind ←→循环 + 模型 picker）+ `views/settings.rs` Providers 段交互化（`a`/`e`/`d`/`Enter` + `▸` cursor + 双击 `d` 删除确认）+ `event.rs` 三新 Action + `slash.rs` `/provider list|add|edit|use|remove`。④app 集成：`Mode::ProviderForm` + `AppPaths` 打包 + `FetchResult` 第 4 路 `select!` 分支（`fetch_id` 防 stale）+ 持久化双轨（Settings 延迟 / Chat 立即）+ `providers_at_entry` Esc 回滚 + `default_provider` 防悬空。⑤main.rs 通道 + `AppPaths` 构造。测试 cyber-agent 85 + core 23 + tui 127 = **235 全过**（+52），clippy 干净。DESIGN.md §3.2/§9.2/§9.4 同步 + 新增 §9.5 Provider Form 模态层。
