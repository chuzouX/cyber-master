//! 「工具库」标签页与 `/toolbox` 命令的 UI 无关逻辑。
//!
//! 1. 自定义工具表单字段编解码（`params` / `tags` 的唯一实现，供 cli_commands 复用）。
//! 2. AI 智能扫描：探测本机安全工具 → 采集 `--help` → 模型判定 Agent 可用性 →
//!    写入 `~/.cyber/tools/*.toml`。进度通过 `AgentEvent::Notice` 回报（无 stderr 输出）。
//!
//! 写盘策略：仅「本地已发现 + 模型判定 Agent 可用 + 同名文件不存在」的条目落盘；
//! 已存在的记入 `skipped_existing` 不覆盖，需人工的记入 `manual` 只汇报；
//! `preview` 模式一个都不写。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

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
            other => {
                bail!("参数 {name} 的必填标记只能是 r 或 o，得到 {other}");
            }
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
3. command 必须以清单中给出的「执行命令」（可执行文件绝对路径，或 `python <路径>` / `java -jar <路径>`）\n\
   **原样开头**，只允许在其后追加参数与 {param} 占位符；禁止改写成 Windows 路径或换用其它解释器。\n\
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

/// 目录扫描候选数超过该值时，先用一次纯名单模型调用分诊安全工具。
const TRIAGE_THRESHOLD: usize = 40;
/// 送入分诊的名字上限（超出按名称排序取前 N 个并通知）。
const MAX_TRIAGE_NAMES: usize = 1200;
/// 分诊不可用（调用失败/空结果）时的候选上限。
const TRIAGE_FALLBACK_MAX: usize = 240;
/// 分诊最多保留条数（写进 prompt，模型据此排序取前 N）。
const TRIAGE_KEEP_MAX: usize = 150;

/// 分诊系统提示词（尾部条数由 [`tool_triage_system_prompt`] 注入）。
const TOOL_TRIAGE_SYSTEM_PROMPT: &str = "\
你是网络安全工具编目专家。\n\
用户会给出一台主机上某个目录里的可执行文件名清单（只有文件名，没有帮助文档）。\n\
请挑出【值得作为 Agent 自动化工具候选】的名字：属于网络侦察、漏洞扫描与利用、Web 安全、口令破解、\n\
无线与内网渗透、取证与逆向、隐写与密码学分析范畴，且存在非交互命令行用法。\n\
\n\
必须排除：通用系统与构建工具（coreutils/util-linux/busybox、包管理、编译器、shell、编辑器、\n\
文本处理、压缩归档、字体与图形库、语言运行时本身）、GUI 图形程序（如 burpsuite、wireshark、ghidra）、\n\
纯交互式控制台、以及明显与安全无关的服务与守护进程。\n\
\n\
输出要求：只输出一个 ```toml 代码块，不要任何解释文字：\n\
```toml\n\
[[keep]]\n\
name = \"nmap\"\n\
reason = \"端口/服务扫描，纯命令行非交互\"\n\
[[keep]]\n\
name = \"sqlmap\"\n\
reason = \"SQL 注入自动化，--batch 非交互\"\n\
```\n\
规则：1) name 必须逐字取自用户给出的清单，禁止发明清单外的名字；2) 只保留有把握的条目，不确定的不要输出；\n\
3) 最多输出 ";

/// 完整分诊提示词（注入保留条数上限）。
fn tool_triage_system_prompt() -> String {
    format!("{TOOL_TRIAGE_SYSTEM_PROMPT}{TRIAGE_KEEP_MAX} 条，按重要性从高到低。")
}

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

