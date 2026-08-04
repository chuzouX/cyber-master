# Agent max_steps 优雅收尾 + 输入框历史呼出

## Context（为什么改）

用户反馈两个问题：

1. **max_steps 自动中断**：agent 跑一小会就显示「最大步数 max_steps=25 gen=3」然后停掉；让它「继续」只说一句话又被停掉。
   - 根因：`agent.rs:run_inner` 的 agent loop（`for step in 0..max_steps`）在模型连续 25 步都调用工具时，直接 `AgentEvent::Error("超过 max_steps(25) 限制，已停止")` 中断——**不给任何结论**。
   - 更糟的是 `ChatState::history()`（[chat.rs:418-427](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-tui/src/chat.rs#L418-L427)）跨轮剥离 ToolCall/ToolResult，所以中断后用户「继续」时，模型看不到之前工具返回的结果 → 只能空泛回一句 → Done。表现为「说一句话就停」。
   - **修复思路**：max_steps 耗尽时，做一次**无工具的收尾流式**（让模型总结已收集的信息），发 `Done` 而非 `Error`。这样用户拿到结论，且该总结进入跨轮 history，使「继续」真正可用。

2. **新功能**：方向键 ↑/↓ 在输入框为空时呼出历史发送的指令（shell 风格）。
   - 用户已确认采用「空输入时呼出」方案：输入框为空时 ↑ 呼出更早、↓ 呼出更新、↓ 到头清空；输入框有内容时 ↑↓ 仍交 textarea 移光标（保留多行编辑）。

---

## Part 1：max_steps 优雅收尾

### 1.1 `crates/cyber-agent/src/agent.rs` — `run_inner`

把循环耗尽后的「直接报错」改为「收尾总结流式」。

当前代码（[agent.rs:158-164](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-agent/src/agent.rs#L158-L164)）：
```rust
// 超过 max_steps
warn!(max_steps, gen, "agent loop 超过最大步数");
let _ = tx.send((gen, AgentEvent::Error(format!("超过 max_steps({max_steps}) 限制，已停止"))));
Ok(())
```

改为：
```rust
// 超过 max_steps：做一次无工具的收尾流式，让模型总结已收集的信息，
// 而非直接报错中断（避免用户看到裸 max_steps 错误且无任何结论）。
warn!(max_steps, gen, "agent loop 超过最大步数，进入收尾总结");
let wrap = format!(
    "（系统提示：已达到工具调用步数上限 {max_steps}。请根据已收集的信息直接给出最终回答或阶段性结论，不要再调用工具。）"
);
messages.push(Message::user(wrap));
let req = StreamRequest::new(messages.clone())
    .with_system(system.clone())
    .with_tools(Vec::new()); // 不暴露工具 → 模型只能给文本
let mut stream = provider.stream(req);
let (text, _calls) = accumulate_stream(&mut stream, tx, gen).await;
if !text.is_empty() {
    messages.push(Message::assistant(text));
}
let _ = tx.send((gen, AgentEvent::Done));
Ok(())
```

要点：
- 收尾流式 `tools=[]` → 模型无法再调工具，只能输出文本；其 `Delta` 经 `accumulate_stream` → `AgentEvent::Token` 流式回传 TUI，用户实时看到总结。
- 发 `Done` 而非 `Error` → TUI 走正常定稿路径（`finalize_stream` 把 buffer 定稿为 assistant 条目），不弹「生成失败」toast。
- `wrap` user 消息只存在于 agent 内部 `messages`（局部），不进 TUI entries，不污染跨轮 history。
- 该总结文本会作为 `ChatEntry::Assistant` 进入 entries → 下次 `history()` 含此总结 → 「继续」有上下文。
- 若收尾流式本身 HTTP 报错，`accumulate_stream` 发 `AgentEvent::Error`，已累积文本仍定稿为 assistant 条目（既有兜底行为）。

### 1.2 `crates/cyber-agent/src/prompt.rs` — 系统提示词引导

`BASE_PROMPT` 末尾追加一句，减少无意义反复调工具：
> 「使用工具收集到足够信息后应直接给出结论，避免无意义地反复调用同一工具。」

注意（memory 教训）：现有 prompt 测试断言含 `s.contains("Cyber Master")`、`s.contains("安全护栏")` 等。新增句子不含这些关键词，不破坏断言；无需改测试。

---

## Part 2：输入框历史呼出（↑/↓）

### 2.1 `crates/cyber-tui/src/chat.rs` — `InputHistory` + `ChatState` 方法

新增 `InputHistory` 结构（in-memory，不单独持久化——由 chat history 的 User 条目派生）：
```rust
#[derive(Default)]
pub struct InputHistory {
    entries: Vec<String>,      // oldest → newest，相邻去重
    browse: Option<usize>,     // 当前浏览索引；None = 未浏览态
}
```

`ChatState` 新增字段 `input_history: InputHistory`（`ChatState::new` 初始化）。

新增方法：
- `pub fn history_prev(&mut self) -> bool`：↑。
  - `entries` 空 → `false`（交 textarea）。
  - `browse=None`：若输入框非空 → `false`（交 textarea 移光标）；若空 → `browse=Some(last)`，`load_history_entry(last)`，`true`。
  - `browse=Some(i)` 且 `i>0` → `browse=Some(i-1)`，load，`true`；`i==0` → `true`（已到最早，保持）。
- `pub fn history_next(&mut self) -> bool`：↓。
  - `browse=None` → `false`（交 textarea）。
  - `browse=Some(i)`：`i+1>=len` → `browse=None`，`input.clear()`（回到最新空输入），`true`；否则 `browse=Some(i+1)`，load，`true`。
- `fn load_history_entry(&mut self, i: usize)`：`input.clear()` + `input.insert_str(&entries[i])`（沿用 `slash_menu_complete` 的 clear+insert_str 模式）。
- `pub fn seed_input_history(&mut self)`：从 `self.entries` 的 `ChatEntry::User` 文本填充 `input_history.entries`（跨会话呼出，无需新持久化文件）。
- `submit()`（[chat.rs:346-363](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-tui/src/chat.rs#L346-L363)）：在 `input.clear()` 前 `self.input_history.record(&text)`（trim 空跳过、与末条相同则跳过、`browse=None`）。

「空输入」判定：`self.input.lines().iter().all(|l| l.is_empty())`。

### 2.2 `crates/cyber-tui/src/event.rs` — 新增 ChatAction 变体

`ChatAction` 新增 `HistoryPrev` / `HistoryNext`。`chat_key_to_action`：
- `KeyCode::Up`（无修饰）→ `HistoryPrev`（原 `Input`）
- `KeyCode::Down`（无修饰）→ `HistoryNext`（原 `Input`）
- `Ctrl+Up/Ctrl+Down` 仍 → `ScrollLineUp/Down`（不变）

更新测试 `chat_plain_up_down_are_input`：普通 ↑/↓ 现映射到 `HistoryPrev/HistoryNext`（不再是 `Input`）。

### 2.3 `crates/cyber-tui/src/app.rs` — 按键分发

`handle_chat_key`（[app.rs:278-355](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-tui/src/app.rs#L278-L355)）新增两臂（镜像 `Input` 臂的 streaming 守卫）：
```rust
ChatAction::HistoryPrev => {
    if !self.chat.streaming && !self.chat.history_prev() {
        // 未呼出历史（输入非空/无历史）→ 交 textarea 移光标
        self.chat.input.input(k);
        self.chat.update_slash_menu();
    } else if !self.chat.streaming {
        self.chat.update_slash_menu(); // 呼出后输入变化（可能以 / 开头）→ 刷新菜单
    }
}
ChatAction::HistoryNext => {
    if !self.chat.streaming && !self.chat.history_next() {
        self.chat.input.input(k);
        self.chat.update_slash_menu();
    } else if !self.chat.streaming {
        self.chat.update_slash_menu();
    }
}
```

斜杠菜单已开时：`slash_menu_key(k)` 在 `chat_key_to_action` 之前消费 ↑/↓（[app.rs:283-285](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-tui/src/app.rs#L283-L285)），不进历史呼出，无冲突。

`App::new` 加载历史后（[app.rs:176-179](file:///c:/Users/chuzo/Desktop/Project/agent/cyber_master/crates/cyber-tui/src/app.rs#L176-L179) `entries.extend(saved)` 之后）调 `self.chat.seed_input_history()`。

---

## 测试

### Part 1（agent）
- 新增 `agent.rs` 测试：`max_steps_exhaustion_does_graceful_summary`。config `max_steps=1`、`auto_tool_call=true`、`mock=true`。断言事件序列含 `Token`（step0 文本）+ `ToolCall` + `ToolResult` + `Token`（收尾总结）+ `Done`，且**无 `Error`**。mock 在 `tools=[]` 时走 echo 模式发文本 + Done，正好驱动收尾流式。
- 既有 agent 测试（mock 默认 2 步收敛）不受影响——max_steps 默认 25 远大于 2。
- prompt 测试：确认新句子不破坏 `no_project_just_base` 等既有断言。

### Part 2（tui）
- `chat.rs` 单测：`InputHistory` record 去重、`history_prev` 空输入呼出/非空不呼出、`history_next` 到头清空、browse 态导航；`seed_input_history` 从 User 条目填充。
- `event.rs`：更新 `chat_plain_up_down_are_input` → 断言映射 `HistoryPrev/HistoryNext`。

---

## 验证

1. `cargo build --workspace`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`（全过；新增 agent max_steps 测试 + tui InputHistory/键位测试）
4. 手动冒烟（`--mock`）：设 `max_steps=1` 触发收尾总结，确认无「生成失败」toast 且有总结文本流式输出；↑/↓ 在空输入框呼出历史。
5. 文档：DESIGN.md §9.x（agent loop 收尾行为）+ PROGRESS.md 变更日志。
