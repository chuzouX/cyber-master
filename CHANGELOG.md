# Changelog

All notable changes to Cyber Master will be documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Added
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
