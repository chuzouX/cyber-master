# Subagent 功能设计计划

## 摘要

为 cyber 添加 subagent 机制：主 agent 可通过 `subagent` 工具将子任务委派给独立的子 agent 并行/串行执行，子 agent 拥有独立的 agent loop、系统提示词和工具集，结果回灌给主 agent 继续对话。TUI 以可折叠的 [subagent] 条目展示子 agent 的执行过程。

## 当前架构分析

### Agent Loop（agent.rs）
- `run_stream` → `run_inner`：单线程、顺序执行
- 主循环：`for step in 0..max_steps` → 流式 LLM → 累积 tool_calls → 逐个串行执行工具 → 结果回灌 → 下一轮
- 工具执行：`registry.execute_streaming(name, input, &ctx, progress_tx)` — 逐个 await，不支持并行
- 死循环检测：`LoopDetector`（连续 3 轮相同指纹触发中止）
- 上下文压缩：`auto_compact_threshold` + `do_compact`

### 工具系统（tool.rs + tools/mod.rs）
- `Tool` trait：`schema()` + `run(input, ctx)` + `run_streaming(input, ctx, progress)`
- `ToolRegistry`：持有 `Vec<Box<dyn Tool>>`，按名查找、批量导出 schema
- 内置工具：read_file, write_file, list_dir, find_file, shell, web_fetch, download_file, ctf_challenge, save_memory
- MCP 工具和 Skill 工具也通过 `ToolRegistry` 注入

### 会话与消息（types.rs）
- `Message { role, content, tool_calls, tool_call_id }` — 标准的 OpenAI 格式
- `AgentEvent` 枚举：Started, Token, Reasoning, ToolCall, ToolProgress, ToolResult, Usage, Done, Error, Compacting, Compacted, ContextUpdate

### TUI（chat.rs + app.rs）
- `ChatEntry` 枚举：User, Assistant, Thinking, ToolCall, ToolResult, System
- 斜杠命令：`/model`, `/compact`, `/ctf`, `/provider`, `/sessions` 等，通过 `handle_slash_command` 分发
- 模式：`Mode::Chat`, `Mode::ModelPicker`, `Mode::ProviderForm`, `Mode::Settings` 等

### 系统提示词（prompt.rs）
- `build_system_prompt()`：thinking_section + BASE_PROMPT_STATIC + 环境 + 记忆 + skill 索引 + 项目上下文 + rules
- subagent 可使用精简版提示词（仅任务指令 + 基础工具使用规则）

## 方案设计

### 1. 新增 SubagentConfig（cyber-core）

在 `cyber-core/src/config.rs` 的 `AgentConfig` 中新增 subagent 配置：

```rust
pub struct SubagentConfig {
    /// 是否启用 subagent 功能
    pub enabled: bool,
    /// 每个 subagent 的最大步数（默认 100）
    pub max_steps: u32,
    /// 允许并行执行的 subagent 数上限（默认 4）
    pub max_parallel: u32,
    /// 子 agent 的系统提示词（可选，默认为精简版）
    pub system_prompt: Option<String>,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_steps: 100,
            max_parallel: 4,
            system_prompt: None,
        }
    }
}
```

### 2. 新增 Subagent 工具（cyber-agent）

`crates/cyber-agent/src/tools/subagent.rs`

**工具定义：**
- `subagent`：主 agent 调用此工具来委派子任务
- 参数：
  - `description`（必填）：子任务简短描述（3-5 词）
  - `task`（必填）：子任务的详细指令
  - `subagent_type`（可选）：子 agent 类型，默认 `"general"`。可选值 `"search"`（仅搜索工具）、`"general"`（全部工具）
  - `max_steps`（可选）：覆盖默认的最大步数

**工具 Schema：**
```json
{
  "name": "subagent",
  "description": "启动一个子 agent 执行独立任务。子 agent 有独立的工具集和上下文，执行完成后返回结果摘要。",
  "parameters": {
    "type": "object",
    "properties": {
      "description": {"type": "string", "description": "子任务简短描述（3-5 词）"},
      "task": {"type": "string", "description": "子任务的详细指令"},
      "subagent_type": {"type": "string", "enum": ["general", "search"], "default": "general"},
      "max_steps": {"type": "integer", "default": 100}
    },
    "required": ["description", "task"]
  }
}
```

