# Changelog

All notable changes to Cyber Master will be documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Added
- **CLI「关于 / About」页**：`cyber` 设置中心新增第 9 页签「9. 关于」（`Tab`/`Shift+Tab`/`←/→` 轮换，数字键 `9` 直达，底部提示统一改为 `1-9 直达`），并新增全屏只读面板 `Panel::About`（`/about` 斜杠命令与 `F1` 打开，与设置页签共用同一份内容渲染 `about_lines` 与同一份快捷键表 `SHORTCUT_KEYS`）。内容分四段——**版本与更新**（当前版本、可更新版本、可执行文件路径、升级方式）、**项目信息**（简介/仓库/CNB 镜像/协议/作者）、**运行环境**（服务商/模型/思考强度、工作目录、会话数、项目级配置是否启用、日志级别、内置工具与 Skill 数、自定义工具数、配置文件与服务商文件与会话目录路径）、**核心能力**，页尾附**快捷键说明书**与**斜杠命令速查**（等价于按 `?` 打开的 Shortcuts 浮层，两处取自同一 `SHORTCUT_KEYS` / `cli_commands::commands()`，不再各写一份）。页面只读：`↑/↓`/`PgUp`/`PgDn`/`Home`/`End` 或滚轮滚动（渲染期零磁盘 I/O，内容快照 `AboutInfo` 在进入页面时一次性采集），`U`/`Enter` 复用既有 `/update check` 检查更新并把结果同步到「可更新版本」行，`Esc` 关闭。`cyber tui` 的 `Mode::About` 不受影响。
- **CLI `/update` 应用内联网检查与升级**：`cyber` 对话页的 `/update` 此前只读 `~/.cyber/update_check.json` 的 1 小时缓存并提示「退出后运行 `cyber update`」。现改为 `/update [check|apply]`：**无参数**＝联网检查（`check_for_updates(true)` 绕过缓存，`--mock` 与单测不联网）并在发现新版本时进入确认态（底栏 `按 y 用安装脚本更新 · 其它键取消`，消息区列出发布页面/说明），**`check`** 只检查，**`apply`**（`yes` / `now` 同义）检查后直接升级。确认后复用 `cyber update` 的安装脚本（`install.ps1` / `install.sh`，经 `cyber_core::update::install_script_command` 与 CLI 逐字一致）后台安装，运行中的进程不退出；当当前二进制就是安装脚本的目标（`CYBER_INSTALL_DIR`，未设为 `<home>/.local/bin`；Windows 无法覆盖运行中的 exe）时先生成等待本进程退出的脚本 `~/.cyber/logs/update-v<ver>.ps1|sh` 再保存会话退出，退出后自动完成安装。安装目标判定/脚本生成抽为 `cyber_core::update` 的 `install_script_command` / `install_script_hint` / `installed_binary_path` / `needs_exit_before_install` / `launch_detached_install`，`cyber update` 的前台命令与取消提示共用同一份实现（`--check` / `--apply` 输出文案与参数不变）。回合进行中仍可检查更新，真正执行安装要求空闲（busy 时提示先 `/cancel`）。`/help` 与 `/` 补全新增 `/update`。
- **安装脚本已装检测 + `cyber update --force`**：`install.sh` / `install.ps1` 现在会先探测本机已安装的 `cyber`（`command -v cyber` / `Get-Command cyber`，回退安装目录），运行 `cyber --version` 输出**已安装版本**与**云端最新版本**并做语义化比较：已是最新则直接退出（提示 `cyber update --force`），落后则询问是否更新到最新版本（回车 / `y` 确认，`n` 取消；`curl | sh` 管道下经 `/dev/tty` 读取，无控制终端默认不更新）。新增 `--force`（`install.sh`）/ `$env:CYBER_FORCE=1`（`install.ps1`）跳过检测与询问直接覆盖安装。CLI 同步新增 `cyber update --force`（`-f`，与 `--check` 互斥）：完全跳过版本检查与交互确认，直接把最新版本解析交给安装脚本并强制覆盖；`cyber update` / `--apply` 与 TUI 的分离安装现在也统一向安装脚本传递 force，避免二次确认。
- **CLI todo 清单三态视图与 `Alt+↑` / `Alt+↓` 快捷键**：`cyber` 对话页底部常驻的 📋 任务清单由二元展开/收起（`CliScreen.todo_closed: bool`）改为三态 `TodoView`——**收起**（不画表格，只在对话区底部保留 1 行进度条 `📋 任务清单 [c/t] (▶ 当前任务) · [Alt+↑] 展开`）、**精简**（表格高度 `clamp(3, 7)`，与改动前逐字节一致）、**全量**（按内容要高度，上限为可用空间；放不下时仍由既有「... 还有 N 项任务」提示行折叠）。`Alt+↑` 逐级展开（收起 → 精简 → 全量）、`Alt+↓` 逐级收起（全量 → 精简 → 收起），到顶/到底按键被消费但不翻转、不刷状态提示；表格标题按态显示 `[Alt+↑] 全展` / `全部 N 项 · [Alt+↓] 收起`。`/todo close`（`hide`）等价于「收起」、`/todo open`（`show`）等价于「精简」，**全量态只能由 `Alt+↑` 第二档到达**（`CliAction::TodoVisibility(bool)` 语义不变）。`?` shortcuts 面板新增该组合键；`cyber tui` 原 Chat 面板的 `todo_closed` 二元行为不变。
- **设置中心「8. 工具库」AI 智能扫描表单**：末行 `▶ 🤖 AI 智能扫描本地安全工具` 按 `Enter` 不再直接用当前默认 Provider 跑扫描，而是打开扫描表单：填写**扫描目标**（本地目录 / 单文件 / 提示词，留空＝自动探测本机常见工具）、用 `←/→` 切换**扫描服务商**、在**扫描模型**行按 `Enter` 打开模型列表（只列 `providers.toml` 的本地清单，不联网；`m`/`f` 手输、`r` 联网重取、`↑/↓` 选择、`Enter` 确认，也可直接打字输入）、用 `←/→` 切换「仅预览不写盘」；保存后立即开始扫描，进度与结果仍输出到对话区。表单默认带出当前默认 Provider 及其配置模型，扫描开始前会回显 `🤖 AI 智能扫描：服务商 [x] · 模型 [y]`。`/toolbox scan` 同步支持 `--provider <名>` / `--model <名>`（`CliTask::ToolboxScan` 新增这两个可选字段，省略时沿用旧行为：当前默认 Provider + 其配置模型）。
- **CLI 模型选择面板自动拉取模型列表**：`/model`（全屏双栏 `Model Picker`）左栏选中/切换服务商后自动调用 `{base_url}/models` 拉取该服务商的真实模型列表，打开面板亦立即拉取；端点构造沿用 `cyber_agent::fetch_models`，`base_url` 已含版本段（`/v1` 等）时不再补 `/v1`。拉取期间右栏显示本地 `providers.toml` 配置清单并附 `⟳ 正在从接口拉取 […] 的模型列表…` 状态条；成功则把接口模型并入列表（本地已配置模型保留，光标不跳），失败显示 `⚠ 模型列表拉取失败：…` 并保持本地清单可选；切换服务商后到达的旧结果按 `fetch_id` 丢弃。
- **Provider 模型列表选择与能力标签**：新增/编辑 provider 时 `model` 字段改为**选择式**。光标在 `model` 行按 `Enter`（或空格）即拉取 `{base_url}/models` 并弹出模型列表面板；每行附 `◈ 视觉` / `◈ 推理` 能力标签（三级解析：显式 `models[model]` 配置 → `~/.cyber/cache/capabilities.json` 实测缓存 → 内置名称规则表）。
  TUI 表单（`cyber tui` / Settings Providers 段）与 coding CLI 全屏表单（`/provider add|edit|add-preset|add-with-kind`）均已覆盖。
