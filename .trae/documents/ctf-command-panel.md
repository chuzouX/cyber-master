# /ctf 命令与题目面板实现计划

## 概述

新增 `/ctf` 斜杠命令，支持 `enable`/`disable` 开关 CTF 模式。CTF 模式开启后，Chat 主页右侧显示「题目面板」，列出所有 CTF 题目及其状态。用户可通过快捷键（Ctrl+T）切换面板可见性，Enter 查看题目详情。题目解出时询问是否撰写 writeup，确认后调用 ctf-writeup skill 生成 markdown 文件并保存到 `~/.cyber/ctf/writeup/{category}/{name}/writeup.md`。

## 当前状态分析

### 已有基础设施
- **斜杠命令**：`slash.rs` 定义 `SlashCommand` 枚举 + `COMMANDS` 目录 + `parse()` 解析器；App `handle_slash_command` 分发
- **面板模式**：`Mode` 枚举含 Sessions/ModelPicker/LogViewer 等面板模式；`render_main` 按 mode 分发渲染
- **路径管理**：`Paths` 结构体集中管理 `~/.cyber/` 下所有路径；`init.rs` 首次启动创建目录
- **Skill 系统**：`ctf-writeup` skill 已存在，可通过 `skill_ctf-writeup` 工具调用
- **Chat 布局**：`views/chat.rs::render` 接收全屏 `area`，内部 vertical layout 分为 历史/输入/usage/hint
- **事件系统**：`event.rs` 定义 `ChatAction` 枚举；`handle_chat_key` 按 action 分发
- **配置**：`config.rs` 顶层 `Config` 含 ui/agent/workflow/tools/storage 段

### 需要新增的基础设施
- CTF 题目数据模型 + 持久化（`~/.cyber/ctf/challenges.json`）
- CTF 内置工具（agent 调用以注册/更新/解题）
- 题目面板视图（列表 + 详情两态）
- Chat 布局改为可选水平分割（左 Chat + 右面板）
- CTF 模式开关 + 面板切换快捷键

## 设计决策

### 1. 题目数据模型

```rust
// cyber-core/src/ctf.rs
pub enum CtfCategory { Misc, Web, Reverse, Pwn, Crypto }

pub enum CtfStatus { InProgress, Solved }

pub struct CtfChallenge {
    pub id: String,                // UUID 前缀
    pub name: String,              // 题目名称
    pub category: CtfCategory,
    pub description: String,       // 题目描述
    pub target: Option<String>,    // 靶机地址（如 nc 1.2.3.4 1234）
    pub flag: Option<String>,      // Flag 值（解出后填入）
    pub tags: Vec<String>,         // 标签
    pub status: CtfStatus,
    pub start_time: String,        // ISO 8601 或自定义格式
    pub end_time: Option<String>,
    pub writeup: Option<String>,   // Writeup 内容（None = 未写）
    pub key_points: Option<String>,// 关键知识点/卡点
}
```

### 2. 持久化

- 题目数据：`~/.cyber/ctf/challenges.json`（单文件存所有题目，JSON 数组）
- Writeup 文件：`~/.cyber/ctf/writeup/{category}/{name}/writeup.md`
- 路径加入 `Paths` 结构体：`ctf_dir`、`ctf_writeup_dir`
- `init.rs` 首次启动创建 `~/.cyber/ctf/` 和 `~/.cyber/ctf/writeup/`

### 3. CTF 内置工具

新增 `ctf_challenge` 内置工具（注册到 ToolRegistry），agent 可调用以管理题目：

```json
// action: "register" — 注册/更新题目
{"action": "register", "name": "题目名", "category": "web", "description": "...", "target": "nc ...", "tags": ["SQLi"]}
// action: "solve" — 标记题目已解出
{"action": "solve", "name": "题目名", "flag": "flag{...}", "key_points": "..."}
```

工具返回操作结果文本。TUI 通过 AgentEvent 接收题目更新事件，刷新面板。

**新增 AgentEvent 变体**：
```rust
CtfChallengeUpdate { challenges: Vec<CtfChallenge> }
```
agent 工具执行后，将最新题目列表通过事件发送给 TUI。

### 4. 斜杠命令

