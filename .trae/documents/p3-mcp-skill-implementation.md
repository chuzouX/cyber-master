# P3 MCP + Skill 实现计划

## Context

P2 Chat 已完成（流式对话 + 工具调用 + agent loop + 历史持久化），但 agent 只能调用 4 个内置工具（read_file/write_file/list_dir/shell）。P3 的目标是把 **MCP 外部工具** 和 **Skill** 接入统一工具表，让 agent 与 `/tools`、`/skill`、`/mcp` 命令能同等使用它们。

`cyber-mcp` 和 `cyber-skills` 两个 crate 目前仅有 stub `lib.rs`（无依赖、无实现）。`Paths` 已预留 `skills_dir` / `mcp_dir` / `mcp_servers_file`，`init.rs` 首启已建目录 + 写默认 `servers.toml`。`Tool` trait（`cyber-agent/src/tool.rs`）是统一抽象，MCP/Skill 通过实现它注入。

**确认的范围**（用户已选）：
- MCP 传输：**仅 stdio**（本地子进程 JSON-RPC）；SSE/HTTP 在 `servers.toml` 解析但启动 warn 跳过
- Skill 触发：**仅显式**（`skill_<name>` 工具 + `/skill` 命令）；不做 triggers 自动匹配
- 新增 `/mcp` 命令（`list`/`status`，不做 `reconnect`）

## 架构决策

### 依赖方向（不变量：cyber-agent 不反向依赖 mcp/skills）
```
cyber-app ──► cyber-tui ──► cyber-agent ──► cyber-core
                │   ├──► cyber-mcp ──► cyber-agent + cyber-core
                │   └──► cyber-skills ──► cyber-agent + cyber-core
                └──► cyber-app ──► cyber-mcp + cyber-skills（透传）
```
- `cyber-mcp` / `cyber-skills` 依赖 `cyber-agent`（实现 `Tool` trait）+ `cyber-core`（`Paths`/`read_utf8`）
- `cyber-tui` 依赖 `cyber-mcp` + `cyber-skills`，负责**组合**统一工具表

### 统一工具表共享：`Arc<ToolRegistry>`
MCP 子进程连接必须跨多轮 agent turn 长存（不能每轮重连）。方案：`App` 持有 `Arc<ToolRegistry>`（启动时构建一次），每次 `spawn_agent` 时 `Arc::clone` 传入 `run_stream`。`ToolRegistry` 内部 `Vec<Box<dyn Tool>>` 无需 Clone。

### `run_stream` 签名改造
新增 `registry: Arc<ToolRegistry>` 参数（9 → 10，保留 `#[allow(clippy::too_many_arguments)]`），删除 `run_inner` 内 `let registry = ToolRegistry::with_builtins();`（agent.rs:89）。其余 `registry.execute/schemas/is_empty` 调用不变（`Arc` 自动 deref）。

### MCP 连接：actor 模式
`Tool::run` 是 `&self`，MCP 需内部可变性。用 actor：每个 `McpConnection` 起一个后台 task 持有 `Child`+stdin/stdout，主线程经 `mpsc` 发请求、`oneshot` 收回执。单 task 串行处理 → JSON-RPC id 单调无竞争，无需 Mutex。超时用 `tokio::time::timeout` 包裹 oneshot。

### Skill 暴露：渐进式披露
每个 `Skill` 包成 `SkillTool`：`schema.description` = skill 描述 + 触发词，`run` 返回 skill body（LLM 看到 description，调工具取详细说明）。命名 `skill_<name>`。

### 配置：不加 `[mcp]`/`[skills]` 段
MCP 配置全由 `~/.cyber/mcp/servers.toml` 承担，Skill 全由目录扫描自描述。`Config` 不动 → 不影响 merge/Settings 测试。

## 实施阶段（P3.1 Skills → P3.2 MCP → P3.3 Wiring）

### P3.1 — cyber-skills（先做，纯本地 IO，可独立验证）

**`crates/cyber-skills/Cargo.toml`**：加 `cyber-core`、`cyber-agent`、`serde`、`serde_json`、`serde_yaml`、`tracing`、`thiserror`；dev `tokio`。

