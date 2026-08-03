# P2 Chat 实现计划 — Provider trait + 流式对话（核心）

## Context

P1 骨架已完成（配置层 + 启动状态机 + ratatui 同步主循环 + Welcome/Settings 页）。当前 `Mode::Chat` 仅是占位页，`cyber-agent` 是空骨架（Cargo.toml 无任何依赖）。用户已确认本次 P2 pass 范围为**核心流式对话**：Provider trait + OpenAI/Anthropic/Ollama 三家流式实现 + tokio 事件总线接入 TUI + ChatView（消息流 + tui-textarea 输入 + token 流式渲染）+ 上下文注入（系统提示词 + .cyber.md rules + 会话历史）。工具调用 / agent loop / 斜杠命令 / @文件 / 历史持久化 / 文件 diff **留 P2.2**。

异步方案选定 **tokio 事件总线**（契合 DESIGN §10.2，P4/P5 工作流事件可复用），而非 std 线程 + mscp 桥接。`event.rs` 注释已预告"P2+ 接入 agent 流式后再升级为 tokio 事件总线"。

## 已核实的两个关键事实

1. **`tui-textarea` 版本陷阱**：原 `tui-textarea` 0.7 pin `ratatui ^0.29.0`，与本项目 ratatui 0.30 **不兼容**。必须用维护 fork **`tui-textarea-2` 0.12**（crates.io 已核实：依赖 `ratatui-core ^0.1.0`，与 ratatui 0.30 共享同一 core；dev-dep `ratatui ^0.30.0`；支持 crossterm `^0.28`；rust-version 1.85）。声明方式：`tui-textarea = { package = "tui-textarea-2", version = "0.12" }`。
2. **`ProjectFrontmatter`**（`cyber-core/src/project.rs`）有 5 字段：`project/scope/authorization/owner`（`Option<String>`）+ `rules: Vec<String>`；`ProjectContext::rules() -> &[String]`。prompt 组装据此设计。

## 设计要点（9 项决策）

| 项 | 决策 |
|---|---|
| Provider trait | 对象安全，`fn stream(messages, system) -> Pin<Box<dyn Stream<Item=StreamEvent> + Send + 'static>>`，**不用 async-trait** |
| `resolve_api_key` | 放 `cyber-core::providers`（纯字符串→env，无 HTTP），同时给 `ProviderConfig::resolved_api_key()` |
| Agent 任务 | `async fn run_stream(config, providers, project, user_input, history, tx, mock)`：建 prompt → factory → 转发 StreamEvent→AgentEvent |
| TUI 异步 | `App::run` 改 async；`main_loop` 用 `tokio::select!` 在 crossterm `EventStream` / `agent_rx.recv()` / `tick` 三路分发 |
| 输入框 | `tui-textarea-2`；**Enter=发送**，Shift/Alt+Enter=换行（透传）；流式期禁用输入 |
| 上下文注入 | base 提示词 + frontmatter(project/scope/authorization/owner) + rules 护栏段；body 暂不注入 |
| 离线可测 | `MockProvider` 逐 token 回放 "收到：{输入}"；`--mock` CLI flag + `CYBER_MOCK_PROVIDER=1` env |
| 错误 | **cyber-agent 新增 `AgentError`**（Http/Stream/Provider/Io/Json/Core），不动 CoreError（38 既有测试全保）；任务内 `?` → `AgentEvent::Error`，TUI 永不因 agent panic 崩 |
| 测试 | SSE/NDJSON 解析喂分片字节、factory 分发、resolve_api_key、prompt 组装、mock 往返；TUI 用 TestBackend |

## 实现步骤（按编译顺序）

### Step 0 — 依赖添加（先做，保证每步可编译）

**`Cargo.toml`（workspace 根 `[workspace.dependencies]`）** 增补：
```toml
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json", "stream"] }
futures = "0.3"
tui-textarea = { package = "tui-textarea-2", version = "0.12" }
```
并把既有 crossterm 行改为 `crossterm = { version = "0.28", features = ["event-stream"] }`。

**`crates/cyber-agent/Cargo.toml`**：deps = `cyber-core, reqwest, tokio, futures, serde, serde_json, thiserror, tracing`；dev-deps `tokio` 加 `test-util`。
**`crates/cyber-tui/Cargo.toml`**：新增 `cyber-agent`、`tui-textarea`、`futures`、`tokio`。
**`crates/cyber-app/Cargo.toml`**：新增 `tokio`（`#[tokio::main]`）。

