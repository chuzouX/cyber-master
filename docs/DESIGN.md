# Cyber Master — 网络安全智能体终端 设计文档

> Rust + ratatui 构建的安全智能体终端，已实现流式对话、CTF 与 MCP/Skill 集成；工作流、Dashboard 实时监控与日志分析仍为规划。

> 📌 **实施进度**：当前实现与验收见 [PROGRESS.md](./PROGRESS.md)。本文件 §0 记录默认 CLI 已实现行为；后文保留原 TUI 设计与历史实现说明，工作流、Dashboard、SQLite 等规划不代表已交付，路线图见 [§13](#13-开发路线图)。

---

## 0. 设计目标

### 当前入口约定

`cyber` 默认进入全屏简洁 coding CLI，替代行式多轮输入；`cyber tui` 显式进入原有全屏功能面板，`cyber run` 保留非交互单次任务，`cyber setup` 打开全屏设置向导（`cyber-tui` 的设置中心：服务商 / API Key / 模型 / 工具库，`Ctrl+S` 两阶段原子保存，配置无效时 `Esc` 不允许退出）。缺少有效配置的首次交互启动先拉起同一向导；脚本不弹向导。此约定替代下文早期的默认 TUI / 唯一界面描述。

CLI 与 headless 在 `cyber-tui` 内共用独立于 `App` 的 `SessionRunner`、JSON 历史和注册表装配，尚未迁移到独立 runtime/storage crate。CLI 启用执行前 nonce 审批；headless 仅接受调用者显式的 `--allow-tool` 授权。两者启动时默认连接已配置的 MCP servers，并支持在 CLI 中通过 `/mcp connect` 重新连接。原 TUI 的权限和 MCP 行为保持原样，不应视为已有新审批保护。

### 默认 coding CLI 交互约定

- **视觉与 Header**：参考 [Oh My Pi (OMP)](https://github.com/can1357/oh-my-pi) 的真实 palette 与终端层次，不拷源码、不仿造未实现 agents/LSP。使用暖金、灰白、cyan/紫色，彩色 ASCII `Cy` + `Cyber Master V<版本>` + model/effort + cwd；不沿用原面板的模式导航标题栏。
- **主区与底部**：可滚动对话区，底部固定输入区与实际状态栏 `provider · model │ ctx 剩余% │ cache 命中率 │ ↑input ↓output`；未知值显示 `--`。不再使用行末 `\` 续行的行式交互，不显示审批模式或 agent 面板入口。
- **快捷键与补全**：空输入 `?` 打开实际 shortcuts，`/help` 显示命令目录；空输入 `Left` 不打开面板。两行圆角输入区上方显示金色无框候选；`/` 与二级参数建议、Tab 接受、Up/Down 选择，Enter 先接受未接受的候选再执行，Esc 关闭候选保留输入。普通 Enter 提交，Alt/Shift+Enter 换行，PgUp/PgDown 滚动，Ctrl+O 展开工具；任务中 Ctrl+C cancel。当前实现空闲且空输入的 Ctrl+C 也会退出，不能将其描述为任何状态都只取消。
- **状态数据**：`ctx` 是当前上下文估算 token 相对有效容量的剩余百分比，容量未知为 `--`；`cache` 是上报命中 token /（命中 + 未命中 token），分母为零为 `--`；input/output 只累计实际 Usage，未上报为 `--`。Usage 仅在本进程当前会话累计，新建/切换会话重置，重开不恢复；切换 provider/model 不重置累计，重新计算上下文容量与估算。
- **命令目录**：`cli_commands.rs::commands` 复用 TUI 目录，过滤 `/mode` 后支持 16 个主命令，加 `/effort` 共 17 项。执行、表单、picker、任务与二级补全已接入，实际语法及原 TUI 差异见 [TUI_COMMANDS.md](./TUI_COMMANDS.md)；目录覆盖不等于所有多子命令/参数/任务状态组合均已端到端验收。
- **Provider 与会话**：Provider add/edit 表单 API key 掩码、取消不保存、私有文件原子持久化，use/remove 与默认项回退已实现；model picker 使用已配置模型，不宣称 CLI 自动联网拉取。session picker/list/read/new/delete 与 ID 切换、JSON 保存已实现，跨会话读取只展示，不注入模型历史。
- **查询与任务**：tools/skill/mcp 查询不执行工具；MCP connect 是独立显式授权启动任务，不是查询副作用。clear 清空保存当前历史，cancel 取消实际任务；compact 调模型生成真实摘要，成功后提交/持久化，失败或取消不覆盖原历史。
- **CTF 隔离**：enable/disable/status、add/list 与已解题目 writeup 任务已实现；CLI add 必须显式合法分类。writeup 需要 ctf-writeup Skill，成功后保存到 `<cwd>/.cyber/ctf/sessions/<sessionid>/<challengeid>/<category>/<name>/writeup.md`，项目/会话/题目隔离；失败或取消不发布成功报告。原 TUI 的旧路径不是 CLI 当前路径，不宣称 Solved writeup 的真实 API 联调。
- **Memory 与保存边界**：全局/项目 memory CRUD、rule enabled/scope/prompt 表单及 list/edit/delete 已实现；enabled 与合法 scope/prompt 决定后续系统提示词中的记忆写入约束，是 advisory guidance，不是 hard guard。项目 config 已含 memory.rules 时修改项目规则，否则修改全局规则。think/effort/max_steps 仅保存全局目标字段，不把项目 merged 配置整份写入全局；内存立即生效，重新加载仍遵循项目覆盖。
- **正文与 Thinking**：复用 `cyber-tui::markdown` 支持子集，不支持表格或完整 CommonMark；只有实际 `reasoning_content` / Reasoning 事件才展示斜体 Markdown `Thinking`，不伪造思考。工具使用紧凑状态背景块，Ctrl+O 展开详情，工具数据不伪造 Usage。交错 reasoning/正文合并到同一回复区域，空 Token 或旧空 Assistant 不生成重复 `Cyber` 标题，保留旧 history 兼容。
- **每轮记录**：CLI 每轮结束将 `TurnSummary { elapsed_ms, finished_at, status }` 写入现有 JSON 历史；显示 `Worked for 3s · done HH:mm`（秒数向下取整，时间按本地时区），状态为 `done` / `error` / `cancelled`。重开保留记录；旧历史无记录时不补造。Worked 展示记录与 Thinking/System 均不回灌模型历史，Usage 累计也不从这些记录恢复；compact 的模型摘要不同，成功后作为压缩上下文供后续 turn 使用。
- **思考档位**：`/effort low|medium|high|xhigh|auto` 只切换现有 agent 思考档位，`medium` 映射 `Middle`、`xhigh` 映射 `Max`，保留 middle/max 别名及 `/think`，沿用系统提示词注入；不新增 provider API 的 `reasoning_effort` 参数。
- **执行前审批**：保留每次请求的 nonce 输入要求，须输入 `once <请求码>` 或 `session <请求码>` 后显式提交；paste 不得自动提交或确认，预输入内容不得误批准。session 授权仍只匹配同一工具及完全相同的参数。这是权限流程，不是 mode，精简底栏不取消审批。
- **Headless 不变**：`cyber run` 的文本/JSON 输出、会话续接、默认拒绝工具和显式 `--allow-tool` 授权保持原样，不引入全屏或交互审批。

已完成真实 Windows ConPTY 119 个断言（47 SSE + 72 commands）：120x30 / 80x12、Unicode 光标、密钥掩码/取消、17 项目录、compact/cancel/session 持久化、memory rule 等。MCP 单测与 5 个真实子进程测试通过，含 lifecycle 修复及 cancel/shutdown，UI connect deny 不会 start。此记录不是完整 Provider 外网或 CTF 已解 writeup 的真实 API 联调证明；不在此填写 workspace/crate 测试总数。

下文 Chat 布局、模式导航及 §9 的原面板键位描述属于 `cyber tui`，不替代以上默认 CLI 实现。§4/§5 的工作流与 Dashboard、SQLite 存储、专用安全工具封装及通用目标白名单校验仍为规划；现有 rules/scope 提示词不构成通用运行时授权边界。

| 目标 | 说明 |
| --- | --- |
| 双模式 | ① 对话交互式（类 Claude Code）② 可视化工作流编排（类 ComfyUI 的节点 DAG） |
| 零配置启动 | 首次启动自动创建 `~/.cyber`，读取 `.cyber.md` 作为项目上下文 |
| 实时可观测 | 工作流进度、节点级日志、资产/漏洞统计实时刷新 |
| 可扩展 | MCP / Skill / 自定义节点 / 安全工具封装 |
| 纯终端 | ratatui + crossterm，无外部 GUI 依赖，跨平台 |

---

## 1. 整体架构

### 1.1 分层架构

```
┌──────────────────────────────────────────────────────────┐
│  Presentation (ratatui TUI)                              │
│  ChatView │ WorkflowEditor │ Dashboard │ LogView │ Modal │
├──────────────────────────────────────────────────────────┤
│  Application (模式路由 / 屏幕导航 / 事件分发)             │
├──────────────────────────────────────────────────────────┤
│  Domain                                                  │
│  Agent(LLM+ToolCall) │ WorkflowEngine(DAG) │ Chat        │
│  SkillRegistry │ McpRegistry │ ToolRegistry              │
├──────────────────────────────────────────────────────────┤
│  Infrastructure                                          │
│  Config │ Storage(SQLite) │ Logger(tracing) │ Process    │
│  Providers(LLM) │ FileSystem(notify) │ Network(reqwest)  │
└──────────────────────────────────────────────────────────┘
```

### 1.2 Cargo Workspace 模块划分

采用多 crate workspace，便于解耦与单测：

```
cyber_master/
├── Cargo.toml                 # workspace 根
├── crates/
│   ├── cyber-app/             # 主二进制 main.rs，装配 + 事件循环
│   ├── cyber-core/            # 配置、路径、错误类型、AppState
│   ├── cyber-tui/             # ratatui 全部 UI（布局/组件/主题/动画）
│   ├── cyber-agent/           # LLM provider、chat、tool-calling、agent loop
│   ├── cyber-workflow/        # DAG 引擎、节点定义、执行器、调度
│   ├── cyber-mcp/             # MCP 客户端（stdio/SSE/HTTP）
│   ├── cyber-skills/          # Skill 加载与调用
│   ├── cyber-tools/           # 安全工具封装（subfinder/nmap/nuclei…）
│   └── cyber-storage/         # SQLite 历史会话/资产/日志持久化
└── docs/
```

当前主要依赖方向为 `app → tui / agent / workflow → core`，`tui → mcp / skills → agent / core`；禁止反向依赖。会话仍由 tui 的 JSON 历史模块保存，图中的 SQLite 与 storage 接管属于规划。

---

## 2. 配置与启动流程

### 2.1 目录结构

**全局（用户级）`~/.cyber/`**（首次启动自动创建）：

```
~/.cyber/
├── config.toml              # 全局设置（主题、默认模式、并发数、超时）
├── providers.toml           # LLM 提供商（OpenAI/Anthropic/Ollama/自定义）
├── profile.toml             # 用户偏好与身份（用于报告署名等）
├── skills/                  # 已安装 Skill（markdown + 可选脚本）
│   └── src-recon/SKILL.md
├── mcp/
│   └── servers.toml         # MCP server 注册表
├── workflows/               # 保存的工作流模板（.cyberflow）
│   └── src-full-flow.cyberflow
├── sessions/                # 会话状态快照（可恢复）
├── history/                 # 聊天历史 JSON（P2.2：按 cwd hash 存 {cwd_hash}.json）
├── history.db               # SQLite：聊天/命令历史（P5 计划，P2.2 暂用 history/ JSON 兜底）
├── assets.db                # SQLite：发现的资产/漏洞
├── logs/                    # 应用日志 + 工作流日志（按日期分文件）
└── reports/                 # 生成的报告（md/html/json）
```

**项目级（当前目录）**：

```
./.cyber.md                  # 项目说明（markdown）—— 启动时读取作为上下文
./.cyber/                    # 可选：项目级覆盖配置
│   ├── config.toml          # 覆盖全局设置
│   ├── workflows/           # 项目专属工作流
│   └── targets.txt          # 目标列表
```

### 2.2 `.cyber.md` 约定

项目说明文件，模型与 UI 都会读取。建议结构：

```markdown
---
project: example-corp-src
scope: 授权 SRC 漏洞挖掘
authorization: contract-2026-08-01
owner: redteam
rules:
  - 仅限 *.example.com
  - 禁止 DoS / 数据破坏
---

# 项目说明
（目标范围、历史发现、注意事项……）
```

YAML frontmatter 提供结构化字段（scope/authorization/rules），正文提供自由描述。frontmatter 中的 `rules` 会注入到 agent 系统提示词作为行为约束，不是 hard guard 或通用目标白名单执行器。

### 2.3 启动流程（启动状态机）

```
解析 CLI 参数 (clap)
        │
        ▼
确保 ~/.cyber 存在 ──(不存在)──▶ 创建默认目录 + 写入默认 config.toml
        │(存在)
        ▼
加载全局 config.toml + providers.toml + mcp/servers.toml
        │
        ▼
检测 CWD 的 .cyber.md ──(有)──▶ 解析 frontmatter + 正文 → 项目上下文
        │(无)                              │
        ▼                                  ▼
检测 CWD 的 .cyber/ ──(有)──▶ 合并项目级覆盖配置
        │                                  │
        ▼                                  ▼
初始化 Provider / MCP / Skill 注册表        │
        │◄─────────────────────────────────┘
        ▼
是否有项目上下文？
   ├─ 否 → Welcome 启动页（引导：新建项目 / 打开工作流 / 进入聊天）
   └─ 是 → 加载上次会话 或 默认进入 Chat 模式
        │
        ▼
进入事件循环（ratatui 主循环）
```

### 2.4 config.toml 示例

```toml
[ui]
theme = "cyberpunk"           # catppuccin | tokyo-night | dracula | gruvbox | nord | cyberpunk
default_mode = "chat"          # chat | workflow | dashboard
animations = true
mouse = true
frame_rate = 60

[agent]
default_provider = "openai"
auto_tool_call = true
max_steps = 25                 # agent loop 最大步数

[workflow]
max_parallel_nodes = 8
default_timeout_secs = 1800
checkpoint = true              # 启用断点续跑

[tools]
prefer_docker = false          # 工具缺失时是否用 docker 镜像
extra_path = []                # 额外工具路径

[storage]
history_retention_days = 90
log_level = "info"
```

---

## 3. 模式一：对话交互模式（Chat Mode）

类 Claude Code 的全屏对话体验。

### 3.1 布局

```
┌─ Cyber Master · chat · example-corp-src · openai/gpt-4o ──────┐
│ [assistant] 正在分析目标范围…                                  │
│ > 我已读取 .cyber.md，授权范围是 *.example.com。              │
│   建议先做子域收集 → 存活探测 → 指纹 → 漏扫。                 │
│                                                               │
│ [tool] subfinder -d example.com        ✓ 142 子域 (3.2s)      │
│ [tool] httpx -silent -title            ✓ 87 存活               │
│                                                               │
│ [user] 把存活资产导入工作流跑 nuclei                          │
│ > 已创建工作流 "nuclei-run"，节点：assets→httpx→nuclei→report │
│   切到 Workflow 视图查看？ (y/n)                              │
├───────────────────────────────────────────────────────────────┤
│ context: .cyber.md · 87 assets · 3 skills active              │
│ > ╭──────────────────────────────────────────────────────╮   │
│   │ _                                                     │   │
│   ╰──────────────────────────────────────────────────────╯   │
│ [Tab]模式 [Ctrl+P]命令面板 [/]斜杠命令 [Ctrl+R]重跑          │
└───────────────────────────────────────────────────────────────┘
```

### 3.2 特性

- **流式输出**：token-by-token 渲染（SSE/流式 API）
- **工具调用内联展示**：工具名、参数摘要、状态、耗时、结果计数；可展开看详情
- **原 TUI 斜杠命令（实际实现）**：17 个主命令及完整子命令以 [TUI_COMMANDS.md](./TUI_COMMANDS.md) 为准，依据 `slash.rs` 与 `app.rs` handler 核对；原 TUI `/memory rule` 仅出现在目录/补全，尚无 handler（CLI 已实现表单）；`/mode workflow|dashboard` 仅切到占位页，不代表工作流或监控已实现。
- **斜杠补全菜单**：输入 `/` 自动弹出命令目录（按前缀大小写不敏感过滤），↑/↓ 选择、Enter/Tab 补全命令名+空格、Esc 关闭；菜单展示用法串与描述（详见 §9.7）
- **服务商管理**：`/provider list|add|edit|use|remove` 在对话中增删改查 LLM 服务商，无需退出到配置文件；表单内「拉取模型」按钮异步 `GET {base}/models` 拉取模型列表供选择（详见 §9.5）
- **项目上下文感知**：`.cyber.md` 自动注入；`@file` 引用文件；`/add` 添加目录到上下文
- **文件编辑 diff**：agent 修改文件时以 diff 块展示，确认后落盘
- **多行输入**：`tui-textarea`，Shift+Enter 换行，Enter 发送
- **历史滚动**：PageUp/PageDown 整页、Ctrl+↑/Ctrl+↓ 单行、鼠标滚轮（3 行/格）翻阅历史聊天记录；贴底时 auto-follow 流式新内容，上滚后视图钉在原内容。预折行缓存 + 可见窗口渲染保证长历史滚动跟手性（详见 §9.7）
- **输入历史呼出**：空输入框下普通 ↑/↓ 呼出已发送消息（shell 风格），跨会话复用；非空时交 textarea 移光标保留多行编辑（详见 §9.7）
- **Agent Loop**：支持自主多步工具调用（受 `max_steps` 与 `rules` 护栏约束）；达到 `max_steps` 时不裸中断，而是发起一次无工具的收尾流式让模型总结已收集的信息后正常 `Done`，使「继续」有上下文
- **Markdown 渲染**：assistant 消息（含流式 buffer）按 Markdown 子集渲染（标题/代码块/行内代码/粗体/粗斜体/斜体/下划线/删除线/行内数学/块级数学/链接/列表/引用/分隔线），详见 §9.6
- **Chat ↔ Workflow 联动**：对话中可一句"把这些资产跑漏扫"自动生成并启动工作流

### 3.3 安全护栏

- frontmatter `rules` 与 scope 注入系统提示词，不是 hard guard。
- 内置工具已有危险命令/路径护栏；默认 CLI 另有执行前 nonce 审批，原 TUI 尚未接入该审批。
- 通用 scope 目标白名单与所有危险操作的二次确认尚未实现，不可从提示词注入推断已有运行时保证。

---

## 4. 模式二：工作流模式（Workflow Mode）—— ComfyUI 风格 DAG

### 4.1 节点画布

TUI 中的可平移/缩放画布，节点为带端口的卡片：

```
┌─ Workflow · src-full-flow ●running ──────────────────────────┐
│ ┌──────────┐    ┌──────────┐    ┌──────────┐                  │
│ │ Targets  │───▶│ Subfinder│───▶│  Httpx   │──┐              │
│ │ file:..  │    │ ●142 ▶log│    │ ✓87 ▶log │  │              │
│ └──────────┘    └──────────┘    └──────────┘  │              │
│                   ┌──────────────────────────┐│              │
│                   │  Classify (tech)         │◀┘              │
│                   │ ●running 45/87           │               │
│                   └─────┬──────────────┬─────┘               │
│              web app    │              │  api                │
│              ┌──────────▼──┐    ┌──────▼──────┐              │
│              │ Nuclei      │    │  Nmap+ffuf  │              │
│              │ ○idle       │    │  ○idle      │              │
│              └──────┬──────┘    └──────┬──────┘              │
│                     └────────┬─────────┘                     │
│                       ┌──────▼──────┐                        │
│                       │  Report     │                        │
│                       │  ○idle      │                        │
│                       └─────────────┘                        │
├──────────────────────────────────────────────────────────────┤
│ [n]ew [d]elete [r]un [s]tep [Space]select [e]dit [l]ogs [q]  │
└──────────────────────────────────────────────────────────────┘
```

节点状态色：`○idle  ●running  ✓done  ✗failed  ⏸paused`，连接线流动动画表示数据流。

### 4.2 节点类型

| 类别 | 节点 | 说明 |
| --- | --- | --- |
| **Input** | targets / ip-range / domain / file / stdin | 数据源 |
| **Recon** | subfinder / amass / assetfinder / crt.sh / chaos | 子域/资产收集 |
| **Classify** | httpx(alive) / naabu(port) / wappalyzer(tech) / filter | 资产分类筛选 |
| **Scan** | nmap / masscan / nuclei / xray / afrog / ffuf / dirsearch / sqlmap / dalfox | 漏洞/端口/目录扫描 |
| **Logic** | branch / parallel / merge / loop / dedupe / rate-limit | 流程控制 |
| **Transform** | parse / extract / jsonpath / regex / format | 数据加工 |
| **Agent** | ask-llm / agent-loop / triage | LLM 节点（单问/自主循环/分类） |
| **MCP** | mcp-tool | 调用任意 MCP 服务器工具 |
| **Skill** | skill | 调用已安装 Skill |
| **Output** | report(md/html/json) / save-db / webhook / notify | 输出 |

### 4.3 数据模型

```rust
// crates/cyber-workflow/src/types.rs
pub struct Workflow {
    pub id: WorkflowId,
    pub name: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub checkpoints: Vec<Checkpoint>,
}

pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,          // 上述节点类型
    pub inputs: Vec<Port>,       // 输入端口
    pub outputs: Vec<Port>,      // 输出端口
    pub params: serde_json::Value, // 节点参数
    pub state: NodeState,        // idle/running/done/failed
    pub progress: f32,           // 0.0..1.0
    pub logs: LogBuffer,         // 环形缓冲
}

pub struct Edge { pub from: PortRef, pub to: PortRef }
```

### 4.4 DAG 执行引擎

- **拓扑排序**确定执行顺序
- **并行调度**：无依赖关系的节点并发执行（受 `max_parallel_nodes` 限制）
- **流式数据传递**：节点间通过 `tokio::mpsc` 通道传递资产流，而非等全集
- **进度上报**：每个节点定期向事件总线推送 `progress` + `log`
- **断点续跑**：checkpoint 保存节点输出，失败/暂停后可从断点恢复
- **重试策略**：节点级 `retry`/`backoff` 配置
- **超时控制**：`default_timeout_secs` + 节点级覆盖

### 4.5 工作流文件 `.cyberflow`

序列化为 TOML（人可读可手改）：

```toml
name = "src-full-flow"
version = 1

[[nodes]]
id = "n1"
kind = "targets"
[node.params]
source = "file"
path = "./.cyber/targets.txt"

[[nodes]]
id = "n2"
kind = "subfinder"
[node.params]
recursive = true
timeout = 300

[[edges]]
from = { node = "n1", port = "out" }
to   = { node = "n2", port = "in" }
```

---

## 5. 实时监控与日志

### 5.1 Dashboard 总览

```
┌─ Dashboard ──────────────────────────────────────────────────┐
│ WORKFLOWS                                  STATS             │
│ ● src-full-flow      [████████░░] 78% 12/15  assets: 142     │
│   └ nuclei   [██░░] 45%  └ httpx ✓                          │
│ ● recon-quick        [██████████] done      alive:   87      │
│ ○ api-deep           idle                   vulns:   3       │
│                                                              │
│ RECENT LOGS                              [level] all ▼       │
│ 14:32:01 INFO  n2 subfinder found 142 subdomains             │
│ 14:32:05 INFO  n3 httpx 87 alive / 55 dead                   │
│ 14:33:12 WARN  n5 nuclei rate-limit, backing off             │
│ 14:33:40 ERROR n5 nuclei exit 1 (retry 2/3)                  │
└──────────────────────────────────────────────────────────────┘
```

### 5.2 工作流详情视图

Tab 切换：`Overview / Nodes / Logs / Stats / Assets`。
- **Nodes**：节点图 + 状态，选中节点 `Enter` 进入节点日志
- **Logs**：该工作流全量日志，支持按节点/级别/关键字过滤、正则搜索
- **Stats**：耗时分布、资产增长曲线、漏洞分类饼图（ratatui-chart）
- **Assets**：发现的资产表，可导出

### 5.3 日志系统

- **结构化日志**：`tracing` + `tracing-subscriber`，字段含 `workflow_id/node_id/level/msg`
- **节点日志缓冲**：每节点环形缓冲（默认 5000 行），溢出落盘
- **持久化**：`~/.cyber/logs/YYYY-MM-DD.log` 滚动
- **日志分析**（用户提到的"日志分析"功能）：`/analyze-logs` 命令把指定范围日志交给 LLM 做归类/异常定位/根因建议，结果以报告块呈现，可一键生成修复工作流

---

## 6. MCP 支持

> **P3 实现状态**：v0.1 三种传输全部落地（[cyber-mcp](../crates/cyber-mcp/src/)）。stdio 经子进程 JSON-RPC；Streamable HTTP 每次 call 一个 POST + `Mcp-Session-Id` 回带；legacy SSE 长连 GET event-stream 收响应 + POST endpoint 发请求。`connect_one` 按 transport 分派到三个 spawn 函数。

### 6.1 传输层

| 传输 | 用途 | v0.1 |
| --- | --- | :---: |
| stdio | 本地 MCP server（子进程 JSON-RPC over stdin/stdout） | ✅ |
| SSE | 远程 server（旧规范，长连 event-stream + POST endpoint） | ✅ |
| Streamable HTTP | 远程 server（新规范，每次 call 一个 POST + `Mcp-Session-Id`） | ✅ |

### 6.2 能力

- `cyber tui` 与 CLI/headless 启动时均按 `mcp/servers.toml` 并行连接配置 server（`McpRegistry::connect_all`，每 server 独立超时，失败 warn + skip 不阻断启动）；CLI 亦支持通过 `/mcp connect` 重连。
- 握手：`initialize`（协议版本 + client/server info）→ `tools/list` 缓存工具 schema 到 `McpConnection`（避免每次拉取）
- v0.1 支持 `tools/list` `tools/call`；`resources/*` `prompts/*` 留待后续阶段
- **统一工具表**：MCP 工具与内置工具、Skill 同等暴露给 agent 与工作流节点（详见 §6.4）
- 工作流中用 `mcp-tool` 节点调用任意 MCP 工具（P4 接入）

### 6.3 servers.toml 示例

```toml
[[servers]]
name = "filesystem"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]

[[servers]]
name = "custom-scanner"
transport = "http"
url = "https://scanner.internal/mcp"
headers = { Authorization = "Bearer ${MCP_TOKEN}" }
```

### 6.4 连接模型（actor 模式）

`Tool::run` 是 `&self`，MCP 需内部可变性。`McpConnection` 本身**传输无关**：只持 `tx: UnboundedSender<McpRequest>` + `next_id: AtomicU64` + `tools`，`call()` / `handshake()` / `call_tool()` 全走 `tx` channel + oneshot。三种传输各自起一个**不同的 actor 实现**处理同一 `McpRequest` 流（见 [connection.rs](../crates/cyber-mcp/src/connection.rs)）：

- **stdio actor**（`spawn_stdio` → `start_actor`）：单 task 串行处理子进程 stdin/stdout。actor loop 用 `tokio::select!` 处理请求通道（写 stdin）+ stdout 行（按 `\n` 切行解析 response，按 id 路由到 pending oneshot；无 id 的 notification 记 log 后忽略；半行缓冲累积）。
- **Streamable HTTP actor**（`spawn_http` → `start_http_actor`）：每次 `Call` 一个 `POST`（`application/json` 请求 / `application/json` 或 `text/event-stream` 响应）。维护 `session_id: Option<String>`——initialize 响应头 `Mcp-Session-Id` 下发后，后续请求回带。响应按 Content-Type 分派：`text/event-stream` → `parse_sse_text` + `extract_jsonrpc_responses` 取 id 匹配者；否则按 JSON 解析。`Notification` best-effort POST（忽略响应体）。client 不设全局超时，由 `call()` 的 30s oneshot 超时兜底。
- **legacy SSE actor**（`spawn_sse` → `start_sse_actor`）：长连 `GET` event-stream 收响应（reader task 把 `bytes_stream` 喂给 `SseParser`，按 id 路由到共享 `pending` 表；`event: endpoint` 携带的 POST URL 存入 `endpoint`），每次 `Call` `POST` 到 endpoint。流结束/出错 → fail 所有 pending。Shutdown 置 shutdown 标志 + abort reader。

通用约定：
- JSON-RPC `id` 单调递增无竞争，无需 Mutex；id 用 `AtomicU64`。notification 用 `JsonRpcRequest::notification(...)`（`id: None`，序列化省略 id 字段）。
- `call(method, params)` 带 30s 超时（`tokio::time::timeout` 包裹 oneshot）；握手用 `spec.timeout_secs`（默认 5s）防卡死。
- `headers` 支持 `${VAR}` 环境变量展开（`expand_env_headers`），使 `Authorization = "Bearer ${MCP_TOKEN}"` 生效。
- lifecycle 已修复并通过单测及真实子进程测试：连接取消与 shutdown 路径清理 actor/子进程；UI deny 在启动前拒绝，不能只把取消理解为丢弃 oneshot。agent 旧事件仍由 generation 隔离。

### 6.5 统一工具表与命名

`McpRegistry::connect_all` 返回 `(Self, Vec<McpTool>, Vec<(String, McpError)>)`，`McpTool` 实现 `cyber_agent::Tool` trait：

- **命名**：`mcp_<server>_<tool>`（非法字符替换为 `_`），与内置工具、`skill_<name>` 前缀隔离，避免冲突。
- `schema.description` = `[<server>] <tool description>`，便于 LLM 区分来源。
- `run` 发 `tools/call` → 把 `content[]` 的 text 项拼成单字符串 → `ToolOutput`。

**跨 turn 共享**：`App` 持有 `Arc<ToolRegistry>`（启动时 `build_registries` 构建一次），每次 `spawn_agent` 时 `Arc::clone` 传入 `run_stream`——MCP 子进程连接跨多轮 agent turn 长存，不每轮重连。退出时 `App::run` 调 `mcp.shutdown_all()`（发 `Shutdown` + await actor 退出 + 回收子进程）。

### 6.6 降级保证

`build_registries` 永不返回 Err：MCP 配置加载失败 / 某 server 连接失败（含 HTTP/SSE 网络错误、握手超时），均收集为 `Vec<String>` boot_errors 经 toast 展示，TUI 仍以「内置工具 + Skills」降级启动。`--mock` 模式跳过 MCP 连接（离线冒烟）。

---

## 7. Skill 支持

> **P3 实现状态**：v0.1 已落地目录扫描 + 渐进式披露 + `/skill` 命令 + `skill_<name>` 工具（[cyber-skills](../crates/cyber-skills/src/)）。`/skill <name>` 仅向 UI 添加 System 展示条目，不注入模型历史；模型获取正文需显式调用 `skill_<name>` 工具。不做 `triggers` 自动匹配（字段保留并写入 schema description 供 LLM 参考）。

### 7.1 Skill 结构

```
~/.cyber/skills/src-recon/
├── SKILL.md          # 名称/描述/触发条件/使用说明（渐进式披露）
└── scripts/          # 可选脚本（v0.1 不自动执行，仅供 body 引用）
    └── enrich.sh
```

SKILL.md frontmatter：

```markdown
---
name: src-recon
description: SRC 子域收集与存活探测的标准流程
triggers: [子域, 资产收集, recon, src]
tools: [subfinder, httpx]
---
# 使用说明
当用户要求子域收集时，先 subfinder 再 httpx 过滤存活……
```

### 7.2 调用方式

- **Chat**：`/skill <name>` 查看说明；`/skill list` 列出全部；LLM 调 `skill_<name>` 工具获取说明
- **Workflow**：`skill` 节点，把 Skill 作为一个可复用子流程（P4 接入）
- **渐进式披露**：模型先读 frontmatter（经 schema description），需要时调工具读正文，节省上下文

### 7.3 加载与覆盖（P3 实现）

`SkillRegistry::load_all(global_dir, project_dir)` 扫描两个目录下每个子目录的 `SKILL.md`（见 [registry.rs](../crates/cyber-skills/src/registry.rs)）：

- **全局**：`~/.cyber/skills/<name>/SKILL.md`
- **项目级**：`<cwd>/.cyber/skills/<name>/SKILL.md`（`Paths::project_skills_dir`）
- **覆盖**：项目级与全局同名时，项目级优先（按 `frontmatter.name` 去重，Project 覆盖 Global）。
- 单个 Skill 加载失败（缺 name / frontmatter 损坏）收集到 errors 列表，不 panic、不阻断其余 Skill 加载。
- frontmatter 解析复用 `.cyber.md` 的 BOM strip + `---` 分隔 + serde_yaml 模式（拷贝到 `cyber-skills::frontmatter`，不泛型化 core 的 parse 以免影响 core 测试）。

### 7.4 SkillTool（渐进式披露）

每个 `Skill` 包成 `SkillTool` impl `cyber_agent::Tool`（见 [tool.rs](../crates/cyber-skills/src/tool.rs)）：

- **命名**：`skill_<name>`，与内置工具、`mcp_<server>_<tool>` 前缀隔离。
- `schema.description` = `[Skill] <description>` + `触发词: ...`（非空时）+ `调用此工具以获取详细使用说明`（第一层披露：LLM 据此判断是否调用）。
- `run` 返回 skill body（第二层披露：详细使用说明）。
- 工具无参数（`{"type":"object","properties":{}}`）。

---

## 8. 安全工具集成（cyber-tools）

### 8.1 封装策略

每个工具一个 wrapper，统一接口：

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> &serde_json::Value;
    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput>;
}
```

### 8.2 内置工具

subfinder, amass, assetfinder, httpx, naabu, dnsx, nmap, masscan, nuclei, xray, afrog, ffuf, dirsearch, gobuster, sqlmap, dalfox, plus `shell`（任意命令，受护栏约束）。

### 8.3 工具发现

- 启动时探测 PATH，记录可用工具与版本
- 缺失工具提示安装方式（go install / brew / docker）
- `prefer_docker=true` 时自动用官方 docker 镜像兜底

---

## 9. UI / UX 设计（ratatui）

### 9.1 全局布局

- **标题栏**：模式 · 项目名 · provider/model · 全局状态
- **主区**：模式专属内容
- **状态栏**：当前键位提示 · 警报 · 资源占用
- **模态层**：命令面板(Ctrl+P) · 帮助(?) · 设置(S) · 确认对话框

### 9.2 键位

| 键 | 动作 |
| --- | --- |
| Tab | 切换模式 Chat/Workflow/Dashboard（Settings 内：切换段） |
| s | 打开设置（模态层，任意模式可进） |
| Ctrl+P | 命令面板 |
| ? | 上下文帮助 |
| Ctrl+S | 保存会话 |
| Ctrl+R | 重跑 / 重连 |
| Esc | 返回 / 取消（Settings 内 dirty 时双击回退） |
| / | 斜杠命令（Chat） |
| Space | 选中节点（Workflow） |
| e | 编辑节点参数 |
| l | 查看节点日志 |
| Enter | 确认 / 进入（Settings 内：编辑字段 / 保存设置 / Providers 段设默认） |
| ←/→ | Settings 内调整字段值（bool 切换 / enum 循环 / number ±step）；Provider Form 内切换 kind |
| ↑/↓ | 导航（Welcome 选项 / Settings 字段行 / Provider Form 字段） |
| a | Settings Providers 段：新增服务商（打开 Provider Form） |
| e | Settings Providers 段：编辑当前服务商 |
| d | Settings Providers 段：删除当前服务商（双击 `d` 确认） |

> **Chat 输入态键位差异（P2）**：Chat 是文本输入模式，`s`/`q`/`/` 等均为打字字符，不触发上表动作。Chat 专属键：`Enter` 发送、`Shift+Enter`/`Alt+Enter`/`Ctrl+J` 换行、`Ctrl+,` 打开设置（替代 `s`）、`Ctrl+C`/`Ctrl+Q` 退出（替代 `q`）、`Esc` 取消流式或返回、`PageUp`/`PageDown` 整页滚动历史、`Ctrl+↑`/`Ctrl+↓` 单行滚动历史、鼠标滚轮（3 行/格，需 `config.ui.mouse`）。普通 ↑/↓：空输入框时呼出输入历史（浏览态继续 ↑/↓ 翻更早/更新，↓ 到头回到空输入），非空时交 textarea 移光标（保留多行编辑）；斜杠补全菜单打开时 ↑/↓/Enter/Tab/Esc 由菜单消费（详见 §9.7）。其余模式仍用上表全局键位。

### 9.3 主题与动画

- 内置主题：catppuccin / tokyo-night / dracula / gruvbox / nord / cyberpunk（赛博朋克：深紫黑底 + 霓虹粉/青，灵感来自 ratatui.rs "Built with Ratatui" 项目群）
- 动画：spinner、进度条、节点状态脉冲、连接线流动、视图淡入；可用 `animations=false` 关闭
- 帧率：默认 60fps，CPU 敏感场景降到 30fps

### 9.4 设置页（Settings 模态层）

P1 阶段实现的配置查看 / 编辑 / 持久化入口。用 `Mode::Settings` 模拟模态层：全局 `s` 或 Welcome 第 4 项进入，`Esc` 返回 `prev_mode`。

**布局**：左侧段侧边栏（6 段）+ 右侧字段表。Providers 段可交互（`a`/`e`/`d`/`Enter` 增删改设默认，详见 §9.5），其余 5 段按 `FieldKind` 编辑。

**字段编辑模型**：`SECTIONS` 用 fn 指针访问 `Config` 字段，按 `FieldKind` 派发：

- `Bool`：`←/→` 或 `Enter` 切换 on/off
- `Enum`：`←/→` 在固定选项间循环（如 theme 在 6 预设间循环）
- `ProviderEnum`：运行时取 `providers.keys()` 排序后循环
- `Number { min, max, step }`：`←/→` ±step 并 clamp
- `ReadOnly`：仅展示

**实时应用（live-apply）**：`theme` 与 `mouse` 改动由 App 立即应用（重解析主题 / 切换鼠标捕获），无需重启；其余字段标记生效时机，保存后下次启动或对应阶段（P2–P5）生效。

**持久化**：右侧底部「保存设置」行 + `Enter` 触发 `save_config`，原子写（`.tmp` → 旧文件备份为 `.bak` → rename）回写 `~/.cyber/config.toml`。不用 `Ctrl+S`，避开 §9.2 会话保存冲突。

**字段生效时机表**：

| 段 | 字段 | 类型 | 生效时机 |
| --- | --- | --- | --- |
| UI | theme | Enum(6) | 即时（live-apply 重渲染） |
| UI | default_mode | Enum | 重启（下次启动按此路由） |
| UI | mouse | Bool | 即时（live-apply 切换捕获） |
| UI | animations | ReadOnly | P6（动画引擎实现后） |
| UI | frame_rate | ReadOnly | —（P7 打磨） |
| Agent | default_provider | ProviderEnum | 即时（标题栏刷新；P2 起真正生效） |
| Agent | auto_tool_call | Bool | P2（agent loop 接入后） |
| Agent | max_steps | Number | P2（agent loop 接入后） |
| Workflow | max_parallel_nodes | Number | P4 |
| Workflow | default_timeout_secs | Number | P4 |
| Workflow | checkpoint | Bool | P4 |
| Tools | prefer_docker | Bool | P3 |
| Tools | extra_path | ReadOnly | —（编辑配置文件） |
| Storage | history_retention_days | Number | P5 |
| Storage | log_level | ReadOnly | —（编辑配置文件 / RUST_LOG） |
| Providers | （整段可交互） | — | `a`/`e`/`d`/`Enter` 管理（详见 §9.5）；Settings 入口延迟随「保存设置」写盘，Chat `/provider` 入口立即写盘 |

**安全机制**：

- **Esc 双击回退**：有未保存改动时，首次 `Esc` 提示「再按 Esc 丢弃」并置 `pending_discard`；二次 `Esc` 用进入时的快照 `config_at_entry` 回滚 `config`、`providers_at_entry` 回滚 `providers`，并 live-apply 复位主题/鼠标，再返回 `prev_mode`。无改动时单次 `Esc` 直接返回。
- **dirty 标记**：标题 `Settings *` + 保存行 `保存设置 *` + 底部提示行三处可见；providers 段有改动时单独标 `dirty_providers`（与 config 的 `dirty` 分离追踪）。
- **项目级覆盖警告**：检测到 `.cyber/config.toml` 时顶部横幅提示「保存仅写全局，被覆盖字段重启后回弹」（deep-merge 在加载时仍会把项目级覆盖叠加上去）。
- **已知限制**：toml crate 不保行内注释；项目级覆盖字段保存后重启仍被覆盖。

### 9.5 Provider Form 模态层

P2 阶段实现的服务商管理入口。从 Settings（Providers 段 `a`/`e`）或 Chat（`/provider add|edit <name>`）两路进入，作为顶层 `Mode::ProviderForm` 渲染居中模态表单。参考 `example/wepclaude` 的 customProviders 逻辑：provider = `{name, kind, base_url, api_key, model, max_tokens, temperature}`，支持新增/编辑/删除/设默认 + 异步拉取模型列表。

**字段**（↑/↓ 导航，Enter 进入编辑）：

| # | 字段 | 编辑方式 |
| --- | --- | --- |
| 0 | name | 文本（Enter 进 textarea 编辑 → Enter 提交 / Esc 取消） |
| 1 | kind | Enum（←/→ 在 `PROVIDER_KINDS` = openai/anthropic/ollama/openai-compatible 间循环） |
| 2 | base_url | 文本（自动 trim + 去尾 `/`） |
| 3 | api_key | 文本 |
| 4 | model | 选择（Enter/空格打开模型列表面板；列表内 ↑/↓ 选、Enter 回填、m/f 手输兜底、r 重新拉取、t/v 实测推理/视觉） |
| 5 | alias | 文本（显示别名，留空用 model id） |
| 6 | context_length | Enum（←/→/空格 在 `CONTEXT_LENGTH_PRESETS` = 默认(留空)/128K/256K/512K/1M/自定义 间循环；选「自定义」后 Enter 进 textarea 手输数字） |
| 7 | max_tokens | 文本（parse 为 u32，失败校验报错） |
| 8 | temperature | 文本（parse 为 f32，失败校验报错） |
| 9-11 | input_per_m / output_per_m / cache_hit_per_m | 文本（每百万 token 价格，可留空） |
| 12 | currency | Enum（←/→ 循环 usd/cny） |
| 13 | notes | 文本（自由备注） |
| 14 | chat_endpoint | 文本（高级设置：自定义流式对话端点，留空默认 `{base_url}/chat/completions`） |
| 15 | models_endpoint | 文本（高级设置：自定义模型列表端点，留空默认 `{base_url}/models`） |
| 16 | thinking.type | Enum（←/→/空格 循环 未设置/enabled/disabled；未设置 = 不下发任何思考参数） |
| 17 | thinking.effort | Enum（←/→/空格 循环 未设置/low/medium/high；未设置 = 不下发 `reasoning_effort`） |
| 18 | 拉取模型 | 按钮（Enter 触发异步 fetch） |
| 19 | 保存 | 按钮（Enter 触发校验 + 持久化） |
| 20 | 取消 | 按钮（Enter / Esc 丢弃表单返回 `prev_mode`） |

字段 5-13 属于「模型专属微调参数」（对当前选中 model 独立生效），14-17 归入「高级设置 高级选项」分组，渲染时以分隔线与分组标题区分。

**高级端点覆盖**：`chat_endpoint` / `models_endpoint` 为空串时序列化为 `None`（`skip_serializing_if`），生效端点回退到按 kind 推导的默认值（见 `ProviderConfig::chat_endpoint()`）。显式清空已有覆盖值时必须从原始 `providers.toml` 删除对应键，否则 `merge_table` 合并会保留陈旧值并在下次启动重新读回（`provider_configuration_bytes` 中与 `context_length` 同处处理）。

**拉取模型（async fetch）**：「拉取模型」按钮 bump `fetch_id`（防 stale）+ 置 `fetching` 态，spawn `cyber_agent::fetch_models` 任务。按 kind 试 `{base}/models` 与 `{base}/v1/models`（anthropic 先 v1，其余先 /models；base_url 已含版本段时不再重复追加 `v1`，并去掉重复候选），headers 按 kind（anthropic→`x-api-key`+`anthropic-version`；openai/compatible→`Authorization: Bearer`；ollama→无 auth）。结果经 `mpsc::UnboundedSender<FetchResult>` 回传主循环第 4 路 `select!` 分支 → `deliver_fetch`（`fetch_id` 不匹配则丢弃）。成功弹出 picker（↑/↓ 选模型 → Enter 回填 model 字段）；失败显示错误文案。

**模型列表选择面板**：字段 4（model）不再是可自由键入的文本 —— Enter/空格在无列表时发起拉取、有列表时直接打开面板（面板占用字段区下方 12 行）。列表每行在模型 id 后附能力标签（`◈ 视觉` / `◈ 推理`），来源为 `cyber_core::resolve_vision_capability` / `resolve_reasoning_capability` 的三级解析：**显式 `models[model].vision|reasoning` 配置 → `~/.cyber/cache/capabilities.json` 实测缓存 → 名称规则表**（`is_vision_model_by_name` / `is_reasoning_model`）。每个渲染帧只读一次缓存文件。面板内：`↑/↓` 选、`Enter` 回填、`m`/`f` 手输、`r` 重新拉取、`t` 实测推理、`v` 实测视觉、`Esc` 关闭；拉取失败或空列表时自动进入手输兜底（`[✎ 手输模式]` 行内标记，model 字段恢复可编辑，Enter 退出）。探针为真实最小请求（`probe_model_reasoning` / `probe_model_vision`），结果经 `ProbeResult{scope}` 回传：`ProbeScope::Picker` 写回 `providers.toml` 的 per-model 能力字段，`ProbeScope::ProviderForm` 只写能力缓存（表单尚未保存，不得提前落盘）。

**thinking（思考参数）下发**：`ProviderConfig::thinking` 为 provider 级（`thinking.type` + `thinking.effort`，两者独立、都可为 `None`）。为 `None` 时四个 provider 的请求体与旧行为逐字节一致。下发规则见下表，`disabled` / `effort` 在各 kind 的无对应字段处一律忽略（不报错）：

| kind | `type=enabled` | `type=disabled` | `effort` |
| --- | --- | --- | --- |
| openai / openai-compatible | `thinking:{"type":"enabled"}` | `thinking:{"type":"disabled"}` | 顶层 `reasoning_effort` |
| anthropic | `thinking:{"type":"enabled","budget_tokens":clamp(max_tokens/2,1024,32000)}` 且 `temperature=1.0`（`max_tokens<=1024` 时不下发，无法满足约束） | 不下发（Messages API 无关闭字段） | 忽略 |
| ollama | `think:true` | `think:false` | 忽略 |
| responses | `reasoning:{"effort":"medium"}` | 不下发 | `reasoning:{"effort":<值>}` |

显式清空（`Some` → `None`）时必须从原始 `providers.toml` 删除 `thinking` 键（与端点覆盖同处处理，顺序在 `merge_table` 之后，故只在「新值为 None」时删除）。

**探针端点修正**：`ProviderConfig::chat_endpoint()` 只为 ollama 特判，anthropic/responses 会误落到 `{base}/chat/completions`。探针统一走 `cyber_agent::probe_endpoint`：显式 `chat_endpoint` 优先，否则 anthropic→`{base}/v1/messages`、responses→`{base}/responses`、其余→`chat_endpoint()`。

**API 版本段去重**：凡实现里固定带 `/v1` 的路径拼接（anthropic `{base}/v1/messages`、模型列表 `{base}/v1/models`）一律先过 `cyber_core::with_api_version`：base_url 的 path 已含版本段（`/v1`、`/v1beta`、`/v2`、`/api-v1` 这类以 `-v<数字>` 结尾的段）则原样使用，否则补 `/v1`。只看 path、忽略 `scheme://host`，因此 `https://v1.example.com` 仍会补版本段。这样把 base_url 配成 `https://api.anthropic.com/v1`（`/provider add-with-kind anthropic` 的默认端点）时不会再拼出 `/v1/v1/messages`；`fetch_endpoints` 另对重复候选去重（`https://host/v1` 只产生一个 `{base}/models`）。

**持久化双轨**：

- **Settings 入口**：Save 只改内存 `self.providers` + 标 `dirty_providers`，**不立即写盘**；统一在 Settings 的「保存设置」行 `Enter` 触发 `save_config` + `save_providers` 一并写 `~/.cyber/`。Esc 双击用 `providers_at_entry` 快照回滚（与 `config_at_entry` 同步）。
- **Chat 入口**（`/provider add|edit`）：Save 立即 `save_providers` 写盘（Chat 无 Settings 的保存触发点）。

**校验**：name 非空 + 不与现有重名（编辑自身除外）+ base_url 非空 + max_tokens/temperature 可 parse。失败保留表单 + toast 提示，不退出。

**max_tokens 默认与钳制**：新建 / 缺省 provider 的默认 `max_tokens` 为 `cyber_core::DEFAULT_MAX_TOKENS`（384_000）。真正发给端点的值是 `ProviderConfig::effective_max_tokens()`：per-model `max_tokens` 优先、回退 provider 级，再按该模型声明的 `context_length` 钳制；未声明 `context_length` 时钳制到 `DEFAULT_OUTPUT_TOKEN_CAP`（128_000）。因此配置里写 384000 是安全的上限声明，实际发送值不会超出模型窗口——想用满更大预算需把该模型的 `context_length` 提到相应数值。截断续写的回合处理见 `cyber_agent::agent`（`MAX_EMPTY_TRUNCATION_ROUNDS` / `TRUNCATION_CONTINUE_NUDGE` / `AgentEvent::Notice`）。

**删除确认**：Settings Providers 段 `d` 双击（首次置 `pending_delete_idx` + 行内 `[待删除!]` 标记，任一其他键清除；二次 `d` 执行删除）。删除/重命名触及 `default_provider` 时自动回退到排序后首个剩余 / 同步改名，并 toast。

**default_provider 防悬空**：删除当前默认 provider → 回退到排序后首个剩余；重命名当前默认 → 同步改名。两者均标 dirty 触发持久化。

### 9.6 Markdown 渲染

P2 阶段为 Chat 的 assistant 消息（含流式 buffer）实现轻量 Markdown 渲染，让 LLM 输出的标题、代码块、列表等结构清晰可读。默认 coding CLI 的正文与真实 Thinking 也复用 `cyber-tui::markdown` 模块（手写解析器，不引入外部 markdown crate）。以下是支持子集，不是完整 CommonMark；表格不支持。

**覆盖子集**：

| 元素 | 语法 | 渲染 |
| --- | --- | --- |
| 标题 | `#`..`######` | accent 色 + 粗体，保留 `#` 前缀 |
| 代码块 | ` ``` ` / `~~~` 围栏 | `│ ` 左标记 + title 色，围栏分隔行隐藏 |
| 行内代码 | `` `code` `` | title 色 + DIM |
| 粗体 | `**text**` | BOLD（内部递归解析，支持 `**`code`**`） |
| 粗斜体 | `***text***` | BOLD + ITALIC（先于 `**` 匹配，否则 `**` 会吞掉前两个 `*`；内部递归） |
| 斜体 | `*text*` | ITALIC（跳过成对 `**` 避免与粗体冲突） |
| 下划线 | `<u>text</u>` / `<ins>text</ins>` | UNDERLINED（Markdown 无原生下划线语法，用 HTML 语法糖） |
| 删除线 | `~~text~~` | CROSSED_OUT |
| 行内数学 | `$...$` | math 色 + ITALIC（开头非空格、闭合前非空格、内容非空，避免 `$5` 货币误触发） |
| 块级数学 | 独占行 `$$` 起止 | 每行缩进 2 + math 色 + ITALIC（内容原样展示，不递归解析避免 `^`/`_` 误触发） |
| 链接 | `[text](url)` | accent 色 + UNDERLINED（url 不显示，保持简洁） |
| 无序列表 | `-`/`*`/`+` + 空格 | accent 色标记 |
| 有序列表 | `数字.` + 空格 | accent 色标记 |
| 引用 | `> text` | `│ ` muted 色标记 |
| 分隔线 | `---`/`***`/`___`（≥3 同字符） | muted 色 `─` 行 |

**匹配优先级**：行内标记扫描按前缀冲突时长者先：行内代码 > 粗斜体 `***` > 粗体 `**` > 斜体 `*` > 下划线 `<u>`/`<ins>` > 删除线 `~~` > 显示数学 `$$` > 行内数学 `$` > 链接 `[`。匹配失败（未找到闭合）时该字符按普通文本处理，继续向后扫描——保证未闭合格式降级为纯文本而非吞字符。

**非目标**（不实现）：嵌套列表、表格、通用 HTML（仅 `<u>`/`<ins>` 作下划线语法糖）、脚注、图片——TUI 场景下收益低且复杂度高。TUI 无法渲染 LaTeX，数学公式仅原样展示公式文本并以 accent 色 + 斜体作视觉标记。

**颜色映射**：不新增 `Theme` 字段，由现有主题色派生（`code_fg=title`、`header=link=list=math=accent`、`quote=hr=muted`），随主题切换经 `invalidate_cache` 触发重建。

**渲染集成**：
- 已定稿 assistant 条目：`[assistant]` 标签独占一行（与 User/System 的内联标签不同——块级结构需自然铺开，标签独占一行避免缩进错位），随后是 Markdown 渲染的内容行；经 `prepare_render` 入行缓存复用。
- 流式 buffer：每帧经 `markdown::render` 现场解析（量小，不入缓存），末行追加 `▌` 光标。buffer 可能为不完整 Markdown（未闭合 `**` 或未闭合围栏），解析器把未闭合格式降级为纯文本，不 panic。

**降级保证**：所有未闭合标记按普通文本处理，不吞字符；流式中途渲染始终稳定。

### 9.7 历史滚动与斜杠补全菜单

Chat 是文本输入态，普通 ↑/↓ 交给 textarea 移光标，因此历史滚动与命令补全需要专属交互。

**历史滚动**（`ChatState::scroll_history`）：

| 输入 | 动作 |
| --- | --- |
| PageUp / PageDown | 整页滚动（页大小 = 上一帧历史区可见高度） |
| Ctrl+↑ / Ctrl+↓ | 单行滚动 |
| 鼠标滚轮上/下 | 滚动 3 行（需 `config.ui.mouse`） |

- `scroll_y` 以绝对顶部行号记录偏移；哨兵值 `SCROLL_FOLLOW`（`usize::MAX`）表示"跟随底部"。
- 贴底时切回 `SCROLL_FOLLOW` → 流式新内容自动滚到底（auto-follow）；上滚后记录绝对偏移，内容增长时视图钉在原内容（不滑向新内容）。
- 度量（总行数 / 可见高度）由 render 每帧经 `Cell` 回写，按键处理据此计算 `max_scroll` 与页大小；首帧前度量为 0 → 滚动 no-op（安全）。
- submit / clear / cancel 后调 `scroll_to_bottom()` 恢复 auto-follow。
- 标题栏显示滚动指示：跟随底部时不显示，上滚后显示位置/总量（如 `[12/45]`）。

**滚动跟手性（预折行缓存 + 可见窗口）**：长历史下滚动卡顿的主因是旧行为每帧 `Paragraph::line_count` + `Wrap` 重算（O(N)）+ 全量 `extend_from_slice` clone。优化为：

- `WrappedCache`（`RefCell`，render 以 `&self` 经内部可变更新）按当前可视宽度把已完成条目 + 流式 tail 预折行为单行 `Line` 列表，缓存键 = `(entries.len, streaming_buffer.len, width)`；key 命中且 `valid` 时 O(1) 复用，仅内容/宽度变化时 O(N) 重建。
- 折行用 `unicode-width`（CJK 占 2 列、tab 占 1 列），贪婪字符宽度填充，相邻同 style 字符合并为一个 span（控制行样式边界）。
- render 只 clone 可见窗口 `wrapped[offset..end]`（O(visible)），以**无 `Wrap`、无 `scroll`** 的 `Paragraph` 渲染——ratatui 渲染复杂度从 O(N) 降到 O(visible)，滚动时内容未变直接命中缓存，跟手性显著改善。
- theme 切换经 `invalidate_cache` 置 `valid=false` 强制重建（颜色变了但折行数不变，key 不会变，须显式失效）。
- 空会话引导文本量小，直接 `Paragraph+Wrap` 不参与缓存/滚动路径。

**斜杠补全菜单**（`ChatState::slash_menu` + `slash::COMMANDS`）：

- 输入首行以 `/` 开头且不含空格时，`update_slash_menu` 每次输入后按前缀大小写不敏感过滤 `COMMANDS` 目录并打开菜单；不匹配或输入离开 `/` 前缀态时自动关闭。流式期输入被禁，菜单不会打开。
- `COMMANDS` 是命令名/用法/描述的单一来源（`CommandSpec { name, usage, desc }`），`HELP_TEXT` 与菜单均据此展示，避免多处维护漂移。
- 菜单打开时 ↑/↓/Enter/Tab/Esc 由 `slash_menu_key` 消费，不传给 textarea 也不触发其他动作：↑/↓ 循环选择、Enter/Tab 用选中命令名+空格替换输入框（用户继续输参数再 Enter 提交）、Esc 关闭。
- 菜单浮于输入框正上方（覆盖历史区底部），用 `Clear` 清背景后渲染 `List`（高亮选中行 `▶`），最高 8 项后内部滚动。
- 输入普通字符后实时刷新过滤；过滤结果变化时保持选中项在范围内。

**输入历史呼出**（`ChatState::input_history` + `history_prev`/`history_next`）：

- 空输入框下普通 ↑ 呼出最新一条已发送消息，继续 ↑ 往更早翻、↓ 往更新翻、↓ 到头清空输入框并退出浏览态（回到最新）。
- **与多行编辑的冲突处理**：输入框非空时 ↑/↓ 返回 `false`（未处理）→ App 层转交 textarea 移光标，保留 Shift+Enter 换行后的多行光标移动。仅空输入框（或已进入浏览态）才呼出历史，避免抢夺多行编辑的光标控制。
- 不单独持久化：`InputHistory.entries` 由 chat history 的 `ChatEntry::User` 条目派生（App 启动加载历史后 `seed_input_history` 填充），新提交经 `record` 追加（trim 空跳过、与末条相同则相邻去重）。跨会话呼出靠下次启动重新 seed，不新增存储文件。
- 浏览态语义：`browse=None` 表示正在输入新内容；首次 ↑ 进入浏览态指向最新条目，继续 ↑ 往更早（到最早保持当前）、↓ 往更新（到头清空并退出）。提交新消息或编辑输入后退出浏览态。
- 流式期由 App 拦截不调用此处（生成中禁用输入）；斜杠补全菜单打开时 ↑/↓ 由菜单消费，不触发历史呼出。

### 9.8 Session 管理（多会话）

同一 cwd 内支持多个独立会话，对话历史从单文件升级为多 session 结构（[history.rs](../crates/cyber-tui/src/history.rs)）：

```text
~/.cyber/history/{cwd_hash}/
  index.json     # { "current": "<id>", "sessions": [SessionMeta] }
  {id}.json      # Vec<ChatEntry>
```

- `SessionMeta { id, title, created_at, updated_at, message_count }`；`SessionIndex { current, sessions }`。`cwd_hash` 沿用 FNV-1a 64bit（跨 Rust 版本稳定），session id 用 `SystemTime` 纳秒 base36 编码（短、单调、无新依赖）。
- **迁移**：`load_index` 见旧单文件 `{cwd_hash}.json`（无 `index.json` 时）→ 自动迁移为单 session（title 取首条 User 前 40 字符或 "默认会话"），旧文件 rename `.legacy.bak`（不删，防回退丢失）。无任何历史 → 建默认空 session（title "新会话"）。
- **title 派生**：`save_current` 时若 meta.title=="新会话" 且 entries 首条为 User → title = 首 40 字符（避免一直显示 "新会话"）。
- **独立性**：`spawn_agent` 传 `self.chat.history()`，天然只含当前 session；`history()` 剥离 ToolCall/ToolResult（工具链仅单次 spawn 内部维护）。各 session 独立文件，切换时整条 `ChatState` 重建。

**斜杠命令**（`slash.rs` + `app.rs::handle_slash_command`，均流式期阻止）：

- `/new` — 保存当前 → 建 meta + 切到新空会话 → 重置 `ChatState` → 持久化 index + 空 entries。
- `/sessions`（空参 / `list`）— 打开 Sessions 面板（`Mode::Sessions`）。
- `/sessions read <id|关键词>` — 跨会话读取：精确 id 命中或 title 部分匹配唯一者 → `read_session_text` 格式化（🧑 User / 🤖 Assistant / ▶ 工具）注入为 `ChatEntry::System`。**仅展示，不入 agent history**（System 条目被 `history()` 剥离，保持 session 独立）。无参列举所有 session；多匹配提示候选。
- `/sessions new` — 同 `/new`。

**Sessions 面板**（`views/sessions.rs` + `handle_sessions_key`，仿 ProviderForm 直接处理原始 KeyEvent）：

- `SessionsPanelState { selected, pending_delete, list }`：`list` 是进入面板时从 `SessionIndex` 克隆的快照，面板内导航/删除均操作快照。
- ↑/↓ 循环选择、Enter 切换（`switch_session` + 返回 Chat）、n 新建、d 删除（双击确认：首次 `d` 设 `pending_delete`，同项二次 `d` 执行；其它键取消）、Esc 返回（不切换）、q/Ctrl+C 退出。
- 渲染：title + message_count + id（截断）+ 当前 `★` 标记 + 待删除 `[待删除!]` 提示，底部 hint 随待删除态切换。
- 删除拒绝删最后一个（至少保留 1 个）；删 current 自动切到相邻会话并重载 chat。

### 9.9 模型选择面板（`/model` / `/vision model`）

两套入口共用一个双栏面板：Coding CLI（`Panel::ModelPicker` + `CliModelPickerState`，`cli.rs`）与全屏 TUI（`Mode::ModelPicker` + `views/model_picker.rs`）。左栏服务商、右栏该服务商的模型。

**拉取时机（只在左栏选定 provider 后联网）**：联网是显式动作，打开面板与左栏移动都只读本地配置，绝不发请求：

1. **打开面板**（TUI `open_model_picker` / `open_vision_model_picker`、CLI `CliAction::Panel/Picker(kind=Models)`，含 `/model`、`/vision model` 无参）→ 只把 `providers.toml` 里该 provider 的模型清单（`models` map 键 + 当前 `model`）填进右栏；`fetched = false`。
2. **左栏移动**（↑/↓、CLI 的 j/k 与鼠标滚轮）→ 同样只换本地清单，`fetch_id` 不变。
3. **左栏 `Enter`（选定 provider）** → 焦点切到右栏并调用 `cyber_agent::fetch_models`；拉取进行中重复按不重复发起（`fetch_id` 不变）。
4. **`r` 手动重新拉取**（两栏均可按，footer hint 标注）→ 同一路径，用于失败重试或拿到新上架模型。

**状态与展示**：CLI 用 `fetching: Option<String>` + `fetch_error` + `fetched`；未拉取时状态条提示 `当前显示 providers.toml 的本地模型 · 按 Enter 选定 provider 并从接口拉取`，拉取中 `⟳ 正在从接口拉取 [x] 的模型列表…`，失败 `⚠ 模型列表拉取失败：…（显示本地配置）`（失败/未拉取时右栏都保留本地清单）。TUI 用 `fetching`/`fetch_error`/`fetched`：拉取中右栏显示 `⟳ 正在拉取模型列表…`，成功后以接口结果为准。两端结果都经 `mpsc` 回传主循环、以 `fetch_id` + provider 名双重校验丢弃串台结果。右栏 `Enter` 确认并持久化（TUI 写 `default_provider` / vision 配置；CLI `select_model_persisted`），`t` 对选中模型跑一次视觉能力实测。

**长列表渲染（性能）**：右栏每帧只构建**可见窗口**内的行（先按选中项算粘性滚动偏移，再 `scroll..scroll+height` 切片），且能力缓存每帧只 `CapabilityStore::load()` 一次（经 `resolve_vision_capability` 统一解析：显式 `models` 配置 → 实测缓存 → 名称规则表）。此前每行调一次（TUI 两次）`get_model_vision_capability(..., None)`，而 `None` 会退化成每次读盘 + 解析 `capabilities.json`：8000 条模型单帧实测 ≈ 960ms（TUI ≈ 2×），改为窗口化 + 单次读取后同样 8000 条单帧 ≈ 6ms。

---

## 10. 状态管理与事件流

### 10.1 中央 AppState

```rust
pub struct AppState {
    pub config: Arc<Config>,
    pub project: Option<ProjectContext>,    // 来自 .cyber.md
    pub mode: Mode,
    pub chat: ChatState,
    pub workflows: WorkflowRegistry,        // 多工作流实例
    pub providers: ProviderRegistry,
    pub mcps: McpRegistry,
    pub skills: SkillRegistry,
    pub tools: ToolRegistry,
    pub storage: Storage,
    pub events_tx: mpsc::UnboundedSender<AppEvent>,
}
```

### 10.2 事件总线

`tokio::broadcast` 分发事件，UI 订阅重绘：

```rust
pub enum AppEvent {
    UserInput(KeyEvent),
    AgentMessage(Msg),
    ToolCall { id, status, result },
    WorkflowUpdate { id, kind: WfEvent },
    NodeProgress { wf, node, progress },
    NodeLog { wf, node, line },
    McpEvent(McpEvent),
    Error(anyhow::Error),
    Tick,
}
```

UI 不直接调领域逻辑，只发命令；领域层处理后回推事件。保证单线程渲染、多线程执行。

---

## 11. 技术栈

| 用途 | crate |
| --- | --- |
| TUI | ratatui, crossterm |
| 异步运行时 | tokio (full) |
| HTTP / LLM API | reqwest, eventsource-stream（流式） |
| 序列化 | serde, serde_json, toml |
| 存储 | rusqlite (assets/history.db) |
| 日志 | tracing, tracing-subscriber |
| 文件监听 | notify（.cyber.md 热重载） |
| 输入框 | tui-textarea |
| CLI 参数 | clap |
| 错误 | color-eyre, thiserror |
| 路径 | directories |
| 图表 | ratatui + 自绘（趋势/饼图） |
| 节点图布局 | 自实现力导向/层次布局 |

---

## 12. 扩展点

1. **自定义节点**：实现 `NodeExecutor` trait 即可注册新节点类型
2. **自定义 Skill**：丢一个 `SKILL.md` 到 `~/.cyber/skills/` 即生效
3. **自定义 MCP server**：在 `servers.toml` 注册
4. **插件系统（后续）**：`libloading` 动态加载 `.dll/.so/.dylib`
5. **报告模板**：`~/.cyber/reports/templates/` 下 Handlebars 模板

---

## 13. 开发路线图

> 各阶段实时进度（勾选状态 / 完成度 / 子任务）见 [PROGRESS.md](./PROGRESS.md)。

| 阶段 | 内容 | 产出 |
| --- | --- | --- |
| **P1 骨架** | workspace 搭建、配置加载、启动流程、Welcome 页、基础 TUI 主循环 | 能启动、能读配置、能渲染空界面 |
| **P2 Chat** | LLM provider、流式对话、工具调用、斜杠命令、上下文注入 | 对话模式可用 |
| **P3 MCP+Skill** | MCP 客户端、Skill 加载、统一工具表 | MCP/Skill 可在 chat 调用 |
| **P4 Workflow 引擎** | DAG 定义、节点画布编辑、执行器、并行调度 | 可编排可运行工作流 |
| **P5 监控+日志** | Dashboard、节点日志、日志分析、断点续跑 | 实时可观测 |
| **P6 安全工具** | cyber-tools 封装、工具发现、docker 兜底 | 内置工具链可用 |
| **P7 打磨** | 主题、动画、报告导出、文档、CI | 发布 v0.1 |

---

## 14. 关键设计决策（Why）

1. **多 crate workspace**：安全工具/工作流/MCP 边界清晰，未来可独立发布为库
2. **DAG 用流式通道而非批处理**：1 万+ 资产场景下内存可控、可早看到结果
3. **配置三层（全局/项目/.cyber.md）**：兼顾"一次配置到处用"与"项目级隔离"
4. **统一工具表**：内置工具、MCP 工具、Skill 对 agent 是同质的，模型无需区分
5. **护栏在系统提示词 + 运行时双重校验**：仅靠提示词不够，目标白名单/危险命令在代码层兜底
6. **日志走 tracing 而非 println**：结构化、可过滤、可持久化，支撑"日志分析"功能
7. **TUI 节点图自实现布局**：ratatui 无现成节点图，层次布局算法简单可控且渲染开销低

---

## 15. 已确认决策

| # | 决策项 | 结论 |
| --- | --- | --- |
| 1 | LLM Provider | **多家并存**：首批同时适配 OpenAI / Anthropic / Ollama，用户在 config 切换；providers.toml 内置三家默认模板 |
| 2 | 工作流画布交互 | **键盘 + 鼠标拖拽**：鼠标可拖动节点、拉线连接，键盘覆盖全部功能（焦点/快捷键），crossterm 启用 `EnableMouseCapture` |
| 3 | 报告格式 | **四种全做**，优先级 Markdown > HTML > JSON > PDF；Report 节点 `format` 参数可选，PDF 走 HTML→print 路线 |
| 4 | Web Dashboard | **v0.1 暂不做**，TUI 为唯一界面；Web 视图列为后续阶段 |

### 15.1 由决策衍生的实现要点

- **providers.toml** 默认含 `[providers.openai]` `[providers.anthropic]` `[providers.ollama]` 三段模板，`default_provider` 指向其一；`cyber-agent` 抽象 `Provider` trait，三家各一实现。
- **画布层**需自实现：节点坐标系（世界坐标 ↔ 屏幕坐标）、平移/缩放、命中测试、拖拽状态机、连线贝塞尔绘制、鼠标事件路由（crossterm `MouseEvent` → 命中节点/端口/空白）。
- **Report 节点**：内置 Handlebars 模板（md/html/json），PDF 复用 html 模板 + `headless_chrome` 或外部 `wkhtmltopdf` 兜底；模板放 `~/.cyber/reports/templates/`。
- **TUI 唯一界面**：无需内嵌 HTTP server，`cyber-app` 不引入 web 依赖，降低体积与攻击面。