**模块**：
- `frontmatter.rs` — 本地拷贝 `cyber-core/src/project.rs:63` 的 `parse` 模式（BOM strip + `---` 分隔 + serde_yaml）。`SkillFrontmatter { name, description, triggers: Vec<String>, tools: Vec<String> }`。不泛型化 core 的 parse（避免影响 38 个 core 测试）。
- `skill.rs` — `Skill { frontmatter, body, path, source: SkillSource(Global|Project) }`；`Skill::load(path, source)` 用 `cyber_core::fsutil::read_utf8` + `frontmatter::parse`，缺 name → Err。
- `registry.rs` — `SkillRegistry { skills: Vec<Arc<Skill>> }`；`load_all(global_dir, project_dir) -> (Self, Vec<(PathBuf, SkillError)>)`：扫描两目录下每个子目录的 `SKILL.md`，项目级覆盖全局同名（按 `frontmatter.name` 去重，Project 优先）。`iter()` / `find(name)`。
- `tool.rs` — `SkillTool { skill: Arc<Skill> }` impl `Tool`：`schema` name=`skill_<name>` desc 含 description+triggers；`run` 返回 `body`。
- `error.rs` — `SkillError`（thiserror）。
- `lib.rs` — re-export 全部。

**测试**：frontmatter parse（复用 project.rs 用例：BOM/无 frontmatter/特殊字符）、Skill::load tempdir、load_all 全局+项目同名覆盖、缺 name 入 errors、空目录、SkillTool::run 返回 body。

### P3.2 — cyber-mcp stdio client（复杂度最高）

**`crates/cyber-mcp/Cargo.toml`**：加 `cyber-core`、`cyber-agent`、`tokio`、`futures`、`serde`、`serde_json`、`toml`、`thiserror`、`tracing`；dev `tokio`（full+test-util）。

**模块**：
- `proto.rs` — JSON-RPC 2.0 类型：`JsonRpcRequest<T>` / `JsonRpcResponse<T>` / `JsonRpcError`；MCP 协议类型 `InitializeParams/Result`、`ToolListResult`、`McpToolSchema{name,description,input_schema}`、`CallToolParams{name,arguments}`、`CallToolResult{content: Vec<McpContent>, is_error}`。
- `config.rs` — `McpServersConfig { servers: Vec<McpServerSpec> }`、`McpServerSpec { name, transport: McpTransport(Stdio|Sse|Http), command, args, env, url, headers, timeout_secs }`；`McpServersConfig::load(path)` 用 `read_utf8` + `toml::from_str`。
- `transport.rs` — `Transport` trait（`async write/read/close`，用 `AsyncRead+AsyncWrite` 抽象，便于测试）；`StdioTransport`（`tokio::process::Command` spawn）；`#[cfg(test)] PipeTransport`（`tokio::io::duplex`）。
- `connection.rs` — `McpConnection { server_name, tx: UnboundedSender<McpRequest>, handle }`；`McpRequest::Call{id,method,params,reply: oneshot} | Shutdown`；`spawn_stdio(spec)` / `spawn_with_transport(transport, spec)`（测试用）；`call(method, params)` 带 30s timeout；`shutdown()`。`actor_loop`：`tokio::select!` 处理 req_rx（写 stdin）+ stdout（按 `\n` 切行解析 response，按 id 路由 pending oneshot；无 id 的 notification log 后忽略；维护 `buf` 处理半行）。
- `client.rs` — 协议层：`initialize` 握手 + `tools/list` 缓存 + `tools/call`。
- `tool.rs` — `McpTool { server: Arc<McpConnection>, name, schema_cache }` impl `Tool`：name=`mcp_<server>_<tool>`（非法字符替换 `_`）；`run` 发 `tools/call` → 拼 `content[]` text 为单字符串 → `ToolOutput`。
- `registry.rs` — `McpRegistry { connections }`；`connect_all(spec) -> (Self, Vec<McpTool>, Vec<(String, McpError)>)`：并行 spawn 每个 server（per-server `timeout_secs` 默认 5s），失败 warn+skip，SSE/HTTP → `UnsupportedTransport` 入 errors。`shutdown_all()`。
- `error.rs` — `McpError`（Io/Json/UnsupportedTransport/InitFailed/Timeout/Rpc/ToolNotFound/ChannelClosed/Core/Agent）。
- `lib.rs` — re-export `McpRegistry`/`McpTool`/`McpServersConfig`/`McpServerSpec`/`McpTransport`/`McpError`。

**测试**：用 `PipeTransport` + 测试 task 模拟 server（回 initialize/tools-list/tools-call），断言全链路；超时 → `McpError::Timeout`；shutdown 后 child killed；channel closed。集成测试（真 stdio echo server）标 `#[ignore]`。

### P3.3 — Wiring（cyber-agent / cyber-core / cyber-tui / cyber-app）

**cyber-agent**（[agent.rs](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-agent/src/agent.rs)）：
- `run_stream` + `run_inner` 加 `registry: Arc<ToolRegistry>` 形参，删 agent.rs:89 内部构造。
- `tests/mock_roundtrip.rs`：`spawn_run` helper 加 `registry` 参数；7 处调用点传 `Arc::new(ToolRegistry::with_builtins())`（机械化，断言不变）。