**执行逻辑：**
1. 解析参数，构造子 agent 的 system prompt（精简版）
2. 根据 `subagent_type` 筛选工具集（`search` 仅暴露 read_file/list_dir/find_file/grep）
3. 创建独立的 `messages` 上下文（仅含 task 作为 user message）
4. 运行子 agent loop（与主 agent 相同的 `run_inner` 逻辑，但 `max_steps` 更小）
5. 收集子 agent 的最终输出文本和历史摘要
6. 返回 `ToolOutput { content: "子 agent 完成：{摘要}\n\n{subagent 最终输出}", is_error: false }`

**子 agent 系统提示词（精简版）：**
```
你是 cyber 的子 agent，负责执行委派给你的独立子任务。
- 你的任务：{task}
- 只做被要求的事，不要做多余的事。
- 完成后直接给出最终回答，不要继续调用工具。
- 你的回答将被回传给主 agent 作为工具结果。
```

### 3. 并行执行支持（cyber-agent/src/agent.rs）

修改 `run_inner` 中的工具执行逻辑，支持并行执行多个 subagent：

**当前逻辑（串行）：**
```rust
for call in calls.values() {
    let out = registry.execute_streaming(...).await;
    messages.push(Message::tool(call.id, out.content));
}
```

**新逻辑（支持并行）：**
- 检测本轮 tool_calls 中是否有 `subagent` 工具调用
- 如果有，将 `subagent` 调用与其他非 subagent 调用分组
- 非 subagent 工具仍串行执行（因可能修改文件系统状态）
- 多个 `subagent` 调用并行执行（`tokio::join_all`）
- 所有结果收集后按顺序回灌到 messages

```rust
// 伪代码
let (subagent_calls, other_calls): (Vec<_>, Vec<_>) = calls.values()
    .partition(|c| c.name == "subagent");

// 串行执行非 subagent 工具
for call in &other_calls { ... }

// 并行执行 subagent
let subagent_results = futures::future::join_all(
    subagent_calls.iter().map(|call| execute_subagent(call, &registry, &ctx))
).await;

for (call, result) in subagent_calls.iter().zip(subagent_results) {
    messages.push(Message::tool(call.id, result));
}
```

### 4. TUI 渲染（cyber-tui）

#### 4.1 新增 ChatEntry 变体

在 `crates/cyber-tui/src/chat.rs` 中新增：

```rust
pub enum ChatEntry {
    // ... 现有变体 ...
    /// Subagent 启动（折叠状态，显示描述 + 展开提示）
    SubagentStart {
        id: String,
        description: String,
        task: String,
        subagent_type: String,
    },
    /// Subagent 执行过程中的工具调用/结果（折叠在 SubagentStart 内部）
    SubagentToolCall {
        parent_id: String,
        tool_id: String,
        name: String,
        arguments: String,
    },
    SubagentToolResult {
        parent_id: String,
        tool_id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    /// Subagent 最终输出（展开后显示）
    SubagentOutput {
        parent_id: String,
        output: String,
    },
}
```

#### 4.2 渲染逻辑

在 `crates/cyber-tui/src/views/chat.rs` 中新增：

```
[subagent] 搜索代码库中的认证逻辑 ▸ 展开
  ├─ ▶ [subagent:tool] grep("Authorization")  ← 折叠时不可见
  │    → 3 个匹配
  ├─ ▶ [subagent:tool] read_file("auth.rs")
  │    → fn check_auth() { ... }
  └─ [subagent:output]
     认证逻辑在 auth.rs:42-67，使用 JWT token 验证...
```

- 默认折叠：仅显示 `[subagent] {description} ▸ 展开` 一行
- Ctrl+O 展开/折叠：显示子 agent 的工具调用、结果和最终输出
- 展开时工具调用用缩进和 `[subagent:tool]` 标记区分

#### 4.3 新增 AgentEvent 变体

在 `crates/cyber-agent/src/types.rs` 中新增：

```rust
pub enum AgentEvent {
    // ... 现有变体 ...
    /// Subagent 启动
    SubagentStart {
        id: String,
        description: String,
    },
    /// Subagent 执行完成
    SubagentDone {
        id: String,
        output: String,
    },
    /// Subagent 执行过程中的工具调用（转发给 TUI）
    SubagentToolCall {
        parent_id: String,
        tool_id: String,
        name: String,
        arguments: String,
    },
    SubagentToolResult {
        parent_id: String,
        tool_id: String,
        name: String,
        output: String,
        is_error: bool,
    },
}
```

### 5. 系统提示词更新

在 `BASE_PROMPT_STATIC` 中新增 subagent 使用指南：