> 完成后 `cargo check --workspace` 应通过。

### Step 1 — cyber-core：`resolve_api_key`（增量，不破坏 API）

`crates/cyber-core/src/providers.rs` 追加：
```rust
pub fn resolve_api_key(s: &str) -> String {
    let s = s.trim();
    if let Some(var) = s.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        std::env::var(var).unwrap_or_default()
    } else { s.to_string() }
}
impl ProviderConfig { pub fn resolved_api_key(&self) -> String { resolve_api_key(&self.api_key) } }
```
`lib.rs` 导出加 `resolve_api_key`。单测：明文透传 / `${VAR}` 展开 / 未设置返回空 / 带空白 / 无后缀视为明文。**不动 CoreError。**

### Step 2 — cyber-agent：类型 + trait + 错误 + 解析器 + mock

新建模块树（`crates/cyber-agent/src/`）：`error.rs` `types.rs` `prompt.rs` `sse.rs` `provider.rs` `mock.rs` `openai.rs` `anthropic.rs` `ollama.rs`，`lib.rs` 声明并 re-export。

- **`error.rs`**：`AgentError { Http(reqwest::Error), Stream(String), Provider(String), Io(io::Error), Json(serde_json::Error), Core(CoreError) }`，`type Result<T> = std::result::Result<T, AgentError>`。
- **`types.rs`**：`Role { System, User, Assistant }`（serde lowercase）、`Message { role, content }`、`StreamEvent { Delta(String), Done, Error(String) }`、`AgentEvent { Started, Token(String), Done, Error(String) }` + `From<StreamEvent>`。
- **`provider.rs`**：`trait Provider: Send { fn stream(&self, messages: Vec<Message>, system: Option<String>) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>>; }` + `provider_factory(cfg, mock) -> Result<Box<dyn Provider>>`（按 `kind` 分发；`mock`/`kind=="mock"` → MockProvider；未知 → Err）。
- **`sse.rs`**：`LineBuf` 行缓冲状态机（喂分片字节按 `\n` 切完整行，UTF-8 不完整字节不产出）+ 三家各一个 `fn parse_line(line) -> Option<StreamEvent>`。
- 三家实现：`new(cfg)` 建 reqwest::Client(rustls) + 缓存 `resolved_api_key`；`stream()` 用 `resp.bytes_stream()` + `LineBuf` + `futures::stream::unfold` 驱动 yield `StreamEvent`。
  - OpenAI: POST `{base}/chat/completions`，`Authorization: Bearer`，body `stream:true`；解析 `choices[0].delta.content`，`data: [DONE]`→Done
  - Anthropic: POST `{base}/v1/messages`，`x-api-key` + `anthropic-version: 2023-06-01`，`system` 为顶层字段；解析 `content_block_delta`→`delta.text`，`message_stop`→Done
  - Ollama: POST `{base}/api/chat`，无 auth，NDJSON；解析 `message.content`，`done==true`→Done
- **`mock.rs`**：`MockProvider::stream()` 逐字符 yield `Delta`（20ms 间隔）再一次 `Done`，文本 = `format!("收到：{last_user_msg}")`。
- **`prompt.rs`**：`build_system_prompt(project: Option<&ProjectContext>) -> String`（BASE_PROMPT + frontmatter 各字段 + rules 护栏段）。
- **`agent.rs`**：`pub async fn run_stream(config, providers, project, user_input, history, tx: UnboundedSender<AgentEvent>, mock)` → 先发 `Started`，内部 `run_inner` 用 `?`，失败转 `AgentEvent::Error`；`tx.send` 失败（TUI 退出）静默返回。

单测：`sse::parse_line` 三家路径、`LineBuf` 分片重组、`provider_factory` 分发、`build_system_prompt`。集成测试 `tests/mock_roundtrip.rs`：`tokio::spawn(run_stream mock)` → collect → 拼回 "收到：xxx" + Done。

### Step 3 — cyber-tui：ChatState + ChatView