#[derive(Debug, Clone, serde::Deserialize)]
struct RawKeepItem {
    #[serde(default)]
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    reason: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct RawKeepBatch {
    #[serde(default)]
    keep: Vec<RawKeepItem>,
}

/// 分诊回复 → 保留的名字（复用 `extract_all_toml_blocks`；trim、按小写去重、保序）。
fn parse_keep_names(reply: &str) -> Vec<String> {
    let blocks = extract_all_toml_blocks(reply);
    let sources = if blocks.is_empty() {
        vec![reply.to_string()]
    } else {
        blocks
    };

    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for source in sources {
        let Ok(batch) = toml::from_str::<RawKeepBatch>(&source) else {
            continue;
        };
        for item in batch.keep {
            let name = item.name.trim().to_string();
            if name.is_empty() || !seen.insert(name.to_ascii_lowercase()) {
                continue;
            }
            names.push(name);
        }
    }
    names
}

/// 候选 ∩ 保留名（大小写不敏感，保留候选原顺序）；保留集为空 → 原样返回候选。
fn filter_kept(candidates: &[DiscoveredCandidate], kept: &[String]) -> Vec<DiscoveredCandidate> {
    if kept.is_empty() {
        return candidates.to_vec();
    }
    let kept_lower: Vec<String> = kept.iter().map(|name| name.to_ascii_lowercase()).collect();
    candidates
        .iter()
        .filter(|candidate| {
            let lower = candidate.name.to_ascii_lowercase();
            kept_lower.contains(&lower)
        })
        .cloned()
        .collect()
}

fn run_command_with_timeout(
    mut command: std::process::Command,
    timeout: std::time::Duration,
) -> Option<std::process::Output> {
    use std::io::Read;
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    // 探测 GUI 工具的 -h/--help 时不得在用户桌面上弹窗：清掉显示相关环境变量，
    // 让 GTK/Qt 程序直接以“无法连接显示”退出（仍受 2.5s 超时兜底）。
    #[cfg(unix)]
    {
        command.env_remove("DISPLAY");
        command.env_remove("WAYLAND_DISPLAY");
    }
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
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("where");
        command.arg(name);
        command
    } else {
        // POSIX 等价：`command -v` 是 shell 内建，不依赖 which 是否安装。
        // 名字经 `$1` 传入，避免字符串拼接注入。
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg("command -v \"$1\"")
            .arg("sh")
            .arg(name);
        command
    };
    if let Ok(output) = command.output() {
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

/// 跨平台常见安全工具（PATH 解析用；Kali 菜单编目之外的第二来源）。
const COMMON_SECURITY_TOOLS: &[&str] = &[
    "nmap",
    "masscan",
    "rustscan",
    "naabu",
    "httpx",
    "nuclei",
    "subfinder",
    "amass",
    "dnsx",
    "katana",
    "ffuf",
    "gobuster",
    "dirb",
    "dirsearch",
    "feroxbuster",
    "wfuzz",
    "nikto",
    "whatweb",
    "wpscan",
    "joomscan",
    "sqlmap",
    "commix",
    "hydra",
    "medusa",
    "patator",
    "ncrack",
    "john",
    "hashcat",
    "hashid",
    "searchsploit",
    "msfconsole",
    "responder",
    "netexec",
    "crackmapexec",
    "smbmap",
    "smbclient",
    "enum4linux",
    "enum4linux-ng",
    "kerbrute",
    "ldapsearch",
    "bloodhound-python",
    "tcpdump",
    "tshark",
    "bettercap",
    "aircrack-ng",
    "wifite",
    "reaver",
    "binwalk",
    "foremost",
    "exiftool",
    "steghide",
    "zsteg",
    "stegseek",
    "fcrackzip",
    "pdfcrack",
    "radare2",
    "rizin",
    "objdump",
    "readelf",
    "strings",
];

/// `.desktop` 内容 → 工具名（首个 `Name=`；忽略 `Name[xx]=` 本地化变体、注释行与其它键）。
fn desktop_name(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let value = line.trim_end().strip_prefix("Name=")?.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

/// 目录里 `.desktop` 的工具名清单；`prefix` 为文件名前缀过滤（`None` = 全部 `*.desktop`）。
fn desktop_menu_names(dir: &Path, prefix: Option<&str>) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return names;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !file_name.ends_with(".desktop") {
            continue;
        }
        if let Some(prefix) = prefix {
            if !file_name.starts_with(prefix) {
                continue;
            }
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(name) = desktop_name(&contents) {
            names.push(name);
        }
    }
    names
}

/// Kali 菜单名字：`(dir, prefix)` 列表 → 去重（大小写不敏感）→ 仅保留 `[A-Za-z0-9_-]+` → 按名排序。
///
/// 字符集过滤是刻意的：菜单里混有 `BackTrack Mode`、`clang++`、`0trace.sh` 一类名字，
/// 它们既不是可执行名（会污染 `command -v` 与工具库文件名），也不该进 tools.toml。
fn kali_menu_names_from(dirs: &[(PathBuf, Option<&str>)]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (dir, prefix) in dirs {
        for name in desktop_menu_names(dir, *prefix) {
            let lower = name.to_ascii_lowercase();
            if names
                .iter()
                .any(|known| known.to_ascii_lowercase() == lower)
            {
                continue;
            }
            if !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
            {
                continue;
            }
            names.push(name);
        }
    }
    names.sort();
    names
}

/// 本机 Kali 菜单目录（Unix；非 Unix 返回空）。
fn kali_menu_dirs() -> Vec<(PathBuf, Option<&'static str>)> {
    #[cfg(unix)]
    {
        vec![
            (PathBuf::from("/usr/share/applications"), Some("kali-")),
            (PathBuf::from("/usr/share/kali-menu/applications"), None),
        ]
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}

/// 无目标扫描的候选：Kali 菜单编目与常见清单**叠加**（编目在容器里可能只解析出少数几个），
/// 名字去重（大小写不敏感）后逐个 `command -v` 解析；Windows 另加预设目录扫描。
fn default_candidates() -> Vec<DiscoveredCandidate> {
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // 非 Unix 上 `kali_menu_dirs()` 为空，编目段自然无候选，仅靠常见清单兜底。
    for name in kali_menu_names_from(&kali_menu_dirs()) {
        if seen.insert(name.to_ascii_lowercase()) {
            names.push(name);
        }
    }
    for name in COMMON_SECURITY_TOOLS {
        if seen.insert(name.to_ascii_lowercase()) {
            names.push((*name).to_string());
        }
    }

    let mut candidates: Vec<DiscoveredCandidate> = Vec::new();
    for name in names {
        if let Some(binary) = find_installed_binary(&name) {
            candidates.push(DiscoveredCandidate {
                name,
                command: binary,
            });
        }
    }

    #[cfg(windows)]
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
                    .any(|existing| existing.name.eq_ignore_ascii_case(&candidate.name))
                {
                    candidates.push(candidate);
                }
            }
        }
    }

