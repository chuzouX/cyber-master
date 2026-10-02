# CLI / TUI 命令参考

本页区分默认 `cyber` coding CLI 与 `cyber tui` 原 Chat 面板。CLI 依据 [cli_commands.rs](../crates/cyber-tui/src/cli_commands.rs) 的目录、补全和执行 handler，以及 [cli.rs](../crates/cyber-tui/src/cli.rs) / [headless.rs](../crates/cyber-tui/src/headless.rs) 的 UI 和任务处理核对；原 TUI 依据 [slash.rs](../crates/cyber-tui/src/slash.rs) / [app.rs](../crates/cyber-tui/src/app.rs)。目录、补全或帮助列出某参数，不等于所有参数组合均已实现。

CLI 支持原 TUI 17 个主命令中除 `/mode` 外的 16 项，加上 `/effort` 共 17 项：`/help`、`/clear`、`/model`、`/provider`、`/tools`、`/skill`、`/mcp`、`/cancel`、`/compact`、`/ctf`、`/max_steps`、`/think`、`/new`、`/sessions`、`/memory`、`/quit`、`/effort`。以下先列 CLI 已实现行为，再列原 TUI 的差异参考。

## CLI 输入与交互

- `/` 打开完整主命令目录，按前缀过滤；支持子命令及已配置 provider/model、session ID、Skill 名称的二级建议，不是 shell 自动补全。
- `Up/Down` 选择候选，`Tab` 接受补全；`Enter` 在有未接受候选时先补全，再按 Enter 执行。接受补全后仍可继续输入参数。`Esc` 关闭候选并保留原输入，不清空命令。
- 主命令及实现支持的子命令大小写不敏感；provider/题目等名称按实际数据匹配。按空白切分参数，不解析 shell 引号，不支持一行执行多个命令。
- 表单直接编辑当前字段，以 Tab/Shift+Tab 切换，Enter 到下一字段、末字段 Enter 或 Ctrl+S 保存，Esc 取消；picker 用方向键选择、Enter 确认、Esc 返回。普通任务、summary/writeup、连接授权共用实际取消流程，不是在 UI 假装完成。
- 生成中命令受 UI 状态限制，须先取消再进行配置/会话操作；不能据静态目录推断任何任务状态下均可执行。

## CLI 已实现行为

