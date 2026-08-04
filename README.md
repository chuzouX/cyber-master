# Cyber Master

> 基于 Rust 的网络安全智能体 CLI 终端：支持流式对话、DAG 工作流编排、MCP/Skill 工具集成与实时监控日志分析。TUI-first，无 Web Dashboard。

[![Rust](https://img.shields.io/badge/Rust-1.96%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE)
[![Status](https://img.shields.io/badge/状态-P1%20完成%20%2F%20P2%20进行中-blue)](./docs/PROGRESS.md)

---

## 目录

- [特性](#特性)
- [架构概览](#架构概览)
- [快速开始](#快速开始)
- [配置](#配置)
- [CLI 用法](#cli-用法)
- [Workspace 结构](#workspace-结构)
- [开发路线图](#开发路线图)
- [开发指南](#开发指南)
- [技术栈](#技术栈)
- [许可证](#许可证)

---

## 特性

- **双模式**
  - **Chat 模式**：类 Claude Code 的全屏流式对话，支持工具调用、斜杠命令、上下文注入
  - **Workflow 模式**：类 ComfyUI 的节点 DAG 编排，键盘 + 鼠标拖拽，流式资产传递
- **零配置启动**：首次启动自动创建 `~/.cyber/` 目录结构与默认配置
- **三层配置**：全局 `~/.cyber/` → 项目级 `.cyber/` → `.cyber.md` frontmatter，逐层覆盖
- **统一工具表**：内置工具、MCP 工具、Skill 对 agent 同质暴露，模型无需区分
- **实时可观测**：工作流进度、节点级日志、资产/漏洞统计实时刷新
- **多 LLM Provider**：OpenAI / Anthropic / Ollama 并存，配置切换
- **安全护栏**：`.cyber.md` rules 注入系统提示词 + 运行时目标白名单/危险命令双保险
- **跨平台纯终端**：ratatui + crossterm，无外部 GUI 依赖

---

## 架构概览

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

依赖方向严格单向：`app → tui / agent / workflow → core → storage`，禁止反向依赖。

详细设计见 [docs/DESIGN.md](./docs/DESIGN.md)。

---

## 快速开始

### 前置条件

- **Rust 1.96+**（推荐用 [rustup](https://rustup.rs/) 安装 stable 工具链）
- 支持 Windows / macOS / Linux

### 构建与运行

```bash
# 克隆仓库
git clone git@github.com:chuzouX/cyber-master.git
cd cyber-master

# Debug 构建
cargo run

# Release 构建（启用 LTO + strip）
cargo run --release
```

首次运行会在 `~/.cyber/` 自动创建默认配置文件，然后渲染 TUI 启动页。

---

## 配置

### 三层配置层次

| 层级 | 路径 | 作用 |
| --- | --- | --- |
| 全局（用户级） | `~/.cyber/config.toml` | 主题、默认模式、并发数、超时等 |
| 项目级覆盖 | `./.cyber/config.toml` | 覆盖全局设置（覆盖项会在 Settings 页提示） |
| 项目说明 | `./.cyber.md` | YAML frontmatter（scope/rules）+ Markdown 正文，作为上下文注入 |

### `~/.cyber/` 目录结构

```
~/.cyber/
├── config.toml              # 全局设置
├── providers.toml           # LLM 提供商（OpenAI/Anthropic/Ollama 默认模板）
├── mcp/servers.toml         # MCP server 注册表
├── skills/                  # 已安装 Skill
├── workflows/               # 保存的工作流模板（.cyberflow）
├── sessions/                # 会话状态快照
├── history.db               # SQLite：聊天/命令历史
├── assets.db                # SQLite：资产/漏洞
├── logs/                    # 应用日志（按日期滚动）
└── reports/                 # 生成的报告（md/html/json）
```

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

## CLI 用法

```bash
cyber [OPTIONS]

Options:
      --cwd <CWD>          工作目录（默认当前目录，决定 .cyber.md / .cyber/ 检测位置）
      --log-level <LEVEL>  日志级别（覆盖 RUST_LOG，如 debug/info/warn）
  -h, --help               帮助
  -V, --version            版本
```

启动后进入 TUI，键位如下：

| 键位 | 作用 |
| --- | --- |
| `↑` / `↓` | 导航选中项 |
| `Enter` | 确认 |
| `Tab` | Chat / Workflow / Dashboard 视图循环切换 |
| `s` | 打开 Settings 设置页（全局） |
| `←` / `→` | Settings 内切换段落 / 调整字段值 |
| `Esc` | 返回上一模式（Settings dirty 时双击回退） |
| `q` / `Ctrl+C` | 退出 |

---

## Workspace 结构

```
cyber_master/
├── Cargo.toml                 # workspace 根
├── crates/
│   ├── cyber-app/             # 主二进制 main.rs，装配 + 事件循环
│   ├── cyber-core/            # 配置、路径、错误类型、AppState、三层加载
│   ├── cyber-tui/             # ratatui UI（布局/组件/主题/事件循环）
│   ├── cyber-agent/           # LLM provider、chat、tool-calling、agent loop
│   ├── cyber-workflow/        # DAG 引擎、节点定义、执行器、调度
│   ├── cyber-mcp/             # MCP 客户端（stdio/SSE/HTTP）
│   ├── cyber-skills/          # Skill 加载与调用
│   ├── cyber-tools/           # 安全工具封装（subfinder/nmap/nuclei…）
│   └── cyber-storage/         # SQLite 历史会话/资产/日志持久化
└── docs/
    ├── DESIGN.md              # 完整设计文档
    └── PROGRESS.md            # 实施进度跟踪
```

---

## 开发路线图

| 阶段 | 内容 | 状态 |
| --- | --- | :---: |
| **P1 骨架** | workspace + 配置层 + 启动状态机 + Welcome 页 + TUI 主循环 + Settings 页 | ✅ 完成 |
| **P2 Chat** | LLM provider、流式对话、工具调用、斜杠命令、上下文注入 | 🟡 进行中 |
| **P3 MCP+Skill** | MCP 客户端、Skill 加载、统一工具表 | ⚪ 未开始 |
| **P4 Workflow 引擎** | DAG 定义、画布编辑、执行器、并行调度 | ⚪ 未开始 |
| **P5 监控+日志** | Dashboard、节点日志、日志分析、断点续跑 | ⚪ 未开始 |
| **P6 安全工具** | cyber-tools 封装、工具发现、docker 兜底 | ⚪ 未开始 |
| **P7 打磨** | 主题、动画、报告导出、文档、CI | ⚪ 未开始 |

实时进度详见 [docs/PROGRESS.md](./docs/PROGRESS.md)。

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
3. 在 provider 工厂函数注册匹配分支

### 添加新 TUI 视图

1. 在 `crates/cyber-tui/src/views/` 新增 `<view>.rs`
2. 在 `Mode` 枚举追加变体
3. 在 `event.rs` 补充键位映射
4. 在主 render 分发

---

## 技术栈

| 领域 | 选型 |
| --- | --- |
| 语言 | Rust 1.96（edition 2021） |
| TUI | ratatui 0.30 + crossterm 0.28（event-stream） |
| 多行输入 | tui-textarea-2 0.12（crossterm_0_28 feature） |
| 异步 | tokio（full） |
| HTTP / 流式 | reqwest 0.12（rustls-tls + stream） + futures + bytes |
| 序列化 | serde + serde_json + serde_yaml + toml 0.8 |
| 错误 | thiserror + color-eyre + anyhow |
| 日志 | tracing + tracing-subscriber（env-filter） |
| CLI | clap 4（derive） |
| 存储 | SQLite（P5 接入） |
| 配置路径 | dirs 5 |

---

## 许可证

[MIT](./LICENSE)