- **模型列表面板按键**：`↑/↓` 选择、`Enter` 确认、`m`/`f` 手输模型名、`r` 重新拉取、`t` 实测推理能力、`v` 实测视觉能力、`Esc` 关闭。列表拉取失败或返回空列表时自动进入手输兜底模式（`model` 恢复可编辑，`[✎ 手输模式]` 行内标记，`Enter` 退出），拉取失败不再导致无法配置模型。
- **思考（thinking）配置**：provider 级新增 `thinking = { type = "enabled"|"disabled", effort = "low"|"medium"|"high" }`（两项独立、均可省略；都省略即不下发任何思考参数，与旧行为逐字节一致）。四个 provider 按 kind 分派正确参数名：openai/openai-compatible → `thinking` + 顶层 `reasoning_effort`；anthropic → `thinking.budget_tokens`（`clamp(max_tokens/2, 1024, 32000)`）并把 `temperature` 固定为 1；ollama → `think:true|false`；responses → `reasoning.effort`。CLI 表单新增 `thinking_type` / `thinking_effort` 两行（←/→/空格 循环，非法值保存时报错）；TUI 表单新增字段 16/17。
- **推理能力实测探针** `cyber_agent::probe_model_reasoning`：带思考参数发一次最小非流式请求，按 kind 判定响应中的思考输出（anthropic `content[].type=thinking`、ollama `message.thinking`、responses `output[].type=reasoning`、openai 家族 `reasoning_content`/`reasoning`/`thinking`），结果写入能力缓存；面板探针同时回写 `providers.toml` 的 per-model `reasoning` 字段。