```
/ctf enable   — 开启 CTF 模式（注入 CTF 系统提示 + 允许显示面板）
/ctf disable  — 关闭 CTF 模式（隐藏面板 + 移除 CTF 系统提示）
/ctf add      — 手动添加题目（简化：参数 /ctf add <name> <category>，或无参时由 agent 辅助识别）
/ctf list     — 在 Chat 中列出题目摘要（System 消息）
```

### 5. 面板布局

Chat 模式下当 CTF 面板可见时，水平分割：
```
┌─ Chat ──────────────────┐┌─ 题目面板 ─┐
│ 历史                     ││ 1 [WEB]... │
│                         ││ 2 [PWN]... │
│                         ││ 3 [MISC].. │
├─────────────────────────┤│            │
│ 输入框                   ││            │
├─────────────────────────┤├────────────┤
│ usage bar               ││ hint       │
└─────────────────────────┘└────────────┘
```

- 面板宽度：固定 40 字符（或终端宽度的 35%，取较小值）
- 面板可见性由 `ctf_panel_visible: bool` 控制
- 面板焦点由 `ctf_panel_focused: bool` 控制

### 6. 快捷键

- **Ctrl+T**：切换题目面板可见性（仅在 CTF 模式开启时有效；未开启时 toast 提示）
  - 面板不可见 → 显示面板 + 聚焦面板
  - 面板可见且聚焦 → 隐藏面板
  - 面板可见但不聚焦 → 聚焦面板
- **Esc**（面板内）：返回列表视图（如在详情中）或取消聚焦（如在列表中）
- **Enter**（面板列表）：进入详情视图
- **↑/↓**（面板内）：导航题目列表

### 7. 面板视图

#### 列表视图
```
┌─ 题目面板 ──────────────┐
│ 1  [WEB]题目名    进行中 │
│    14:30          --     │
│ 2  [PWN]题目名   已完成   │
│    14:00  15:00  10m53s  │
│                   已写WP  │
│ 3  [MISC]题目名   进行中  │
│    13:00          --     │
├─────────────────────────┤
│ ↑↓选择 Enter查看 Esc退出 │
└─────────────────────────┘
```

#### 详情视图
```
┌─ [PWN]题目名 ───────────┐
│ 题目描述：xxxxxxxxxxx   │
│                         │
│ 靶机：nc 1.2.3.4 1234   │
│                         │
│ Flag：flag{xxxxxxxx}    │
│                         │
│ Tag：标签1、标签2        │
│                         │
│ 14:00 - 15:00  10m53s   │
│ ─────────────────────── │
│ Writeup                 │
│ xxxxxxxxxxxxxxxxx       │
│ ─────────────────────── │
│ 关键知识点/卡点          │
│ xxxxxxxxxxxxxxxxx       │
├─────────────────────────┤
│ Esc返回列表             │
└─────────────────────────┘
```

### 8. Writeup 生成流程

1. agent 调用 `ctf_challenge` 工具 action=solve → 题目状态变为 Solved
2. TUI 收到 `CtfChallengeUpdate` 事件，检测到题目刚解出
3. TUI 在 Chat 中追加 System 消息：「题目 [PWN]xxx 已解出！是否撰写 writeup？」
4. 用户输入 `/ctf writeup <题目名>` 或在面板详情中按 `w` 键触发 writeup 生成
5. App 调用 `skill_ctf-writeup` 工具（通过 agent spawn），生成 writeup 内容
6. writeup 内容保存到 `~/.cyber/ctf/writeup/{category}/{name}/writeup.md`
7. 同时更新 `challenges.json` 中该题目的 `writeup` 字段
8. 面板状态更新为「已写WP」

## 实施步骤

### Step 1: 数据模型与持久化（cyber-core）

**文件**: `crates/cyber-core/src/ctf.rs`（新建）
- 定义 `CtfChallenge`、`CtfCategory`、`CtfStatus`
- 实现 `serde::Serialize/Deserialize`
- 实现 `CtfCategory::as_str()` / `from_str()`
- 实现时间格式化辅助函数（`format_duration` 等）

**文件**: `crates/cyber-core/src/paths.rs`（修改）
- `Paths` 新增字段：`ctf_dir: PathBuf`、`ctf_writeup_dir: PathBuf`
- `Paths::at()` 中初始化：`ctf_dir = cyber_home.join("ctf")`、`ctf_writeup_dir = ctf_dir.join("writeup")`

