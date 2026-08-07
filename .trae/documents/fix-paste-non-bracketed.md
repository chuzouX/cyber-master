# 修复非 bracketed paste 终端下的粘贴问题

## 问题分析

当前 TUI 已支持 bracketed paste（`EnableBracketedPaste` + `Event::Paste` 整块插入），但在**不支持 bracketed paste 的终端**（如部分 Windows Console、SSH 会话）上：

1. 粘贴的文本被拆成逐字符 `KeyEvent` 送达
2. 每个 `KeyEvent` 独立处理 → `chat_key_to_action` 把 Enter 映射为 `ChatAction::Submit`
3. **粘贴多行文本时，第一个 Enter 就触发提交**，后续内容丢失

### Claude Code 的做法（参考）

`usePasteHandler.ts` 用三层检测：

| 层 | 机制 | 我们的对标 |
|---|---|---|
| 1. bracketed paste | `event.keypress.isPasted` | ✅ 已有 `Event::Paste` |
| 2. 大输入检测 | `input.length > 800` | ❌ crossterm 每次只送 1 个 char，无法用长度判断 |
| 3. 持续缓冲 | `pastePendingRef` + 100ms timeout | ❌ 缺失 |

**关键差异**：Node.js 会把粘贴内容批量成单个 `input` 字符串，crossterm 是逐个 `KeyEvent`。因此不能照搬长度检测，改用**时间间隔检测**。

## 方案：基于时间间隔的粘贴检测

人打字间隔 > 50ms，粘贴逐字符间隔 < 2ms。用 2ms 阈值区分：

### 新增 `PasteDetector`（在 `chat.rs` 内）

```rust
const RAPID_THRESHOLD: Duration = Duration::from_millis(2);
const FLUSH_TIMEOUT: Duration = Duration::from_millis(50);

enum KeyDisposition {
    Process,            // 正常处理（非粘贴）
    Buffer,             // 缓冲（粘贴中）
    FlushThenProcess,   // 先 flush 缓冲，再正常处理当前键
}

struct PasteDetector {
    buffer: String,
    last_key_time: Option<Instant>,
}
```

**`observe(k: KeyEvent) -> KeyDisposition` 逻辑：**

1. 只缓冲**无修饰键的 Char 和 Enter**（Ctrl+C/Esc/方向键等立即 flush + 处理）
2. 计算与上次按键的时间差 `< 2ms` → `Buffer`（push char/`\n` 到 buffer）
3. 时间差 `>= 2ms` 且 buffer 非空 → `FlushThenProcess`（先 flush，再处理当前键）
4. buffer 空 → `Process`（正常处理）

**`flush_if_stale() -> Option<String>`**：buffer 非空且距上次按键 > 50ms → 返回 buffer 内容（tick 兜底）

### 修改 `app.rs`

**`handle_chat_key` 开头插入检测：**

```rust
fn handle_chat_key(&mut self, k: KeyEvent) {
    match self.chat.paste_detector.observe(k) {
        KeyDisposition::Buffer => return,  // 缓冲中，不处理
        KeyDisposition::FlushThenProcess => {
            if let Some(text) = self.chat.paste_detector.flush() {
                self.chat.paste(&text);  // 整块插入
            }
            // 继续处理当前键
        }
        KeyDisposition::Process => {}
    }
    // ... 现有 handle_chat_key 逻辑不变
}
```

**tick 分支添加 stale flush：**

```rust
_ = tick.tick() => {
    if let Some(text) = self.chat.paste_detector.flush_if_stale() {
        self.chat.paste(&text);
    }
}
```

### 场景验证

| 场景 | 行为 |
|---|---|
| 正常打字 + Enter 提交 | 间隔 > 50ms → `Process` → 正常 Submit ✅ |
| 粘贴多行文本（无 bracketed paste） | 每字符 < 2ms → `Buffer`，Enter 也变 `\n` 进 buffer → 50ms 后 flush 整块插入 ✅ |
| 粘贴单行文本 | 字符快速到达 → `Buffer`，最后 flush 整块插入（与正常打字效果相同）✅ |
| 粘贴中按 Ctrl+C | Ctrl+C 是带修饰键 → `FlushThenProcess` → flush + 处理 Ctrl+C ✅ |
| 斜杠菜单打开时输入 | 缓冲不影响：`/` 开头时 `paste` 方法会调 `update_slash_menu` ✅ |
| bracketed paste 终端 | `Event::Paste` 正常工作，不触发 `PasteDetector`（走 `handle_paste` 路径）✅ |

## 修改文件

| 文件 | 修改内容 |
|---|---|
| `crates/cyber-tui/src/chat.rs` | 新增 `PasteDetector`、`KeyDisposition`，添加到 `ChatState`，新增 `paste` 方法已有 |
| `crates/cyber-tui/src/app.rs` | `handle_chat_key` 开头加检测；tick 加 `flush_if_stale` |

## 测试

| 测试 | 验证点 |
|---|---|
| `paste_detector_normal_key_is_process` | 单个按键、无前驱 → Process |
| `paste_detector_rapid_keys_are_buffered` | 两个 < 2ms 的 Char → Buffer |
| `paste_detector_enter_becomes_newline_in_buffer` | 快速 Enter → buffer 中为 `\n` |
| `paste_detector_flush_on_special_key` | buffer 中有内容时按 Esc → FlushThenProcess |
| `paste_detector_flush_if_stale` | buffer 非空 + 时间 > 50ms → 返回内容 |
| `paste_detector_ctrl_c_not_buffered` | 带修饰键的 Char → 不缓冲 |

## 验证步骤

1. `cargo build` — 编译通过
2. `cargo test -p cyber-tui` — 单测通过
3. 手动测试：在不支持 bracketed paste 的终端中粘贴多行文本，不应触发提交