### Changed
- **设置中心改为「只读 → Enter 编辑」两态，←/→ 在只读态切换标签页**：`cyber` 的设置中心（`Panel::Settings`）此前光标停在任意值行时 ←/→（含 `h`/`l`/空格/Enter）即刻改值，没有只读/编辑区分。现新增 `CliSettingsState::editing`：焦点行默认只读，按 `Enter`（仅 `focused_row_is_value()` 判定的值行）进入编辑态，指针由 `▶` 变 `✎`、底部提示变「编辑中：←/→ 调整数值 · Enter/Esc 完成 · ↑/↓ 移动焦点」，此时 ←/→（`h`/`l`/空格）才改值；`Enter`/`Esc` 退出编辑态且**不关闭面板**，`↑/↓`/`Home`/`End`/`PgUp`/`PgDn`/`k`/`j` 退出编辑态并继续移动焦点。只读态下 ←/→（`h`/`l`）改为切换设置标签页（与 `Tab`/`Shift+Tab`、数字 1-8 并存），值行上的空格被吞掉并提示「按 Enter 进入编辑后再调整该项」。动作行（Providers、MCP 控制台、Skills 详情、工具库、记忆列表）与 ↑/↓ 移动、字母动作键（`A`/`E`/`D`/`M`/`T`/`O`）行为不变，`Enter` 在动作行仍是原行为（设默认 / 打开弹窗 / 表单）。编辑态下 `Tab`/数字键/`r`/`s`/`Ctrl+S`/`F3`/`Ctrl+,` 保持原全局语义，切标签、滚轮移动、从设置中心打开的弹层返回时自动清除编辑态。环境变量与记忆规则行属值行：`Enter` 进编辑后用空格切脱敏/启停、`←/→` 换作用域，打开编辑表单改用保留的 `E` 键（行内与底部提示同步改为「Enter 编辑 · E 表单」）。`cyber tui` 的 `Mode::Settings`（`app.rs` / `views/settings.rs`）是另一套界面，本次不改。
- **设置中心「8. 工具库」把 `🤖 AI 智能扫描本地安全工具` 入口移到列表首行**（原先在列表末尾）：行 `0` 固定为扫描入口，其下依次是自定义工具（显示行 ↔ 工具下标的映射抽为 `toolbox_tool_row`）。`Enter` 打开扫描表单的行为与表单文案不变；`A 添加` 不再依赖当前行（空清单或焦点停在扫描入口时同样可录入），`Enter/E 编辑` 与连按两次 `D 删除` 仍只作用于自定义工具行。状态栏提示与底部导航提示同步改为「首行 Enter 打开 AI 扫描表单」。
- **CLI 回合进行中（thinking/busy）只读查看类指令与面板立即可用**：`cyber` 在 `CliScreen.busy`（模型流式输出、思考、工具执行、等待审批的全过程）期间会把 `SessionRunner` 从 `runner: &mut Option<SessionRunner>` 中 `take()` 走，因而 `cli_commands::execute` 无法调用——上一版只能把需要 runner 的指令（含 `/settings`、`/tools`、`/think` 无参、`/model` 无参、`/env list` 等**只读**指令）统统排队到回合结束。现新增回合内只读快照 `CliScreen::view_snapshot`（`sync()` 处以纯克隆刷新，零磁盘 I/O）与借用式只读视图 `headless::ViewCtx`（`SessionRunner::view()` / `CliScreen::view()`），并抽出 `cli_commands::readonly_action(view, line)` 作为**空闲与 busy 共用的唯一只读分派**：不写盘、不改会话的指令 —— `/help`、`/tools`、`/skill [list|<name>]`、`/think`（无参）、`/effort`（无参，经 `parse_line` remap 为 `/think`，顺带修掉 busy 下被误判为 Unknown 的缺陷）、`/max_steps`（无参）、`/env [list]`、`/web [status]`、`/vision [status]`、`/subagents [status]`、`/ctf status|list`、`/toolbox [list]`、`/memory [list]`、`/memory rule list`、`/mcp [panel|list|status|add|edit|delete]`、`/provider [panel|dashboard|list|models [name]]`、`/model`（无参）、`/sessions [list]`、`/mode …`、`/update …` —— **立即生效**，且与空闲路径走同一实现、输出逐字节一致；写盘/改会话的指令（`/new`、`/clear`、`/compact`、`/think <level>`、`/max_steps <N>`、`/provider use|add|edit|remove|wizard|add-preset|add-with-kind|add-custom`、`/env add|edit|set|remove`、`/web|/vision on|off|…`、`/subagents enable|disable|max_*`、`/toolbox add|edit|remove|scan`、`/mcp connect`、`/ctf [enable|disable|add|writeup|panel|open]`、`/memory add|edit|delete|project`、`/sessions read|new|delete`）**仍按输入顺序入队**，回合收尾由 `drain_queued_inputs` 依次执行（入队即回执「已排队：回合结束后执行…（排队 N 条指令）」，输入框清空；面板内选中如在 `/sessions` 选择器按 `Enter`/`n`/`d` 与在输入框敲同一指令等价，同样入队而非静默丢弃）；未知命令立即报 `Error: Unknown command; use /help` 且不入队。排队命令与排队提示词共用同一 `VecDeque`（`QueuedKind::{Prompt,Command}` 保序），**绝不**写入 steering 通道，模型看不到 `/model` 这类文本。`/settings` 在 busy 下立即打开**真实配置快照**面板（此前若直接执行会退化成 `Config::default()` 空面板，且保存会显示「✔ 设置已保存并立即生效」却什么都没写），可正常浏览/编辑草稿；`Ctrl+S` 与需要 runner 的设置操作（Provider/Env/Memory/工具库表单、CTF 开关、setup 向导退出）一律提示「回合进行中：…回合结束后生效 / 再执行」，不落盘、不显示假成功，草稿与 `dirty` 保留。`/model` 面板在 busy 下立即可开（面板数据来自 `CliModelPickerState::from_view`），面板内确认切换模型同样延后提示。`/mode`、`/help`、`/todo` 从 `execute` 拆出的 runner 无关实现继续复用；`cyber tui`（`app.rs`）行为不变。
- **任务清单卡片跟随「当前进度」滚动**：CLI（`cyber`）与 TUI（`cyber tui`）底部常驻的 📋 任务清单此前固定显示最前面 `rows - 1` 条，任务一多进行中项就滚出视口。现窗口改为跟随焦点行：优先显示第一个 `in_progress`（无进行中时取最后一条已完成/失败项），窗口尾部贴住焦点行以最小化滚动；折叠提示按方向显示「↑ 上方还有 N 项」「... 还有 N 项」「↑ 上方还有 N 项 · 下方 M 项」。窗口计算抽为共享纯函数 `views::todo_visible_window`（两处渲染不再各写一份截断逻辑）。清单不超过可视行数时渲染与折叠文案与改动前逐字节一致。
- **系统提示词与 todo 工具强化「善用 + 实时更新」**：`BASE_PROMPT_STATIC` 的「结构化任务管理」条目扩为八条细则，新增**触发边界**（「何时必须建清单」：3 个以上有序步骤 / 跨多轮工具调用 / 需逐项验证 / 用户要求分步；单次问答与单条命令不要建清单，也不要为已完成的工作补建）与**每步自检**（调用其它工具前先确认该步对应哪个 id、是否已 in_progress），并把「禁止先干活后补状态」写进开工/收工两条。`TodoTool` 侧同步加码：`schema().description` 由一行概述改为「触发时机 + 实时同步流程 + 不要反复 list」的操作性说明，`status`/`notes` 参数写明「同时只允许一个 in_progress」「completed 必须已验证」「notes 记关键结论与证据」；变更类操作（add/update/remove）的结果尾部新增一行状态相关提示（`next_step_hint`：指出下一个待办/进行中 id，或提示失败项需写明原因），使每次清单改动都直接回喂「下一步该做什么」，形成实时更新闭环；`list` 不附提示以免被误当催促。