**文件**: `crates/cyber-core/src/init.rs`（修改）
- `create_global_layout` 中添加 `paths.ctf_dir` 和 `paths.ctf_writeup_dir` 目录创建

**文件**: `crates/cyber-core/src/lib.rs`（修改）
- 导出 `pub mod ctf;` 及其类型

### Step 2: CTF 题目存储管理（cyber-tui）

**文件**: `crates/cyber-tui/src/ctf_store.rs`（新建）
- `load_challenges(ctf_dir: &Path) -> Vec<CtfChallenge>`：从 `challenges.json` 加载
- `save_challenges(ctf_dir: &Path, challenges: &[CtfChallenge])`：保存到 `challenges.json`
- `save_writeup(ctf_writeup_dir: &Path, challenge: &CtfChallenge, content: &str)`：保存 writeup.md
  - 路径：`{ctf_writeup_dir}/{category}/{name}/writeup.md`
  - 自动创建目录

### Step 3: CTF 内置工具（cyber-agent）

**文件**: `crates/cyber-agent/src/tools/ctf_challenge.rs`（新建）
- `CtfChallengeTool` 结构体，持有 `Arc<Mutex<Vec<CtfChallenge>>>` + event sender
- 实现 `ToolHandler` trait
- `run()` 方法解析 action：
  - `register`：新增或更新题目（按 name 匹配）
  - `solve`：标记已解出 + 记录 flag + key_points
  - `list`：返回所有题目摘要
- 执行后通过 event channel 发送 `CtfChallengeUpdate` 事件

**文件**: `crates/cyber-agent/src/tools/mod.rs`（修改）
- 注册 `ctf_challenge` 工具

**文件**: `crates/cyber-agent/src/types.rs`（修改）
- `AgentEvent` 新增 `CtfChallengeUpdate { challenges: Vec<CtfChallenge> }` 变体

### Step 4: CTF 系统提示注入

**文件**: `crates/cyber-agent/src/prompt.rs`（修改）
- 新增 `ctf_system_prompt()` 函数，返回 CTF 模式下的附加系统提示
- 内容：指示 agent 使用 `ctf_challenge` 工具注册/更新题目状态
- `run_stream` 中当 CTF 模式开启时，将 CTF 提示追加到 system prompt

### Step 5: 斜杠命令（cyber-tui）

**文件**: `crates/cyber-tui/src/slash.rs`（修改）
- `COMMANDS` 新增 `/ctf` 条目
- `SlashCommand` 新增 `Ctf(String)` 变体
- `parse()` 中 `/ctf` → `SlashCommand::Ctf(args)`

**文件**: `crates/cyber-tui/src/app.rs`（修改）
- `handle_slash_command` 中处理 `SlashCommand::Ctf`：
  - `enable`：设 `ctf_enabled = true`，toast 提示
  - `disable`：设 `ctf_enabled = false`，隐藏面板
  - `add <name> <category>`：创建新题目（追加到 challenges）
  - `list`：在 Chat 中注入 System 消息列出题目
  - `writeup <name>`：触发 writeup 生成

### Step 6: App 状态与事件处理

**文件**: `crates/cyber-tui/src/app.rs`（修改）

App 新增字段：
```rust
/// CTF 模式是否开启。
ctf_enabled: bool,
/// 题目面板是否可见（Ctrl+T 切换）。
ctf_panel_visible: bool,
/// 题目面板是否聚焦（聚焦时按键由面板消费）。
ctf_panel_focused: bool,
/// CTF 题目列表（持久化在 ~/.cyber/ctf/challenges.json）。
ctf_challenges: Vec<CtfChallenge>,
/// 面板内选中索引。
ctf_selected: usize,
/// 面板内视图：false=列表, true=详情。
ctf_detail_view: bool,
/// 面板内详情视图滚动偏移。
ctf_detail_scroll: usize,
```

事件处理：
- `handle_agent_event` 新增 `CtfChallengeUpdate` 分支：更新 `ctf_challenges`
- `handle_chat_key` 新增 Ctrl+T 处理
- 面板聚焦时拦截 ↑/↓/Enter/Esc 交面板处理

### Step 7: 面板视图渲染

**文件**: `crates/cyber-tui/src/views/ctf_panel.rs`（新建）
- `render(frame, area, theme, challenges, selected, detail_view, detail_scroll)`
- 列表视图：遍历 challenges 渲染行（序号、[分类]名称、状态、时间、用时、WP标记）
- 详情视图：渲染选中题目的完整信息
- 底部 hint 行