```
# Subagent 使用
- 对于可独立完成的子任务（如搜索代码库、分析特定文件），使用 `subagent` 工具委派。
- subagent 有独立的工具集和上下文，不会污染主 agent 的对话历史。
- 多个无依赖的 subagent 可以在同一轮并行启动。
- 子任务描述应简短（3-5 词），详细指令写在 `task` 参数中。
- 不要为简单操作（如读一个已知路径的文件）创建 subagent。
```

### 6. 配置持久化

在 `cyber.toml` 的 `[agent]` 段中新增：

```toml
[agent.subagent]
enabled = true
max_steps = 100
max_parallel = 4
```

## 实现步骤

### Step 1: cyber-core — 配置结构
- 文件：`crates/cyber-core/src/config.rs`
- 新增 `SubagentConfig` 结构体 + `Default` impl
- 在 `AgentConfig` 中新增 `subagent: SubagentConfig` 字段
- 新增 `SubagentType` 枚举（`General`, `Search`）

### Step 2: cyber-agent — Subagent 工具
- 文件：`crates/cyber-agent/src/tools/subagent.rs`（新建）
- 实现 `SubagentTool` struct + `Tool` trait
- 实现 `run_subagent()` 函数：构造精简 prompt → 运行子 agent loop → 返回结果
- 在 `tools/mod.rs` 中注册 `SubagentTool`

### Step 3: cyber-agent — 并行执行支持
- 文件：`crates/cyber-agent/src/agent.rs`
- 修改 `run_inner` 中的工具执行循环：检测 subagent 调用并并行执行
- 新增 `execute_subagent()` 辅助函数
- 子 agent 的 `AgentEvent` 转发到主 TUI 通道（使用 `Subagent*` 变体）

### Step 4: cyber-agent — 类型扩展
- 文件：`crates/cyber-agent/src/types.rs`
- 新增 `AgentEvent::SubagentStart`, `SubagentDone`, `SubagentToolCall`, `SubagentToolResult`

### Step 5: cyber-tui — ChatEntry 扩展
- 文件：`crates/cyber-tui/src/chat.rs`
- 新增 `ChatEntry::SubagentStart`, `SubagentToolCall`, `SubagentToolResult`, `SubagentOutput`
- 新增 `ChatState.subagent_collapsed: HashMap<String, bool>` 跟踪折叠状态

### Step 6: cyber-tui — 渲染
- 文件：`crates/cyber-tui/src/views/chat.rs`
- 新增 `render_subagent_entry()` 函数
- 支持 Ctrl+O 展开/折叠 subagent 详情
- 在 `render_body()` 中集成 subagent 条目渲染

### Step 7: cyber-tui — 事件处理
- 文件：`crates/cyber-tui/src/app.rs`
- 在 `handle_agent_event()` 中处理 `SubagentStart`, `SubagentDone`, `SubagentToolCall`, `SubagentToolResult`
- 将 subagent 事件转换为 `ChatEntry` 插入

### Step 8: 系统提示词更新
- 文件：`crates/cyber-agent/src/prompt.rs`
- 在 `BASE_PROMPT_STATIC` 中追加 subagent 使用指南段落

### Step 9: 编译验证
- `cargo check` + `cargo test` 全部通过

## 关键设计决策

1. **子 agent 共享父 agent 的 Provider**：子 agent 使用与父 agent 相同的 LLM provider 和配置，不引入新的 provider 选择逻辑。
2. **子 agent 工具集过滤**：`search` 类型仅暴露只读工具（read_file, list_dir, find_file, grep），`general` 类型暴露全部工具。
3. **子 agent 结果仅回传最终输出**：不将子 agent 的完整对话历史回灌到父 agent，避免 token 爆炸。仅回传最终文本输出。
4. **并行 subagent 共享 `ToolRegistry`（Arc clone）**：与现有 MCP 工具注册模式一致，子 agent 持有 `Arc<ToolRegistry>` 的 clone。
5. **子 agent 没有独立的上下文压缩**：子 agent 的 `max_steps` 较小（默认 100），步数耗尽时自动收尾总结，不触发压缩。
6. **TUI 折叠为默认行为**：避免 subagent 的详细执行过程干扰主对话流的阅读体验。

## 不包含的内容
- 子 agent 的持久化/恢复（会话历史中仅保存 subagent 的最终输出摘要）
- 子 agent 的超时机制
- 子 agent 间的通信（subagent 之间不互相调用）
- 子 agent 的 cost 追踪（与主 agent 合并为同一 provider 的 usage）