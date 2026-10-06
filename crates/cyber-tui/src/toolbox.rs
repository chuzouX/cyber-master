//! 「工具库」标签页与 `/toolbox` 命令的 UI 无关逻辑。
//!
//! 1. 自定义工具表单字段编解码（`params` / `tags` 的唯一实现，供 cli_commands 复用）。
//! 2. AI 智能扫描：探测本机安全工具 → 采集 `--help` → 模型判定 Agent 可用性 →
//!    写入 `~/.cyber/tools/*.toml`。进度通过 `AgentEvent::Notice` 回报（无 stderr 输出）。
//!
//! 写盘策略：仅「本地已发现 + 模型判定 Agent 可用 + 同名文件不存在」的条目落盘；
//! 已存在的记入 `skipped_existing` 不覆盖，需人工的记入 `manual` 只汇报；
//! `preview` 模式一个都不写。

use std::path::Path;

use color_eyre::eyre::{bail, Result};
use cyber_core::{CustomToolConfig, CustomToolParam, ProviderConfig};
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use cyber_agent::AgentEvent;

/// 解析表单 `params` 字段。
///
/// 语法：`entry (";" entry)*`，`entry := name "|" ("r"|"o") ["|" description] ["|" default]`；
/// 空串 = 无参数。
pub(crate) fn parse_params_field(raw: &str) -> Result<Vec<CustomToolParam>> {
    let mut params = Vec::new();
    for entry in raw.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut parts = entry.split('|').map(str::trim);
        let name = parts.next().unwrap_or("").to_string();
        if name.is_empty() {
            bail!("参数名不能为空（格式：名称|r或o|说明|默认值，多个参数用分号分隔）");
        }
        let kind = parts.next().unwrap_or("o").to_ascii_lowercase();
        let required = match kind.as_str() {
            "r" | "required" | "必填" => true,
            "" | "o" | "optional" | "选填" => false,
            other => bail!("参数 {name} 的必填标记只能是 r 或 o，得到 {other}"),
        };
        let description = parts.next().unwrap_or("").to_string();
        let default = parts
            .next()
            .map(str::to_string)
            .filter(|value| !value.is_empty());
        if parts.next().is_some() {
            bail!("参数 {name} 格式错误：最多四段（名称|r或o|说明|默认值）");
        }
        params.push(CustomToolParam {
            name,
            description,
            required,
            default,
        });
    }
    Ok(params)
}