- **模型列表统一按名称首字母排序**：CLI 三处模型清单（Provider 表单的「选择默认模型 / Select Model」浮层、设置中心 Providers 段 `M` 模型选择、`/model` 双栏面板右栏）此前按接口返回顺序 / 字节序排列，现统一为**不区分大小写**的首字母升序（`claude-3` < `GLM-4.6` < `o3`），仅大小写不同的条目按原串稳定排序；光标定位（当前模型）不受影响。
- **模型选择面板改为「左栏选定 provider 后按 Enter 才联网拉取」**：打开面板与左栏 ↑/↓（含 j/k、鼠标滚轮）切换 provider 现在都只显示 `providers.toml` 的本地模型清单，不发起任何请求；在左栏按 `Enter` 选定 provider 时才调用 `fetch_models` 拉取云端模型列表（`r` 可手动重取，用于失败重试/刷新新上架模型）。未拉取时状态条提示「当前显示 providers.toml 的本地模型 · 按 Enter 选定 provider 并从接口拉取」，拉取中/失败沿用原有状态条；拉取进行中重复按 `Enter`/`r` 不重复发起请求。
- **修复长模型列表渲染卡顿（性能）**：面板右栏此前每帧对**每一行**调用一次 `get_model_vision_capability(..., None)`（TUI 两次），而 `cache = None` 会退化为每次读盘并解析 `~/.cyber/cache/capabilities.json`，且每帧构建全部行。现改为每帧只 `CapabilityStore::load()` 一次（经 `resolve_vision_capability` 统一解析：显式 `models` 配置 → 实测缓存 → 名称规则表）并只构建可见窗口内的行。8000 条模型单帧实测：旧实现 CLI ≈ 960ms / TUI ≈ 1.9s（约 1 fps），新实现 ≈ 6ms。
- **文档纠正**：`docs/TUI_COMMANDS.md` 此前写「`/model` 不自动联网发现模型」，与实际行为不符，已改为描述「打开即拉取 / 切 provider 重取 / `r` 手动重取」；`docs/DESIGN.md` 新增 §9.9 记录面板的拉取时机与状态展示。

