# Cyber Master

> 基于 Rust 的网络安全智能体 CLI 终端：流式对话、CTF 协作、MCP/Skill 工具集成与 DAG 工作流编排。TUI-first，无 Web Dashboard。

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
- **工作流 DAG**：节点编排、并行执行、tokio mpsc 流式资产传递、断点续跑
- **上下文管理**：自动压缩（compact）、历史持久化、跨会话读取、输入历史回溯
- **思考强度切换**：low / middle / high / max / auto 五档，动态注入系统提示词
- **安全护栏**：`.cyber.md` rules 注入系统提示词 + 运行时危险命令拦截
- **多主题**：cyberpunk / catppuccin / tokyo-night / dracula / gruvbox / nord
- **跨平台**：Windows（cmd /C）/ macOS / Linux（sh -c），系统提示词自动注入平台信息

---

## 架构概览

```
┌──────────────────────────────────────────────────────────┐
│  Presentation (ratatui TUI)                              │
│  ChatView │ WorkflowEditor │ CtfPanel │ Settings │ About │
├──────────────────────────────────────────────────────────┤
│  Application (模式路由 / 事件分发 / 粘贴检测 / 输入历史)  │
├──────────────────────────────────────────────────────────┤
│  Domain                                                  │
│  Agent(LLM+ToolCall+SSE) │ WorkflowEngine(DAG) │ Chat    │
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

### 从源码构建（需 Rust 1.75+）

```bash
git clone https://github.com/chuzouX/cyber-master.git
cd cyber-master
cargo build --release
```

构建产物在 `target/release/cyber-app`（或 Windows 下的 `cyber-app.exe`）。

### 首次启动

首次运行自动在 `~/.cyber/` 下生成配置：

```
~/.cyber/
├── config.toml          # 全局配置（主题、模式、agent 参数）
├── providers.toml       # LLM 提供商配置
├── mcp_servers.toml     # MCP server 配置
├── skills/              # Skill 目录（.md 文件）
├── ctf/                 # CTF 题目数据
├── ctf/writeup/         # CTF writeup 输出
├── history/             # 对话历史（按 cwd hash 分文件）
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
| 项目说明 | `./.cyber.md` | YAML frontmatter（scope/rules）+ Markdown 正文，注入系统提示词 |

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

`rules` 字段会注入 agent 系统提示词作为安全护栏。

---

## 用法

### CLI 启动

```bash
cyber [OPTIONS]

Options:
      --cwd <CWD>          工作目录（默认当前目录，决定 .cyber.md 检测位置）
      --log-level <LEVEL>  日志级别（覆盖 RUST_LOG，如 debug/info/warn）
  -h, --help               帮助
  -V, --version            版本
```

### 斜杠命令

| 命令 | 说明 |
| --- | --- |
| `/help` | 显示帮助 |
| `/clear` | 清空对话历史 |
| `/model [provider]` | 选择 provider + model |
| `/provider <list\|add\|edit\|use\|remove>` | 管理服务商 |
| `/tools` | 列出可用工具 |
| `/skill <name\|list>` | 查看 Skill 详细说明 |
| `/mcp <list\|status>` | 查看 MCP server 状态 |
| `/think [low\|middle\|high\|max\|auto]` | 切换思考强度 |
| `/max_steps <N>` | 工具调用步数上限 |
| `/compact [instructions]` | 手动压缩上下文 |
| `/ctf <enable\|disable\|add\|list\|writeup>` | CTF 模式管理 |
| `/sessions <list\|read\|new>` | 会话管理 |
| `/mode <name>` | 切换模式 |
| `/new` | 新建会话 |
| `/cancel` | 取消当前生成 |
| `/quit` | 退出 |

### 快捷键

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

### 粘贴检测

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

开启后：
- 系统提示词注入 CTF 测试方法论（优先级 + 工具使用规范）
- `ctf_challenge` 工具自动注册/更新题目状态
- 题目面板实时显示进度
- 解出后记录 flag 和关键知识点
- `/ctf writeup` 生成解题报告

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