/// `parse_params_field` 的逆向格式化（`params` 字段初值回填）。
pub(crate) fn format_params_field(params: &[CustomToolParam]) -> String {
    params
        .iter()
        .map(|param| {
            let mut parts = vec![
                param.name.clone(),
                if param.required { "r" } else { "o" }.to_string(),
            ];
            if !param.description.is_empty() {
                parts.push(param.description.clone());
            } else if param.default.is_some() {
                parts.push(String::new());
            }
            if let Some(default) = &param.default {
                parts.push(default.clone());
            }
            parts.join("|")
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// 解析表单 `tags` 字段：按逗号切分、trim、丢弃空项。
pub(crate) fn parse_tags_field(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

/// `parse_tags_field` 的逆向格式化（`tags` 字段初值回填）。
pub(crate) fn format_tags_field(tags: &[String]) -> String {
    tags.join(",")
}

/// 工具在 LLM Agent 自主任务中的适用性分类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolSuitability {
    /// 适合 Agent 自主调用（纯 CLI、支持参数占位符、非交互批处理、有明确终止条件）
    AgentCompatible,
    /// 需人工操作（GUI 视窗、持续交互式控制台、缺乏非交互模式等）
    ManualOnly,
}

/// 经 AI 分析评估后的本地安全工具模型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnalyzedTool {
    name: String,
    suitability: ToolSuitability,
    reason: String,
    description: String,
    command: String,
    parameters: Vec<CustomToolParam>,
    manual_advice: Option<String>,
}

impl AnalyzedTool {
    fn is_agent_compatible(&self) -> bool {
        self.suitability == ToolSuitability::AgentCompatible
    }

    fn to_custom_tool_config(&self) -> CustomToolConfig {
        CustomToolConfig {
            name: self.name.clone(),
            description: self.description.clone(),
            command: self.command.clone(),
            tags: vec!["security".into(), "scanned".into()],
            parameters: self.parameters.clone(),
        }
    }
}

const TOOL_CLASSIFICATION_SYSTEM_PROMPT: &str = "\
你是一个网络安全渗透测试工具体系与 LLM 自动化架构专家。\n\
请分析用户提供的工具清单、帮助文档或提示词，对每一个识别到的工具做出【Agent 适用性判定】。\n\
\n\
判定标准：\n\
1. can_agent_use = true：仅限于能够无头（Headless）运行、可通过纯命令行参数全自动执行、支持批处理且有明确终止条件的 CLI 工具（如 nmap, sqlmap, fscan, nuclei, dirsearch, httpx, ffuf, subfinder 等）。\n\
   - 必须提供 command 命令行模板（保留原执行路径，核心目标/输入参数使用 {param} 占位符包裹）。\n\
   - 必须提供 parameters 数组，包含所有占位符参数说明、必填/选填状态及默认值。\n\
   - reason: 简述为什么适合 Agent 自动化调用（如\"纯命令行参数控制，支持非交互式批量执行\"）。\n\
2. can_agent_use = false：所有 GUI 图形视窗程序（如 Goby, Burp Suite, Wireshark, Postman）、所有持续交互式控制台（如 msfconsole, 交互式 shell/gdb）、或缺少非交互模式必须人工界面的工具。\n\
   - reason: 详细说明不可作为 Agent 工具的具体原因（如\"图形化界面软件(GUI)，无头子进程调用会导致永久阻塞\"或\"交互式控制台，直接执行会导致进程挂起\"）。\n\
   - manual_advice: 提供针对安全人员的人工操作与协作建议（如\"适合在安全人员桌面独立视窗中运行，不应加入 Agent 工具表\"）。\n\
\n\
输出格式要求：\n\
请务必输出为一个 ```toml 代码块，格式示例如下：\n\
```toml\n\
[[tools]]\n\
name = \"sqlmap\"\n\
can_agent_use = true\n\
reason = \"纯命令行控制，支持 --batch 非交互自动化利用\"\n\
description = \"自动化 SQL 注入检测与利用工具\"\n\
command = \"python D:/tools/sqlmap/sqlmap.py -u {url} --batch {extra}\"\n\
tags = [\"sqli\", \"web\"]\n\
[[tools.parameters]]\n\
name = \"url\"\n\
description = \"目标 URL 地址\"\n\
required = true\n\
[[tools.parameters]]\n\
name = \"extra\"\n\
description = \"额外参数\"\n\
required = false\n\
default = \"--smart\"\n\
\n\
[[tools]]\n\
name = \"goby\"\n\
can_agent_use = false\n\
reason = \"图形化视窗软件 (GUI)，需人工在界面点击操作，Agent 在后台无头子进程调用会导致永久阻塞无响应\"\n\
description = \"图形化网络资产梳理与漏洞探测平台\"\n\
manual_advice = \"适合安全人员在本地桌面独立视窗中运行，不应封装为 Agent 自动化工具\"\n\
```\n\
严禁输出任何多余的解释、前言或总结文字，只输出 ```toml ... ```。";

#[derive(Debug, Clone, serde::Deserialize)]
struct RawAiToolItem {
    #[serde(default)]
    name: String,
    #[serde(default)]
    can_agent_use: Option<bool>,
    #[serde(default)]
    suitability: Option<String>,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    command: String,
    #[serde(default)]
    #[allow(dead_code)]
    tags: Vec<String>,
    #[serde(default)]
    parameters: Vec<CustomToolParam>,
    #[serde(default)]
    manual_advice: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct RawAiToolBatch {
    #[serde(default)]
    tools: Vec<RawAiToolItem>,
}

fn parse_analyzed_tools(reply: &str) -> Vec<AnalyzedTool> {
    let blocks = extract_all_toml_blocks(reply);
    let mut raw_items = Vec::new();

    let try_parse_block = |code: &str, items: &mut Vec<RawAiToolItem>| {
        if let Ok(batch) = toml::from_str::<RawAiToolBatch>(code) {
            if !batch.tools.is_empty() {
                items.extend(batch.tools);
                return true;
            }
        }
        if let Ok(single) = toml::from_str::<RawAiToolItem>(code) {
            if !single.name.trim().is_empty() {
                items.push(single);
                return true;
            }
        }
        if let Ok(cfg) = toml::from_str::<CustomToolConfig>(code) {
            if !cfg.name.trim().is_empty() {
                items.push(RawAiToolItem {
                    name: cfg.name,
                    can_agent_use: Some(true),
                    suitability: None,
                    reason: "支持纯命令行运行与参数化调用".into(),
                    description: cfg.description,
                    command: cfg.command,
                    tags: cfg.tags,
                    parameters: cfg.parameters,
                    manual_advice: None,
                });
                return true;
            }
        }
        false
    };

    if !blocks.is_empty() {
        for block in &blocks {
            try_parse_block(block, &mut raw_items);
        }
    } else {
        try_parse_block(reply, &mut raw_items);
    }

    raw_items
        .into_iter()
        .filter_map(|raw| {
            let name = raw.name.trim().to_string();
            if name.is_empty() {
                return None;
            }

            let is_compatible = if let Some(flag) = raw.can_agent_use {
                flag
            } else if let Some(suitability) = raw.suitability.as_deref() {
                let lower = suitability.to_ascii_lowercase();
                lower.contains("agent") || lower.contains("cli") || lower.contains("compat")
            } else {
                let lower = raw.reason.to_ascii_lowercase();
                let is_gui_or_interactive = lower.contains("gui")
                    || lower.contains("图形")
                    || lower.contains("交互")
                    || lower.contains("视窗");
                !is_gui_or_interactive && !raw.command.trim().is_empty()
            };

            let suitability = if is_compatible {
                ToolSuitability::AgentCompatible
            } else {
                ToolSuitability::ManualOnly
            };

            let reason = if raw.reason.trim().is_empty() {
                match suitability {
                    ToolSuitability::AgentCompatible => {
                        "支持纯命令行运行与非交互批处理".to_string()
                    }
                    ToolSuitability::ManualOnly => {
                        "图形界面程序或持续交互式控制台，不适宜 Agent 自主调用".to_string()
                    }
                }
            } else {
                raw.reason.trim().to_string()
            };

            Some(AnalyzedTool {
                name,
                suitability,
                reason,
                description: raw.description.trim().to_string(),
                command: raw.command.trim().to_string(),
                parameters: raw.parameters,
                manual_advice: raw.manual_advice.map(|s| s.trim().to_string()),
            })
        })
        .collect()
}

fn extract_all_toml_blocks(content: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut search = content;
    while let Some(start) = search.find("```toml") {
        let after = &search[start + 7..];
        if let Some(end) = after.find("```") {
            blocks.push(after[..end].trim().to_string());
            search = &after[end + 3..];
        } else {
            break;
        }
    }
    if blocks.is_empty() {
        let mut search = content;
        while let Some(start) = search.find("```") {
            let after = &search[start + 3..];
            if let Some(end) = after.find("```") {
                blocks.push(after[..end].trim().to_string());
                search = &after[end + 3..];
            } else {
                break;
            }
        }
    }
    blocks
}

fn run_command_with_timeout(
    mut command: std::process::Command,
    timeout: std::time::Duration,
) -> Option<std::process::Output> {
    use std::io::Read;
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let mut child = command.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_end(&mut stdout);
                }
                if let Some(mut err) = child.stderr.take() {
                    let _ = err.read_to_end(&mut stderr);
                }
                return Some(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
}

fn find_installed_binary(name: &str) -> Option<String> {
    let cmd = if cfg!(windows) { "where" } else { "which" };
    if let Ok(output) = std::process::Command::new(cmd).arg(name).output() {
        if output.status.success() {
            let out_str = String::from_utf8_lossy(&output.stdout);
            let first_line = out_str.lines().next().unwrap_or(name).trim();
            if !first_line.is_empty() {
                return Some(first_line.to_string());
            }
        }
    }
    None
}

fn fetch_tool_help(name: &str) -> Option<String> {
    let run = |flag: &str| -> Option<String> {
        let mut cmd = std::process::Command::new(name);
        cmd.arg(flag);
        let output = run_command_with_timeout(cmd, std::time::Duration::from_millis(2500))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let text = if stdout.trim().len() > 50 {
            stdout.to_string()
        } else {
            stderr.to_string()
        };
        if text.trim().len() > 20 {
            let lines: Vec<&str> = text.lines().take(200).collect();
            Some(lines.join("\n"))
        } else {
            None
        }
    };
    run("-h").or_else(|| run("--help"))
}

fn run_help_for_command(cmd_prefix: &str, tool_name: &str) -> Option<String> {
    let trimmed = cmd_prefix.trim();
    let (prog, args) = if let Some(rest) = trimmed.strip_prefix("python ") {
        ("python", vec![rest.trim().trim_matches('"')])
    } else if let Some(rest) = trimmed.strip_prefix("java -jar ") {
        ("java", vec!["-jar", rest.trim().trim_matches('"')])
    } else {
        (trimmed.trim_matches('"'), Vec::new())
    };

    let run_flag = |flag: &str| -> Option<String> {
        let mut command = std::process::Command::new(prog);
        for arg in &args {
            command.arg(arg);
        }
        command.arg(flag);
        let output = run_command_with_timeout(command, std::time::Duration::from_millis(2500))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let text = if stdout.trim().len() > 50 {
            stdout.to_string()
        } else {
            stderr.to_string()
        };
        if text.trim().len() > 20 {
            let lines: Vec<&str> = text.lines().take(200).collect();
            Some(lines.join("\n"))
        } else {
            None
        }
    };

    run_flag("-h")
        .or_else(|| run_flag("--help"))
        .or_else(|| fetch_tool_help(tool_name))
}

#[derive(Debug, Clone)]
struct DiscoveredCandidate {
    name: String,
    command: String,
}

fn scan_directory_for_candidates(dir: &Path) -> Vec<DiscoveredCandidate> {
    let mut candidates = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return candidates;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                let ext_lower = ext.to_ascii_lowercase();
                if ["exe", "bat", "cmd", "sh", "py", "jar"].contains(&ext_lower.as_str()) {
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("tool")
                        .to_string();
                    let command = if ext_lower == "py" {
                        format!("python \"{}\"", path.display())
                    } else if ext_lower == "jar" {
                        format!("java -jar \"{}\"", path.display())
                    } else {
                        format!("\"{}\"", path.display())
                    };
                    candidates.push(DiscoveredCandidate { name, command });
                }
            }
        } else if path.is_dir() {
            let folder_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if let Ok(sub_entries) = std::fs::read_dir(&path) {
                for sub in sub_entries.flatten() {
                    let sub_path = sub.path();
                    if sub_path.is_file() {
                        let stem = sub_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                        let ext = sub_path
                            .extension()
                            .and_then(|s| s.to_str())
                            .unwrap_or("")
                            .to_ascii_lowercase();
                        if (stem.eq_ignore_ascii_case(&folder_name)
                            || stem == "main"
                            || stem == "run")
                            && ["py", "exe", "jar", "bat", "sh"].contains(&ext.as_str())
                        {
                            let name = folder_name.clone();
                            let command = if ext == "py" {
                                format!("python \"{}\"", sub_path.display())
                            } else if ext == "jar" {
                                format!("java -jar \"{}\"", sub_path.display())
                            } else {
                                format!("\"{}\"", sub_path.display())
                            };
                            candidates.push(DiscoveredCandidate { name, command });
                            break;
                        }
                    }
                }
            }
        }
    }

    candidates
}