### Fixed
- **设置中心里打开的自定义工具 / AI 智能扫描表单，背景回落到对话界面**：`CliScreen::settings_opened_dialog()` 此前只白名单 `FormKind::EnvVar` / `FormKind::MemoryRule`，于是从「8. 工具库」页按 `Enter`/`E`（编辑自定义工具）或末行 `Enter`（AI 智能扫描）打开的表单被画在对话区上，下层背景是 logo 头部 + 对话区 + 任务清单 + 输入框，而不是打开的设置中心面板。现改为「除 `FormKind::Provider`（由 `draw()` 更早的全屏分支接管）外的任何表单，只要是从设置中心打开的（`settings_return_tab` 非空）就以设置中心为背景」，与既有环境变量 / 记忆规则表单、Models 列表一致，且后续新增表单变体默认获得正确背景。
- **工具库列表里超长描述导致整行只剩「…」**：共享截断函数 `views::clipped_spans` 在预算于某个 span 内部耗尽时用 `break 'outer` 跳出整个循环，把该 span 已放下的可见前缀连其后所有 span 一起丢弃（只补一个 `…`）。设置中心「8. 工具库」行的顺序是 `[名称] 描述 · 命令`，因此描述一长（如 `[fenjing_crack] …`）就只显示名称加省略号，描述与命令全不可见。现改为保留已消费的前缀后再截断，该修复同时覆盖所有使用此函数的列表/面板行。工具库行另外对描述与命令做单行归一化（`single_line`），避免模型生成的多行字段把整行挤出视口。
- **Provider 表单里关闭模型面板会连带关闭表单**：在 `/provider add|edit` 全屏表单中打开「选择默认模型 / Select Model」面板后按 `Esc`，此前外层 Esc 处理会先把整个表单 `take` 掉（用户看到面板与 Provider 编辑界面一起消失）。现在表单内的模型面板打开时 `Esc` 只关面板（列表模式与手输兜底模式一致），表单保持在原地；面板关闭后 `Esc` 恢复原有「关闭表单」语义。同时收紧面板的确认键：面板内只接受无修饰键的 `Enter`（`Alt+Enter` 等不再被当作确认并改写 `model` 字段）。TUI 表单本就先由面板消费 Esc，行为不变。
- **探针端点误用到 `chat/completions`**：`probe_model_vision` 此前对所有 kind 都 POST `ProviderConfig::chat_endpoint()`（仅 ollama 特判），anthropic 与 responses 探针会打到错误路径。新增 `cyber_agent::probe_endpoint`：显式 `chat_endpoint` 优先，否则 anthropic → `{base}/v1/messages`、responses → `{base}/responses`、其余 → `chat_endpoint()`。
- **清空 thinking 后残留**：将 `thinking` 置空保存时同步从 `providers.toml` 删除该键，避免 `merge_table` 合并保留陈旧值并在下次启动重新读回。
- **base_url 已含 `v1` 时重复追加导致 404**：anthropic 的 `{base}/v1/messages`、模型列表的 `{base}/v1/models` 等固定带版本段的路径此前无条件拼接，base_url 配成 `https://api.anthropic.com/v1`（`/provider add-with-kind anthropic` 的默认端点）时会打出 `/v1/v1/messages`（实测该路由返回 `404 Invalid URL`）。新增 `cyber_core::with_api_version`：base_url 的 path 已含版本段（`/v1`、`/v1beta`、`/v2`、`/api-v1`…）则原样使用，否则补 `/v1`；`fetch_endpoints` 同时去掉重复候选，不再产生 `{base}/v1/v1/models` 这类无效回退。

