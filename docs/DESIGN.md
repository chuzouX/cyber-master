# Cyber Master — 网络安全智能体终端 设计文档

> Rust + ratatui 构建的双模式（对话 / 工作流）安全智能体 CLI 终端，支持 MCP、Skill、实时监控与日志分析。

> 📌 **实施进度**：各阶段的实时完成情况见 [PROGRESS.md](./PROGRESS.md)（每完成一项即更新勾选与说明）。本文件为静态设计依据，路线图见 [§13](#13-开发路线图)。

---

## 0. 设计目标

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

依赖方向：`app → tui / agent / workflow → core → storage`，禁止反向依赖。

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
├── history.db               # SQLite：聊天/命令历史
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

YAML frontmatter 提供结构化字段（scope/authorization/rules），正文提供自由描述。frontmatter 中的 `rules` 会注入到 agent 系统提示词，作为安全护栏。

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
- **斜杠命令**：`/help /workflow /skill /mcp /model /clear /save /load /report /targets /scan /dashboard`
- **项目上下文感知**：`.cyber.md` 自动注入；`@file` 引用文件；`/add` 添加目录到上下文
- **文件编辑 diff**：agent 修改文件时以 diff 块展示，确认后落盘
- **多行输入**：`tui-textarea`，Shift+Enter 换行，Enter 发送
- **历史导航**：↑/↓ 翻阅历史
- **Agent Loop**：支持自主多步工具调用（受 `max_steps` 与 `rules` 护栏约束）
- **Chat ↔ Workflow 联动**：对话中可一句"把这些资产跑漏扫"自动生成并启动工作流

### 3.3 安全护栏

- frontmatter `rules` 注入系统提示词
- 危险命令（rm、 DoS 类、未授权目标）二次确认
- 操作目标白名单校验（不在 scope 的目标拒绝执行）

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

### 6.1 传输层

| 传输 | 用途 |
| --- | --- |
| stdio | 本地 MCP server（子进程） |
| SSE | 远程 server（旧规范） |
| Streamable HTTP | 远程 server（新规范） |

### 6.2 能力

- 启动时按 `mcp/servers.toml` 拉起/连接所有 server，健康检查
- 工具发现 + schema 缓存（避免每次拉取）
- 支持 `tools/list` `tools/call` `resources/*` `prompts/*`
- **统一工具表**：MCP 工具与内置工具、Skill 同等暴露给 agent 与工作流节点
- 工作流中用 `mcp-tool` 节点调用任意 MCP 工具

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

---

## 7. Skill 支持

### 7.1 Skill 结构

```
~/.cyber/skills/src-recon/
├── SKILL.md          # 名称/描述/触发条件/使用说明（渐进式披露）
└── scripts/          # 可选脚本
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

- **Chat**：`/skill src-recon` 或 agent 自动按 `triggers` 匹配加载
- **Workflow**：`skill` 节点，把 Skill 作为一个可复用子流程
- **渐进式披露**：模型先读 frontmatter，需要时再读正文，节省上下文

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
| Enter | 确认 / 进入（Settings 内：编辑字段 / 保存设置） |
| ←/→ | Settings 内调整字段值（bool 切换 / enum 循环 / number ±step） |
| ↑/↓ | 导航（Welcome 选项 / Settings 字段行） |

### 9.3 主题与动画

- 内置主题：catppuccin / tokyo-night / dracula / gruvbox / nord / cyberpunk（赛博朋克：深紫黑底 + 霓虹粉/青，灵感来自 ratatui.rs "Built with Ratatui" 项目群）
- 动画：spinner、进度条、节点状态脉冲、连接线流动、视图淡入；可用 `animations=false` 关闭
- 帧率：默认 60fps，CPU 敏感场景降到 30fps

### 9.4 设置页（Settings 模态层）

P1 阶段实现的配置查看 / 编辑 / 持久化入口。用 `Mode::Settings` 模拟模态层：全局 `s` 或 Welcome 第 4 项进入，`Esc` 返回 `prev_mode`。

**布局**：左侧段侧边栏（6 段）+ 右侧字段表。Providers 段只读（编辑 `~/.cyber/providers.toml`），其余 5 段可编辑。

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
| Providers | （整段只读） | — | 编辑 `~/.cyber/providers.toml` |

**安全机制**：

- **Esc 双击回退**：有未保存改动时，首次 `Esc` 提示「再按 Esc 丢弃」并置 `pending_discard`；二次 `Esc` 用进入时的快照 `config_at_entry` 回滚 `config` 并 live-apply 复位主题/鼠标，再返回 `prev_mode`。无改动时单次 `Esc` 直接返回。
- **dirty 标记**：标题 `Settings *` + 保存行 `保存设置 *` + 底部提示行三处可见。
- **项目级覆盖警告**：检测到 `.cyber/config.toml` 时顶部横幅提示「保存仅写全局，被覆盖字段重启后回弹」（deep-merge 在加载时仍会把项目级覆盖叠加上去）。
- **已知限制**：toml crate 不保行内注释；项目级覆盖字段保存后重启仍被覆盖。

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