    candidates
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

/// 目录扫描的候选文件判定：脚本或本机可执行程序，排除普通数据文件。
///
/// Windows：扩展名白名单 `exe/bat/cmd/sh/py/jar`。
/// Unix：扩展名命中白名单 **或** 常规文件带任一执行位（`0o111`）——Kali 的
/// `/usr/bin` 里绝大多数是**无扩展名** ELF，只认扩展名会全部漏掉。
fn is_tool_binary(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        if ["exe", "bat", "cmd", "sh", "py", "jar"].contains(&ext.to_ascii_lowercase().as_str()) {
            return true;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = path.metadata() {
            return metadata.permissions().mode() & 0o111 != 0;
        }
    }
    false
}

/// 候选的执行命令：`.py` / `.jar` 走解释器，其余（含无扩展名的 ELF）直呼绝对路径。
fn candidate_command(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "py" {
        format!("python \"{}\"", path.display())
    } else if ext == "jar" {
        format!("java -jar \"{}\"", path.display())
    } else {
        format!("\"{}\"", path.display())
    }
}

/// 候选名字：文件主名（无扩展名时为整个文件名）。
fn candidate_name(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("tool")
        .to_string()
}

fn scan_directory_for_candidates(dir: &Path) -> Vec<DiscoveredCandidate> {
    let mut candidates = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return candidates;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if is_tool_binary(&path) {
                candidates.push(DiscoveredCandidate {
                    name: candidate_name(&path),
                    command: candidate_command(&path),
                });
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
                    if !is_tool_binary(&sub_path) {
                        continue;
                    }
                    let stem = sub_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                    if stem.eq_ignore_ascii_case(&folder_name) || stem == "main" || stem == "run" {
                        candidates.push(DiscoveredCandidate {
                            name: folder_name.clone(),
                            command: candidate_command(&sub_path),
                        });
                        break;
                    }
                }
            }
        }
    }

    candidates
}