---

## [0.6.0] - 2026-10-06

### Added
- **`/exit` 作为 `/quit` 的别名**：空闲与流式（busy）两条路径均可直接退出，行为与 `/quit` 完全一致（保存会话后退出）；
  未加入补全菜单，与原 `/session`（`/sessions`）、`/models`（`/model`）别名一致。
- **Provider 表单「高级设置」分组**：新增/编辑服务商时可直接填写
  `chat_endpoint`（自定义对话端点，留空默认 `{base_url}/chat/completions`）与
  `models_endpoint`（自定义模型列表端点，留空默认 `{base_url}/models`）。
  CLI 表单（`/provider add|edit|add-preset|add-with-kind`）与 TUI 表单（`cyber tui` / Settings Providers 段）均已覆盖。
- **TUI Provider 表单跟随光标滚屏**：字段数量增长后字段区不再被底部裁剪，光标移到任意字段都会自动滚入视口。

### Fixed
- **设置中心弹层背景错误**：从设置中心打开的「编辑环境变量 / 编辑记忆规则」表单与「Models」模型选择弹层，
  下层背景此前会回落到对话界面（logo 头部 + 对话区 + 输入框），现已保持设置中心面板（边框标题 + 页签栏 + 当前页内容），
  弹层居中覆盖其上；按键路由与 `Esc` / 保存后返回设置面板的语义完全不变。
- **清空高级端点覆盖值后残留**：将 `chat_endpoint` / `models_endpoint` 置空保存时，会同步从 `providers.toml` 删除对应键，避免合并写入保留陈旧值并在下次启动被重新读回。

### Removed
- **CLI 空输入 `Ctrl+D` 退出快捷键**：`Ctrl+D` 不再退出进程。无面板时交回输入框自身语义（Emacs 风格「删除光标处字符」）；
  设置中心与 CTF 面板内显式忽略该按键，避免落入单字母 `d` 的删除动作（provider / 环境变量 / 记忆规则 / 题目）。
  shortcuts 面板与文档同步删除该条目，退出仍可用空输入 `Ctrl+C` 或 `/quit`。

---

## [0.5.1] - 2026-10-05

### Added
- **CNB 镜像源更新回退与一键升级安装支持**：
  - 在 GitHub Releases API 不可用或受限时，自动回退至 CNB 镜像源检测最新版本。
  - `/update` 支持提示用户确认并直接拉取安装脚本执行原地更新。
  - `/version` 增加 CNB 镜像发布地址展示，并在更新提示中附带国内云原生一键安装命令。
- **终端文本选区交互优化**：
  - Chat 与 CLI 视口支持 `Shift + 单击` 连续扩展文本选区。
  - 鼠标释放时保持高亮选区以便 `Ctrl+C` 复制到剪贴板。

### Fixed
- **无头 CI 环境剪贴板容错**：解决 Linux 无显示服务（Headless）环境下剪贴板断言导致的集成测试失败。
- **CNB Release 目标提交绑定**：修复自动化镜像工作流中 Release Tag 未绑定准确 commit 的问题。

### Changed
- **发布预检流程精简**：优化本地发布检查脚本 (`check-release`) 为 7 步并保持与 CI 工作流对齐。

---

## [0.5.0] - 2026-10-05

### Added
- **自定义安全工具聚合清单 (`custom_tools_list`) 与双向前缀容错匹配**：
  - 新增 `CustomToolsListTool` 聚合所有配置的自定义工具并提供多维度过滤与命令执行模板。
  - `ToolRegistry::get` 实现 `custom_<name>` 与 `<name>` 双向别名自动匹配。
  - 增强系统与 CTF 提示词，引导模型在缺少专项工具时优先探索自定义工具清单。
- **终端鼠标拖拽滚动条与跨视口逻辑文本选区复制**：
  - `cyber-tui` 新增 `selection` 模块 (`ContentCoord`, `TextSelection`, `extract_text`, `apply_selection_to_row`)。
  - Chat 和 CLI 视口支持鼠标点击/拖拽滚动条滑块、边缘加速滚动。