| 命令/语法 | 实际行为与边界 |
| --- | --- |
| `/help` | 显示实际 CLI 目录；空输入 `?` 打开 shortcuts。 |
| `/clear` | 清空并保存当前会话历史、重置 Usage，不删除全部会话。 |
| `/cancel` | 取消当前 agent、compact、writeup 或连接任务；隔离旧事件并保存实际结果。任务中 Ctrl+C 同样 cancel；当前空闲且空输入时 Ctrl+C 会退出。 |
| `/quit` / 空输入 Ctrl+D | 保存会话并退出。 |
| `/model [provider [model]]` | 无参数打开已配置 provider/model picker；带参数选择并持久化，不自动联网发现模型。 |
| `/provider [list\|add\|edit name\|use name\|remove name]` | list 隐藏 endpoint/凭据；add/edit 打开表单，use 持久化选择，remove 直接删除并处理默认项回退，无二次删除确认。 |
| `/tools` | 查询实际注册工具 schema，不执行工具，不把工具数据算成令牌 Usage。 |
| `/skill [list\|name]` | 查询目录或显示正文；通知不入模型历史，模型须调用 `skill_<name>` 获取正文。 |
| `/mcp [list\|status]` | 展示配置 server、transport 与 connected/not connected；查询不会连接。 |
| `/mcp connect` | 默认 no autostart，显式任务经当前 nonce 批准才连接配置 server；deny 不启动。已有连接时拒绝重复 connect，配置变更需重启；没有按 server 名连接/断连/重连子命令。 |
| `/compact [instructions]` | 真实模型摘要任务，成功后替换模型历史并持久化；空历史拒绝，失败/取消不提交排队摘要，不等于清屏。 |
| `/think [low\|middle\|high\|max\|auto]` | 查看/保存现有思考档位，沿用系统提示词，不新增 API `reasoning_effort`，不保证返回 reasoning。 |
| `/effort [low\|medium\|high\|xhigh\|auto]` | `/think` 别名；medium→Middle、xhigh→Max，仍接受 middle/max。 |
| `/max_steps [N]` | 查询或保存 1-1000 的工具调用步数上限。 |
| `/new` / `/sessions new` | 保存当前，创建并切到新会话；重置本进程 Usage。 |
| `/sessions` / `/sessions list` | 打开同 cwd session picker；Enter 切换，n 新建，d 双击确认删除，Esc 返回。 |
| `/sessions ID` | 按 ID 切换会话，区别于原 TUI 面板语法。 |
| `/sessions read [ID\|关键词]` | 无参数列出会话；唯一匹配时展示内容，多个匹配列候选，不切换、不注入模型历史。 |
| `/sessions delete ID` | 按 ID 删除，至少保留一个会话；删除当前会话时切换到剩余项，保存 index/entries。 |
| `/ctf [status\|enable\|disable]` | 查询/切换当前运行 CTF 开关；不提供 slash 修改解题状态。 |
| `/ctf add name category` | 添加当前会话题目，必须显式提供 misc/web/reverse/pwn/crypto；非法分类、路径组件、重名拒绝，不采用原 TUI 的 misc 回退。 |
| `/ctf list` | 列出当前会话题目、分类和解题状态。 |
| `/ctf writeup name` | 精确匹配已有已解题目，发起模型生成任务；需要 ctf-writeup Skill，成功后保存隔离报告，失败/取消不发布。 |
| `/memory` / `/memory list` | 查询全局/项目记忆，各 scope 编号从 1 开始。 |
| `/memory add text` / `/memory project text` | 追加全局/项目 memory.md，后续系统提示词读取。 |
| `/memory edit scope index text` | 修改指定编号内容，scope 必须 global/project。 |
| `/memory delete scope index` | 删除内容，remove 是别名；不接受多余正文。 |
| `/memory rule` / `/memory rule add` | 打开 enabled/scope/prompt 规则表单。 |
| `/memory rule list` | 列出规则编号、enabled、scope、prompt。 |
| `/memory rule edit index` / `/memory rule delete index` | 表单编辑或直接删除规则，编号从 1 开始。 |

Provider 表单包含 name/kind/endpoint/apikey/model/maxtokens/temperature/context_length；API key 和可能包含凭据的 endpoint 均掩码显示，取消不保存。保存使用私有文件权限和同目录原子发布，不生成公开凭据备份；不宣称 CLI 表单支持原 TUI 的联网「拉取模型」按钮。

`/think`、`/effort`、`/max_steps` 只更新全局配置的目标字段，保留无关字段，避免把项目 merged 配置写入全局。内存立即生效；若项目覆盖同一字段，重新加载仍按项目优先。Memory rule 在项目 config 已有 memory.rules 时修改项目规则，否则修改全局规则，不整份回写 merged 配置。enabled=false、无效 scope 或空 prompt 不注入；合法 scope 为 global/project/both，按标签进入后续系统提示词约束，不是 hard guard 或工具访问控制。

CLI writeup 当前保存到 `<cwd>/.cyber/ctf/sessions/<sessionid>/<challengeid>/<category>/<name>/writeup.md`，按项目、会话、题目隔离。题目状态由实际 CTF 工具维护；目录验收和已解条件回归不等于真实 Provider API 的 Solved→writeup 全链路联调。

## 验证边界

真实 Windows ConPTY 已通过 119 个断言（47 SSE + 72 commands），含 120x30 / 80x12、Unicode 光标、密钥掩码/取消、17 项目录、compact/cancel/session 持久化及 memory rule。MCP 单测与 5 个真实子进程测试通过，覆盖 lifecycle、cancel/shutdown，UI connect deny 不会 start。未宣称完整 Provider 外网或已解 CTF writeup 真实 API 联调；静态补全及主命令验收不代表所有多级子命令、自由文本、大小写、额外参数与任务状态组合均已端到端验证。