**文件**: `crates/cyber-tui/src/views/mod.rs`（修改）
- 导出 `pub mod ctf_panel;`

**文件**: `crates/cyber-tui/src/views/chat.rs`（修改）
- `render()` 新增可选参数 `ctf_panel: Option<CtfPanelState>`
- 当面板可见时，水平分割 area：左 Chat（Constraint::Min(0)）+ 右面板（Constraint::Length(40)）
- 左侧传入 chat render，右侧传入 ctf_panel render

**文件**: `crates/cyber-tui/src/app.rs`（修改）
- `render_main` Chat 分支：构造 CTF 面板状态传入 chat render

### Step 8: 快捷键映射

**文件**: `crates/cyber-tui/src/event.rs`（修改）
- `ChatAction` 新增 `ToggleCtfPanel` 变体
- `chat_key_to_action` 中 Ctrl+T → `ToggleCtfPanel`

### Step 9: Writeup 生成

**文件**: `crates/cyber-tui/src/app.rs`（修改）
- 新增 `spawn_writeup(challenge_name: String)` 方法
- 构造 agent 请求：system prompt = ctf-writeup skill body + 题目上下文
- 流式生成 writeup 内容
- 完成后保存到 `~/.cyber/ctf/writeup/{category}/{name}/writeup.md`
- 更新 `challenges.json` 中该题目的 `writeup` 字段
- 面板状态更新

### Step 10: 测试

**测试覆盖**：
- `ctf.rs`：模型序列化/反序列化、category 转换
- `ctf_store.rs`：加载/保存 round-trip、writeup 文件路径
- `ctf_challenge.rs`（工具）：register/solve/list 动作
- `slash.rs`：`/ctf enable`/`/ctf disable` 解析
- `event.rs`：Ctrl+T 映射
- `views/ctf_panel.rs`：列表/详情渲染不 panic
- `app.rs`：CTF 面板开关、聚焦切换

## 文件变更清单

| 文件 | 操作 | 说明 |
|------|------|------|
| `crates/cyber-core/src/ctf.rs` | 新建 | 数据模型 |
| `crates/cyber-core/src/paths.rs` | 修改 | 新增 ctf 路径 |
| `crates/cyber-core/src/init.rs` | 修改 | 首次创建 ctf 目录 |
| `crates/cyber-core/src/lib.rs` | 修改 | 导出 ctf 模块 |
| `crates/cyber-tui/src/ctf_store.rs` | 新建 | 持久化 |
| `crates/cyber-tui/src/views/ctf_panel.rs` | 新建 | 面板渲染 |
| `crates/cyber-tui/src/views/mod.rs` | 修改 | 导出 ctf_panel |
| `crates/cyber-tui/src/views/chat.rs` | 修改 | 水平分割布局 |
| `crates/cyber-tui/src/event.rs` | 修改 | Ctrl+T 映射 |
| `crates/cyber-tui/src/slash.rs` | 修改 | /ctf 命令 |
| `crates/cyber-tui/src/app.rs` | 修改 | 状态+事件+处理 |
| `crates/cyber-agent/src/types.rs` | 修改 | CtfChallengeUpdate 事件 |
| `crates/cyber-agent/src/tools/ctf_challenge.rs` | 新建 | CTF 工具 |
| `crates/cyber-agent/src/tools/mod.rs` | 修改 | 注册工具 |
| `crates/cyber-agent/src/prompt.rs` | 修改 | CTF 系统提示 |
| `crates/cyber-agent/src/agent.rs` | 修改 | 注入 CTF 提示 |

## 验证步骤

1. `cargo check --workspace` 编译通过
2. `cargo test --workspace` 全部测试通过
3. 手动验证流程：
   - 启动 TUI → `/ctf enable` → toast 提示 CTF 模式已开启
   - Ctrl+T → 右侧出现题目面板（空列表）
   - `/ctf add test-challenge web` → 面板出现新题目
   - Enter → 进入详情视图
   - Esc → 返回列表
   - Ctrl+T → 面板隐藏
   - `/ctf disable` → 面板不可显示
   - Ctrl+T（未开启时）→ toast 提示需先开启 CTF 模式