- 支持鼠标框选折行文本、`Ctrl+C` 跨行复制到系统剪贴板以及 `Esc` 清除选区。
- **MCP 扩展工具聚合清单 (`mcp_tools_list`) 与多级前缀容错匹配**：
  - 新增 `McpToolsListTool` 聚合外部 MCP 工具，支持多维度关键字过滤与 Schema 参数查看。
  - 启动阶段将具体 MCP 工具以隐藏模式注册 (`register_hidden`)，避免撑爆模型上下文与单轮工具限制。
  - `ToolRegistry::get` 支持 `mcp_<server>_<tool>`、`<server>_<tool>` 及 `<tool>` 多级动态别名解析。
  - `cyber-mcp` 协议层扩展支持服务端返回的纯字符串 JSON-RPC 错误负载。
- **CI/CD 与自动化镜像同步工作流**：
  - 新增 Git Pre-Push Hook 与本地发布预检系统 (`.githooks/`)，拦截不合规的 Release Tag 并在本地执行对齐 GHA 的全量检查。
  - 新增 GitHub 到 CNB.cool 自动化镜像工作流 (`sync-cnb.yml` 整仓镜像，`sync-cnb-release.yml` 资产同步)。

---

## [0.4.2] - 2026-10-04

### Added
- **子代理面板美化 + opencode 式覆盖对话视图**：
  - `Ctrl+G` 纯列表面板支持按 Enter 直接进入子代理运行视图，无缝覆盖主对话区域并保持底部输入框可用（边看边聊）。
  - 顶栏替换为子代理运行态信息（#id · 名称 · 状态徽标 · 运行时长），转录行以对话样式实时流式呈现（Thinking 块、折叠式工具卡、Markdown 正文）。
  - 完善键盘滚动（`↑`/`↓`/`PgUp`/`PgDn`）、`End` 恢复贴底追踪、`Ctrl+O` 展开/折叠内部工具卡、`Esc` 退出视图回到对话。
- **后台任务与子代理结果自动回灌并开启思考**：
  - 后台 Shell 命令与子代理任务完成时，将完整结果作为 Markdown 注入会话记录（放宽至 16,000 字符并解除 400 字符限制）。
  - 若主模型处于空闲状态，自动注入通知提示词并进入思考状态（`spawn_turn`）；若处于忙碌状态，通过 `steering` 动态注入正在执行的推理流。
- **GFM 表格渲染与 Markdown 增强**：
  - `markdown.rs` 新增完整的 GFM 表格块解析与盒线对齐渲染（表头高亮、`├─┼─┤` 细边框、基于 Spans 实际视觉宽度的中英混排精确对齐、`\|` 转义管道符支持）。
  - `self.message` 与 `ChatEntry::System` 全面支持 Markdown 解析与格式化展示。
- **子代理防撑爆与推理排版加固**：
  - 子代理工具输出进入消息历史时增加 6000 字符预算截断，系统提示新增单步聚合命令引导，杜绝长目录遍历撑爆上下文。
  - `TranscriptWriter` 引入推理流聚合缓冲，修复逐词事件断行丢失空格导致词/数字撕裂（如 "118 42"、"node _modules"）的问题。
- **子代理结果永不空值**：每个结束的子代理（completed/error/timed_out）都携带非空、有意义的结果内容。
- **后台模式**：AI 工具级 `bg_shell` / `bg_status` / `bg_kill`；命令级 `/bg run` / `/bg shell` / `/bg list` / `/bg kill` / `/bg tail`；`Ctrl+B` 后台任务管理面板。

### Changed
- `ToolCtx` / `SubagentRuntime` / `run_stream_with_permissions` 贯通携带 `SubagentArchive` 与 `BackgroundRegistry`。
- 固化 `scripts/count-loc.ps1` 代码行数统计脚本（含构建产物 / 仅源码双口径）。
---

## [0.4.1] - 2026-10-04

