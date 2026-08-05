# 思考强度切换功能实现计划

## 概述

新增 `/think` 命令切换 5 档思考强度（low / middle / high / max / auto），通过动态调整系统提示词的「工作方式」段落控制模型的推理深度。支持配置持久化与状态栏显示。

## 当前状态分析

- 系统提示词在 `prompt.rs` 的 `build_system_prompt(project)` 中构建，「工作方式」段落硬编码在 `BASE_PROMPT` 常量中
- `CTF_PROMPT` 是条件追加的参考模式（`run_inner` 中 `if ctf_enabled { system.push_str(CTF_PROMPT) }`）
- `run_stream` 已有 `ctf_enabled: bool` 参数的传递链路：`App::spawn_agent` → `run_stream` → `run_inner` → `build_system_prompt`
- `AgentConfig`（config.rs:59）有 `default_provider`/`auto_tool_call`/`max_steps` 三个字段，`#[serde(default)]` 支持向后兼容
- `/max_steps` 命令是最佳参考模式：无参数显示当前值，有参数设置值，写入 `config.agent.max_steps` 并即时生效
- 状态栏 usage bar 在 `views/chat.rs:173` 的 `render_usage_bar` 中渲染，已有 `│` 分隔的多段布局
- `SlashCommand` 枚举在 `slash.rs:115`，`COMMANDS` 目录在 `slash.rs:28`

## 实现方案

### 1. ThinkingIntensity 枚举 — `cyber-core/src/config.rs`

在 `AgentConfig` 之前新增枚举：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingIntensity {
    Low,     // 不输出思考，直接执行
    #[default]
    Middle,  // 3-5 行思考限制（当前行为）
    High,    // 10-15 行思考，允许深入分析
    Max,     // 无限制，充分思考
    Auto,    // 自动：CTF 模式=High，否则=Middle
}

impl ThinkingIntensity {
    pub fn as_str(self) -> &'static str { ... }
    pub fn label(self) -> &'static str { ... }  // 中文标签：低/中/高/最大/自动
    pub fn from_str(s: &str) -> Option<Self> { ... }
    /// Auto 模式解析为实际档位
    pub fn resolve(self, ctf_enabled: bool) -> Self {
        match self {
            Self::Auto => if ctf_enabled { Self::High } else { Self::Middle },
            other => other,
        }
    }
}
```

### 2. AgentConfig 扩展 — `cyber-core/src/config.rs`

```rust
pub struct AgentConfig {
    pub default_provider: String,
    pub auto_tool_call: bool,
    pub max_steps: u32,
    #[serde(default)]
    pub thinking_intensity: ThinkingIntensity,  // 新增
}
```

`Default` 实现中 `thinking_intensity: ThinkingIntensity::default()` (= Middle)。

### 3. 系统提示词动态生成 — `cyber-agent/src/prompt.rs`

将 `BASE_PROMPT` 拆为两部分：
- **BASE_PROMPT_STATIC**：安全策略 + 避免重复操作 + 任务执行 + 工具使用 + 谨慎操作 + 输出效率（不含「工作方式」）
- **工作方式段落**：由 `thinking_section(intensity)` 函数根据档位动态生成

```rust
fn thinking_section(intensity: ThinkingIntensity) -> &'static str {
    match intensity {
        ThinkingIntensity::Low => "# 工作方式\n\
- 直接执行，不要输出思考过程。先行动后解释。\n\
- 遇到不确定的问题时，调用工具验证比纯推理更高效。\n\
- 不要一次性规划所有步骤：先收集信息，再根据结果决定下一步。",
        ThinkingIntensity::Middle => "# 工作方式\n\
- 边做边想，想一步做一步：每次思考控制在 3-5 行以内，然后立即调用工具或给出结论。\n\
- 先行动后解释：遇到不确定的问题时，调用工具验证比纯推理更高效。\n\
- 思考是为了决定下一步动作，不是为了列举所有可能性。如果思考超过 5 行仍未产生明确的工具调用计划，说明你在过度推理，应立即停下并调用最相关的工具。\n\
- 不要一次性规划所有步骤：先收集信息，再根据结果决定下一步。",
        ThinkingIntensity::High => "# 工作方式\n\
- 可以深入思考，但每次思考控制在 10-15 行以内，然后调用工具或给出结论。\n\
- 先行动后解释：遇到不确定的问题时，调用工具验证比纯推理更高效。\n\
- 思考是为了决定下一步动作和诊断问题根因。如果思考超过 15 行仍未产生明确的工具调用计划，应立即停下并调用最相关的工具。\n\
- 不要一次性规划所有步骤：先收集信息，再根据结果决定下一步。",
        ThinkingIntensity::Max => "# 工作方式\n\
- 可以充分思考，不受行数限制。深入分析问题后再行动。\n\
- 先行动后解释：遇到不确定的问题时，调用工具验证比纯推理更高效。\n\
- 思考是为了决定下一步动作和诊断问题根因。\n\
- 不要一次性规划所有步骤：先收集信息，再根据结果决定下一步。",
        ThinkingIntensity::Auto => unreachable!("Auto 应在调用前 resolve"),
    }
}
```

`build_system_prompt` 签名变更为：
```rust
pub fn build_system_prompt(
    project: Option<&ProjectContext>,
    intensity: ThinkingIntensity,
) -> String
```

组装顺序：`thinking_section(intensity)` + `BASE_PROMPT_STATIC` + 项目上下文 + rules。

### 4. run_stream 传递 — `cyber-agent/src/agent.rs`

`run_stream` 和 `run_inner` 新增参数 `intensity: ThinkingIntensity`：

```rust
pub async fn run_stream(
    ...,
    ctf_enabled: bool,
    intensity: ThinkingIntensity,  // 新增
) {
    ...
    let resolved = intensity.resolve(ctf_enabled);
    let mut system = build_system_prompt(project, resolved);
    ...
}
```

### 5. SlashCommand 扩展 — `cyber-tui/src/slash.rs`

```rust
/// `/think [level]` — 查看或设置思考强度。空串 = 查看当前值。
Think(String),
```

`parse()` 中新增：
```rust
"/think" => SlashCommand::Think(args.to_string()),
```

`COMMANDS` 目录新增（在 `/max_steps` 之后）：
```rust
CommandSpec { name: "/think", usage: "/think [low|middle|high|max|auto]", desc: "查看或设置思考强度" },
```

`HELP_TEXT` 新增一行：
```
  /think [level]    查看或设置思考强度（low / middle / high / max / auto）