- 新建 `crates/cyber-tui/src/chat.rs`：`ChatState { messages: Vec<ChatMessage>, input: TextArea<'static>, streaming: bool, streaming_buffer: String }` + `new()` + `submit() -> Option<String>`（流式期返回 None；非空则 push user 消息、清空 input、置 streaming=true）。`lib.rs` 加 `pub mod chat; pub use chat::ChatState;`。
- 重写 `crates/cyber-tui/src/views/chat.rs` 的 `render`：签名增 `state: &ChatState`；布局 `vertical([Min(0), Length(3), Length(1)])` = 历史 / 输入框 / hint；历史区遍历 `messages`（`[user]`/`[assistant]` 行），流式时把 `streaming_buffer` 作为进行中 assistant 消息追加（带 `▌` 光标）；输入区 `frame.render_widget(&state.input, area)`，流式期置灰 + 标题 "生成中…"。`render_placeholder` 保留（Workflow/Dashboard 仍用）。
- `app.rs` 的 `render_main` Chat 分支改为传 `&self.chat`。

### Step 4 — cyber-tui：event 层增加 Chat 键路径

`crates/cyber-tui/src/event.rs`：**保留** `Action`/`next_action`/`key_to_action`（Settings/Welcome 单测仍用）。把 `key_to_action` 拆出公开 `pub fn next_action_from_key(k) -> Option<Action>`（供异步路径非 Chat 模式复用）。新增：
```rust
pub enum ChatAction { Submit, Newline, Back, SwitchMode, OpenSettings, Quit, Input }
pub fn chat_key_to_action(k: KeyEvent) -> ChatAction
```
Chat 模式下字母（含 `s`）→ `Input`（交 textarea）；**无修饰 Enter → Submit**（须在 `textarea.input` 前拦截，tui-textarea 默认 Enter=换行）；Shift/Alt+Enter → `Newline`；`Ctrl+,` → `OpenSettings`（编辑器惯例，避免与 `s` 打字冲突，仅 Chat 模式）；`q`/Ctrl+C → `Quit`（保留 P1 出口）；Esc → `Back`；Tab → `SwitchMode`。仅 `KeyEventKind::Press` 处理。

### Step 5 — cyber-tui：App 异步化（核心，最易踩借用坑）

- `App` 增字段：`chat: ChatState`、`agent_tx: UnboundedSender<AgentEvent>`、`mock: bool`。**不存 `agent_rx`**（rx 留 `main_loop` 局部 `&mut`，避免 `select!` 内 `&mut self` 与 `rx.recv()` 借用冲突——本计划能编译的关键）。
- `App::new` 增参 `mock` + `agent_tx`（main 建通道，rx 传给 `run`）。
- `App::run(mut self, mut agent_rx) -> async`：`ratatui::init` → 条件 `EnableMouseCapture` → `main_loop(&mut terminal, &mut agent_rx).await` → 无条件 `DisableMouseCapture` + `ratatui::restore()`（panic hook 仍生效）。
- `main_loop` 骨架：
```rust
let mut events = crossterm::event::EventStream::new().fuse();
let mut tick = tokio::time::interval(Duration::from_millis(33));
loop {
    terminal.draw(|f| self.render(f))?;
    tokio::select! {
        biased;
        maybe_ev = events.next() => { if let Some(Ok(ev)) = maybe_ev { self.handle_event(ev); } }
        maybe_ae = agent_rx.recv() => { if let Some(ae) = maybe_ae { self.handle_agent_event(ae); } }
        _ = tick.tick() => {}
    }
    if self.should_quit { break; }
}
```
- `handle_event(ev)`：仅 Press；Ctrl+C 全局退出；`Mode::Chat → handle_chat_key(k)`（走 ChatAction），其余模式 → `next_action_from_key` 复用 P1 `handle_action`。
- `handle_agent_event`：`Started→streaming=true`、`Token→streaming_buffer.push_str`、`Done→finalize_stream`、`Error→toast+finalize`。`finalize_stream` 把 buffer 转为 assistant 消息、`streaming=false`。
- `spawn_agent(text)`：history 由 `chat.messages` 转 `agent::Message`；`agent_tx.clone()` + config/providers/project clone；`tokio::spawn(run_stream(...))`。**所有 agent 错误走 `?`→AgentEvent::Error**，任务内不 unwrap/expect。
- 既有 `handle_action`/`Action` 路径全保留（Settings/Welcome 单测不受影响）。

### Step 6 — cyber-app：main 异步化 + `--mock`

