# Subagent 进入/退出 + 特殊标识 实现计划

## 摘要

参考 Claude Code 的 subagent 设计，实现：
1. **进入**：用户按 Enter 展开 subagent，查看其内部执行过程（工具调用、结果、思考）
2. **退出**：再次按 Enter 折叠回摘要
3. **特殊标识**：subagent 条目有独立的视觉标识（颜色、边框、类型标签）

## 当前状态分析

**现状问题：**
- `ChatEntry::SubagentStart` + `ChatEntry::SubagentOutput` 是两个独立条目，无关联、无折叠
- `AgentEvent::SubagentStart` / `SubagentDone` 定义了但**从未被 emit**（事件通道 `tx` 未传入 `ToolCtx`）
- `run_subagent` 只返回最终压缩文本，内部消息（工具调用、结果）被丢弃
- 无展开/折叠交互

**核心差距：**
1. 子 agent 内部消息在执行后即丢弃，无法回传展示
2. ToolCtx 无事件通道，子 agent 无法向 TUI 推送事件
3. ChatEntry 不支持嵌套结构

## 设计方案

### 数据结构变更

**1. 新增 `SubagentMessage`（types.rs）**

```rust
/// 子 agent 内部的一条消息记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentMessage {
    pub role: SubagentMsgRole,  // assistant / tool / thinking
    pub content: String,
    /// 工具调用信息（role=tool 时填充）
    pub tool_name: Option<String>,
    pub tool_arguments: Option<String>,
    pub tool_id: Option<String>,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentMsgRole {
    Assistant,
    Tool,
    Thinking,
}
```

**2. 更新 `AgentEvent::SubagentDone`（types.rs）**

```rust
SubagentDone {
    id: String,
    output: String,
    messages: Vec<SubagentMessage>,  // 新增：内部消息
},
```

**3. 更新 `ChatEntry`（chat.rs）**

合并 `SubagentStart` + `SubagentOutput` 为单一 `Subagent` 条目：

```rust
/// Subagent 执行条目。默认折叠显示摘要，Enter 展开查看内部消息。
Subagent {
    id: String,
    description: String,
    subagent_type: String,     // "general" | "search"
    output: String,            // 最终压缩摘要
    messages: Vec<SubagentMessage>,  // 内部消息（展开时渲染）
},
```

**4. `ChatState` 新增状态（chat.rs）**

```rust
/// 已展开的 subagent 条目索引集合（类似 expanded_tool_results）。
expanded_subagents: HashSet<usize>,
```

### 事件通道改造

**5. `ToolCtx` 新增事件通道（tool.rs）**

```rust
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub rules: Vec<String>,
    pub scope: Option<String>,
    pub env: Vec<(String, String)>,
    pub provider_config: Option<cyber_core::ProviderConfig>,
    /// 事件通道（供 subagent 等工具推送事件到 TUI）。
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<(u64, AgentEvent)>>,
    pub event_gen: u64,  // 事件代数
}
```

### 执行流程变更

**6. `run_subagent` 返回内部消息（agent.rs）**

- 签名变更：`run_subagent(...) -> Result<(String, Vec<SubagentMessage>)>`
- 在循环中收集所有 assistant 消息和 tool 消息为 `Vec<SubagentMessage>`
- 返回最终文本 + 内部消息

**7. `SubagentTool::run` 发送事件（subagent.rs）**

```
subagent_start → emit SubagentStart(id, description)
run_subagent  → 收集 (output, messages)
subagent_done  → emit SubagentDone(id, output, messages)
返回 ToolOutput(content=output)
```

**8. 主 agent 传递事件通道（agent.rs）**

在 `run_inner` 中构建 `ToolCtx` 时，将 `tx` 和 `gen` 传入。

### TUI 渲染

**9. 渲染 Subagent 条目（chat.rs）**

**折叠态：**
```
  🔀 [subagent:general] 分析CTF题目1  → 这是一道SQL注入题...  [Enter 展开]
```
- 特殊颜色：`theme.subagent`（新增，默认蓝紫色）
- 类型标签：`[general]` / `[search]` 灰色小字
- 摘要：输出文本截断到一行

**展开态：**
```
  ┌ 🔀 [subagent:general] 分析CTF题目1  [Enter 折叠] ─────┐
  │   → read_file("challenge.py")                          │
  │   ← [result] import flask...                           │
  │   → shell("python3 solve.py")                          │
  │   ← [result] flag{...}                                 │
  │   [assistant] 分析结论：这是一道SQL注入题...           │
  └────────────────────────────────────────────────────────┘
```
- 用 Unicode 框线 (`┌─┐│└┘`) 包裹
- 内部消息缩进 2 格，工具调用用 `→` 前缀，结果用 `←` 前缀
- 框线颜色与 subagent 主题色一致

**10. 新增 `push_subagent_lines` 函数（chat.rs）**

替代 `push_subagent_start` + `push_subagent_output`，根据 `expanded_subagents` 决定渲染折叠或展开态。

### 交互

**11. 按键处理（chat.rs, app.rs）**

- 新增 `ChatAction::ToggleSubagent`
- 在 `ChatState` 中新增 `toggle_subagent_expansion(&mut self)` 方法
- 从当前选中行反查最近的 subagent 条目索引，切换其展开状态
- 按键：`Enter`（在非输入态时）

**12. 导航选中 subagent 条目（app.rs）**

- 当用户用 `↑↓` 在聊天历史中移动时，若光标落在 subagent 条目上，Enter 触发展开/折叠
- 需要实现"选中条目"的概念，追踪当前高亮的条目索引

### 历史持久化

**13. history.rs**

- 序列化：`Subagent { id, description, subagent_type, output, messages }` → JSON
- 反序列化：同上
- 文本导出：折叠态显示一行摘要，展开态显示完整内部消息

### 文件变更清单

| 文件 | 变更 |
|------|------|
| `crates/cyber-agent/src/types.rs` | 新增 `SubagentMessage`、`SubagentMsgRole`；更新 `SubagentDone` |
| `crates/cyber-agent/src/tool.rs` | `ToolCtx` 新增 `event_tx`、`event_gen` |
| `crates/cyber-agent/src/agent.rs` | `run_subagent` 返回 `Vec<SubagentMessage>`；注入事件通道 |
| `crates/cyber-agent/src/tools/subagent.rs` | 发送 `SubagentStart`/`SubagentDone` 事件 |
| `crates/cyber-tui/src/chat.rs` | 合并 `ChatEntry` 变体；`expanded_subagents`；渲染函数；按键处理 |
| `crates/cyber-tui/src/app.rs` | 事件处理；传递事件通道；ToggleSubagent action |
| `crates/cyber-tui/src/history.rs` | 序列化/反序列化新的 Subagent 条目 |
| `crates/cyber-tui/src/theme.rs` | 新增 `subagent` 颜色（如需要） |

### 验证步骤

1. `cargo check` 零 warning 零 error
2. `cargo test` 全部通过
3. 手动测试：发送 subagent 任务 → 看到折叠的 subagent 条目 → 按 Enter 展开查看内部消息 → 再按 Enter 折叠