```

### 6. App 命令处理 — `cyber-tui/src/app.rs`

`handle_slash_command` 中新增 `SlashCommand::Think(arg)` 分支，模式与 `MaxSteps` 完全一致：

```rust
SlashCommand::Think(arg) => {
    if arg.is_empty() {
        // 无参数：显示当前值
        self.chat.entries.push(ChatEntry::System(format!(
            "当前思考强度 = {}（用法：/think <low|middle|high|max|auto>）",
            self.config.agent.thinking_intensity.label()
        )));
    } else {
        match ThinkingIntensity::from_str(arg.trim()) {
            Some(level) => {
                self.config.agent.thinking_intensity = level;
                self.chat.entries.push(ChatEntry::System(format!(
                    "思考强度已设为 {}（{}）",
                    level.as_str(),
                    level.label()
                )));
            }
            None => {
                self.chat.entries.push(ChatEntry::System(format!(
                    "无效参数：{arg}（可选值：low / middle / high / max / auto，当前 = {}）",
                    self.config.agent.thinking_intensity.label()
                )));
            }
        }
    }
}
```

`spawn_agent` 中传递 intensity：
```rust
let intensity = self.config.agent.thinking_intensity;
let handle = tokio::spawn(async move {
    run_stream(
        ..., ctf_enabled, intensity,
    ).await;
});
```

### 7. 状态栏显示 — `cyber-tui/src/views/chat.rs`

`render_usage_bar` 新增参数 `intensity: ThinkingIntensity`，在 provider/model 段之后、ctx 段之前插入：

```rust
spans.push(Span::raw("│"));
spans.push(Span::styled(
    format!(" think:{} ", intensity.as_str()),
    Style::default().fg(theme.muted),
));
```

`render` 函数签名新增 `intensity` 参数，`app.rs::render_main` 传 `self.config.agent.thinking_intensity`。

### 8. 流式期阻止

与 `/mode`、`/max_steps` 一致，流式期 `/think` 也应被阻止（`/cancel` 后才能切换）。在 `handle_slash_command` 入口处的流式检查中已统一处理。

## 涉及文件清单

| 文件 | 改动 |
|------|------|
| `cyber-core/src/config.rs` | 新增 `ThinkingIntensity` 枚举 + `AgentConfig` 加字段 |
| `cyber-agent/src/prompt.rs` | 拆分 BASE_PROMPT，`build_system_prompt` 加 intensity 参数，新增 `thinking_section()` |
| `cyber-agent/src/agent.rs` | `run_stream`/`run_inner` 加 intensity 参数，调用处传参 |
| `cyber-tui/src/slash.rs` | 新增 `Think` 变体 + parse + COMMANDS + HELP_TEXT |
| `cyber-tui/src/app.rs` | `handle_slash_command` 加 Think 分支，`spawn_agent` 传 intensity |
| `cyber-tui/src/views/chat.rs` | `render_usage_bar` + `render` 加 intensity 参数，状态栏显示 |

## 测试计划

1. **config.rs**：`ThinkingIntensity` serde roundtrip + `from_str` + `resolve(Auto)`
2. **prompt.rs**：各档位 prompt 包含正确的行数限制关键词；`build_system_prompt(None, Middle)` 包含 "3-5 行"
3. **slash.rs**：`parse("/think")` = `Think("")`；`parse("/think high")` = `Think("high")`
4. **app.rs**：`/think` 无参数显示当前值；`/think high` 设置成功；`/think abc` 报错

## 验证步骤

1. `cargo build` 全量编译通过
2. `cargo test -p cyber-core` — config 序列化测试
3. `cargo test -p cyber-agent` — prompt 测试
4. `cargo test -p cyber-tui` — slash 命令 + app 测试
5. 手动验证：启动 TUI → `/think` 查看当前值 → `/think low` 切换 → 状态栏显示 `think:low` → 发消息观察思考行为