### Added
- **CLI 模式全功能设置中心 (CLI Settings Panel)**
  - 支持通过 `/settings` 命令、`F3` 键或 `Ctrl+,` 快捷键打开/关闭全键盘驱动的浮层设置面板。
  - 聚合 7 大标签页（Tabs），100% 对齐 TUI 11 大配置领域：
    1. `Agent & 模型`：默认服务商、模型选择、思考强度、审批模式、自动工具调用、步数上限 (1-1000)、联网搜索。
    2. `界面与交互`：主题配色 (即时生效)、鼠标滚轮捕获、默认启动模式、流式动效开关、工作流最大并行节点与超时、断点续跑检查点。
    3. `子任务并发`：子代理开关、单轮最大任务数、最大并发线程数、单任务超时时限、子代理执行步数上限。
    4. `工具与 MCP`：优先容器隔离执行 (Docker)、CTF 渗透答题模式、额外 PATH 注入、MCP 外部服务状态与健康度、已加载技能清单 (Skills)。
    5. `服务商管理`：已配置服务商清单、Enter 快速设为默认服务商、A/E 键打开交互表单、D 键安全删除服务商。
    6. `环境与记忆`：Shell/Agent 子进程自定义环境变量（支持脱敏保护与明文切换）、长期记忆约定规则管理（支持项目级/全局/全域作用域切换与增删）。
    7. `系统与存储`：会话历史保留天数、实时日志级别调节、本地配置路径、服务商密钥文件路径、会话存储目录及统计。
  - 就地即时微调：布尔值一键翻转、枚举项循环切换、数字数值步进调节（支持 Shift 加速步进）。
  - 未保存修改防丢拦截：修改后按 `Esc` 弹出居中警告弹窗（Enter 保存应用、二次 Esc 确认丢弃、方向键取消留在面板）。
  - 标签栏多档自适应（宽屏全称、中屏紧凑、窄屏精简）与水平滑动视口焦点跟随，彻底解决狭窄终端下 Tab 7 截断问题。
  - 长列表（服务商、环境变量、记忆规则）垂直平滑滚动焦点跟随，光标移动时光标项与详细信息自动滚动呈现在可视区域内。
- **初始化向导增强 (Setup Wizard 2.0)**
  - 支持提取大模型回复中的全部 TOML 配置块，支持交互式批量检视与快速导入。
  - 自动扫描本地安全渗透工具并提取 `--help` 帮助文档，联动大模型一键生成标准 Custom Tool TOML 配置。
- **运行时 Agent 转向注入 (Steering Control)**
  - 支持在 Agent 生成和思考期间向执行通道动态发送转向输入指令，灵活干预推理路线。
- **会话标题显式标注**
  - 在 TUI 会话管理面板顶部标题栏中，动态显示当前活动会话的自定义标题。

### Changed
- 会话管理与权限审批模式在 TUI 与 CLI 间保持双向状态同步与热生效。
- 优化 CTF 解题流提取逻辑，消除冗余机械的工具调用输出。

### Fixed
- 修复 Windows 控制台下标准输入输出句柄继承与并发管道冲突。
- 修复流式回车符号合并 (Carriage Return Consolidation) 导致的终端进度条重叠。
- 修复 MCP 外部服务退出时并发 shutdown 超时与资源释放。
- 修复现代 Rust 编译器兼容性告警与 Clippy 规范性问题。

---

## [0.4.0] - 2026-10-03

### Added
- **子 Agent 批量委派与独立卡片渲染 (Subagent Delegation)**
  - 新增 `delegate_tasks` 工具，支持主 Agent 并发委派多个独立的子任务并在空历史隔离循环中运行。
  - 为每个子 Agent 分配独立卡片 (`Subagent [i/N] <name>`)，实时标注任务提示、工具调用动态与完成结果，支持 `Ctrl+O` 展开/折叠。
- **任务管理与跟踪体系 (Todo System)**
  - 内置 `todo` 追踪工具与 `/todo` 斜杠命令族 (list/add/done/remove/clear/open/close)。
  - 支持长任务多步拆解、状态流转与会话持久化隔离，活跃待办自动注入系统提示词引导模型推进。
- **权限判定与联网配置**
  - 智能会话授权 (Session Grant) 与自动审批 (Auto Mode) 放宽安全只读探测命令匹配。
  - 新增 `tools.web_search` 配置项（默认开启），支持实时禁用或开启网页检索与抓取 (`web_fetch`)。
- **交互与快捷键**
  - 新增 `Ctrl+T` 快捷键随时切换/打开 CTF 题目面板。
  - 优化 `cyber help / --help` CLI 命令帮助与示例。

---

## [0.3.0] - 2026-10-01

### Added
- MCP (Model Context Protocol) 标准外部服务接入与集成 (stdio/sse/http 传输支持)。
- Skill 技能管理体系与渐进式系统提示词披露。
- 自定义工具（Custom Tools）规范解析与运行时动态执行。

---

## [0.2.0] - 2026-09-28

### Added
- 完整 TUI (Ratatui) 交互界面，包括 Chat、Workflow 编排、Dashboard 及 Settings 模态层。
- 多服务商管理（OpenAI, DeepSeek, Anthropic, Ollama 等）与实时模型选择。
- 长期记忆（Memory Rules）跨会话与项目级隔离存储。

---

## [0.1.0] - 2026-09-20

### Added
- Cyber Master 基础工程骨架与跨平台 CLI。
- 基础命令行工具执行、文件读写与核心 Agent 思考驱动循环。
