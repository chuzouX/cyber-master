# Cyber Master 功能实现文档（v0.2.0 之后新增）

> 本文覆盖自 v0.2.0 发布以来的三大核心功能：**Subagent 系统**、**自定义工具（Custom Tools）**、
> **search_tools 工具**，包含架构设计、实现细节、配置规范与使用方法。

---

## 目录

1. [Subagent 系统](#1-subagent-系统)
2. [自定义工具（Custom Tools）](#2-自定义工具custom-tools)
3. [search_tools 工具](#3-search_tools-工具)

---

## 1. Subagent 系统

### 1.1 概述

Subagent 允许主 agent 把独立子任务委派给一个**完全独立的子 agent** 执行。子 agent 拥有：

- 独立的 agent loop（与主 agent 同构，但独立运转）
- 独立的精简系统提示词
- 按 `subagent_type` 过滤后的工具集
- 独立的对话上下文（完成后仅回传最终摘要，**不污染**主 agent 的对话历史）

### 1.2 架构与文件分布

```
crates/cyber-agent/src/tools/subagent.rs   # SubagentTool：把 run_subagent 包装成 Tool
crates/cyber-agent/src/agent.rs            # run_subagent()：子 agent loop 核心
                                            #   - SUBAGENT_SYSTEM_PROMPT 精简提示词
                                            #   - subagent_stream_with_retry 空响应重试
                                            #   - filtered_tools 工具集过滤
crates/cyber-core/src/config.rs            # SubagentConfig / SubagentType 配置定义
crates/cyber-agent/src/types.rs            # SubagentMessage（内部消息，供 TUI 展示）
crates/cyber-tui/src/views/subagents.rs    # TUI：Subagent 管理面板（Ctrl+F）
crates/cyber-tui/src/chat.rs               # TUI：chat 内 Subagent 条目（折叠/展开）
```

### 1.3 工具 Schema

主 agent 通过调用 `subagent` 工具启动子 agent：

| 参数 | 类型 | 必填 | 默认 | 说明 |
|---|---|---|---|---|
| `description` | string | 是 | - | 子任务简短描述（3-5 词），用于 TUI 列表展示 |
| `task` | string | 是 | - | 子任务详细指令（完整写入子 agent 首条 user 消息） |
| `subagent_type` | string | 否 | `general` | `general`=全部工具；`search`=仅只读工具 |
| `max_steps` | integer | 否 | 100 | 最大工具调用步数（上限受配置钳制） |

### 1.4 核心执行流程（`run_subagent`）

```text
SubagentTool.run(input)
  ├─ 从 ToolCtx 取 provider_config（复用父 agent 的 provider 配置）
  ├─ 发送 AgentEvent::SubagentStart { id, description, subagent_type } → TUI
  └─ run_subagent(provider_cfg, registry, subagent_cfg, type, task, max_steps, ctx)
        ├─ provider_factory(provider_cfg)          # 独立 provider 实例
        ├─ filtered_tools(registry, subagent_type) # 工具集过滤
        ├─ messages = [user(task)]                 # 独立对话上下文
        └─ loop (0..max_steps):
             ├─ subagent_stream_with_retry()       # 空响应退避重试
             ├─ 无 tool_calls → 截断续写 → break（产出最终文本）
             ├─ push assistant(text + tool_calls)
             ├─ 逐个执行工具：
             │    ├─ parse_tool_call_args(call, registry, Some(&turn_text))
             │    │     # 参数抢救：arguments 为空时从回复文本提取满足
             │    │     # required 字段的 JSON（通道层缺陷兜底）
             │    └─ registry.execute(name, input, ctx)
             ├─ push tool 结果消息
             ├─ 参数错误撞墙保护（param_error_threshold）
             ├─ 连续重复调用检测（LoopDetector → loop_detect_threshold）
             └─ 自动压缩（上下文超阈值时触发 compact）
  ├─ Ok((output, messages)) → 发送 AgentEvent::SubagentDone { id, output, messages }
  └─ Err(e) → 返回 is_error 的 ToolOutput
```

关键设计点：

1. **结果隔离**：子 agent 的完整对话历史（`messages: Vec<SubagentMessage>`）只随
   `SubagentDone` 事件进 TUI 详情页；主 agent 收到的 tool result 仅是最终摘要文本。
2. **provider 复用**：子 agent 用 `ToolCtx.provider_config` 克隆父配置，同一模型
   同一网关，无需额外配置。
3. **事件流打通**：`ToolCtx.event_tx + event_gen` 把子 agent 生命周期事件实时推给
   TUI，携带 generation 计数防止 cancel 后的 stale 事件。

### 1.5 工具集过滤（`filtered_tools`）

| SubagentType | 可用工具 |
|---|---|
| `general` | 注册表全部工具（含 MCP / Skill / 自定义工具） |
| `search` | 白名单：`read_file`、`list_dir`、`find_file`、`web_fetch`（纯只读） |

`search` 类型适合纯信息收集任务：无副作用、不消耗写操作风险额度。

### 1.6 子 agent 系统提示词

精简版（区别于主 agent 的完整提示词），核心约束：

- **边做边想**：思考控制在 3-5 行内，立即调用工具或给结论
- **先行动后解释**：不确定的问题用工具验证，不纯推理
- **不过度规划**：先收集信息，再根据结果决定下一步
- **必出结果**：任务失败也要说明原因，禁止空输出

### 1.7 可靠性机制

| 机制 | 参数 | 默认 | 说明 |
|---|---|---|---|
| 空响应重试 | `SUBAGENT_EMPTY_RETRIES` | 3 | 指数退避 2s/4s/8s；429 限流、网关抖动不再直接失败 |
| 截断自动续写 | `MAX_TRUNCATION_CONTINUATIONS` | 3 | `finish_reason=length` 时自动续写，续写文本拼进最终输出 |
| 参数错误撞墙 | `param_error_threshold` | 3 | 同一工具连续参数错误达阈值 → 注入带完整 schema + 示例的警告消息；0=禁用 |
| 死循环检测 | `loop_detect_threshold` | 5 | 连续相同工具调用指纹（name+arguments）达阈值判定死循环；0=禁用 |
| 参数抢救 | - | 常开 | tool call arguments 为空时，从本轮回复文本提取满足 required 字段的 JSON 直接使用 |

### 1.8 配置（`SubagentConfig`）

```toml
# .cyber/config.toml
[agent.subagent]
enabled = true                # 是否启用（默认 false；关闭后 subagent 工具不注册）
max_steps = 100               # 每个 subagent 最大步数
max_parallel = 4              # 并行 subagent 数上限
loop_detect_threshold = 5     # 连续重复调用检测阈值（0=禁用）
param_error_threshold = 3     # 同工具连续参数错误阈值（0=禁用）
```

### 1.9 TUI 展示

**Subagent 管理面板**（Ctrl+F）：

- 三种视图：列表视图 / 任务输入视图（n 新建）/ 详情视图（Enter 查看内部消息）
- 列表展示当前 session 内运行中/已完成的 subagent（描述、类型、状态）
- ↑↓ 选择，Enter 进详情，Esc 返回 Chat 模式

**Chat 内嵌条目**（`ChatEntry::Subagent`）：

- 默认折叠：仅显示 `[subagent] description` + 最终摘要
- Enter 展开：渲染完整内部消息流（assistant 思考 / tool 调用 / tool 结果）
- 条目随 chat 历史持久化（`SubagentMessage` 序列化到 session 历史）

### 1.10 使用规范（给 LLM 的约定，已注入主 agent 系统提示词）

- 可独立完成的子任务（搜索代码库、分析特定文件、研究主题）→ 用 `subagent` 委派
- `description` 保持 3-5 词；详细指令写在 `task` 里
- 纯信息收集用 `subagent_type="search"`
- **不要**为简单操作（读一个已知路径的文件）开 subagent——直接调对应工具
- 多个无依赖 subagent 可在同一轮并行发起

---

## 2. 自定义工具（Custom Tools）

### 2.1 概述

用户无需写 Rust 代码，通过 **TOML 文件**即可把任意 shell 命令包装成 LLM 可调用的工具。
工具带**标签（tags）**体系，与 `search_tools` 联动实现按标签发现。

### 2.2 架构与文件分布

```
crates/cyber-core/src/custom_tool.rs       # CustomToolConfig / CustomToolParam
                                            #   + load_custom_tools() 目录加载器
crates/cyber-agent/src/tools/custom_tool.rs # CustomTool：Tool trait 实现
                                            #   - 占位符替换 substitute_command()
                                            #   - shell 执行 execute_command()（流式）
crates/cyber-tui/src/views/settings.rs     # TUI：Settings 里的自定义工具管理界面
~/.cyber/tools/*.toml                       # 工具定义文件（一文件一工具）
```

### 2.3 工具定义文件规范

**位置**：`~/.cyber/tools/*.toml`（每个 TOML 文件 = 一条工具）

**完整字段规范**：

```toml
# ~/.cyber/tools/nmap_scan.toml

# [必填] 工具名：唯一标识，注册为 custom_<name>。
# 命名规范：小写字母 + 数字 + 下划线（将作为 LLM 调用的工具名，需清晰表意）
name = "nmap_scan"

# [必填] 工具描述：注入 schema，LLM 据此判断是否调用。写清「做什么 + 何时用」。
description = "nmap 端口扫描：对目标执行 TCP 端口扫描。适用于信息收集阶段探测开放端口。"

# [必填] shell 命令：支持 {param_name} 占位符替换（与 parameters[].name 对应）。
command = "nmap {target} -p {ports} -sV"

# [可选] 标签列表：供 search_tools 按标签筛选。推荐使用约定标签（见 2.6）。
tags = ["ctf", "recon"]

# [可选] 参数定义列表
[[parameters]]
name = "target"          # [必填] 参数名 = command 中的 {target} 占位符
description = "目标 IP 或域名"  # [必填] 参数说明（注入 schema 供 LLM 理解）
required = true          # [可选] 是否必填，默认 false

[[parameters]]
name = "ports"
description = "端口范围，如 80、1-1000、top100"
required = false         # 可选参数
default = "top100"       # [可选] 默认值：调用未提供时使用
```

**加载规则**：

| 规则 | 行为 |
|---|---|
| 目录不存在 | 静默跳过（返回空列表，非错误） |
| 单个 TOML 解析失败 / name 空 / command 空 | 记 warn，**跳过该文件**，不阻断其余工具 |
| 非 `.toml` 文件 / 子目录 | 忽略 |
| 工具名冲突 | 后注册覆盖（与所有工具注册表行为一致） |

**校验约束**（`load_one`）：

- `name` 非空、`command` 非空，否则该文件加载失败进 errors 列表
- `tags` / `parameters` 缺省时默认空数组

### 2.4 执行流程

```text
LLM 调用 custom_nmap_scan({"target": "10.0.0.1", "ports": "1-1000"})
  └─ CustomTool.run(input)
       ├─ substitute_command(input)
       │     遍历 parameters：
       │       value = input[name] ?? param.default ?? ""
       │       command.replace("{name}", value)
       │     → "nmap 10.0.0.1 -p 1-1000 -sV"
       └─ execute_command(cmd, progress)
             ├─ Windows: cmd /C <cmd>（raw_arg，支持重定向等 shell 语法）
             │   Unix:    sh -c <cmd>
             ├─ stdout 逐行读 → progress 通道（TUI 实时显示）+ 累积输出
             ├─ stderr 逐行读 → "[stderr] xxx" 前缀累积
             ├─ 超时 300s（CUSTOM_TOOL_TIMEOUT_SECS）→ kill + "[命令超时，已终止]"
             └─ 退出码非 0 → is_error=true（LLM 收到并自行修正）
```

关键设计点：

1. **流式输出**：实现 `run_streaming` trait 方法，长耗时命令（扫描、爆破）的 stdout
   逐行实时推到 TUI，不是憋到结束才出结果。
2. **参数兜底链**：`输入值 → 默认值 → 空串`，可选参数不提供也能跑。
3. **跨平台**：Windows `cmd /C` + `raw_arg`（避免引号转义破坏管道/重定向语法）；
   Unix `sh -c`。
4. **超时保护**：300 秒硬超时，防止失控命令挂死 agent loop。

### 2.5 Schema 生成规则

`CustomTool::schema()` 从配置自动生成 JSON Schema：

- `name` → `custom_<name>`（`custom_` 前缀与内置工具 / `mcp_*` / `skill_*` 命名空间隔离）
- 每个参数 → `properties` 里一个 `string` 类型属性（含 description / default）
- `required=true` 的参数 → schema `required` 数组
- `tags` 原样注入 `ToolSchema.tags`（供 search_tools 筛选）

### 2.6 标签（tags）约定

标签是小写字符串，`search_tools` 按子串匹配（大小写不敏感）。约定俗成：

| 标签 | 语义 |
|---|---|
| `ctf` | CTF 解题相关 |
| `recon` | 信息收集 / 侦察 |
| `web` | Web 安全 |
| `pwn` | 二进制利用 |
| `crypto` | 密码学 |
| `misc` | 杂项 |
| `meta` | 元工具（search_tools 自身） |

自定义标签合法（如 `bluetooth`、`fuzzing`），只要 search_tools 按子串能匹配到即可。

### 2.7 完整示例

**示例 1：无参数工具**

```toml
# ~/.cyber/tools/ifconfig.toml
name = "ifconfig"
description = "查看本机网络接口与 IP 配置"
command = "ipconfig /all"
tags = ["recon"]
```

LLM 调用：`custom_ifconfig({})`

**示例 2：必填 + 可选参数**

```toml
# ~/.cyber/tools/dirsearch.toml
name = "dirsearch"
description = "Web 目录爆破：对目标 URL 进行目录与文件枚举"
command = "python dirsearch.py -u {url} -e {extensions} -t {threads}"
tags = ["ctf", "web", "recon"]

[[parameters]]
name = "url"
description = "目标 URL（含协议）"
required = true

[[parameters]]
name = "extensions"
description = "扩展名字典，如 php,asp,jsp"
required = false
default = "php,html,js"

[[parameters]]
name = "threads"
description = "并发线程数"
required = false
default = "20"
```

LLM 调用：`custom_dirsearch({"url": "http://target.com"})` →
`python dirsearch.py -u http://target.com -e php,html,js -t 20`

**示例 3：多参数全必填**

```toml
# ~/.cyber/tools/hydra_ssh.toml
name = "hydra_ssh"
description = "SSH 密码爆破：用字典对目标 SSH 服务爆破"
command = "hydra -L {userlist} -P {passlist} -t {threads} ssh://{target}"
tags = ["ctf", "brute"]

[[parameters]]
name = "target"
description = "目标 IP 或 host:port"
required = true

[[parameters]]
name = "userlist"
description = "用户名字典路径"
required = true

[[parameters]]
name = "passlist"
description = "密码字典路径"
required = true

[[parameters]]
name = "threads"
description = "并发数（默认 16）"
required = false
default = "16"
```

### 2.8 安全须知

- 自定义工具**绕过内置工具的安全护栏**（scope/rules 不拦截 command 内容），
  仅受 shell 工具同级的 300s 超时约束——定义前确认命令本身安全。
- 参数值直接字符串替换进 shell 命令（无转义）。**不要**把不可信输入直接作为参数
  来源；授权范围内的安全测试场景（CTF / 自有资产）适用。
- 工具文件即代码：只从可信来源导入 `.toml` 工具定义。

---

## 3. search_tools 工具

### 3.1 概述

按**标签**搜索工具注册表，返回匹配工具的名称 + 标签 + 描述。解决的问题：工具数量
增长后（内置 + MCP + Skill + 自定义），LLM 在系统提示词里「盲翻」长工具列表效率低、
易遗漏。标签化检索把「读完整列表」变成「按任务域查」。

### 3.2 文件位置

```
crates/cyber-agent/src/tools/search_tools.rs  # SearchToolsTool
```

### 3.3 工具 Schema

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `tag` | string | 否 | 要搜索的标签。**空串或未提供** → 列出所有带标签的工具 |

工具自身标签：`["meta"]`（元工具）。

### 3.4 匹配规则

```text
tag 提供（非空） → 命中所有 tags 中任一标签「包含」该子串的工具（大小写不敏感）
tag 空 / 缺失    → 列出所有 tags 非空的工具
```

子串包含匹配意味着搜 `recon` 能命中 `["recon"]`，搜 `co` 能命中 `["recon", "crypto"]`。

### 3.5 输出格式

```text
- **custom_nmap_scan** [ctf, recon]: nmap 端口扫描：对目标执行 TCP 端口扫描…
- **custom_dirsearch** [ctf, web, recon]: Web 目录爆破：对目标 URL 进行目录与文件枚举…
```

无命中时返回引导文案（提示通过 `.cyber/tools/*.toml` 定义带标签的自定义工具）。

### 3.6 推荐标签（工具 schema description 中已内置提示）

```
ctf（CTF 相关）、recon（信息收集）、web（Web 安全）、
pwn（二进制）、crypto（密码学）、misc（杂项）
```

### 3.7 典型调用流

```text
LLM: 用户要打 CTF，先看有什么工具
  → search_tools({"tag": "ctf"})
  → 返回 custom_nmap_scan / custom_dirsearch / ctf_challenge ...
  → LLM 按需调用具体工具（工具 schema 已在 tools 定义中给出）
```

### 3.8 与自定义工具的联动

自定义工具的 `tags` 字段是标签生态的主要生产端：

- 用户在 `.cyber/tools/*.toml` 里为命令打标签
- `search_tools` 即时检索（注册表共享 `Arc<ToolRegistry>`，无需刷新）
- MCP / Skill 工具同样可带 tags 参与检索

---

## 附录：三大功能的关系图

```text
                    ┌─────────────────────────────┐
                    │   ToolRegistry（统一工具表）   │
                    ├─────────────────────────────┤
                    │ builtins（shell/read_file…） │
                    │ mcp_<server>_<tool>          │
                    │ skill_<name>                 │
                    │ custom_<name>  ←── 2. 自定义工具（TOML 定义，带 tags）
                    │ subagent       ←── 1. Subagent 系统（本身也是工具）
                    │ search_tools   ←── 3. 按标签检索上面的所有工具
                    └──────────────┬──────────────┘
                                   │
                    ┌──────────────┴──────────────┐
                    │  主 Agent Loop（agent.rs）    │
                    │  LLM 决策 → 工具调用 → 回灌    │
                    └──────────────┬──────────────┘
                                   │ subagent 工具触发
                    ┌──────────────┴──────────────┐
                    │  子 Agent Loop（run_subagent）│
                    │  独立上下文 / 过滤工具集        │
                    │  完成后仅回传摘要              │
                    └─────────────────────────────┘
```

- **Subagent** 是「任务级隔离」：上下文不污染主对话
- **Custom Tools** 是「能力扩展」：零代码加工具
- **search_tools** 是「发现层」：标签化检索解决工具过多后的查找问题