/// 按连续分块切给 `workers` 个线程执行 `f(index, item)`，返回与 `items` 同序的结果。
/// `workers == 0` 视为 1；空输入返回空 Vec。
fn parallel_map<T, R, F>(items: &[T], workers: usize, f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> R + Sync,
{
    if items.is_empty() {
        return Vec::new();
    }
    let workers = workers.max(1).min(items.len());
    let per_chunk = items.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let handles: Vec<_> = items
            .chunks(per_chunk)
            .enumerate()
            .map(|(chunk_index, chunk)| {
                let f = &f;
                scope.spawn(move || {
                    chunk
                        .iter()
                        .enumerate()
                        .map(|(offset, item)| f(chunk_index * per_chunk + offset, item))
                        .collect::<Vec<R>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    })
}

/// 探测进度通知间隔：约 20 条（最少 1）。
fn probe_notice_step(total: usize) -> usize {
    (total / 20).max(1)
}

/// `--help` 采集并行度（每候选最长 2 × 2.5s，串行在 200 候选时会拖到数分钟）。
const PROBE_WORKERS: usize = 8;

fn fetch_candidates_help(
    candidates: &[DiscoveredCandidate],
    notify: &(dyn Fn(String) + Sync),
) -> Vec<(String, String, Option<String>)> {
    let total = candidates.len();
    if total == 0 {
        notify("探测 --help: 无候选工具".into());
        return Vec::new();
    }
    notify(format!(
        "探测 --help: 0/{total}（{PROBE_WORKERS} 线程并行）"
    ));
    let step = probe_notice_step(total);
    let done = AtomicUsize::new(0);
    let results = parallel_map(candidates, PROBE_WORKERS, |_index, candidate| {
        let help = run_help_for_command(&candidate.command, &candidate.name);
        let finished = done.fetch_add(1, Ordering::Relaxed) + 1;
        if finished.is_multiple_of(step) || finished == total {
            notify(format!(
                "探测 --help: {finished}/{total}（最近：{}）",
                candidate.name
            ));
        }
        (candidate.name.clone(), candidate.command.clone(), help)
    });
    let hit = results.iter().filter(|(_, _, help)| help.is_some()).count();
    notify(format!("探测完成：{total} 个候选，{hit} 个拿到帮助文本"));
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
        _ = &mut *cancel => {
            bail!("已取消工具扫描");
        }
        _ = events.closed() => {
            bail!("已取消工具扫描");
        }
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
            cyber_agent::StreamEvent::Error(err) => {
                bail!("模型返回错误: {err}");
            }
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

/// 扫描是否已被用户取消/调用方已断开（供分诊失败后的回退判定）。
fn scan_cancel_requested(
    events: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &mut oneshot::Receiver<()>,
) -> bool {
    events.is_closed() || cancel.try_recv().is_ok()
}

/// 分诊不可用时的回退：按名称排序后取前 [`TRIAGE_FALLBACK_MAX`] 个候选。
fn triage_fallback(
    candidates: Vec<DiscoveredCandidate>,
    reason: &str,
    events: &mpsc::UnboundedSender<AgentEvent>,
) -> Vec<DiscoveredCandidate> {
    notify(
        events,
        format!("分诊不可用（{reason}），改为按名称取前 {TRIAGE_FALLBACK_MAX} 个候选"),
    );
    let mut candidates = candidates;
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    candidates.truncate(TRIAGE_FALLBACK_MAX);
    candidates
}

/// 目录候选分诊：候选 ≤ [`TRIAGE_THRESHOLD`] 直接返回；否则一次纯名单模型调用后过滤。
///
/// 取消（oneshot 已触发或事件通道关闭）→ `Err("已取消工具扫描")` 向上传播；
/// 其余失败/空结果 → 通知原因并退回按名排序的前 [`TRIAGE_FALLBACK_MAX`] 个候选。
async fn triage_candidates(
    cfg: &ProviderConfig,
    dir_label: &str,
    candidates: Vec<DiscoveredCandidate>,
    events: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<Vec<DiscoveredCandidate>> {
    let total = candidates.len();
    if total <= TRIAGE_THRESHOLD {
        return Ok(candidates);
    }

    let mut names: Vec<String> = candidates.iter().map(|c| c.name.clone()).collect();
    names.sort();
    if names.len() > MAX_TRIAGE_NAMES {
        notify(
            events,
            format!("分诊名单超长：只送前 {MAX_TRIAGE_NAMES} 个名字（候选共 {total}）"),
        );
        names.truncate(MAX_TRIAGE_NAMES);
    }
    notify(
        events,
        format!("AI 分诊中：候选 {total} 个（送入 {} 个名字）…", names.len()),
    );

    let mut user_prompt = format!("目录: {dir_label}\n候选文件名（共 {total}）:\n");
    for name in &names {
        user_prompt.push_str(name);
        user_prompt.push('\n');
    }

    let reply = ask_llm_cancellable(
        cfg,
        &tool_triage_system_prompt(),
        &user_prompt,
        events,
        cancel,
    )
    .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => {
            if scan_cancel_requested(events, cancel) {
                return Err(color_eyre::eyre::eyre!("已取消工具扫描"));
            }
            return Ok(triage_fallback(candidates, &error.to_string(), events));
        }
    };

    let kept = parse_keep_names(&reply);
    if kept.is_empty() {
        return Ok(triage_fallback(candidates, "模型未返回有效名单", events));
    }
    let kept_candidates = filter_kept(&candidates, &kept);
    if kept_candidates.is_empty() {
        return Ok(triage_fallback(candidates, "保留名单与候选不匹配", events));
    }

    let kept_count = kept_candidates.len();
    notify(
        events,
        format!("分诊结果：保留 {kept_count} / 丢弃 {}", total - kept_count),
    );
    Ok(kept_candidates)
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

/// 每批 `[start, end)` 范围；`size == 0` 视为不切分。
fn batch_bounds(total: usize, size: usize) -> Vec<(usize, usize)> {
    if size == 0 || total == 0 {
        return Vec::new();
    }
    (0..total)
        .step_by(size)
        .map(|start| (start, (start + size).min(total)))
        .collect()
}

/// 单次模型调用的工具数上限（候选数上去后一次塞全部会撞上下文上限）。
const AI_BATCH: usize = 20;

/// 逐批调用 `evaluate_tools_with_ai` 并拼接结果（失败向上传播，取消仍生效）。
async fn evaluate_all_with_ai(
    cfg: &ProviderConfig,
    tools: &[(String, String, Option<String>)],
    events: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<Vec<AnalyzedTool>> {
    let bounds = batch_bounds(tools.len(), AI_BATCH);
    let mut analyzed = Vec::new();
    for (index, (start, end)) in bounds.iter().enumerate() {
        notify(
            events,
            format!(
                "调用模型分类…（批 {}/{}，本批 {} 个工具）",
                index + 1,
                bounds.len(),
                end - start
            ),
        );
        analyzed.extend(evaluate_tools_with_ai(cfg, &tools[*start..*end], events, cancel).await?);
    }
    Ok(analyzed)
}

/// 只保留名字出现在候选清单里的分析结果（大小写不敏感）；`allowed` 为空则原样返回。
fn filter_analyzed_to_candidates(
    analyzed: Vec<AnalyzedTool>,
    allowed: &[String],
) -> Vec<AnalyzedTool> {
    if allowed.is_empty() {
        return analyzed;
    }
    let allowed_lower: Vec<String> = allowed
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    analyzed
        .into_iter()
        .filter(|tool| {
            let lower = tool.name.to_ascii_lowercase();
            allowed_lower.contains(&lower)
        })
        .collect()
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

/// 扫描输入分流：候选清单（先剔已在库项 → 需要时 AI 分诊 → 探测 + 分类）或纯提示词。
enum ScanPlan {
    Candidates {
        candidates: Vec<DiscoveredCandidate>,
        /// 目录扫描的目录名（>40 个候选时用它做一次纯名单 AI 分诊）。
        triage_label: Option<String>,
    },
    Freeform(String),
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
    let plan = match target.map(str::trim).filter(|t| !t.is_empty()) {
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
                    ScanPlan::Freeform(format!("我的安全工具存放在目录: {input}"))
                } else {
                    ScanPlan::Candidates {
                        candidates,
                        triage_label: Some(input.to_string()),
                    }
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
                ScanPlan::Candidates {
                    candidates: vec![DiscoveredCandidate {
                        name,
                        command: input.to_string(),
                    }],
                    triage_label: None,
                }
            } else {
                ScanPlan::Freeform(input.to_string())
            }
        }
        None => {
            let candidates = default_candidates();
            notify(
                events,
                format!("扫描中：已发现 {} 个候选", candidates.len()),
            );
            if candidates.is_empty() {
                notify(
                    events,
                    "未在系统 PATH 或预设目录中检测到常见安全工具；可用 /toolbox scan <目录> 指定路径",
                );
            }
            ScanPlan::Candidates {
                candidates,
                triage_label: None,
            }
        }
    };

    let mut existing_names: Vec<String> = Vec::new();
    let analyzed = match plan {
        ScanPlan::Freeform(prompt) => {
            evaluate_freeform_with_ai(cfg, &prompt, events, cancel).await?
        }
        ScanPlan::Candidates {
            candidates,
            triage_label,
        } => {
            // 已在工具库里的候选不再探测、不再消耗模型额度（含分诊那次调用）。
            let (existing, remaining): (Vec<_>, Vec<_>) = candidates
                .into_iter()
                .partition(|candidate| tools_dir.join(format!("{}.toml", candidate.name)).exists());
            existing_names = existing
                .into_iter()
                .map(|candidate| candidate.name)
                .collect();
            let remaining = match triage_label {
                Some(label) => triage_candidates(cfg, &label, remaining, events, cancel).await?,
                None => remaining,
            };
            if remaining.is_empty() {
                if !existing_names.is_empty() {
                    notify(
                        events,
                        format!(
                            "全部 {} 个候选都已在工具库中，跳过探测与分类",
                            existing_names.len()
                        ),
                    );
                }
                Vec::new()
            } else {
                let notify_fn = |message: String| notify(events, message);
                let probed = fetch_candidates_help(&remaining, &notify_fn);
                let probed_names: Vec<String> =
                    probed.iter().map(|(name, _, _)| name.clone()).collect();
                let analyzed = evaluate_all_with_ai(cfg, &probed, events, cancel).await?;
                filter_analyzed_to_candidates(analyzed, &probed_names)
            }
        }
    };

    let mut report = commit_analyzed(analyzed, tools_dir, preview, events);
    if !existing_names.is_empty() {
        report.skipped_existing.splice(0..0, existing_names);
    }
    Ok(report)
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

    fn candidate(name: &str, command: &str) -> DiscoveredCandidate {
        DiscoveredCandidate {
            name: name.to_string(),
            command: command.to_string(),
        }
    }

    fn analyzed_tool(name: &str, command: &str) -> AnalyzedTool {
        AnalyzedTool {
            name: name.to_string(),
            suitability: ToolSuitability::AgentCompatible,
            reason: "纯命令行".into(),
            description: String::new(),
            command: command.to_string(),
            parameters: Vec::new(),
            manual_advice: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn scan_directory_accepts_extensionless_executables() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let executable = base.join("nmap");
        std::fs::write(&executable, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        // 无扩展名且无执行位 + 普通数据文件都不该被当作候选。
        std::fs::write(base.join("plain"), b"data").unwrap();
        std::fs::write(base.join("notes.txt"), b"data").unwrap();

        let candidates = scan_directory_for_candidates(base);
        let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["nmap"]);
        assert_eq!(
            candidates[0].command,
            format!("\"{}\"", executable.display())
        );
    }

    #[test]
    fn desktop_menu_parses_names_and_ignores_localized_and_junk() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::fs::write(
            base.join("kali-a.desktop"),
            "Name=amass\nExec=/usr/share/kali-menu/exec-in-shell \"amass -h\"\nCategories=kali-discovery\n",
        )
        .unwrap();
        std::fs::write(
            base.join("kali-b.desktop"),
            "Name[zh_CN]=中文名\nName=arpspoof\nComment=Name=不是名字\n",
        )
        .unwrap();
        // 名字含空格 → 字符集过滤（会污染 command -v 与工具库文件名）。
        std::fs::write(base.join("junk.desktop"), "Name=Kali Docs\n").unwrap();
        std::fs::write(base.join("other.desktop"), "Name=nmap\n").unwrap();

        let names = kali_menu_names_from(&[
            (base.to_path_buf(), Some("kali-")),
            (base.to_path_buf(), None),
        ]);
        assert_eq!(names, vec!["amass", "arpspoof", "nmap"]);
    }

    #[test]
    fn parse_keep_names_extracts_only_toml_keep_entries() {
        let reply = r#"
我猜可能还有 nmap-legacy 与 ffuf-extra。
```toml
[[keep]]
name = "nmap"
reason = "端口/服务扫描"
[[keep]]
name = "sqlmap"
reason = "SQL 注入自动化"
[[keep]]
name = "SQLMap"
reason = "大小写重复项"
```
以上。
"#;
        assert_eq!(parse_keep_names(reply), vec!["nmap", "sqlmap"]);
    }

    #[test]
    fn filter_kept_preserves_candidate_order_and_drops_unknown() {
        let candidates = vec![
            candidate("a", "a"),
            candidate("b", "b"),
            candidate("c", "c"),
        ];
        let kept = vec!["c".to_string(), "A".to_string()];
        let filtered = filter_kept(&candidates, &kept);
        let names: Vec<&str> = filtered.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "c"]);

        assert_eq!(filter_kept(&candidates, &[]).len(), 3);
    }

    #[test]
    fn parallel_map_preserves_order_and_maps_every_item() {
        let items: Vec<usize> = (0..12).collect();
        let expected: Vec<usize> = (0..12).map(|index| index * 100 + index).collect();
        assert_eq!(
            parallel_map(&items, 4, |index, item| index * 100 + item),
            expected
        );
        // workers == 0 视为 1，不得 panic。
        assert_eq!(
            parallel_map(&items, 0, |index, item| index * 100 + item),
            expected
        );
        assert!(parallel_map::<usize, usize, _>(&[], 4, |_, _| 0).is_empty());
    }

    #[test]
    fn ai_batches_split_evenly_and_cover_every_tool() {
        assert_eq!(batch_bounds(45, 20), vec![(0, 20), (20, 40), (40, 45)]);
        assert_eq!(batch_bounds(40, 20), vec![(0, 20), (20, 40)]);
        assert!(batch_bounds(0, 20).is_empty());
        assert!(batch_bounds(10, 0).is_empty());
    }

    #[test]
    fn filter_analyzed_drops_names_outside_candidates() {
        let analyzed = vec![
            analyzed_tool("nmap", "nmap -sV {target}"),
            analyzed_tool("nmap-extra", "nmap-extra"),
            analyzed_tool("sqlmap", "sqlmap -u {url}"),
        ];
        let allowed = vec!["NMAP".to_string()];
        let filtered = filter_analyzed_to_candidates(analyzed.clone(), &allowed);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "nmap");
        assert_eq!(filter_analyzed_to_candidates(analyzed, &[]).len(), 3);
    }

    /// 真机基准：Kali 菜单编目 + 常见清单解析出的已安装工具数（改造前恒为空 → 只剩 15 个硬编码名）。
    #[cfg(unix)]
    #[test]
    #[ignore = "需要 Kali 真机（/usr/share/applications 与 PATH 上的安全工具）"]
    fn kali_host_catalog_finds_installed_tools() {
        let count = default_candidates().len();
        println!("默认候选 {count} 个");
        assert!(
            count >= 150,
            "Kali 编目应解析出 >=150 个已安装工具，实得 {count}"
        );
    }
}