fn fetch_candidates_help(
    candidates: &[DiscoveredCandidate],
    notify: &mut dyn FnMut(String),
) -> Vec<(String, String, Option<String>)> {
    let mut results = Vec::with_capacity(candidates.len());
    let total = candidates.len();
    if total == 0 {
        notify("探测 --help: 无候选工具".into());
        return results;
    }
    for (index, candidate) in candidates.iter().enumerate() {
        notify(format!(
            "探测 --help: {} ({}/{})",
            candidate.name,
            index + 1,
            total
        ));
        let help = run_help_for_command(&candidate.command, &candidate.name);
        results.push((candidate.name.clone(), candidate.command.clone(), help));
    }
    results
}

/// 扫描结果汇总（写盘明细 + 跳过 + 需人工项）。
#[derive(Debug, Clone, Default)]
pub(crate) struct ScanReport {
    /// (name, command)：非预览模式为已写入，预览模式为将会写入。
    pub written: Vec<(String, String)>,
    pub skipped_existing: Vec<String>,
    /// (name, reason)：模型判定需人工操作，仅汇报不写盘。
    pub manual: Vec<(String, String)>,
    pub preview: bool,
}

/// 扫描结论的多行文本（对话区输出）。
pub(crate) fn format_report(report: &ScanReport) -> String {
    let mut lines = Vec::new();
    if report.preview {
        lines.push(format!(
            "预览模式：未写入任何文件（{} 个候选通过分类）",
            report.written.len()
        ));
    } else {
        lines.push(format!(
            "已写入 {} 个自定义工具（重启后生效，/toolbox list 查看）",
            report.written.len()
        ));
    }
    for (name, command) in &report.written {
        let mark = if report.preview { "+" } else { "✓" };
        lines.push(format!("{mark} {name} — {command}"));
    }
    for name in &report.skipped_existing {
        lines.push(format!("= {name}（已存在，跳过）"));
    }
    for (name, reason) in &report.manual {
        lines.push(format!("! {name} — 需人工：{reason}"));
    }
    if lines.len() == 1 && !report.preview {
        lines.push("（未发现可自动接入的 Agent 工具）".into());
    }
    lines.join("\n")
}