`crates/cyber-app/src/main.rs`：`Cli` 增 `#[arg(long)] mock: bool`；`#[tokio::main] async fn main`；`mock = cli.mock || env CYBER_MOCK_PROVIDER==1`；`let (tx, rx) = unbounded_channel::<AgentEvent>();`；`App::new(..., mock, tx).run(rx).await`。

## 最棘手处 / 风险

1. **`select!` 借用坑**：rx 必须是 `main_loop` 局部 `&mut`，tx 是 `self` 字段。`agent_rx.recv()` 不碰 `self`，`handle_*` 只在分支体 `&mut self`，互不冲突。
2. **EventStream 不可与同步 poll/read 混用**：完全替换 P1 的 `next_action` 轮询为 `EventStream.next()`，非 Chat 模式只是把 `key_to_action` 包进异步分支。`events` 必须 `.fuse()`。Windows console handle 可用。
3. **tui-textarea 版本**：必须 `tui-textarea-2` 0.12（已核实 crates.io）。误用原版会出现两份 ratatui（0.29+0.30），`Frame` 类型不匹配编译失败。
4. **Enter 拦截**：tui-textarea 默认 Enter→换行，须在 `textarea.input(k)` 前按 `ChatAction::Submit` 拦截无修饰 Enter；`Shift/Alt+Enter` 才透传。部分终端不报 Shift+Enter，hint 提示 `Ctrl+J` 兜底换行。
5. **panic 安全**：`ratatui::init` panic hook 在 async 下仍生效；`tokio::spawn` 的 agent 任务 panic 会被 JoinError 吞掉——所有错误走 `?`→`AgentEvent::Error`，不在任务内 unwrap。
6. **依赖方向**：`cyber-tui` 新增对 `cyber-agent` 的依赖（App 在 tui 内、需调 `run_stream` + 共享 `AgentEvent`/`Message`）。这是 App 物理位置决定的必要边，与 DESIGN "app → tui/agent → core" 高层意图不冲突。
7. **`q` 在 Chat**：保留 P1 退出语义（有项目也退出）；若希望 Chat 内 `q` 打字，改 `Ctrl+Q` 退出——列为可选调整。
8. **流式重绘性能**：每 token 重绘整条历史，P2 可接受；万 token 级响应可能卡顿，留 P2.2 用 `Paragraph::scroll` + 仅追加末行优化。

## 验证

1. **编译 + lint**：`cargo build --workspace` / `cargo clippy --workspace --all-targets -D warnings` / `cargo test --workspace`（期望：core 旧 10 + resolve_api_key；agent 新 ~10；tui 旧 28 + 新 ~3）。
2. **离线流式冒烟**（无需 key/联网）：`cargo run -p cyber-app -- --mock` → 输入 "你好" Enter → 逐 token 出现 "收到：你好"（带 `▌`）→ 完成变 `[assistant]`。验证 Tab 切模式、`Ctrl+,` 进 Settings、Esc（无项目回 Welcome / 有项目停留）、q 退出、终端恢复正常。
3. **真实 OpenAI**（需联网 + key）：`$env:OPENAI_API_KEY="sk-..."` 后 `cargo run -p cyber-app`（沙箱可能阻断网络）。
4. **真实 Ollama**（本地，无需 key）：`ollama serve` + `ollama pull qwen2.5:32b`，Settings 切 default_provider=ollama（P1 已支持 ProviderEnum 循环），`cargo run -p cyber-app` 验证 NDJSON 流式。
5. **Windows 异步事件流**：Windows Terminal 验证 `EventStream` 不丢键、Settings 开 mouse 下滚屏正常、Ctrl+C 退出终端恢复。

## 关键文件

- `crates/cyber-agent/src/provider.rs` — Provider trait + factory（P2 契约核心）
- `crates/cyber-agent/src/agent.rs` — `run_stream` 任务（串联 prompt/factory/stream/事件转发）
- `crates/cyber-agent/src/sse.rs` — SSE/NDJSON 行缓冲解析（三家共用）
- `crates/cyber-tui/src/app.rs` — `run` 异步化 + `select!` 主循环 + `handle_agent_event`/`spawn_agent`
- `crates/cyber-tui/src/views/chat.rs` — ChatView 重写（历史 + 流式 buffer + textarea）
- `Cargo.toml` — workspace 依赖（reqwest/futures/tui-textarea-2/crossterm event-stream，决定能否编译）
