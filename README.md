# Cyber Master

> 基于 Rust 的网络安全智能体终端：流式对话、CTF 协作与 MCP/Skill 工具集成。面向 CLI 与 TUI，无 Web Dashboard；DAG 工作流编排尚未实现。

`cyber` 默认打开全屏简洁 coding CLI；`cyber tui` 打开原有全屏功能面板；`cyber setup` 配置模型；`cyber run` 执行非交互任务。CLI 与 TUI 共用现有 JSON 会话历史，headless 行为不变。

[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE)
[![Platform](https://img.shields.io/badge/平台-Windows%20%7C%20macOS%20%7C%20Linux-blue)]()

---

## 目录

- [特性](#特性)
- [架构概览](#架构概览)
- [安装](#安装)
- [配置](#配置)
- [用法](#用法)
- [Skill 系统](#skill-系统)
- [CTF 模式](#ctf-模式)
- [Workspace 结构](#workspace-结构)
- [开发路线图](#开发路线图)
- [开发指南](#开发指南)
- [技术栈](#技术栈)
- [作者](#作者)
- [许可证](#许可证)

---

## 特性

- **多 Provider 流式对话**：OpenAI / Anthropic / Ollama / OpenAI 兼容端点，流式输出含思考链（reasoning_content）
- **统一工具体系**：内置工具（shell / read_file / write_file / web_fetch / download_file 等）+ MCP（stdio/HTTP/SSE）+ Skill 渐进式披露
- **Skill 知识库**：100+ 安全测试方法论 Skill，系统提示词自动注入索引，agent 按线索匹配调用
- **CTF 协作面板**：题目注册 / 状态管理 / flag 记录 / writeup 生成，测试优先级引导（信息收集 → Skill → 工具测试 → 脚本）
- **CLI 命令与管理表单**：斜杠/二级补全、Provider 与 memory rule 表单、模型/会话 picker；工作流与 Dashboard 仍为原 TUI 占位页
- **上下文管理**：自动压缩（compact）、历史持久化、跨会话读取、输入历史回溯
- **思考强度切换**：low / middle / high / max / auto 五档，动态注入系统提示词
- **权限与提示约束**：CLI 工具执行前 nonce 审批、MCP 显式连接授权；`.cyber.md` rules 与 memory rules 属于提示词约束，不是 hard guard，内置工具另有运行时护栏
- **多主题**：cyberpunk / catppuccin / tokyo-night / dracula / gruvbox / nord
- **跨平台**：Windows（cmd /C）/ macOS / Linux（sh -c），系统提示词自动注入平台信息

---

## 架构概览

```
┌──────────────────────────────────────────────────────────┐
│  Presentation (ratatui TUI)                              │
│  CodingCLI │ ChatView │ CtfPanel │ Settings │ About      │
├──────────────────────────────────────────────────────────┤
│  Application (模式路由 / 事件分发 / 粘贴检测 / 输入历史)  │
├──────────────────────────────────────────────────────────┤
│  Domain                                                  │
│  Agent(LLM+ToolCall+SSE) │ SessionRunner │ Chat           │
│  SkillRegistry │ McpRegistry │ ToolRegistry              │
├──────────────────────────────────────────────────────────┤
│  Infrastructure                                          │
│  Config │ Storage(JSON) │ Logger(tracing) │ Providers    │
│  FileSystem │ Network(reqwest) │ Process(shell)          │
└──────────────────────────────────────────────────────────┘
```

依赖方向严格单向：`app → tui / agent / workflow → core`，禁止反向依赖。

---

## 安装

### 一行安装（预编译 Release）

Linux / macOS：

```bash
curl -fsSL https://raw.githubusercontent.com/chuzouX/cyber-master/main/install.sh | sh
```

Windows（PowerShell 5.1+）：

```powershell
irm https://raw.githubusercontent.com/chuzouX/cyber-master/main/install.ps1 | iex
```

支持 Linux（glibc）与 macOS 的 x86_64 / aarch64，以及 Windows x86_64；其他系统或架构会拒绝安装。默认下载最新 Release，必须成功下载并通过对应 `.sha256` 校验，否则中止。SHA256 用于检查文件完整性，不替代对发布来源的信任。

Unix 默认安装到 `~/.local/bin/cyber`，Windows 默认安装到 `%USERPROFILE%\.local\bin\cyber.exe`。可用 `CYBER_VERSION` 指定 tag，`CYBER_INSTALL_DIR` 指定安装目录，`CYBER_REPO` 指定仓库；Windows 使用同名 `$env:` 环境变量。

安装器保留已有 shell 配置，幂等添加用户 PATH：Bash 写入 `.bashrc` 和生效的登录 profile，Zsh 写入 `${ZDOTDIR:-$HOME}/.zshrc`，Fish 写入用户配置目录的 `fish/conf.d/cyber-path.fish`，POSIX shell 写入 `.profile`。未知 shell 只给出手动配置提示。**Unix 安装后需重新打开终端**（POSIX shell 需重新登录）；也可立即用完整二进制路径启动。Windows 持久更新用户 PATH，并独立同步当前 PowerShell 会话；其他已打开的终端需重开。

### 从源码构建（需 Rust 1.75+）

```bash
git clone https://github.com/chuzouX/cyber-master.git
cd cyber-master
cargo build --release --locked
```

构建产物在 `target/release/cyber`（或 Windows 下的 `target/release/cyber.exe`）；`cyber-app` 是 Cargo 包名，不是二进制名。

### 首次启动

安装完成后，在项目目录运行 `cyber`。缺少可用配置时，首次交互启动会进入向导：选择 Provider、服务地址、凭据和模型，确认后保存到用户主目录的 `.cyber/`，随后进入对话。已有有效配置的用户无需重复设置。`cyber tui` 同样会先检查配置。

用 `cyber setup` 可主动重跑向导。凭据可引用环境变量或隐藏输入；直接输入的密钥保存在限制访问权限的全局 `providers.toml` 中。向导不启动 MCP、不测试网络连接；取消不会保存用户输入。`setup.toml` 单独记录完成状态，状态不会绕过配置有效性检查。`--mock` 无需真实模型凭据。

全局路径默认是用户主目录下的 `.cyber/`，不是系统的 XDG / AppData 配置目录。可通过 `CYBER_HOME` 指定完整数据目录，用于便携安装或隔离运行。主要路径如下：

```
~/.cyber/
├── config.toml          # 全局配置（主题、模式、agent 参数）
├── providers.toml       # LLM 提供商配置
├── setup.toml           # 配置向导完成状态
├── mcp/servers.toml     # MCP server 配置
├── tools/               # 自定义工具 TOML
├── skills/              # Skill 目录（.md 文件）
├── ctf/                 # CTF 题目数据
├── history/             # 按 cwd hash / session ID 保存 JSON 历史
├── sessions/            # 会话存储
└── logs/                # 日志
```

---

## 配置

### 三层配置层次

| 层级 | 路径 | 作用 |
| --- | --- | --- |
| 全局（用户级） | `~/.cyber/config.toml` | 主题、默认模式、agent 参数等 |
| 项目级覆盖 | `./.cyber/config.toml` | 覆盖全局设置 |
| 项目说明 | `./.cyber.md` | YAML frontmatter（scope/rules）注入系统提示词；正文暂不自动注入 |

### `config.toml` 示例

```toml
[ui]
theme = "cyberpunk"          # catppuccin | tokyo-night | dracula | gruvbox | nord | cyberpunk
default_mode = "chat"        # chat | workflow | dashboard
animations = true
mouse = true
frame_rate = 60

[agent]
default_provider = "openai"  # 见 providers.toml
auto_tool_call = true
max_steps = 25               # agent loop 最大步数

[agent.subagents]
enabled = true               # false 时不注册 delegate_tasks
max_tasks = 8                # 单次批量任务上限
max_parallel = 4             # 子任务并发上限
timeout_secs = 300           # 每项任务超时
max_steps = 25               # 每个子 agent 的 loop 步数上限

[workflow]
max_parallel_nodes = 8
default_timeout_secs = 1800
checkpoint = true            # 启用断点续跑

[tools]
prefer_docker = false        # 工具缺失时是否用 docker 镜像兜底
extra_path = []              # 额外工具路径

[storage]
history_retention_days = 90
log_level = "info"
```

### 自定义工具

自定义工具通过 TOML 文件把安全、可重复的 shell 命令注册为 LLM 可调用工具。启动时扫描以下目录的顶层 `.toml` 文件：

- 全局工具：`~/.cyber/tools/`
- 项目工具：当前项目的 `.cyber/tools/`（若该目录存在）

每个 TOML 文件定义一个工具。`name` 和 `command` 为必填项；`description`、`tags` 和 `parameters` 可选，但建议填写清晰的 `description`；解析失败、名称为空或命令为空的文件会被跳过，并在启动提示中报告错误。

#### TOML 语法

```toml
# ~/.cyber/tools/ifconfig.toml
name = "ifconfig"                              # 必填：小写字母、数字和下划线，建议唯一且语义清晰
description = "查看本机网络接口配置"             # 必填：告诉模型工具做什么、何时使用
command = "ipconfig /all"                       # 必填：在 shell 中执行的命令
tags = ["recon", "network"]                    # 可选：供 search_tools 按标签发现

[[parameters]]
name = "target"                                 # 在 command 中对应 {target}
description = "目标主机或 URL"                   # 参数说明
required = true                                  # 可选，默认 false
default = ""                                     # 可选；未提供参数时使用
```

参数使用 `[[parameters]]` 数组定义，并通过 `{参数名}`占位符写入 `command`。调用时工具名称自动加上 `custom_` 前缀，例如上例注册为 `custom_ifconfig`。参数值按“调用参数 → default → 空字符串”的顺序替换；因此可选参数可以省略。

一个带默认值的完整例子：

```toml
name = "dirsearch"
description = "对目标 URL 执行目录枚举"
command = "python dirsearch.py -u {url} -e {extensions} -t {threads}"
tags = ["web", "recon"]

[[parameters]]
name = "url"
description = "目标 URL"
required = true

[[parameters]]
name = "extensions"
description = "扩展名列表"
default = "php,html,js"

[[parameters]]
name = "threads"
description = "并发线程数"
default = "20"
```

保存文件后重启 Cyber Master，或从设置栏的 Custom Tools 面板确认是否加载。可用 `/tools` 查看已注册工具；带标签的工具也可通过 `search_tools` 发现。

#### 安全与命名规范

- 只加载可信来源的 TOML；`command` 会直接交给 Windows `cmd /C` 或 Unix `sh -c` 执行。
- 参数值是字符串替换，不会自动转义或 Shell quoting；不要把不可信输入直接拼入命令。
- 自定义工具不继承内置工具的 scope/rules 命令拦截，仅有 300 秒执行超时，因此请自行限制命令权限和输入范围。
- 建议 `name` 使用 `^[a-z0-9_]+$`，避免与其他工具重名；注册后的完整名称为 `custom_<name>`。
- `tags` 使用短的小写标签，例如 `ctf`、`web`、`recon`、`pwn`、`crypto`、`misc`；自定义标签也可以使用。

更多执行流程、Schema 规则和排错说明见 [`docs/FEATURES.md`](docs/FEATURES.md) 的“自定义工具”章节。
### Provider 配置

编辑 `~/.cyber/providers.toml`：

```toml
[providers.deepseek]
kind = "openai"
base_url = "https://api.deepseek.com/v1"
api_key = "${DEEPSEEK_API_KEY}"
model = "deepseek-chat"
max_tokens = 4096
temperature = 0.7
```

支持 `kind`：`openai` / `anthropic` / `ollama` / `openai-compatible`

环境变量替换：`api_key = "${VAR_NAME}"` 自动读取环境变量。

### `.cyber.md` 项目说明

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

`rules` 字段会注入 agent 系统提示词作为行为约束，不是代码层目标白名单或 hard guard。

---

## 用法

### CLI 启动

在任意项目目录启动，无需先创建 `.cyber.md`：

| 命令 | 用途 |
| --- | --- |
| `cyber` | 全屏简洁 coding 界面，持续对话、流式输出 |
| `cyber tui` | 原全屏 Ratatui 面板，含 CTF、设置及工作流/Dashboard 占位页 |
| `cyber setup` | 运行或重跑配置向导 |
| `cyber run "任务"` | 单次非交互任务，适用于脚本或外部 agent |

当前可用示例：

```bash
cyber                                      # 默认交互 CLI
cyber tui                                  # 全屏 TUI
cyber --mock                               # 离线交互 CLI
cyber tui --mock                           # 离线 TUI
cyber setup                                # 配置模型
cyber run "解释这个概念"                    # 非交互，流式文本输出
cyber run "分析当前项目" --format json      # 结构化输出
cyber run "继续上次任务" --session abc      # 续接指定会话
cyber run --help                            # 查看 provider/model 等任务选项
```

CLI 由 `cli_commands.rs` 处理 TUI 目录中除 `/mode` 外的 19 个主命令，并增加 `/effort`，共 20 项，完整行为与子命令边界见 [命令参考](docs/TUI_COMMANDS.md)。`/effort low|medium|high|xhigh|auto` 保留 `middle` / `max` 别名，`medium` 对应内部 `Middle`，`xhigh` 对应 `Max`；`/think` 与 `/effort` 只改变系统提示词档位，不新增 provider API 的 `reasoning_effort` 参数。`/think`、`/max_steps`、`/subagents`、`/env` 和 `/web` 保存到全局配置的目标字段，不把合并后的项目覆盖整份写入全局；项目覆盖在重新加载时仍优先。`--cwd` 会验证并规范化目录；项目配置仍只在指定目录查找，不向父目录继承。

### CLI 界面与快捷键

默认 CLI 为全屏简洁 coding 界面，视觉参考 [Oh My Pi (OMP)](https://github.com/can1357/oh-my-pi)，不拷贝其源码，也不仿造未实现的 agents/LSP 界面。实际 palette 使用暖金、灰白、cyan 与紫色；header 显示彩色 ASCII `Cy`、`Cyber Master V<版本>`、model/effort 和 cwd。中间为可滚动对话区，底部为两行圆角输入区、金色无框补全候选及实际状态栏：

```text
provider · model │ ctx 剩余% │ cache 命中率 │ ↑input ↓output
```

`ctx` 根据当前上下文 token 估算与有效上下文容量计算剩余百分比；容量未知时显示 `--`。`cache` 为已报告的命中 token /（命中 + 未命中 token）；分母为零时显示 `--`。`↑input ↓output` 累计 provider 实际上报的 Usage，未上报时显示 `--`，不以估算代替 Usage。累计仅属于本进程内当前会话，新建或切换会话时重置，重开进程不从历史恢复；切换 provider/model 不重置累计。底栏不显示审批模式或 agent 面板入口，空输入 `Left` 不打开面板。

正文和真实 `reasoning_content` 复用现有 Markdown 子集渲染，支持标题、围栏代码、行内样式、链接、列表、引用、分隔线及数学文本标记；不支持表格，也不宣称完整 CommonMark。仅收到实际 reasoning 内容时显示斜体 `Thinking`，不生成或伪造思考文本。工具调用使用紧凑状态背景块，`Ctrl+O` 展开/折叠详情，工具数据不冒充令牌 Usage。交错 reasoning/正文与空 Token 不重复生成 `Cyber` 标题，旧历史中的空 Assistant 条目仍可兼容读取。每轮结束持久化耗时、结束时间和状态，重开后显示如 `Worked for 3s · done HH:mm`，失败或取消对应 `error` / `cancelled`；旧历史没有此记录时不补造。

| 按键 | 说明 |
| --- | --- |
| `?`（空输入） / `/help` | 分别打开实际 shortcuts / 显示命令目录 |
| `/`、`Tab`、`Up/Down` | 主命令及二级补全，选择候选 |
| `Enter` | 有未接受候选时先补全，再 Enter 执行；普通输入直接提交 |
| `Esc` | 关闭候选时保留输入；表单取消不保存；任务中可取消 |
| `Ctrl+O` | 展开/折叠工具详情 |
| `Alt+Enter` / `Shift+Enter` | 换行 |
| `PgUp` / `PgDown` | 滚动对话历史 |
| `Ctrl+C` | 任务中取消；当前空闲且空输入时也可退出 |
| `Ctrl+D`（空输入） | 退出 |

执行前审批仍须输入当前请求的 nonce（`once <请求码>` 或 `session <请求码>`）后显式提交；paste 不得自动提交或确认授权，预输入内容也不得误批准。这是工具权限流程，不是可切换的 mode；底栏精简不改变下述权限策略或 headless 行为。

**权限与当前限制：** CLI 在工具执行前询问；输入提示中的 `once <请求码>` 或 `session <请求码>` 才能授权，避免粘贴或预输入内容误批准。自动审批（Auto）模式已放行只读与安全探测命令（如 `cat`、`grep`、`cargo`、`git` 等）；会话授权（Session）对同一工具及已被授权的主干命令集合（如 `cargo` 等）或只读命令后续放行，避免仅因调整参数而反复弹窗。破坏性操作（如写入重定向、文件修改、未授权命令）仍要求确认。`cyber run` 不询问、默认拒绝所有工具；可重复传入 `--allow-tool <名称>` 显式授权指定工具，例如 `cyber run "列出文件" --allow-tool list_dir`。该授权允许目标工具的任意参数，不支持通配符，不绕过内置护栏；谨慎授权 `shell` 等工具。权限拒绝和任务错误均以非零退出码结束，JSON 包含失败结果。CLI 启动时默认按配置连接已配置的 MCP servers（若配置或网络异常则跳过并提示）。这不是系统级沙箱，批准操作后仍可能产生不可撤销的副作用。

`cyber` / `cyber tui` 要求交互终端；脚本使用 `cyber run`。非交互命令不弹出向导，配置缺失时提示 `cyber setup`。当前全局选项：

```bash
cyber [OPTIONS] [COMMAND]

Options:
      --cwd <CWD>          工作目录（默认当前目录，决定 .cyber.md 检测位置）
      --log-level <LEVEL>  日志级别（覆盖 RUST_LOG，如 debug/info/warn）
      --mock               离线 Mock 模式（CLI / TUI / run）
  -h, --help               帮助
  -V, --version            版本
```

### 斜杠命令

以下命令由 CLI 与原 TUI 各自的 handler 处理；CLI 不支持 `/mode`，另有 `/effort`。完整子命令、实际语法与前端差异见 [命令参考](docs/TUI_COMMANDS.md)，不能把目录或二级建议当作所有参数组合均已实现。

| 命令 | 说明 |
| --- | --- |
| `/help` | 显示帮助 |
| `/clear` | 清空对话历史 |
| `/model [provider]` | 选择 provider + model |
| `/provider <list\|add\|edit\|use\|remove>` | 管理服务商 |
| `/subagents [status\|enable\|disable\|max_tasks N\|max_parallel N\|timeout N\|max_steps N]` | 查看或持久化批量子 agent 配置；启用状态变更需重启更新工具目录 |
| `/env [list\|set KEY VALUE\|set-sensitive KEY VALUE\|remove KEY]` | 管理工具子进程环境变量；敏感值在列表、补全和 TUI 历史中不明文显示 |
| `/web [status\|on\|off\|enable\|disable]` | 开启或禁用联网搜索与网页抓取（web_fetch）；实时生效并持久化 |
| `/tools` | 列出可用工具 |
| `/skill <name\|list>` | 查看 Skill 详细说明 |
| `/mcp <list\|status\|connect>` | 查询 MCP 配置与连接状态，或重连配置的 MCP servers |
| `/think [low\|middle\|high\|max\|auto]` | 切换思考强度 |
| `/max_steps <N>` | 工具调用步数上限 |
| `/compact [instructions]` | 手动压缩上下文 |
| `/ctf [status\|enable\|disable\|add name category\|list\|writeup name]` | 查看开关、添加/列出题目，为已解出题目生成报告；CLI 分类必填，原 TUI 可省略 |
| `/sessions <list\|read\|new\|delete ID>` | CLI picker、读取、新建、删除及 ID 切换；原 TUI 通过面板删除/切换 |
| `/mode <chat\|workflow\|dashboard>` | 切换视图；workflow/dashboard 为占位页 |
| `/memory [list\|add\|project\|edit\|delete\|rule]` | 记忆 CRUD；CLI rule 表单及 list/edit/delete，原 TUI rule 尚无 handler |
| `/effort [low\|medium\|high\|xhigh\|auto]` | CLI `/think` 别名，保留 middle/max |
| `/new` | 新建会话 |
| `/cancel` | 取消当前生成 |
| `/quit` | 退出 |

`/skill <name>` 和 `/sessions read <id|关键词>` 仅在 UI 展示 System 条目，不把说明正文或跨会话内容注入模型历史；模型获取 Skill 正文需调用 `skill_<name>` 工具。

CLI Provider 表单支持 add/edit/use/remove，API key 掩码显示，取消不写入，凭据文件私有持久化；`/model` 打开已配置模型 picker，不宣称自动联网拉取。`/clear` 清空并保存当前历史，`/cancel` 取消实际任务，`/compact [instructions]` 发起真实模型摘要任务，成功后持久化压缩结果。Memory rule 表单的 `enabled`、`scope`（global/project/both）和 `prompt` 控制后续系统提示词约束，不是 hard guard。

### TUI 快捷键

| 按键 | 说明 |
| --- | --- |
| `Enter` | 发送消息 |
| `Shift+Enter` | 换行 |
| `Ctrl+L` | 日志查看器 |
| `Ctrl+T` | CTF 面板 |
| `Ctrl+O` | 展开/折叠最近条目 |
| `F9` | 切换鼠标捕获 / 选区模式 |
| `s` | 进入设置 |
| `Esc` | 返回 |

### TUI 粘贴检测

- 支持 bracketed paste（主机制）
- 回退：基于按键时间间隔检测粘贴（30ms 阈值），粘贴中的 Enter 转为换行符，不触发提交

---

## Skill 系统

Skill 是经过实战验证的安全测试方法论，以 `.md` 文件存储在 `~/.cyber/skills/`。

- **渐进式披露**：每个 Skill 暴露为 `skill_<name>` 工具，调用后返回详细使用说明
- **系统提示词注入**：所有 Skill 的名称和简介自动注入系统提示词，agent 可一眼扫描匹配
- **CTF 优先级**：信息收集 → Skill 知识库 → 工具测试 → 脚本/爆破（严禁跳级）
- **子 Skill 路由**：部分 Skill（如 sqli）支持子 Skill 结构，按场景路由

---

## CTF 模式

```
/ctf enable
```

原 TUI 开启后的面板行为（默认 CLI 不提供此题目面板）：
- 系统提示词注入 CTF 测试方法论（优先级 + 工具使用规范）
- `ctf_challenge` 工具自动注册/更新题目状态
- 题目面板实时显示进度
- 解出后记录 flag 和关键知识点
- `/ctf status`（或无参数 `/ctf`）查看开关状态；原 TUI `/ctf add <name> [misc|web|reverse|pwn|crypto]` 添加题目，`/ctf list` 列出题目
- `/ctf writeup <name>` 为名称精确匹配且已解出的题目生成解题报告，不能省略题目名

CLI `/ctf add <name> <category>` 要求显式合法分类，区别于原 TUI 的可选分类/回退行为。CLI writeup 成功后保存到隔离的项目路径 `.cyber/ctf/sessions/<sessionid>/<challengeid>/<category>/<name>/writeup.md`；原 TUI 路径仍为 `.cyber/ctf/<分类>/<题目>/writeup.md`。已解状态是生成前置条件，任务失败或取消不发布成功报告。

### 当前验收边界

已完成真实 Windows ConPTY 119 个断言（47 个本地 SSE + 72 个命令交互），覆盖 120x30 / 80x12、Unicode 光标、密钥掩码与取消、17 项 CLI 目录、compact/cancel/session 持久化及 memory rule。MCP 单测与 5 个真实子进程测试通过，覆盖 lifecycle 修复及 cancel/shutdown；UI 连接 deny 不会 start。以上不代表完整 Provider 外网联调，也不代表已解 CTF writeup 的真实 API 全链路联调；未覆盖的多子命令组合边界见命令参考。

---

## Workspace 结构

```
cyber_master/
├── Cargo.toml                 # workspace 根
├── crates/
│   ├── cyber-app/             # 主二进制 main.rs，装配 + 事件循环
│   ├── cyber-core/            # 配置、路径、错误类型、项目上下文、CTF 类型
│   ├── cyber-tui/             # ratatui UI（布局/组件/主题/事件循环/粘贴检测）
│   ├── cyber-agent/           # LLM provider、SSE 解析、系统提示词、agent loop、工具
│   ├── cyber-workflow/        # DAG 引擎、节点定义、执行器、调度
│   ├── cyber-mcp/             # MCP 客户端（stdio/HTTP/SSE）
│   ├── cyber-skills/          # Skill 加载、frontmatter 解析与工具封装
│   ├── cyber-tools/           # 工具共享类型
│   └── cyber-storage/         # 存储抽象
└── assets/
    └── skills/                # 内置 Skill 资源
```

---

## 开发路线图

| 模块 | 内容 | 状态 |
| --- | --- | :---: |
| 配置 + 启动 | 三层配置、路径管理、`.cyber.md` frontmatter、首次启动引导 | ✅ 完成 |
| TUI 框架 | ratatui 布局、6 主题、事件循环、Welcome/Settings/About 页 | ✅ 完成 |
| Chat 对话 | 流式输出、思考链、粘贴检测、输入历史、滚动缓存 | ✅ 完成 |
| Agent + Provider | OpenAI/Anthropic/Ollama/OpenAI-compatible、SSE 解析、HTTP 状态码检查 | ✅ 完成 |
| 内置工具 | shell/read_file/write_file/find_file/list_dir/web_fetch/download_file/ctf_challenge | ✅ 完成 |
| Skill 系统 | frontmatter 解析、注册表、渐进式披露、系统提示词索引注入 | ✅ 完成 |
| MCP 客户端 | stdio/HTTP/SSE 三种传输、工具注册、连接管理 | ✅ 完成 |
| CTF 模式 | 题目注册/状态管理/flag 记录/面板/writeup、测试优先级引导 | ✅ 完成 |
| 上下文管理 | 自动压缩（compact）、历史持久化、跨会话读取 | ✅ 完成 |
| 思考强度 | low/middle/high/max/auto 五档动态注入 | ✅ 完成 |
| Workflow DAG | 节点编排、并行执行、流式资产传递 | ⚪ 待实现 |
| 存储层 | SQLite 资产/漏洞/日志持久化 | ⚪ 待实现 |
| 安全工具封装 | subfinder/nmap/nuclei 等工具集成、docker 兜底 | ⚪ 待实现 |
| Dashboard | 实时监控、节点日志分析 | ⚪ 待实现 |

---

## 开发指南

### 构建

```bash
cargo build                # debug
cargo build --release      # release（LTO + strip）
```

### 测试

```bash
cargo test --workspace
```

### Lint

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

### 添加新 LLM Provider

1. 在 `crates/cyber-agent/src/` 新增 `<provider>.rs` 实现 `Provider` trait
2. 在 `crates/cyber-core/assets/default_providers.toml` 增加默认模板
3. 在 `crates/cyber-agent/src/provider.rs` 的 `provider_factory` 注册匹配分支

### 添加新 TUI 视图

1. 在 `crates/cyber-tui/src/views/` 新增 `<view>.rs`
2. 在 `Mode` 枚举追加变体
3. 在 `event.rs` 补充键位映射
4. 在主 render 分发

---

## 技术栈

| 领域 | 选型 |
| --- | --- |
| 语言 | Rust（edition 2021） |
| TUI | ratatui 0.30 + crossterm 0.28（event-stream） |
| 多行输入 | tui-textarea-2 0.12（crossterm_0_28 feature） |
| 异步 | tokio（full） |
| HTTP / 流式 | reqwest 0.12（rustls-tls + stream） + futures + bytes |
| 序列化 | serde + serde_json + serde_yaml + toml 0.8 |
| 错误 | thiserror + color-eyre + anyhow |
| 日志 | tracing + tracing-subscriber（env-filter） |
| CLI | clap 4（derive） |
| 配置路径 | dirs 5 |

---

## 作者

- **chuzouX**
- 博客：https://chuzoux.top/
- 主页：https://space.chuzoux.top/
- GitHub：https://github.com/chuzouX

---

## 许可证

[MIT](./LICENSE)