fn notify(events: &mpsc::UnboundedSender<AgentEvent>, message: impl Into<String>) {
    let _ = events.send(AgentEvent::Notice(message.into()));
}

/// 单轮模型调用（可选取消）。
async fn ask_llm_cancellable(
    cfg: &ProviderConfig,
    system_prompt: &str,
    user_prompt: &str,
    events: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<String> {
    tokio::select! {
        biased;
        _ = &mut *cancel => bail!("已取消工具扫描"),
        _ = events.closed() => bail!("已取消工具扫描"),
        reply = ask_llm(cfg, system_prompt, user_prompt) => reply,
    }
}

async fn ask_llm(cfg: &ProviderConfig, system_prompt: &str, user_prompt: &str) -> Result<String> {
    let provider = cyber_agent::provider_factory(cfg, false)?;
    let request = cyber_agent::StreamRequest::new(vec![cyber_agent::Message::user(user_prompt)])
        .with_system(system_prompt);
    let mut stream = provider.stream(request);
    let mut output = String::new();
    while let Some(event) = stream.next().await {
        match event {
            cyber_agent::StreamEvent::Delta(text) => output.push_str(&text),
            cyber_agent::StreamEvent::Error(err) => bail!("模型返回错误: {err}"),
            _ => {}
        }
    }
    if output.trim().is_empty() {
        bail!("模型返回内容为空");
    }
    Ok(output)
}

async fn evaluate_tools_with_ai(
    cfg: &ProviderConfig,
    tools: &[(String, String, Option<String>)],
    events: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<Vec<AnalyzedTool>> {
    let mut user_prompt =
        String::from("请评估以下本地安全工具的 Agent 适用性，并输出 TOML 配置块：\n\n");
    for (name, cmd, help) in tools {
        user_prompt.push_str(&format!("--- 工具: {name} ---\n执行命令: {cmd}\n"));
        if let Some(help) = help {
            let summary: Vec<&str> = help.lines().take(40).collect();
            user_prompt.push_str(&format!("帮助文档摘要:\n{}\n\n", summary.join("\n")));
        } else {
            user_prompt.push_str(
                "帮助文档: (无法通过 -h/--help 获取输出，可能为图形界面程序或非标准工具)\n\n",
            );
        }
    }
    notify(events, "调用模型分类…");
    let reply = ask_llm_cancellable(
        cfg,
        TOOL_CLASSIFICATION_SYSTEM_PROMPT,
        &user_prompt,
        events,
        cancel,
    )
    .await?;
    Ok(parse_analyzed_tools(&reply))
}

async fn evaluate_freeform_with_ai(
    cfg: &ProviderConfig,
    user_input: &str,
    events: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<Vec<AnalyzedTool>> {
    notify(events, "调用模型分类…");
    let reply = ask_llm_cancellable(
        cfg,
        TOOL_CLASSIFICATION_SYSTEM_PROMPT,
        user_input,
        events,
        cancel,
    )
    .await?;
    Ok(parse_analyzed_tools(&reply))
}

/// 分流写入 + 汇总（唯一写盘入口）。
fn commit_analyzed(
    analyzed: Vec<AnalyzedTool>,
    tools_dir: &Path,
    preview: bool,
    events: &mpsc::UnboundedSender<AgentEvent>,
) -> ScanReport {
    let mut report = ScanReport {
        preview,
        ..Default::default()
    };
    let (compatible, manual): (Vec<_>, Vec<_>) = analyzed
        .into_iter()
        .partition(AnalyzedTool::is_agent_compatible);
    notify(
        events,
        format!(
            "分类完成：可用 {} / 需人工 {}",
            compatible.len(),
            manual.len()
        ),
    );

    for tool in compatible {
        if tools_dir.join(format!("{}.toml", tool.name)).exists() {
            report.skipped_existing.push(tool.name);
            continue;
        }
        if !preview {
            if let Err(error) =
                cyber_core::save_custom_tool(tools_dir, &tool.to_custom_tool_config())
            {
                report
                    .manual
                    .push((tool.name, format!("写入失败: {error}")));
                continue;
            }
        }
        report.written.push((tool.name, tool.command));
    }
    for tool in manual {
        report.manual.push((tool.name, tool.reason));
    }
    report
}

/// AI 智能扫描：探测候选 → 采集帮助 → 模型分类 → 按策略写盘。
pub(crate) async fn run_scan(
    cfg: &ProviderConfig,
    tools_dir: &Path,
    target: Option<&str>,
    preview: bool,
    events: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<ScanReport> {
    let analyzed = match target.map(str::trim).filter(|t| !t.is_empty()) {
        Some(raw) => {
            let input = raw.trim_matches('"');
            let input_path = Path::new(input);
            if input_path.is_dir() {
                notify(events, format!("扫描目录：{}", input_path.display()));
                let candidates = scan_directory_for_candidates(input_path);
                notify(
                    events,
                    format!("扫描中：已发现 {} 个候选", candidates.len()),
                );
                if candidates.is_empty() {
                    notify(events, "目录中未检索到脚本或可执行文件，转为提示词推理");
                    evaluate_freeform_with_ai(
                        cfg,
                        &format!("我的安全工具存放在目录: {input}"),
                        events,
                        cancel,
                    )
                    .await?
                } else {
                    let mut notify_fn = |message: String| notify(events, message);
                    let probed = fetch_candidates_help(&candidates, &mut notify_fn);
                    evaluate_tools_with_ai(cfg, &probed, events, cancel).await?
                }
            } else if input_path.is_file()
                || input.starts_with("python ")
                || input.starts_with("java -jar ")
            {
                let name = input_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("tool")
                    .to_string();
                notify(events, format!("单文件/指定命令：{name}"));
                let candidate = DiscoveredCandidate {
                    name,
                    command: input.to_string(),
                };
                let mut notify_fn = |message: String| notify(events, message);
                let probed = fetch_candidates_help(&[candidate], &mut notify_fn);
                evaluate_tools_with_ai(cfg, &probed, events, cancel).await?
            } else {
                evaluate_freeform_with_ai(cfg, input, events, cancel).await?
            }
        }
        None => {
            let mut candidates = Vec::new();
            for name in [
                "nmap",
                "sqlmap",
                "dirsearch",
                "subfinder",
                "nuclei",
                "nikto",
                "hydra",
                "gobuster",
                "ffuf",
                "whatweb",
                "wpscan",
                "masscan",
                "fscan",
                "xray",
                "httpx",
            ] {
                if let Some(bin_path) = find_installed_binary(name) {
                    candidates.push(DiscoveredCandidate {
                        name: name.to_string(),
                        command: bin_path,
                    });
                }
            }
            for common_dir in [
                "D:\\tools",
                "C:\\tools",
                "D:\\SecTools",
                "C:\\SecTools",
                "D:\\Program Files (x86)\\Nmap",
            ] {
                let dir = Path::new(common_dir);
                if dir.is_dir() {
                    for candidate in scan_directory_for_candidates(dir) {
                        if !candidates
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(&candidate.name))
                        {
                            candidates.push(candidate);
                        }
                    }
                }
            }
            notify(
                events,
                format!("扫描中：已发现 {} 个候选", candidates.len()),
            );
            if candidates.is_empty() {
                notify(
                    events,
                    "未在系统 PATH 或预设目录中检测到常见安全工具；可用 /toolbox scan <目录> 指定路径",
                );
                Vec::new()
            } else {
                let mut notify_fn = |message: String| notify(events, message);
                let probed = fetch_candidates_help(&candidates, &mut notify_fn);
                evaluate_tools_with_ai(cfg, &probed, events, cancel).await?
            }
        }
    };

    Ok(commit_analyzed(analyzed, tools_dir, preview, events))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_field_round_trips_and_rejects_bad_entries() {
        let params = parse_params_field("target|r|目标|127.0.0.1;port|o||80").unwrap();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].name, "target");
        assert!(params[0].required);
        assert_eq!(params[0].default.as_deref(), Some("127.0.0.1"));
        assert_eq!(params[1].name, "port");
        assert!(!params[1].required);
        assert_eq!(params[1].description, "");
        assert_eq!(params[1].default.as_deref(), Some("80"));

        let formatted = format_params_field(&params);
        assert_eq!(formatted, "target|r|目标|127.0.0.1;port|o||80");
        assert_eq!(parse_params_field(&formatted).unwrap(), params);

        assert!(parse_params_field("").unwrap().is_empty());
        assert!(parse_params_field("   ").unwrap().is_empty());
        assert!(parse_params_field("|r").is_err());
        assert!(parse_params_field("host|x").is_err());
        assert!(parse_params_field("host|r|d|default|extra").is_err());
    }

    #[test]
    fn tags_field_splits_trims_and_drops_empties() {
        assert_eq!(parse_tags_field(""), Vec::<String>::new());
        assert_eq!(parse_tags_field("a, b ,,c"), vec!["a", "b", "c"]);
        assert_eq!(format_tags_field(&["a".into(), "b".into()]), "a,b");
    }

    #[test]
    fn report_formats_written_skipped_and_manual_entries() {
        let report = ScanReport {
            written: vec![("nmap".into(), "nmap -sV {target}".into())],
            skipped_existing: vec!["fscan".into()],
            manual: vec![("goby".into(), "GUI".into())],
            preview: false,
        };
        let text = format_report(&report);
        assert!(text.starts_with("已写入 1 个自定义工具"));
        assert!(text.contains("✓ nmap — nmap -sV {target}"));
        assert!(text.contains("= fscan（已存在，跳过）"));
        assert!(text.contains("! goby — 需人工：GUI"));

        let preview = ScanReport {
            preview: true,
            ..report
        };
        let text = format_report(&preview);
        assert!(text.contains("预览模式"));
        assert!(text.contains("+ nmap — nmap -sV {target}"));
    }

    #[test]
    fn parse_analyzed_tools_classifies_agent_and_manual_tools() {
        let reply = r#"
```toml
[[tools]]
name = "sqlmap"
can_agent_use = true
reason = "纯命令行控制，支持 --batch 非交互批处理"
description = "自动化 SQL 注入检测与利用"
command = "python D:/tools/sqlmap/sqlmap.py -u {url} --batch"
[[tools.parameters]]
name = "url"
description = "目标 URL 地址"
required = true

[[tools]]
name = "goby"
can_agent_use = false
reason = "图形化视窗软件 (GUI)，需人工在界面点击操作"
description = "图形化网络资产梳理与漏洞探测平台"
manual_advice = "适合安全人员在本地桌面独立视窗中运行"
```
"#;
        let tools = parse_analyzed_tools(reply);
        assert_eq!(tools.len(), 2);
        assert!(tools[0].is_agent_compatible());
        assert_eq!(tools[0].parameters.len(), 1);
        assert_eq!(tools[0].to_custom_tool_config().name, "sqlmap");
        assert!(!tools[1].is_agent_compatible());
        assert!(tools[1].reason.contains("GUI"));
    }

    #[test]
    fn scan_directory_detects_scripts_and_subfolder_entrypoints() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::fs::write(base.join("fscan.exe"), b"").unwrap();
        std::fs::write(base.join("notes.txt"), b"ignore").unwrap();
        std::fs::create_dir(base.join("sqlmap")).unwrap();
        std::fs::write(base.join("sqlmap").join("sqlmap.py"), b"").unwrap();
        std::fs::create_dir(base.join("custom")).unwrap();
        std::fs::write(base.join("custom").join("main.py"), b"").unwrap();

        let candidates = scan_directory_for_candidates(base);
        let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(candidates.len(), 3);
        assert!(names.contains(&"fscan"));
        assert!(names.contains(&"sqlmap"));
        assert!(names.contains(&"custom"));
    }

    #[tokio::test]
    async fn preview_scan_writes_nothing_and_reports_preview() {
        let dir = tempfile::tempdir().unwrap();
        let tools_dir = dir.path().join("tools");
        std::fs::create_dir(&tools_dir).unwrap();
        let (events, mut rx) = mpsc::unbounded_channel();
        let (_cancel_tx, mut cancel) = oneshot::channel();
        // 目录里没有可执行候选 → 走 freeform 分支，必然调用模型（无 provider → 报错）。
        let result = run_scan(
            &ProviderConfig::default(),
            &tools_dir,
            Some(dir.path().to_str().unwrap()),
            true,
            &events,
            &mut cancel,
        )
        .await;
        assert!(result.is_err(), "no provider is available in tests");
        let notices: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                AgentEvent::Notice(message) => Some(message),
                _ => None,
            })
            .collect();
        assert!(notices.iter().any(|n| n.contains("扫描目录")));
        assert_eq!(std::fs::read_dir(&tools_dir).unwrap().count(), 0);
        assert!(format_report(&ScanReport {
            preview: true,
            ..Default::default()
        })
        .contains("预览模式"));
    }
}
