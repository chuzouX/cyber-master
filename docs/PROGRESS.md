# Cyber Master — 实施进度跟踪

> 本文件实时反映各阶段实施进度。每完成一项即更新对应勾选状态与说明。
> 设计依据见 [DESIGN.md](./DESIGN.md)，路线图对应 [§13](./DESIGN.md#13-开发路线图)。

**最近更新**：2026-08-04（max_steps 调大至 50 + 死循环检测 + shell PATHEXT 修复）

---

## 总览

| 阶段 | 状态 | 完成度 | 说明 |
| --- | :---: | :---: | --- |
| [P1 骨架](#p1-骨架) | ✅ 完成 | 100% | workspace + 配置层 + 启动状态机 + ratatui 主循环 + Welcome 页 |
| [P2 Chat](#p2-chat) | ✅ 完成 | 100% | 流式对话 + 工具调用 + agent loop + 斜杠命令 + 历史持久化 + 流式重绘优化 + 服务商 CRUD + 模型拉取 |
| [P3 MCP+Skill](#p3-mcpskill) | ✅ 完成 | 100% | MCP 三传输客户端（stdio / Streamable HTTP / legacy SSE，actor 模式）+ Skill 目录扫描（渐进式披露）+ 统一工具表 + `/skill` `/mcp` `/new` `/sessions` 命令 + 多 Session 管理 |
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
- [x] `max_steps` 限制（默认 25）：超限不裸中断，而是追加「步数上限」user 提示 + 一次无工具收尾流式让模型总结已收集的信息后正常发 `Done`（总结文本入 history 使「继续」有上下文），而非无限循环或裸 `Error`
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
>
> 拆分：P3.1 Skills（纯本地 IO）→ P3.2 MCP stdio client（actor 模式）→ P3.3 Wiring（统一工具表 + TUI 命令）。全部完成。
>
> 范围确认（v0.1）：MCP 传输**仅 stdio**（SSE/HTTP 在 `servers.toml` 解析但启动跳过）；Skill **仅显式触发**（`skill_<name>` 工具 + `/skill` 命令，不做 triggers 自动匹配）；新增 `/mcp` 命令（`list`/`status`）。

### 3.1 cyber-skills（Skill 加载 + 渐进式披露）✅

- [x] `Cargo.toml` 加 `cyber-core` / `cyber-agent` / `serde` / `serde_json` / `serde_yaml` / `tracing` / `thiserror` + dev `tokio`
- [x] `frontmatter.rs` — `SkillFrontmatter { name, description, triggers, tools }` + `parse`（BOM strip + `---` 分隔 + serde_yaml，复用 `.cyber.md` 解析模式，拷贝而非泛型化 core 以免影响 38 个 core 测试）
- [x] `skill.rs` — `Skill { frontmatter, body, path, source: SkillSource(Global|Project) }` + `Skill::load(path, source)` 用 `cyber_core::fsutil::read_utf8`，缺 name → Err（[skill.rs](../crates/cyber-skills/src/skill.rs)）
- [x] `registry.rs` — `SkillRegistry::load_all(global_dir, project_dir) -> (Self, Vec<(PathBuf, SkillError)>)`：扫描两目录下子目录 `SKILL.md`，项目级覆盖全局同名（按 `frontmatter.name` 去重，Project 优先）；`iter()` / `find(name)` / `len()` / `is_empty()` / `new()`（[registry.rs](../crates/cyber-skills/src/registry.rs)）
- [x] `tool.rs` — `SkillTool { skill: Arc<Skill> }` impl `Tool`：name=`skill_<name>`，desc 含 description+triggers+「渐进式披露」提示，`run` 返回 body（[tool.rs](../crates/cyber-skills/src/tool.rs)）
- [x] `error.rs` — `SkillError`（thiserror）
- [x] 测试 23 例：frontmatter（BOM/无 frontmatter/特殊字符/无闭合分隔）+ Skill::load（tempdir/非文件/缺 name/BOM）+ load_all（全局/项目新增/项目覆盖同名/空目录/缺 SKILL.md 跳过/无效 skill 入 errors 不 panic）+ SkillTool（命名前缀/desc 含 description+triggers/triggers 空省略行/run 返回 body/run 忽略 input）

### 3.2 cyber-mcp（stdio 客户端，actor 模式）✅

- [x] `Cargo.toml` 加 `cyber-core` / `cyber-agent` / `tokio` / `futures` / `serde` / `serde_json` / `toml` / `thiserror` / `tracing` + dev `tokio`（full+test-util）
- [x] `proto.rs` — JSON-RPC 2.0 类型（`JsonRpcRequest/Response/Error`）+ MCP 协议类型（`InitializeParams/Result`、`ToolListResult`、`McpToolSchema`、`CallToolParams/Result`、`McpContent`）+ `client_info()` + `PROTOCOL_VERSION`（[proto.rs](../crates/cyber-mcp/src/proto.rs)）
- [x] `config.rs` — `McpServersConfig { servers }` + `McpServerSpec { name, transport, command, args, env, url, headers, timeout_secs }` + `McpTransport(Stdio|Sse|Http)` + `load(path)` 用 `read_utf8`+`toml::from_str` + `parse_env_map`（`${ENV}` 展开）+ `normalized_timeout()`（默认 5s）（[config.rs](../crates/cyber-mcp/src/config.rs)）
- [x] `transport.rs` — `Transport` trait（async write/read/close）+ `StdioTransport`（`tokio::process::Command` spawn）+ `#[cfg(test)] PipeTransport`（`tokio::io::duplex`，测试用）
- [x] `connection.rs` — `McpConnection`（非泛型，可 `Arc` 共享）：`spawn_stdio(spec)` / `spawn_with_transport(transport, spec)`（测试用）；actor loop `tokio::select!`（req_rx 写 stdin + stdout 按 `\n` 切行解析 + 按 id 路由 pending oneshot + notification log 忽略 + 半行缓冲）；`call(method, params)` 带 30s 超时；`shutdown()`；`tools()` / `server_name()`（[connection.rs](../crates/cyber-mcp/src/connection.rs)）
- [x] `tool.rs` — `McpTool { server: Arc<McpConnection>, schema: McpToolSchema }` impl `Tool`：name=`mcp_<server>_<tool>`（sanitize 非法字符→`_`），desc=`[<server>] <desc>`，`run` 发 `tools/call` 拼 content[] text → `ToolOutput`（[tool.rs](../crates/cyber-mcp/src/tool.rs)）
- [x] `registry.rs` — `McpRegistry::connect_all(spec) -> (Self, Vec<McpTool>, Vec<(String, McpError)>)`：`futures::future::join_all` 并行 spawn（per-server timeout），失败 warn+skip，SSE/HTTP → `UnsupportedTransport` 入 errors；`shutdown_all()`（join handles await）；`len()` / `is_empty()` / `iter()`（[registry.rs](../crates/cyber-mcp/src/registry.rs)）
- [x] `error.rs` — `McpError`（Io/Json/UnsupportedTransport/InitFailed/Timeout/Rpc/ToolNotFound/ChannelClosed/Core/Agent/Toml）
- [x] 测试 24 例：proto（serialize/deserialize request/response/error/tool-list/call-tool-result/missing is_error 默认 false/notification 无 id）+ config（parse stdio/http+headers/env/默认超时/零超时回退/空配置/缺文件/temp 文件加载）+ connection（用 `PipeTransport`+测试 task 模拟 server：call roundtrip / rpc error 传播 / 超时 / call_tool 返回 content / mcp_content 是 text）+ tool（命名前缀 / sanitize / 空 description 仅 server）

### 3.3 Wiring（cyber-agent / cyber-core / cyber-tui / cyber-app）✅

**cyber-core**：
- [x] `Paths::project_skills_dir(cwd)` → `project_local_dir(cwd).join("skills")`（[paths.rs](../crates/cyber-core/src/paths.rs)）
- [x] `fsutil` 已 `pub mod`，`read_utf8` 可被 mcp/skills 调用（P1 即就绪）

**cyber-agent**（[agent.rs](../crates/cyber-agent/src/agent.rs)）：
- [x] `run_stream` + `run_inner` 加 `registry: Arc<ToolRegistry>` 形参（9→10 参数，保留 `#[allow(clippy::too_many_arguments)]`），删内部 `ToolRegistry::with_builtins()` 构造
- [x] `tests/mock_roundtrip.rs`：`spawn_run` helper 加 `registry` 参数，7 处调用点传 `Arc::new(ToolRegistry::with_builtins())`（机械化，断言不变）

**cyber-tui**：
- [x] `Cargo.toml` 加 `cyber-mcp` + `cyber-skills`（依赖方向：tui → mcp/skills → agent + core）
- [x] `AppRegistries { tools: Arc<ToolRegistry>, skills: Arc<SkillRegistry>, mcp: Option<Arc<McpRegistry>> }`（手动 `Debug`，因 `ToolRegistry` 无 `Debug`）+ `with_builtins()` 测试构造（[app.rs](../crates/cyber-tui/src/app.rs)）
- [x] `App::new` 加 `registries: AppRegistries` 参数；`App` 持 `self.registries`
- [x] `spawn_agent`：`let registry = self.registries.tools.clone();` 传入 `run_stream`（跨 turn 共享，MCP 连接长存）
- [x] `/tools` handler 改用 `self.registries.tools.schemas()`（删 `ToolRegistry::with_builtins()`）
- [x] `handle_skill_slash(args)`：`/skill list`(空) 列出 skills（名称+来源[全局/项目]+描述）；`/skill <name>` 注入 body 为 System 条目；未知 skill 提示
- [x] `handle_mcp_slash(args)`：`/mcp list|status` 列出 server 连接状态（名称+工具数；mock 或无 server 时提示）
- [x] `bootstrap.rs`（新文件）：`pub async fn build_registries(paths, cwd, mock) -> (AppRegistries, Vec<String>)` — builtins + skills（同步扫描）+ MCP（非 mock 时 `connect_all`，mock 跳过）；返回 errors 供 toast；**永不返回 Err**（保证 TUI 启动）（[bootstrap.rs](../crates/cyber-tui/src/bootstrap.rs)）
- [x] `slash.rs`：加 `/skill` + `/mcp` 到 `COMMANDS`/`SlashCommand`/`parse`/`HELP_TEXT` + 测试
- [x] `make_app` 测试 helper 加 `AppRegistries`（用 `with_builtins` + `SkillRegistry::new()` + `mcp=None`），所有 app 测试调用点同步更新
- [x] `App::run` 退出前 `mcp.shutdown_all()`（若 Some）+ abort agent 任务

**cyber-app**（[main.rs](../crates/cyber-app/src/main.rs)）：
- [x] `Cargo.toml` 加 `cyber-mcp` + `cyber-skills`（透传）
- [x] `load_app_context` 后、`App::new` 前（`paths` move 前 borrow）：`let (registries, boot_errors) = build_registries(&ctx.paths, &paths.cwd, mock).await;`，boot_errors 经 `set_toast` 展示

### 3.4 P3 质量保证 ✅

- [x] `cargo build --workspace` 干净（1.78s 增量）
- [x] `cargo clippy --workspace --all-targets -- -D warnings` 干净（10.57s，无 warning）
- [x] `cargo test --workspace`：cyber-agent 86（79 lib + 7 集成）+ cyber-core 23 + cyber-mcp 24 + cyber-skills 23 + cyber-tui 207 = **workspace 363 全过**（较 P2 收尾 312 新增 51：mcp 24 + skills 23 + agent 集成 +1 max_steps + tui 调整）
- [x] 依赖方向不变量保持：cyber-agent 不反向依赖 mcp/skills（mcp/skills 依赖 agent 实现 `Tool` trait）
- [x] DESIGN.md §6/§7 同步实现状态（传输层 v0.1 标注 / actor 模式 / 统一工具表命名 / 降级保证 / Skill 加载覆盖 / 渐进式披露）
- [x] 配置不动 `Config`（MCP 由 `servers.toml`、Skill 由目录自描述）→ 不影响 merge/Settings 测试

### 3.5 已知限制 / 留待后续

- Skill 仅显式触发：不做 `triggers` 自动匹配（字段保留写入 schema description 供 LLM 参考）
- `/mcp` 仅 `list`/`status`，不做 `reconnect`（server 配置变更需重启）
- Skill `scripts/` 目录 v0.1 不自动执行脚本，仅供 body 文本引用
- HTTP event-stream 响应用 `resp.text()` + 调用级 timeout 读取（v0.1 不支持服务器长连持续推送）
- 跨 session 读取仅同 cwd 内；`/sessions read` 注入为 System 条目（展示用，不入 agent history）

### 3.6 P3.2 扩展：MCP SSE/HTTP + 多 Session ✅

> P3 收尾后补齐两处遗留：MCP SSE/Streamable HTTP 客户端 + 多 Session 管理。全部 build/clippy/test 全绿。

**MCP Streamable HTTP + legacy SSE 客户端**（[cyber-mcp](../crates/cyber-mcp/src/)）：

- [x] `sse.rs`（新）— `SseParser`（增量状态机：feed `&[u8]` → `Vec<SseEvent>`，按 `\n` 切行、`event:`/`data:` 分派、空行派发、`\r\n` + 多行 `data:` 拼接）+ `parse_sse_text`（一次性解析完整 body）+ `extract_jsonrpc_responses`（从 event: message/默认事件抽 JSON-RPC 响应）
- [x] `proto.rs` — `JsonRpcRequest.id: Option<u64>` + `#[serde(skip_serializing_if = "Option::is_none")]`；`new(id,...)` → `Some(id)`（序列化不变，现有 `call_roundtrip` 测试 `req.id == Some(0)` 仍过）；新增 `notification(method, params)` → `id: None`（真 notification，无 id 字段）
- [x] `connection.rs` — `McpConnection` 保持传输无关（只持 tx + next_id + tools）；新增 `spawn_http(spec)` → `start_http_actor`（每次 Call 一个 POST，维护 `Mcp-Session-Id`，按 Content-Type 分派 JSON / event-stream）+ `spawn_sse(spec)` → `start_sse_actor`（长连 GET event-stream reader task + POST endpoint）；`expand_env_headers`（`${VAR}` 展开）
- [x] `error.rs` — 加 `Network { server, detail }` / `BadResponse { server, detail }` 变体
- [x] `registry.rs::connect_one` — 按 transport 三分派（Stdio/Http/Sse → 对应 spawn 函数）
- [x] `Cargo.toml` 加 `reqwest = { workspace = true }`
- [x] 测试：sse 解析单测（增量分片/`\r\n`/多行 data/event 分派/extract_jsonrpc）+ `expand_env_headers` 单测 + HTTP/SSE e2e（本地 TcpListener 串行回 initialize 带 `Mcp-Session-Id` / tools/list / tools/call，断言握手 + call roundtrip + session id 回带）+ 缺 url 报 InitFailed

**多 Session 管理**（[cyber-tui](../crates/cyber-tui/src/)）：

- [x] `history.rs` 重写为多 session 结构：`~/.cyber/history/{cwd_hash}/{index.json, {id}.json}`；`SessionMeta { id, title, created_at, updated_at, message_count }` + `SessionIndex { current, sessions }`；session id 用 SystemTime 纳秒 base36；`load_index`（含旧单文件迁移为单 session + `.legacy.bak`）/ `save_index` / `load_entries` / `save_entries` / `create_session_meta` / `load_current` / `save_current`（含 title 派生）/ `list_sessions` / `read_session_text` / `delete_session`（删 current 自动切剩余首个）
- [x] `app.rs` — `App` 加 `sessions: SessionIndex` + `sessions_panel: SessionsPanelState` 字段；`Mode::Sessions` 变体；`switch_session` / `create_session` / `delete_session` / `handle_sessions_slash` / `handle_sessions_key`（Up/Down/Enter/n/d 双击确认/Esc/q）；`handle_event` / `render_main` / `render_status_bar` 分派 Sessions；`handle_slash_command` 加 `/new` + `/sessions` 分支；`make_app` 测试 helper 独占 history_dir + 填充 sessions
- [x] `slash.rs` — `COMMANDS` / `SlashCommand` / `parse` / `HELP_TEXT` 加 `/new` + `/sessions <list|read <id|关键词>|new>`
- [x] `views/sessions.rs`（新）— Sessions 面板渲染（title + message_count + id 截断 + 当前 ★ + 待删除 [待删除!] + 底部 hint）
- [x] 测试：history 21 例（迁移/隔离/save-load 往返/删空覆盖/损坏回退空/多 session 隔离/删除切 current/read 格式化）+ views/sessions 5 例 + app 9 例（/new 创建重置 / /sessions 进面板 / Enter 切换 / d 单 session 拒绝 / d 双 session 删除 / /sessions read 跨读注入 / Esc 不切换 / 流式期阻止 / 面板渲染）

### 3.7 P3.2 质量保证 ✅

- [x] `cargo build --workspace` 干净
- [x] `cargo clippy --workspace --all-targets -- -D warnings` 干净
- [x] `cargo test --workspace`：cyber-core 23 + cyber-mcp 48（+24：sse/http 解析 + e2e）+ cyber-skills 23 + cyber-tui 236（+29：history 重写 + sessions 面板 + app session）= **workspace 全过**
- [x] DESIGN.md §6.1 传输层表 SSE/HTTP→✅ + §6.4 actor 模式补 http/sse 三条；新增 §9.8 Session 小节
- [x] 既有 mcp 24 测试全保留（proto.id 改 Option 不破坏 `call_roundtrip` 的 `req.id == Some(0)`）

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

- **2026-08-04**：agent loop 改进——max_steps 默认值 25→50 + 死循环检测。①`max_steps` 默认值从 25 调至 50（`config.rs` + `settings.rs` 回退值同步），给复杂任务更多空间。②死循环检测（`agent.rs` 新增 `LoopDetector` + `fingerprint`）：每轮工具调用算指纹（所有 call 的 `name|arguments` 排序拼接，消除顺序差异），连续 3 轮相同 → 提前中止 agent loop（比空跑到 max_steps 省钱省时）。不同参数不算重复（`read_file(a)` vs `read_file(b)` 指纹不同），单轮多工具时整组指纹参与比较。触发后走与 max_steps 耗尽相同的无工具收尾流式，但提示文案改为「检测到连续多次相同的工具调用，可能已陷入循环」。测试：9 个 LoopDetector unit test（相同/不同/重置/threshold=1/多工具组合/额外工具重置）+ 1 个集成测试 `mock_tool_loop_not_loop_detected`（正常两步收敛不误触发）。`cargo test --workspace` 全过（cyber-agent 93 lib + 8 集成），clippy 干净。

- **2026-08-04**：shell 工具 Windows 修复——裸名命令被 `.py` 脚本遮蔽并在 IDE 弹窗的问题。根因：用户系统 `PATHEXT` 含 `.PY`，且 `.py` 文件关联被 PyCharm 抢占（`"...\pycharm64.exe" "%1"`），导致 `cmd /C readelf -l pwn` 优先解析到 PATH 靠前的 `readelf.py`（pyelftools）并走文件关联 → 在 IDE 打开脚本源码、无 stdout，同时遮蔽后面的原生 `readelf.exe`（mingw64）。修复：`ShellTool` 在 Windows 注入 `c.env("PATHEXT", sanitize_pathext(...))`，剥除 `.PY`/`.PYW`（空回退 `.COM;.EXE;.BAT;.CMD`），让 cmd 仅解析原生可执行格式 → 既杜绝 IDE 弹窗，又让被遮蔽的 `.exe` 生效（实测 `readelf -l pwn` 现返回 ELF 头）。新增 5 个 `#[cfg(windows)]` 测试（含 `where readelf` 回归断言解析到 `.exe` 而非 `.py`）。需跑 `.py` 脚本须显式 `python xxx.py`。

- **2026-08-04**：P3.2 扩展——MCP Streamable HTTP + legacy SSE 客户端 + 多 Session 管理（§3.6–3.7）。①MCP 远程传输（§6.1/6.4）：`cyber-mcp` 加 `sse.rs`（`SseParser` 增量状态机 + `parse_sse_text` + `extract_jsonrpc_responses`）+ `proto.rs` `JsonRpcRequest.id: Option<u64>`（`new`→`Some(id)` 序列化不变、新增 `notification`→`id: None` 真 notification）+ `connection.rs` `spawn_http`/`spawn_sse`（`McpConnection` 保持传输无关，HTTP 维护 `Mcp-Session-Id` 按 Content-Type 分派 JSON/event-stream，SSE 长连 GET reader task + POST endpoint）+ `expand_env_headers`（`${VAR}` 展开）+ `error.rs` `Network`/`BadResponse` 变体 + `registry.rs::connect_one` 三分派 + `reqwest` 依赖。②多 Session（§9.8）：`history.rs` 重写为 `~/.cyber/history/{cwd_hash}/{index.json,{id}.json}`（`SessionMeta`+`SessionIndex`，id 用纳秒 base36，旧单文件自动迁移 + `.legacy.bak`，title 派生，`delete_session` 删 current 切剩余首个）；`app.rs` 加 `sessions`/`sessions_panel` 字段 + `Mode::Sessions` + `switch/create/delete_session` + `handle_sessions_slash`/`handle_sessions_key` + 各路分派；`slash.rs` `/new` + `/sessions <list|read <id|关键词>|new>`；`views/sessions.rs` 面板渲染。跨 session 读取仅同 cwd，注入 System 条目（被 `history()` 剥离保持独立）。测试 cyber-mcp 48（+24 sse/http 解析 + e2e）+ tui 236（+29 history 重写 + sessions 面板 + app session）= **workspace 全过**，clippy 干净。DESIGN.md §6.1 传输层表 SSE/HTTP→✅ + §6.4 actor 三条 + §6.6 降级 + 新增 §9.8 Session 同步。

- **2026-08-04**：P3 MCP+Skill 完成（§3.1–3.5），**P3 完成度 → 100%**。范围确认：MCP 仅 stdio（SSE/HTTP 解析但跳过）、Skill 仅显式触发（不做 triggers 自动匹配）、新增 `/mcp` 命令。①cyber-skills（§3.1）：`frontmatter`（复用 `.cyber.md` BOM+`---`+serde_yaml 解析模式拷贝）+ `Skill::load`（用 `fsutil::read_utf8`，缺 name→Err）+ `SkillRegistry::load_all`（全局+项目级扫描，项目覆盖全局同名，单 Skill 失败入 errors 不阻断）+ `SkillTool`（`skill_<name>`，schema desc 含 description+triggers，`run` 返回 body = 渐进式披露）。②cyber-mcp（§3.2）：`proto`（JSON-RPC 2.0 + MCP 协议类型）+ `config`（`McpServersConfig`/`McpServerSpec`，`${ENV}` 展开，默认 5s 超时）+ `transport`（`Transport` trait + `StdioTransport` + `#[cfg(test)] PipeTransport`）+ `connection`（**actor 模式**：单 task 串行处理 stdin/stdout，`mpsc` 请求 + `oneshot` 回执，`AtomicU64` id 无竞争，30s call 超时，`select!` 行解析+id 路由+notification 忽略+半行缓冲；非泛型 `McpConnection` 可 `Arc` 共享）+ `McpTool`（`mcp_<server>_<tool>`，sanitize 非法字符）+ `McpRegistry::connect_all`（`join_all` 并行 spawn，失败 warn+skip，SSE/HTTP→`UnsupportedTransport`）+ `shutdown_all`。③Wiring（§3.3）：cyber-agent `run_stream`+`run_inner` 加 `registry: Arc<ToolRegistry>` 形参（删内部构造，9→10 参数保留 `#[allow(too_many_arguments)]`）；cyber-core `Paths::project_skills_dir`；cyber-tui `AppRegistries`（`Arc<ToolRegistry>` 跨 turn 共享，MCP 连接长存）+ `build_registries`（**永不返回 Err**，boot_errors 经 toast 降级启动）+ `/skill`(`/skill list` 列出/`/skill <name>` 注入 body)+`/mcp`(`list`/`status`)+ `App::run` 退出 `shutdown_all`；cyber-app main 调 `build_registries`。配置不动 `Config`（MCP 由 `servers.toml`、Skill 由目录自描述）→ 不影响 merge/Settings 测试。测试 cyber-agent 86（79 lib + 7 集成）+ core 23 + mcp 24 + skills 23 + tui 207 = **workspace 363 全过**（较 P2 收尾 312 新增 51），clippy 干净。DESIGN.md §6（传输层 v0.1 标注 + actor 模式 + 统一工具表命名 + 降级保证）+ §7（Skill 加载覆盖 + 渐进式披露 SkillTool）同步。

- **2026-08-04**：max_steps 优雅收尾 + 输入历史呼出。①max_steps 优雅收尾（§2.7）：原行为是 agent loop 达 `max_steps` 后直接发 `AgentEvent::Error` 裸中断，用户看到「最大步数 max_steps=25」错误且无任何结论，「继续」也因无上下文而立即结束。改为循环耗尽时追加「已达到工具调用步数上限 N，请根据已收集的信息直接给出最终回答或阶段性结论」user 提示 + 一次 `tools=[]` 收尾流式（模型无法再调工具只能输出文本）→ 经 `accumulate_stream` 流式回传 Token → 发 `Done`（非 `Error`）。总结文本成为 assistant 条目入 history，使「继续」有上下文承载。②输入历史呼出（§9.7）：新增 `InputHistory` 结构（`entries: Vec<String>` + `browse: Option<usize>` 浏览态），普通 ↑/↓ 在**空输入框**时 shell 风格呼出已发送消息；非空时返回 `false` 交 textarea 移光标（保留 Shift+Enter 换行后的多行编辑）。不单独持久化——由 chat history 的 `ChatEntry::User` 条目派生（App 启动加载历史后 `seed_input_history` 填充），新提交经 `record` 追加（trim 空/相邻去重），跨会话呼出靠下次启动重新 seed。新增 `ChatAction::HistoryPrev/HistoryNext` + 键位映射（普通 Up/Down，斜杠菜单打开时由菜单消费优先）。测试 cyber-agent 86（79 lib + 7 集成，集成 +1 `mock_max_steps_exhaustion_does_graceful_summary`）+ core 23 + tui 203（+10 含输入历史 record/seed/submit/event 往返）= **workspace 312 全过**，clippy 干净。DESIGN.md §3.2 特性 + §9.2 键位 + §9.7 输入历史呼出小节同步。

- **2026-08-04**：初始化 PROGRESS.md。P1 配置层（1.1 / 1.2 / 1.6）完成；启动状态机（1.3）部分完成；TUI 主循环（1.4）与 Welcome 页（1.5）未开始。
- **2026-08-04**：P1 收口。完成 1.3 启动状态机（clap + 路由）、1.4 ratatui 主循环（`ratatui::init/restore` + 同步事件循环 + 5 主题 + 鼠标开关 + 标题/状态栏）、1.5 Welcome 页（三选项导航 + Chat/Workflow/Dashboard 占位 + Tab/Esc/q 键位）。新增 `cyber-tui` 四模块（app/event/theme/views）+ 重写 `cyber-app` main。clippy 全 workspace 干净，单测全过，首帧渲染验证通过。**P1 完成度 → 100%**。
- **2026-08-04**：新增 `cyberpunk` 主题（深紫黑底 #0D0221 + 霓虹粉 accent #FF2A6D + 霓虹青 title/选中 #05D9E8），灵感来自 ratatui.rs "Built with Ratatui" 项目群的暗底霓虹 TUI 美学。`Theme::resolve` + 单测 + 配置注释 + DESIGN.md §2.4/§9.3 同步更新。
- **2026-08-04**：默认主题由 `tokyo-night` 改为 `cyberpunk`。改动 `UiConfig::default` + `default_config.toml` 模板 + `Theme::resolve` 未知回退（拆分 `tokyo-night` 显式臂与 `_` 回退臂，避免 tokyo-night 被错误回退）+ DESIGN.md §2.4 示例 + 单测。
- **2026-08-04**：实现 Settings 设置页（§1.7）。新增 `Mode::Settings` 模态层 + `views::settings`（fn 指针 `SECTIONS` + `FieldKind` 编辑模型 + `LiveApply` 即时应用）+ `save_config` 原子写持久化（`.tmp`/`.bak`）+ Esc 双击回退（`config_at_entry` 快照）+ 项目级覆盖警告横幅。扩展 `Action`（`OpenSettings`/`Left`/`Right`）与键位映射，Welcome 第 4 项 + 状态栏提示同步。单测 settings 10 + app 8，workspace 合计 38 全过，clippy 干净。DESIGN.md §9.2 键位表 + §9.4 设置页小节（含字段生效时机表）同步。
- **2026-08-04**：P2.2 Chat 核心能力补全（§2.6–2.10）。①工具调用协议：`Tool` trait + `ToolRegistry` + 四内置工具（read_file/write_file/list_dir/shell）+ 安全护栏（命令黑名单 + cwd 逃逸检测）+ 三家 parser 支持 tool-call delta。②agent loop：`accumulate_stream` 累积 ToolCallDelta 按 index 合并 → 执行工具 → 结果回灌 → 再流式，`max_steps` 限制；generation 计数器隔离 cancel 后 stale 事件（`run_stream` 入参 gen，事件携带 `(gen, AgentEvent)`）。③斜杠命令：`slash` 模块（7 命令 + 大小写不敏感解析），Submit 分支拦截 `/` 开头输入。④Mock 双模：echo（tools 空）/ tool-loop（tools 非空，两步驱动 agent loop 全链路）。⑤`ChatEntry` 枚举重设计（User/Assistant/ToolCall/ToolResult/System）+ views/chat 渲染工具调用 `▶`/结果 `→`/`✗`。测试 cyber-agent 71 + core 16 + tui 82 = **169 全过**，clippy 干净。**P2 完成度 → 90%**。
- **2026-08-04**：P2.2 收尾——对话历史持久化 + 流式重绘优化（§2.11–2.13），**P2 完成度 → 100%**。①历史持久化：`cyber-tui::history` 模块（FNV-1a `cwd_hash` + 原子写 `load`/`save`），存 `~/.cyber/history/{cwd_hash}.json` 按 cwd 隔离；`Paths.history_dir` + init 首启建目录；`ChatEntry` 改 **adjacently tagged** serde（internally tagged 无法序列化 newtype 变体）；App 启动加载 + Done/Error/cancel/clear/quit/退出 8 处 `save_history`；失败仅 warn 不阻断。②流式重绘优化：`ChatState.cached_history` 行缓存（`prepare_render` 在 `&mut self` draw 前 hook 按 entries.len/theme 变化重建，render `&self` 只读复用）+ 条目→行转换下沉 `chat::render_entries` + theme 切换 `invalidate_cache` + `Paragraph::line_count` 自动滚动到底部（ratatui `unstable-rendered-line-info` feature）+ 缓存未就绪回退现场构建。测试 cyber-agent 71 + core 16 + tui 96 = **183 全过**（+14），clippy 干净。
- **2026-08-04**：服务商 CRUD + 模型拉取（§2.14），参考 `example/wepclaude` customProviders 逻辑。①cyber-core：`PROVIDER_KINDS` + `ProviderConfig::normalize()` + `ProvidersConfig` CRUD（`sorted_names`/`upsert`/`remove`）+ `save_providers` 原子写。②cyber-agent：`fetch_models` 异步拉取（按 kind 试 `/models`/`/v1/models` + provider 专属 headers）+ `extract_model_ids` 端口 JS。③cyber-tui 视图：`views/providers.rs` 模态表单（`ProviderFormState` + `FormAction` + textarea 编辑 + kind ←→循环 + 模型 picker）+ `views/settings.rs` Providers 段交互化（`a`/`e`/`d`/`Enter` + `▸` cursor + 双击 `d` 删除确认）+ `event.rs` 三新 Action + `slash.rs` `/provider list|add|edit|use|remove`。④app 集成：`Mode::ProviderForm` + `AppPaths` 打包 + `FetchResult` 第 4 路 `select!` 分支（`fetch_id` 防 stale）+ 持久化双轨（Settings 延迟 / Chat 立即）+ `providers_at_entry` Esc 回滚 + `default_provider` 防悬空。⑤main.rs 通道 + `AppPaths` 构造。测试 cyber-agent 85 + core 23 + tui 127 = **235 全过**（+52），clippy 干净。DESIGN.md §3.2/§9.2/§9.4 同步 + 新增 §9.5 Provider Form 模态层。
- **2026-08-04**：Chat Markdown 渲染（§2.15）。新增 `cyber-tui::markdown` 模块（手写解析器，不引入外部 markdown crate，与项目自实现 FNV hash / SSE 行缓冲一致）：块级状态机（代码围栏进出）+ 行内 `parse_rich` 递归解析，覆盖标题/代码块/行内代码/粗体/斜体/删除线/链接/无序+有序列表/引用/分隔线；颜色由 `Theme` 派生（`code_fg=title`/`header=link=list=accent`/`quote=hr=muted`）不新增 Theme 字段；未闭合 `**`/`` ` ``/围栏降级为纯文本不 panic（流式未完整 Markdown 安全）。集成：`chat.rs` `push_assistant_lines`（Assistant 标签独占一行 + Markdown 内容铺开，替换原内联标签）+ `views/chat.rs` `push_streaming_tail` 改用 `markdown::render` + 末行 `▌` 光标。修复 clippy `manual_strip`→`strip_prefix`、`manual_range_contains`→`(1..=6).contains`。测试 cyber-agent 85 + core 23 + tui 150 = **258 全过**（+23 markdown 测试），clippy 干净。DESIGN.md §3.2 加特性 + 新增 §9.6 Markdown 渲染。
- **2026-08-04**：Chat 历史滚动 + 斜杠命令补全菜单（§9.7）。①历史滚动：`ChatState.scroll_y`（`SCROLL_FOLLOW`=usize::MAX 哨兵表示跟随底部）+ `Cell<usize>` 度量（render 每帧回写总行数/可见高度，按键据此算 max_scroll 与页大小）；键位 PageUp/PageDown 整页、Ctrl+↑/Ctrl+↓ 单行、鼠标滚轮 3 行/格（`config.ui.mouse`）；贴底 auto-follow 流式新内容，上滚后视图钉原内容；submit/clear/cancel 调 `scroll_to_bottom` 恢复跟随；标题栏显示位置/总量指示。②斜杠补全：`slash::CommandSpec` 目录（name/usage/desc 单一来源，`HELP_TEXT` 与菜单共用）+ `filter_commands` 大小写不敏感前缀过滤；`SlashMenu` 状态 + `update_slash_menu`（输入首行 `/` 且无空格时打开，实时刷新）；`slash_menu_key` 消费 ↑/↓/Enter/Tab/Esc（不传 textarea）—— ↑/↓ 循环选择、Enter/Tab 补全命令名+空格、Esc 关闭；`views/chat.rs` `render_slash_menu` 浮于输入框上方（Clear 清背景 + List 高亮选中 `▶`，最高 8 项）。③`event.rs` 新增 `ChatAction::ScrollPageUp/Down/ScrollLineUp/Down` + 键位映射 + 单测。修复 clippy `collapsible_match`（鼠标分支改 match guard）、`get_first`（`.get(0)`→`.first()`）、`needless_borrows_for_generic_args`（`&format!`→`format!`）+ `CommandSpec` 加 `PartialEq/Eq` derive（菜单过滤比较）。测试 cyber-agent 79 + core 23 + tui 181 = **workspace 289 全过**（tui +31：滚动/斜杠菜单单测），clippy 干净。DESIGN.md §3.2 特性 + §9.2 键位 + 新增 §9.7 同步。
- **2026-08-04**：Markdown 语法扩展 + 历史滚动跟手性优化。①Markdown 语法扩展（§9.6）：`try_match` 新增粗斜体 `***`（BOLD+ITALIC，先于 `**` 匹配避免吞 `*`，内部递归）、下划线 `<u>`/`<ins>`（UNDERLINED，HTML 语法糖）、行内数学 `$...$`（math 色+ITALIC，开头/结尾非空格且内容非空，避免 `$5` 货币误触发）、块级数学 `$$...$$`（独占行 `$$` 起止，每行缩进 2 + math 色 + ITALIC，内容原样不递归解析避免 `^`/`_` 误触发）；`MdColors` 加 `math=accent` 字段；块级数学状态机（`in_math` 与 `in_code` 平行）。②滚动跟手性优化（§9.7）：新增 `WrappedCache`（`RefCell`，render 以 `&self` 内部可变）按可视宽度预折行已完成条目+流式 tail，缓存键 `(entries.len, streaming_buffer.len, width)`，key 命中 O(1) 复用、变化时 O(N) 重建；`wrap_lines` 用 `unicode-width`（CJK 占 2 列、tab 占 1 列）贪婪填行 + 相邻同 style 字符合并 span；`views/chat.rs` `render_history` 重写为只 clone 可见窗口 `wrapped[offset..end]`（O(visible)）+ 无 `Wrap` 无 `scroll` 的 `Paragraph`，消除旧每帧 `line_count`+`Wrap` 重算（O(N)）与全量 clone——长历史滚动跟手性显著改善；theme 切换 `invalidate_cache` 置 `valid=false` 强制重建（颜色变折行数不变 key 不变须显式失效）；新增 workspace 依赖 `unicode-width = "0.2"`。测试 cyber-agent 79 + core 23 + tui 193 = **workspace 295 全过**（tui +12：粗斜体/下划线/数学公式/预折行单测），clippy 干净。DESIGN.md §3.2 特性 + §9.6 表格扩展（粗斜体/下划线/行内+块级数学 + 匹配优先级）+ §9.7 加滚动跟手性小节同步。
