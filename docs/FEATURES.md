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

主 agent 通过 `delegate_tasks` 一次提交多个互不依赖的任务。每项任务在空历史中启动独立
agent loop，并动态指定 specialist system prompt、任务、上下文、provider/model 与工具白名单。
批次受并发、数量、逐任务超时和步数限制；返回结果始终与输入顺序一致。

子 agent 复用主 agent 的项目规则、CTF 提示、记忆、统一工具注册表和权限代理，但不会继承父
对话历史，也不能再次调用 `delegate_tasks`。`tools = []` 表示纯文本任务，不继承父工具。

### 1.2 工具协议

```json
{
  "tasks": [
    {
      "name": "dependency-review",
      "system_prompt": "You are a dependency security reviewer.",
      "task": "Review the dependency policy.",
      "context": "Optional task-specific context.",
      "provider": "openai",
      "model": "gpt-4o",
      "tools": ["read_file", "find_file"]
    }
  ]
}
```

`name`、`system_prompt`、`task`、`tools` 必填；`context` 默认空字符串；`provider` 和
`model` 可选。未提供 provider/model 时继承父 turn；provider 必须是 `providers.toml`
中已配置的名称，model 仅覆盖所选 provider 的 model。重复 name 合法，系统按输入索引区分；
重复工具名会去重。空批次和超过 `max_tasks` 的批次整次报错；单项字段为空、provider/tool
未知或请求递归委派时，仅该项返回 validation error。

结果格式：

```json
{
  "results": [
    {"name": "a", "status": "completed", "output": "..."},
    {"name": "b", "status": "error", "error": "..."},
    {"name": "c", "status": "timed_out", "error": "Timed out after 300 seconds"}
  ]
}
```

部分成功仍是正常工具结果；全部失败时工具结果标记为 error。子 agent 的内部 token、工具卡和
上下文更新不会写入父对话；父界面只显示普通 `delegate_tasks` 工具卡、批任务进度行和结果。

### 1.3 执行与安全边界

1. `DelegateTasksTool` 使用有界并发执行任务，每项独立应用 timeout，不创建 detached task。
2. 父 turn 取消会 drop 整批 future；已发生的外部工具副作用不能回滚。
3. schema 只暴露白名单工具；执行前再次检查工具名，模型伪造调用也不能触达 registry。
4. 委派工具和每个子工具均经过现有权限代理；session grant 仍按工具名和完整 JSON 参数匹配。
5. 子 agent 与父 agent 共用唯一的 loop 实现，包括自动压缩、JSON 参数检查、循环检测和步数
   耗尽后的无工具总结。
6. 子任务只向父事件通道转发 usage。生命周期通过工具 progress 行显示，不新增专用历史类型或
   管理面板。

适合委派：可并行的代码审查、独立资料分析、互不依赖的检查。简单操作（例如读取一个已知文件）
应直接调用普通工具。

### 1.4 配置

```toml
[agent.subagents]
enabled = true
max_tasks = 8
max_parallel = 4
timeout_secs = 300
max_steps = 25
```

`enabled = false` 时不注册工具，因此 provider schema 和 `search_tools` 均不可见。
`max_tasks`、`max_parallel`、`timeout_secs`、`max_steps` 的 `0` 在运行时按 `1` 处理；
`max_parallel` 还会钳制到有效 `max_tasks`。旧配置缺少本表时使用以上默认值。

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