**cyber-core**（[paths.rs](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-core/src/paths.rs)）：
- 新增 `Paths::project_skills_dir(cwd) -> PathBuf`（`project_local_dir(cwd).join("skills")`）。
- 确认 `fsutil` 已 `pub mod`（lib.rs:7），`read_utf8` 可被 mcp/skills 调用。

**cyber-tui**：
- `Cargo.toml` 加 `cyber-mcp` + `cyber-skills`。
- 新增 `AppRegistries { tools: Arc<ToolRegistry>, skills: Arc<SkillRegistry>, mcp: Option<Arc<McpRegistry>> }`（`mcp` 供 `/mcp` 与关闭用）。
- `App::new` 加 `registries: AppRegistries` 参数；`App` 持 `self.registries`。
- `spawn_agent`（app.rs:521）：`let registry = self.registries.tools.clone();` 传入 `run_stream`。
- `/tools` handler（app.rs:633）：改用 `self.registries.tools.schemas()`，删 `ToolRegistry::with_builtins()`。
- 新增 `handle_skill_slash(args)`：`/skill list` 列出 skills；`/skill <name>` 注入 body 为 System 条目。
- 新增 `handle_mcp_slash(args)`：`/mcp list|status` 列出 server 连接状态。
- 新增 `bootstrap.rs`：`pub async fn build_registries(paths, cwd, mock) -> (AppRegistries, Vec<String>)` — builtins + skills（同步扫描）+ MCP（非 mock 时 `connect_all`，mock 跳过）；返回 errors 供 toast。永不返回 Err（保证 TUI 启动）。
- `slash.rs`：加 `/skill` + `/mcp` 到 `COMMANDS`/`SlashCommand`/`parse`/`HELP_TEXT` + 测试。
- `make_app` 测试 helper 加 `AppRegistries`（用 `with_builtins` + `SkillRegistry::default()` + `mcp=None`）；所有 app 测试调用点同步更新。
- `App::run` 退出前 `mcp.shutdown_all()`（若 Some）。

**cyber-app**（[main.rs](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-app/src/main.rs)）：
- `Cargo.toml` 加 `cyber-mcp` + `cyber-skills`（透传）。
- `load_app_context` 后、`App::new` 前：`let (registries, boot_errors) = cyber_tui::build_registries(&ctx.paths, &cwd, mock).await;`（注意在 `paths` move 前 borrow）。boot_errors 经 toast 展示。

## 关键文件

- [crates/cyber-agent/src/agent.rs](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-agent/src/agent.rs) — `run_stream` 签名 + 删内部 registry
- [crates/cyber-agent/tests/mock_roundtrip.rs](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-agent/tests/mock_roundtrip.rs) — 7 处调用点更新
- [crates/cyber-tui/src/app.rs](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-tui/src/app.rs) — App 字段 + spawn_agent + /tools + /skill + /mcp + make_app
- [crates/cyber-tui/src/slash.rs](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-tui/src/slash.rs) — /skill + /mcp 命令
- [crates/cyber-tui/src/bootstrap.rs](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-tui/src/bootstrap.rs) — 新增 build_registries
- [crates/cyber-mcp/src/](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-mcp/src/) — 8 子模块（stub → 完整实现）
- [crates/cyber-skills/src/](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-skills/src/) — 5 子模块（stub → 完整实现）
- [crates/cyber-core/src/paths.rs](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-core/src/paths.rs) — project_skills_dir
- [crates/cyber-app/src/main.rs](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-app/src/main.rs) — build_registries 调用

**复用现有工具**：`cyber_core::fsutil::read_utf8`、`cyber_core::Paths`、`cyber-agent::Tool/ToolSchema/ToolOutput/ToolCtx`、`project.rs:63` parse 模式（拷贝）、`init.rs` 已建目录、`default_mcp_servers.toml` 已存在。

## 验证

每阶段 `cargo build -p <crate>` + `cargo clippy -p <crate> -- -D warnings` + `cargo test -p <crate>`。

全量收口：
- `cargo build --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace`（312 → ~325 全过）
- `cyber --mock` 启动不 panic（mock 跳过 MCP）
- `/tools` 输出含 builtins + skills
- `/skill list` 列出 `~/.cyber/skills/` 下 skill；`/skill <name>` 注入 body
- `/mcp list` 显示 server 连接状态
- `~/.cyber/mcp/servers.toml` 配 stdio server（如 `npx -y @modelcontextprotocol/server-filesystem .`）→ 启动后 `/tools` 含 `mcp_filesystem_*`；agent 调用该工具 → ToolCall/ToolResult 事件正常展示
- 故意配错 server 命令 → 启动 warn + toast，TUI 仍可用（降级为仅 builtins+skills）
- 文档：DESIGN §6/§7 + PROGRESS P3 同步