## 原 TUI 输入规则

- TUI 主命令名大小写不敏感；参数保留原样，并非所有子命令或名称都大小写不敏感。
- 以 `/` 开头的输入由 App 拦截，不作为普通提问发送。输入 `/` 可打开命令补全，方向键选择，Enter/Tab 补全后继续输入参数并提交。
- 下表方括号表示可选参数，尖括号表示占位参数，使用时不输入括号。实现按空白切分，不提供 shell 引号解析。
- 生成中部分操作会被 handler 拒绝；取消用 `/cancel`。键盘输入/面板自身还有状态限制，不能把 handler 未设检查理解为任意状态均可操作。

## 原 TUI 17 个主命令

| 命令 | 实际行为与限制 |
| --- | --- |
| `/help` | 在对话中显示 TUI 命令帮助；不是 CLI shortcuts 面板。帮助文本并不完整，子命令以本页核对结果为准。 |
| `/clear` | 清空当前会话对话并保存，随后显示清空提示；生成中拒绝。不是删除所有会话。 |
| `/mode <chat\|workflow\|dashboard>` | 切换视图；生成中拒绝。workflow/dashboard 目前仅占位页，不代表 DAG 执行或监控已可用。 |
| `/model [provider]` | 无参数打开 provider/model 选择面板；带参数只切换已存在的 provider，沿用其配置模型。生成中拒绝，不支持 CLI 的 `/model provider model` 语法。 |
| `/provider [子命令]` | 列出、表单新增/编辑、设默认、删除 provider，详见下文；生成中拒绝。 |
| `/tools` | 展示当前注册工具的名称与说明，包含实际装配的内置、Skill、自定义及 MCP 工具；不执行工具。 |
| `/skill [list\|name]` | 无参数或 `list` 列出名称、全局/项目来源与简介；名称参数显示对应正文。仅添加 UI System 展示条目，不注入模型历史；模型获取正文需调用 `skill_<name>`。 |
| `/mcp [list\|status]` | 展示已连接 server 名称，未启用/无连接则提示；没有 reconnect 或配置管理。当前 handler 忽略参数，其他参数也只显示同一列表，不表示实现了新操作。 |
| `/cancel` | 取消正在生成的任务，abort 并推进 generation 隔离旧事件，保存已收到内容；无生成任务则提示。 |
| `/compact [instructions]` | 对当前模型历史手动压缩，可追加自定义摘要指令；历史为空、生成中或压缩中拒绝。会调用模型，不是单纯 UI 清屏。 |
| `/ctf [子命令]` | 查看开关状态、启停、添加/列出题目、为已解出题目生成 writeup，详见下文。 |
| `/max_steps [N]` | 无参数查看当前工具调用步数上限；`N` 必须是 1-1000 的整数，更新当前运行配置。 |
| `/think [low\|middle\|high\|max\|auto]` | 无参数查看档位，带参数更新现有思考强度配置；沿用系统提示词注入，不新增 provider API `reasoning_effort`，不保证 provider 返回 reasoning。CLI 对应 `/effort`，用 medium/xhigh 标签。 |
| `/new` | 保存当前并创建/切到新空会话；生成中拒绝。 |
| `/sessions [子命令]` | 同 cwd 会话面板、跨会话内容展示或新建，详见下文；生成中拒绝。 |
| `/memory [子命令]` | 查看或写入全局/项目记忆，支持 edit/delete，详见下文。目录/补全/帮助中的 `rule` 没有 handler，不可用。 |
| `/quit` | 保存历史并退出 TUI。 |

## 原 TUI Provider 子命令

| 语法 | 行为 |
| --- | --- |
| `/provider` / `/provider list` | 列出 provider、默认标记、kind、地址和配置模型。 |
| `/provider add` | 打开新增表单；表单可填写配置与异步拉取模型，保存后写 providers 文件。 |
| `/provider edit <name>` | 打开已有 provider 编辑表单，名称须存在；Chat 入口保存立即写 providers 文件。 |
| `/provider use [name]` | 无名称显示当前 provider；带名称切换已存在的 provider，并保存 config。 |
| `/provider remove <name>` | 删除并写 providers 文件，删除默认项时内存默认值回退到排序后首个剩余项。此 slash 路径直接删除，没有 Settings 面板的双击确认。 |

子命令大小写不敏感，provider 名称按实际配置精确匹配。`/model provider` 是当前运行切换；需保存默认配置时使用 `/provider use provider`。

## 原 TUI CTF 子命令

| 语法 | 行为 |
| --- | --- |
| `/ctf` / `/ctf status` | 显示 CTF 开关状态，不是修改某题目的解题状态。`status` 已有 handler，虽未列入命令目录/参数建议。 |
| `/ctf enable` | 开启 CTF，后续 agent 提示词使用 CTF 方法论；Ctrl+T 可切换题目面板。 |
| `/ctf disable` | 关闭 CTF 并隐藏题目面板。 |
| `/ctf add <name> [category]` | 添加题目，名称取第一个空白分隔字段；分类为 misc/web/reverse/pwn/crypto，省略或无法识别时回退 misc。不使用 `/ctf add category name`。 |
| `/ctf list` | 列出题目的编号、分类、名称、当前解题状态。 |
| `/ctf writeup <name>` | 对已有题目名称精确匹配，且必须已解出；调用模型生成报告。生成、压缩或 writeup 任务进行中拒绝；不能省略名称。 |

示例：

```text
/ctf enable
/ctf status
/ctf add login web
/ctf list
/ctf writeup login
```

writeup 生成成功后按项目题目目录保存到 `<cwd>/.cyber/ctf/<分类>/<题目>/writeup.md`。题目状态/flag 由题目面板或 `ctf_challenge` 工具管理；没有 `/ctf status <name> <状态>` 子命令。

## 原 TUI Sessions 子命令

| 语法 | 行为 |
| --- | --- |
| `/sessions` / `/sessions list` | 打开同 cwd 的 Sessions 面板；方向键选择、Enter 切换、n 新建、d 双击删除、Esc 返回。至少保留一个会话。 |
| `/sessions read` | 展示同 cwd 会话标题、ID、消息数与当前标记。 |
| `/sessions read <id\|关键词>` | 按 ID 相等或标题包含关键词筛选；唯一匹配时展示内容，多个匹配时提示候选。仅添加 UI System 条目，不切换会话、不注入模型历史。 |
| `/sessions new` | 同 `/new`。 |

会话保存在 `~/.cyber/history/{cwd_hash}/index.json` 和 `{id}.json`（`CYBER_HOME` 可覆盖全局根目录），兼容旧单文件历史迁移。`/sessions <ID>` 不是 TUI 切换语法，请通过面板切换。

## 原 TUI Memory 子命令

| 语法 | 行为 |
| --- | --- |
| `/memory` / `/memory list` | 分别列出全局/项目记忆与从 1 开始的编号。 |
| `/memory add <text>` | 追加全局记忆，保存到全局 `memory.md`。 |
| `/memory project <text>` | 追加项目记忆，保存到 `<cwd>/.cyber/memory.md`。 |
| `/memory edit <global\|project> <index> <text>` | 更新指定 scope 的编号内容。 |
| `/memory delete <global\|project> <index>` | 删除指定 scope 的编号内容；`remove` 是实际 handler 支持的别名。 |
| `/memory rule` | **未实现**：虽出现在 COMMANDS、参数建议和 HELP_TEXT，handler 无分支，会提示未知记忆子命令。不能据此声称支持规则编辑。 |

写入记忆与 `/skill`、`/sessions read` 的 UI 展示不同：记忆文件会在后续 agent 提示词构建时读取。编辑/删除使用 `/memory list` 中对应 scope 的编号，不是会话消息编号。scope 的实际解析还接受 `local` 为项目级，其余值回退全局；为避免误改，明确使用 `global` 或 `project`。
