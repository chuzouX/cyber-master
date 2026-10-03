//! Fail-closed, pre-execution permissions shared by non-TUI clients.
//! The legacy TUI `run_stream` entry does not opt into this policy.

use std::collections::HashSet;
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    AllowOnce,
    /// Only this tool and these exact JSON arguments, until the session changes.
    AllowSession,
    Deny,
}

/// 用户对审批请求的操作选项。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ApprovalChoice {
    Once,
    Session,
    Deny,
}

impl ApprovalChoice {
    pub fn label(self) -> &'static str {
        match self {
            Self::Once => "Allow once",
            Self::Session => "Allow for this session",
            Self::Deny => "Deny execution",
        }
    }

    pub fn decision(self) -> PermissionDecision {
        match self {
            Self::Once => PermissionDecision::AllowOnce,
            Self::Session => PermissionDecision::AllowSession,
            Self::Deny => PermissionDecision::Deny,
        }
    }
}

/// 审批模式：控制工具执行前的授权拦截策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum PermissionMode {
    /// 自动审批：低风险只读工具自动放行，高风险操作（命令执行/写文件/下载等）弹出确认
    #[default]
    Auto,
    /// 手动审批：每次调用任何工具都会提示 Permission Required
    Manual,
    /// 无限制：不弹出任何确认提示，始终自动放行（底层安全护栏仍生效）
    Unlimited,
}

impl PermissionMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动审批",
            Self::Manual => "手动审批",
            Self::Unlimited => "无限制",
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
            Self::Unlimited => "unlimited",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "auto" | "自动" | "自动审批" | "a" | "2" => Some(Self::Auto),
            "manual" | "手动" | "手动审批" | "m" | "1" => Some(Self::Manual),
            "unlimited" | "无限制" | "u" | "3" => Some(Self::Unlimited),
            _ => None,
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Manual => Self::Auto,
            Self::Auto => Self::Unlimited,
            Self::Unlimited => Self::Manual,
        }
    }
}

/// 提取 shell 类命令的子命令/分段（按 `&&`、`||`、`;`、`|` 分割）。
fn split_command_chain(cmd: &str) -> Vec<&str> {
    cmd.split([';', '|', '&'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// 获取命令段的首个命令名（小写，去除路径与前缀修饰，如 `sudo` / `cmd /c` 等）。
fn extract_primary_command(segment: &str) -> String {
    let tokens = segment.split_whitespace();
    for tok in tokens {
        let lower = tok.to_ascii_lowercase();
        let clean = lower
            .strip_prefix("builtin")
            .unwrap_or(&lower)
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-');
        let base = clean
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(clean)
            .strip_suffix(".exe")
            .or_else(|| clean.strip_suffix(".cmd"))
            .or_else(|| clean.strip_suffix(".bat"))
            .unwrap_or(clean);
        if base == "sudo" || base == "nohup" || base == "time" || base == "doas" {
            continue;
        }
        if base == "cmd" || base == "sh" || base == "bash" || base == "zsh" {
            // 递归跳过执行壳标志（如 cmd /c 或 sh -c）
            continue;
        }
        if base.starts_with('-') || base.starts_with('/') {
            continue;
        }
        return base.to_string();
    }
    String::new()
}

/// 常见的安全/只读或查询类命令集合。
const SAFE_SHELL_COMMANDS: &[&str] = &[
    "ls",
    "dir",
    "cat",
    "more",
    "less",
    "head",
    "tail",
    "nl",
    "od",
    "xxd",
    "hexdump",
    "strings",
    "echo",
    "printf",
    "pwd",
    "cd",
    "which",
    "where",
    "type",
    "file",
    "stat",
    "wc",
    "sort",
    "uniq",
    "diff",
    "cmp",
    "comm",
    "grep",
    "rg",
    "findstr",
    "ag",
    "ack",
    "git",
    "cargo",
    "rustc",
    "python",
    "python3",
    "py",
    "node",
    "npm",
    "deno",
    "bun",
    "go",
    "java",
    "javac",
    "dotnet",
    "ruby",
    "perl",
    "php",
    "env",
    "printenv",
    "set",
    "whoami",
    "id",
    "uname",
    "hostname",
    "uptime",
    "w",
    "ps",
    "top",
    "htop",
    "tasklist",
    "netstat",
    "ss",
    "ip",
    "ifconfig",
    "ipconfig",
    "arp",
    "route",
    "ping",
    "traceroute",
    "tracert",
    "nslookup",
    "dig",
    "host",
    "whois",
    "curl",
    "wget",
    "nmap",
    "nc",
    "ncat",
    "netcat",
    "tcpdump",
    "tshark",
    "openssl",
    "readelf",
    "objdump",
    "nm",
    "checksec",
    "gdb",
    "radare2",
    "r2",
];

/// 检查命令是否包含文件重定向写入操作（如 `>` 或 `>>`）。
fn has_redirect_write(cmd: &str) -> bool {
    cmd.contains('>')
}

/// 判断一条 shell 命令是否整体属于只读/安全操作。
fn is_safe_shell_command(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return false;
    }
    if has_redirect_write(trimmed) {
        return false;
    }
    let segments = split_command_chain(trimmed);
    if segments.is_empty() {
        return false;
    }
    for seg in segments {
        let primary = extract_primary_command(seg);
        if primary.is_empty() || !SAFE_SHELL_COMMANDS.contains(&primary.as_str()) {
            return false;
        }
    }
    true
}
/// 提取命令字符串中各段的命令主干名集合（如 `["git", "grep"]`）。
fn extract_command_names(cmd: &str) -> Vec<String> {
    split_command_chain(cmd)
        .into_iter()
        .map(extract_primary_command)
        .filter(|s| !s.is_empty())
        .collect()
}

/// 检查待执行工具的参数是否被某个已批准的会话条目所涵盖。
/// 1. 完全相同工具和参数直接放行；
/// 2. 对于 shell / bash / exec 命令：只要待执行命令涉及的各子命令均已被该 session 授权（或属于已知安全只读命令），
///    且不包含重定向写入，即允许执行（无需因为调优参数如 `--target` 或追加子目录而重新弹窗）；
/// 3. 对于自定义工具（custom_*）：如果待执行参数结构一致且未引入重定向或注入字符，允许同一会话放行。
fn matches_session_grant(
    granted_tool: &str,
    granted_args: &Value,
    candidate_tool: &str,
    candidate_args: &Value,
) -> bool {
    if granted_tool != candidate_tool {
        return false;
    }
    // 完全相同参数直接放行
    if granted_args == candidate_args {
        return true;
    }
    // 针对 shell 命令的宽松放行逻辑
    if matches!(candidate_tool, "shell" | "bash" | "exec") {
        let cand_cmd = candidate_args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("");
        // 如果新命令包含破坏性重定向写入，要求单独确认
        if has_redirect_write(cand_cmd) {
            return false;
        }
        let granted_cmd = granted_args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("");
        let granted_names = extract_command_names(granted_cmd);
        let cand_names = extract_command_names(cand_cmd);
        if !cand_names.is_empty()
            && cand_names.iter().all(|name| {
                granted_names.contains(name) || SAFE_SHELL_COMMANDS.contains(&name.as_str())
            })
        {
            return true;
        }
    }
    // 针对自定义工具 custom_* 的放行逻辑（同一工具名已在会话授权后，允许微调参数）
    if candidate_tool.starts_with("custom_") {
        return true;
    }
    false
}

/// 安全评估结果，包含安全置信度与判定依据。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SafetyAssessment {
    /// 安全置信度：0.0（极度危险）~ 1.0（完全安全）
    pub confidence: f32,
    /// 自动审批模式下是否可直接放行
    pub auto_approved: bool,
    /// 评估判定理由或风险标签
    pub reason: String,
}

/// 自动审批放行的安全置信度阈值（>= 0.70 自动放行）
pub const AUTO_APPROVE_CONFIDENCE_THRESHOLD: f32 = 0.70;

/// 检查命令是否命中毁灭性/极度危险规则（必须手动审批）。
pub fn check_extreme_danger_command(cmd: &str) -> Option<&'static str> {
    let lower = cmd.to_ascii_lowercase();
    let trimmed = lower.trim();

    // 1. Fork 炸弹
    if trimmed.contains(":(){ :|:& };:")
        || trimmed.contains(":(){:|:&};:")
        || trimmed.contains("%0|%0")
    {
        return Some("检测到 Fork 炸弹代码，可能导致系统资源耗尽假死");
    }

    // 2. 关机、重启、停机指令
    let segments = split_command_chain(trimmed);
    for seg in &segments {
        let primary = extract_primary_command(seg);
        if matches!(
            primary.as_str(),
            "shutdown" | "reboot" | "poweroff" | "halt"
        ) || primary == "stop-computer"
            || primary == "restart-computer"
        {
            return Some("检测到系统关机/重启/停机指令");
        }
        if primary == "init" && (seg.contains(" 0") || seg.contains(" 6")) {
            return Some("检测到切换系统运行级别至关机/重启 (init 0/6)");
        }
    }

    // 3. 磁盘格式化与磁盘分区操作
    for seg in &segments {
        let primary = extract_primary_command(seg);
        if matches!(primary.as_str(), "mkfs" | "fdisk" | "parted" | "diskpart")
            || primary.starts_with("mkfs.")
        {
            return Some("检测到磁盘格式化或底层分区破坏指令");
        }
        if primary == "format"
            && (seg.contains(" c:") || seg.contains(" d:") || seg.contains(" /fs"))
        {
            return Some("检测到磁盘格式化指令 (format)");
        }
    }

    // 4. 底层设备与裸块直接写入（如 dd of=/dev/sd* 或重定向到 /dev/sd*）
    if trimmed.contains("of=/dev/sd")
        || trimmed.contains("of=/dev/nvme")
        || trimmed.contains("of=/dev/hd")
        || trimmed.contains("of=/dev/vd")
        || trimmed.contains(r"\\.\physicaldrive")
    {
        return Some("检测到向物理磁盘底层设备裸写破坏性指令 (dd)");
    }
    if let Some(pos) = trimmed.find('>') {
        let target = trimmed[pos + 1..].trim();
        let target_word = target.split_whitespace().next().unwrap_or("");
        if target_word.starts_with("/dev/sd")
            || target_word.starts_with("/dev/nvme")
            || target_word.starts_with("/dev/hd")
            || target_word.starts_with("/dev/vd")
            || target_word.starts_with(r"\\.\physicaldrive")
        {
            return Some("检测到重定向写入系统物理磁盘底层设备");
        }
    }

    // 5. 毁灭性删除根目录或主用户目录
    for seg in &segments {
        let words: Vec<&str> = seg.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        let first = extract_primary_command(words[0]);
        if first == "rm" || first == "unlink" {
            let is_recursive = words
                .iter()
                .any(|w| w.starts_with('-') && (w.contains('r') || w.contains('R')));
            if is_recursive {
                for &target in &words[1..] {
                    if target.starts_with('-') {
                        continue;
                    }
                    let norm = if target == "/" {
                        "/"
                    } else {
                        target.trim_end_matches(['/', '\\'])
                    };
                    if target == "/"
                        || target == "/*"
                        || norm == "/"
                        || norm == "/*"
                        || norm == "~"
                        || norm == "$home"
                        || norm == "%userprofile%"
                        || norm == "c:"
                    {
                        return Some("检测到针对系统根目录或用户主目录的毁灭性递归删除 (rm -rf /)");
                    }
                    if norm == ".git" || norm == ".git/" || norm == ".git\\" {
                        return Some("检测到针对代码版本控制库元数据的彻底删除 (rm -rf .git)");
                    }
                    if matches!(
                        norm,
                        "/etc"
                            | "/boot"
                            | "/usr"
                            | "/bin"
                            | "/sbin"
                            | "/lib"
                            | "/lib64"
                            | "/var"
                            | "c:\\windows"
                            | "c:/windows"
                            | "c:\\system32"
                            | "c:/system32"
                            | "c:\\program files"
                            | "c:/program files"
                    ) {
                        return Some("检测到针对系统核心目录的递归删除");
                    }
                }
            }
        }
        if first == "rd" || first == "rmdir" {
            let is_recursive = words.iter().any(|w| {
                let lw = w.to_ascii_lowercase();
                lw == "/s" || lw == "-s"
            });
            if is_recursive {
                for &target in &words[1..] {
                    let norm = if target == "/" {
                        "/"
                    } else {
                        target.trim_end_matches(['/', '\\'])
                    };
                    if target == "/" || norm == "/" || norm == "c:" {
                        return Some("检测到针对系统根分区的毁灭性递归删除 (rd /s)");
                    }
                    if norm == ".git" {
                        return Some("检测到针对代码版本控制库元数据的彻底删除 (rd /s .git)");
                    }
                }
            }
        }
        if (first == "del" || first == "erase")
            && seg.contains("/s")
            && (seg.contains("c:\\*") || seg.contains("c:/*"))
        {
            return Some("检测到针对系统分区的递归文件清空指令 (del /s c:\\*)");
        }
    }

    // 6. 系统特权凭据导出与覆写
    if trimmed.contains("reg save hklm\\sam")
        || trimmed.contains("reg save hklm\\system")
        || trimmed.contains("sekurlsa::logonpasswords")
        || (trimmed.contains("procdump") && trimmed.contains("lsass"))
    {
        return Some("检测到凭据窃取与内存转储操作 (SAM/LSASS)");
    }
    if (trimmed.contains("> /etc/shadow") || trimmed.contains(">> /etc/shadow"))
        || (trimmed.contains("> /etc/passwd") || trimmed.contains(">> /etc/passwd"))
    {
        return Some("检测到尝试重定向篡改系统用户密码凭据文件 (/etc/shadow)");
    }

    // 7. 全局破坏安全策略
    if trimmed.contains("set-executionpolicy")
        && trimmed.contains("bypass")
        && trimmed.contains("localmachine")
    {
        return Some("检测到全局修改 PowerShell 执行策略为 Bypass (LocalMachine)");
    }
    if trimmed.contains("set-mppreference")
        && trimmed.contains("-disablerealtimemonitoring")
        && trimmed.contains("true")
    {
        return Some("检测到尝试停用操作系统实时防病毒保护 (Windows Defender)");
    }
    if trimmed.contains("netsh advfirewall set allprofiles state off") {
        return Some("检测到尝试全局关闭系统防火墙");
    }

    None
}

/// 检查文件路径是否属于极度危险的系统敏感路径。
pub fn check_extreme_danger_path(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase().replace('\\', "/");
    let trimmed = lower.trim();

    // 1. 深度跨目录穿越试图逃逸至根目录
    if trimmed.starts_with("../../../..") || trimmed.contains("/../../../../") {
        return Some("检测到多层路径穿越试图逃逸到系统根路径");
    }

    // 2. 覆写系统核心配置与敏感凭据
    if trimmed == "/etc/shadow" || trimmed == "/etc/passwd" || trimmed == "/etc/sudoers" {
        return Some("尝试直接写入系统核心特权配置文件 (/etc/shadow, /etc/passwd)");
    }
    if trimmed.contains("/.ssh/id_rsa")
        || trimmed.contains("/.ssh/id_ed25519")
        || trimmed.contains("/.ssh/id_ecdsa")
    {
        return Some("尝试覆盖或访问系统私钥凭据 (id_rsa)");
    }

    // 3. 覆写系统核心二进制或库文件
    if trimmed.starts_with("/bin/")
        || trimmed.starts_with("/sbin/")
        || trimmed.starts_with("/usr/bin/")
        || trimmed.starts_with("/usr/sbin/")
        || trimmed.starts_with("c:/windows/system32/")
    {
        return Some("尝试向操作系统二进制执行目录写入文件");
    }

    None
}

/// 检查是否命中极度危险规则。
pub fn check_extreme_danger(tool: &str, arguments: &Value) -> Option<&'static str> {
    let lower_tool = tool.to_ascii_lowercase();
    match lower_tool.as_str() {
        "shell" | "bash" | "exec" => {
            let cmd = arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            check_extreme_danger_command(cmd)
        }
        "write_file" | "write" | "edit" | "apply_patch" => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            check_extreme_danger_path(path)
        }
        _ => None,
    }
}

/// 对待执行工具及其参数进行综合安全性与风险置信度评估。
pub fn assess_tool_safety(tool: &str, arguments: &Value) -> SafetyAssessment {
    let lower_tool = tool.to_ascii_lowercase();

    // 1. 优先检查极度危险硬编码规则：命中则立即判定极度危险（置信度 0.05）
    if let Some(danger_rule) = check_extreme_danger(&lower_tool, arguments) {
        return SafetyAssessment {
            confidence: 0.05,
            auto_approved: false,
            reason: format!("命中极度危险规则：{danger_rule}"),
        };
    }

    // 2. 只读与查询类日常工具（完全安全）
    match lower_tool.as_str() {
        "read_file" | "list_dir" | "find_file" | "search_tools" | "web_fetch" | "use_skill"
        | "save_memory" | "todo" => {
            return SafetyAssessment {
                confidence: 1.0,
                auto_approved: true,
                reason: "低风险只读工具".into(),
            };
        }
        "ctf_challenge" => {
            return SafetyAssessment {
                confidence: 0.95,
                auto_approved: true,
                reason: "常规 CTF 题目协作与进度更新".into(),
            };
        }
        "mcp_connect" => {
            return SafetyAssessment {
                confidence: 0.85,
                auto_approved: true,
                reason: "MCP 服务外部连接".into(),
            };
        }
        "delegate_tasks" => {
            return SafetyAssessment {
                confidence: 0.90,
                auto_approved: true,
                reason: "并发子 Agent 任务委派".into(),
            };
        }
        _ => {}
    }

    // 3. 文件修改类工具 (write_file, write, edit, apply_patch)
    if matches!(
        lower_tool.as_str(),
        "write_file" | "write" | "edit" | "apply_patch"
    ) {
        let path = arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let norm_path = path.replace('\\', "/").to_ascii_lowercase();
        if norm_path.contains("../") {
            return SafetyAssessment {
                confidence: 0.40,
                auto_approved: false,
                reason: "写入路径包含上级目录跨目录引用 (..)".into(),
            };
        }
        if norm_path.starts_with(".git/") || norm_path.starts_with(".ssh/") {
            return SafetyAssessment {
                confidence: 0.35,
                auto_approved: false,
                reason: "尝试修改版本控制元数据或用户凭据目录".into(),
            };
        }
        return SafetyAssessment {
            confidence: 0.90,
            auto_approved: true,
            reason: "工作区内文件常规编辑与创建".into(),
        };
    }

    // 4. 文件下载类工具 (download_file, download)
    if matches!(lower_tool.as_str(), "download_file" | "download") {
        let url = arguments
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let path = arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if url.contains("127.0.0.1")
            || url.contains("localhost")
            || url.starts_with("http://10.")
            || url.starts_with("http://192.168.")
        {
            return SafetyAssessment {
                confidence: 0.45,
                auto_approved: false,
                reason: "下载目标指向本地或内网私有地址（潜在 SSRF 风险）".into(),
            };
        }
        if path.contains("..") {
            return SafetyAssessment {
                confidence: 0.40,
                auto_approved: false,
                reason: "文件保存路径包含跨目录引用 (..)".into(),
            };
        }
        return SafetyAssessment {
            confidence: 0.85,
            auto_approved: true,
            reason: "常规文件下载至工作区".into(),
        };
    }

    // 5. Shell 类命令 (shell, bash, exec)
    if matches!(lower_tool.as_str(), "shell" | "bash" | "exec") {
        let cmd = arguments
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if cmd.is_empty() {
            return SafetyAssessment {
                confidence: 0.50,
                auto_approved: false,
                reason: "空 Shell 指令".into(),
            };
        }
        // 如果是已知的安全只读命令
        if is_safe_shell_command(cmd) {
            return SafetyAssessment {
                confidence: 1.0,
                auto_approved: true,
                reason: "安全只读 Shell 命令".into(),
            };
        }

        // 常规开发构建、编译、运行、包管理与网络探测
        let segments = split_command_chain(cmd);
        let mut all_dev_or_net = true;
        for seg in &segments {
            let primary = extract_primary_command(seg);
            let is_dev_or_net = matches!(
                primary.as_str(),
                "cargo"
                    | "rustc"
                    | "python"
                    | "python3"
                    | "py"
                    | "node"
                    | "npm"
                    | "pnpm"
                    | "yarn"
                    | "bun"
                    | "deno"
                    | "go"
                    | "gcc"
                    | "g++"
                    | "clang"
                    | "make"
                    | "cmake"
                    | "ninja"
                    | "mvn"
                    | "gradle"
                    | "pip"
                    | "pip3"
                    | "git"
                    | "docker"
                    | "curl"
                    | "wget"
                    | "nc"
                    | "ncat"
                    | "netcat"
                    | "nmap"
                    | "ping"
                    | "traceroute"
                    | "tracert"
                    | "dig"
                    | "nslookup"
                    | "whois"
                    | "tcpdump"
                    | "tshark"
                    | "gdb"
                    | "r2"
                    | "radare2"
                    | "checksec"
                    | "readelf"
                    | "objdump"
                    | "nm"
                    | "strings"
                    | "openssl"
                    | "sqlmap"
                    | "nikto"
                    | "gobuster"
                    | "dirsearch"
                    | "ffuf"
                    | "mkdir"
                    | "touch"
                    | "cp"
                    | "mv"
                    | "tar"
                    | "zip"
                    | "unzip"
                    | "gzip"
                    | "echo"
                    | "printf"
                    | "chmod"
                    | "test"
                    | "timeout"
                    | "sleep"
            );
            if !is_dev_or_net {
                all_dev_or_net = false;
                break;
            }
        }
        if all_dev_or_net {
            return SafetyAssessment {
                confidence: 0.88,
                auto_approved: true,
                reason: "常规开发、编译构建或安全分析命令".into(),
            };
        }

        // 包含重定向写入到常规文件的命令（如 python exp.py > res.txt）
        if has_redirect_write(cmd) {
            return SafetyAssessment {
                confidence: 0.80,
                auto_approved: true,
                reason: "常规命令输出重定向至工作区".into(),
            };
        }

        // 一般 Shell 命令（未见明显高危特征）
        return SafetyAssessment {
            confidence: 0.78,
            auto_approved: true,
            reason: "常规操作命令，未检出高危特征".into(),
        };
    }

    // 默认工具评估
    SafetyAssessment {
        confidence: 0.80,
        auto_approved: true,
        reason: "常规工具操作".into(),
    }
}

/// 判定某工具是否属于高风险操作（用于自动审批模式下进行拦截）。
/// 兼容旧接口：基于 assess_tool_safety 的 auto_approved 判定。
pub fn is_high_risk_tool(tool: &str, arguments: &Value) -> bool {
    !assess_tool_safety(tool, arguments).auto_approved
}

pub struct PermissionRequest {
    pub tool: String,
    pub arguments: Value,
    /// Bind terminal responses to this request, never to prefetched input.
    pub nonce: String,
    pub reply: oneshot::Sender<PermissionDecision>,
    /// 安全置信度：0.0（极度危险）~ 1.0（完全安全）
    pub confidence: f32,
    /// 评估判定理由或风险标签
    pub risk_reason: String,
}

impl PermissionRequest {
    pub fn new(
        tool: String,
        arguments: Value,
        nonce: String,
        reply: oneshot::Sender<PermissionDecision>,
    ) -> Self {
        let assessment = assess_tool_safety(&tool, &arguments);
        Self {
            tool,
            arguments,
            nonce,
            reply,
            confidence: assessment.confidence,
            risk_reason: assessment.reason,
        }
    }
}

impl PermissionRequest {
    pub fn decision_for(&self, response: &str) -> PermissionDecision {
        match response.trim() {
            response if response == format!("once {}", self.nonce) => PermissionDecision::AllowOnce,
            response if response == format!("session {}", self.nonce) => {
                PermissionDecision::AllowSession
            }
            _ => PermissionDecision::Deny,
        }
    }
}

fn request_nonce() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // Mix per-request timing with randomly seeded hashes so prefetched input
    // cannot predict the next request token, without a new dependency.
    let state = RandomState::new();
    format!(
        "{:016x}{:016x}",
        state.hash_one((time, sequence, 0)),
        state.hash_one((time, sequence, 1))
    )
}

pub struct PermissionBroker {
    requests: Option<mpsc::UnboundedSender<PermissionRequest>>,
    grants: Mutex<Vec<(String, Value)>>,
    explicit_tools: HashSet<String>,
    denials: AtomicU64,
    mode: Mutex<PermissionMode>,
}

impl PermissionBroker {
    pub fn deny_all() -> Self {
        Self {
            requests: None,
            grants: Mutex::new(Vec::new()),
            explicit_tools: HashSet::new(),
            denials: AtomicU64::new(0),
            mode: Mutex::new(PermissionMode::Manual),
        }
    }

    /// Authorize exact tool names, with any arguments, solely from explicit
    /// caller policy. Never populate this from model output or project config.
    /// Names have no wildcard/`all` semantics; clients validate their registry.
    /// Tool implementations still enforce their normal guards.
    pub fn explicit_tools(tool_names: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self {
            explicit_tools: tool_names
                .into_iter()
                .map(|name| name.as_ref().to_owned())
                .collect(),
            ..Self::deny_all()
        }
    }

    pub fn interactive() -> (Self, mpsc::UnboundedReceiver<PermissionRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                requests: Some(tx),
                grants: Mutex::new(Vec::new()),
                explicit_tools: HashSet::new(),
                denials: AtomicU64::new(0),
                mode: Mutex::new(PermissionMode::Manual),
            },
            rx,
        )
    }

    pub fn clear_session(&self) {
        self.grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub fn mode(&self) -> PermissionMode {
        *self.mode.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        *self.mode.lock().unwrap_or_else(|e| e.into_inner()) = mode;
    }

    /// Monotonic structured signal, distinct from ordinary tool errors.
    pub fn denial_count(&self) -> u64 {
        self.denials.load(Ordering::Relaxed)
    }

    pub async fn authorize(&self, tool: &str, arguments: &Value) -> bool {
        let allowed = self.decide(tool, arguments).await;
        if !allowed {
            self.denials.fetch_add(1, Ordering::Relaxed);
        }
        allowed
    }

    async fn decide(&self, tool: &str, arguments: &Value) -> bool {
        if self.explicit_tools.contains(tool) {
            return true;
        }
        let assessment = assess_tool_safety(tool, arguments);
        let current_mode = self.mode();
        match current_mode {
            PermissionMode::Unlimited => return true,
            PermissionMode::Auto => {
                if assessment.auto_approved {
                    return true;
                }
            }
            PermissionMode::Manual => {}
        }
        if self
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(name, args)| matches_session_grant(name, args, tool, arguments))
        {
            return true;
        }
        let Some(requests) = &self.requests else {
            return false;
        };
        let (reply, decision) = oneshot::channel();
        if requests
            .send(PermissionRequest {
                tool: tool.to_owned(),
                arguments: arguments.clone(),
                nonce: request_nonce(),
                reply,
                confidence: assessment.confidence,
                risk_reason: assessment.reason,
            })
            .is_err()
        {
            return false;
        }
        match decision.await.unwrap_or(PermissionDecision::Deny) {
            PermissionDecision::AllowOnce => true,
            PermissionDecision::AllowSession => {
                self.grants
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((tool.to_owned(), arguments.clone()));
                true
            }
            PermissionDecision::Deny => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn session_grant_matches_and_allows_relaxed_safe_parameter_changes() {
        let (broker, mut requests) = PermissionBroker::interactive();
        broker.set_mode(PermissionMode::Manual);
        let broker = Arc::new(broker);
        let worker_broker = broker.clone();
        let worker = tokio::spawn(async move {
            worker_broker
                .authorize("shell", &json!({"command": "cargo check"}))
                .await
        });
        requests
            .recv()
            .await
            .unwrap()
            .reply
            .send(PermissionDecision::AllowSession)
            .unwrap();
        assert!(worker.await.unwrap());
        // 完全相同参数放行
        assert!(
            broker
                .authorize("shell", &json!({"command": "cargo check"}))
                .await
        );
        drop(requests);
        // 已授权命令的主干工具（cargo）+ 安全工具（grep）在改变参数后放行，无需重复弹窗
        assert!(
            broker
                .authorize("shell", &json!({"command": "cargo test --lib"}))
                .await
        );
        assert!(
            broker
                .authorize(
                    "shell",
                    &json!({"command": "cargo build && grep warning log.txt"})
                )
                .await
        );
        // 未授权的非安全新命令仍拦截
        assert!(
            !broker
                .authorize("shell", &json!({"command": "useradd malicious"}))
                .await
        );
        // 包含重定向写入的高危修改仍拦截
        assert!(
            !broker
                .authorize("shell", &json!({"command": "cargo run > /etc/passwd"}))
                .await
        );
        // 其他工具不匹配
        assert!(
            !broker
                .authorize("custom", &json!({"command": "cargo test"}))
                .await
        );
        // 自定义工具一旦经 session 授权，微调参数也放行
        let (custom_broker, mut custom_reqs) = PermissionBroker::interactive();
        custom_broker.set_mode(PermissionMode::Manual);
        let custom_broker = Arc::new(custom_broker);
        let cb = custom_broker.clone();
        let custom_worker = tokio::spawn(async move {
            cb.authorize("custom_scan", &json!({"target": "127.0.0.1"}))
                .await
        });
        custom_reqs
            .recv()
            .await
            .unwrap()
            .reply
            .send(PermissionDecision::AllowSession)
            .unwrap();
        assert!(custom_worker.await.unwrap());
        assert!(
            custom_broker
                .authorize("custom_scan", &json!({"target": "10.0.0.1"}))
                .await
        );
        broker.clear_session();
        assert!(
            !broker
                .authorize("shell", &json!({"command": "cargo check"}))
                .await
        );
    }

    #[tokio::test]
    async fn dropped_reply_denies_without_hanging() {
        let (broker, mut requests) = PermissionBroker::interactive();
        broker.set_mode(PermissionMode::Manual);
        let worker = tokio::spawn(async move { broker.authorize("shell", &json!({})).await });
        drop(requests.recv().await.unwrap());
        assert!(!worker.await.unwrap());
    }

    #[test]
    fn approval_requires_exact_current_nonce() {
        let (reply, _) = oneshot::channel();
        let request = PermissionRequest {
            tool: "shell".into(),
            arguments: json!({}),
            nonce: request_nonce(),
            reply,
            confidence: 0.0,
            risk_reason: String::new(),
        };
        for response in [
            "once",
            "session",
            "once stale-nonce",
            "session stale-nonce",
            "deny",
        ] {
            assert_eq!(request.decision_for(response), PermissionDecision::Deny);
        }
        assert_eq!(
            request.decision_for(&format!("once {}", request.nonce)),
            PermissionDecision::AllowOnce
        );
        assert_eq!(
            request.decision_for(&format!("session {}", request.nonce)),
            PermissionDecision::AllowSession
        );
        assert_eq!(
            request.decision_for(&format!("once {} extra", request.nonce)),
            PermissionDecision::Deny
        );
        assert_ne!(request.nonce, request_nonce());
    }

    #[tokio::test]
    async fn denial_is_structured_for_noninteractive_and_closed_ui() {
        let broker = PermissionBroker::deny_all();
        assert_eq!(broker.denial_count(), 0);
        assert!(!broker.authorize("shell", &json!({})).await);
        assert_eq!(broker.denial_count(), 1);
        let (broker, requests) = PermissionBroker::interactive();
        broker.set_mode(PermissionMode::Manual);
        drop(requests);
        assert!(!broker.authorize("shell", &json!({})).await);
        assert_eq!(broker.denial_count(), 1);
    }

    #[tokio::test]
    async fn explicit_tools_authorizes_only_exact_names_for_any_arguments() {
        let broker = PermissionBroker::explicit_tools(["shell", "shell"]);
        assert!(
            broker
                .authorize("shell", &json!({"command": "echo first"}))
                .await
        );
        assert!(
            broker
                .authorize("shell", &json!({"command": "echo second"}))
                .await
        );
        assert_eq!(broker.denial_count(), 0);
        assert!(!broker.authorize("write_file", &json!({})).await);
        assert!(!broker.authorize("Shell", &json!({})).await);
        assert_eq!(broker.denial_count(), 2);
    }

    #[tokio::test]
    async fn empty_explicit_tools_denies_by_default_and_never_expands_patterns() {
        let broker = PermissionBroker::explicit_tools(Vec::<String>::new());
        assert!(!broker.authorize("shell", &json!({})).await);
        let broker = PermissionBroker::explicit_tools(["*", "all", "shell*"]);
        assert!(!broker.authorize("shell", &json!({})).await);
        assert!(!broker.authorize("write_file", &json!({})).await);
    }

    #[tokio::test]
    async fn permission_modes_control_tool_execution_filtering() {
        let (broker, mut requests) = PermissionBroker::interactive();
        broker.set_mode(PermissionMode::Auto);
        assert!(
            broker
                .authorize("read_file", &json!({"path": "a.txt"}))
                .await
        );
        assert!(broker.authorize("list_dir", &json!({"path": "."})).await);
        assert!(broker.authorize("search_tools", &json!({})).await);
        let shell_broker = Arc::new(broker);
        // 安全 shell 命令在 Auto 模式下直接放行
        assert!(
            shell_broker
                .authorize("shell", &json!({"command": "git status"}))
                .await
        );
        assert!(
            shell_broker
                .authorize("shell", &json!({"command": "cargo check --workspace"}))
                .await
        );
        assert!(
            shell_broker
                .authorize("shell", &json!({"command": "cat a.txt | grep error"}))
                .await
        );

        // 常规工作区文件清理或写操作在 Auto 模式下已大胆自动放行
        assert!(
            shell_broker
                .authorize("shell", &json!({"command": "rm -f temp.txt"}))
                .await
        );
        assert!(
            shell_broker
                .authorize("write_file", &json!({"path": "src/lib.rs"}))
                .await
        );

        // 极度危险指令在 Auto 模式下被拦截并弹出确认
        let b2 = shell_broker.clone();
        let handle =
            tokio::spawn(
                async move { b2.authorize("shell", &json!({"command": "rm -rf /"})).await },
            );
        let req = requests.recv().await.unwrap();
        assert_eq!(req.tool, "shell");
        assert!(
            req.confidence <= 0.10,
            "极度危险指令置信度应极低: {}",
            req.confidence
        );
        assert!(req.risk_reason.contains("极度危险"));
        req.reply.send(PermissionDecision::AllowOnce).unwrap();
        assert!(handle.await.unwrap());
        shell_broker.set_mode(PermissionMode::Unlimited);
        assert!(
            shell_broker
                .authorize("shell", &json!({"command": "echo free"}))
                .await
        );
        assert!(
            shell_broker
                .authorize("write_file", &json!({"path": "f.txt"}))
                .await
        );

        shell_broker.set_mode(PermissionMode::Manual);
        let b3 = shell_broker.clone();
        let handle =
            tokio::spawn(async move { b3.authorize("read_file", &json!({"path": "m.txt"})).await });
        let req = requests.recv().await.unwrap();
        assert_eq!(req.tool, "read_file");
        req.reply.send(PermissionDecision::Deny).unwrap();
        assert!(!handle.await.unwrap());
    }

    #[test]
    fn extreme_danger_rules_detect_destructive_commands_and_paths() {
        // 1. 毁灭性删除
        let res1 = assess_tool_safety("shell", &json!({"command": "rm -rf /"}));
        assert!(!res1.auto_approved);
        assert!(res1.confidence <= 0.10);
        assert!(res1.reason.contains("极度危险"));

        let res2 = assess_tool_safety("shell", &json!({"command": "rd /s c:\\"}));
        assert!(!res2.auto_approved);
        assert!(res2.confidence <= 0.10);

        let res_git = assess_tool_safety("shell", &json!({"command": "rm -rf .git"}));
        assert!(!res_git.auto_approved);
        assert!(res_git.confidence <= 0.10);

        // 2. 关机/重启
        let res_shutdown = assess_tool_safety("shell", &json!({"command": "shutdown /s /t 0"}));
        assert!(!res_shutdown.auto_approved);
        assert!(res_shutdown.confidence <= 0.10);

        // 3. 磁盘格式化与裸写设备
        let res_format = assess_tool_safety("shell", &json!({"command": "format c: /fs:ntfs"}));
        assert!(!res_format.auto_approved);
        assert!(res_format.confidence <= 0.10);

        let res_dd =
            assess_tool_safety("shell", &json!({"command": "dd if=/dev/zero of=/dev/sda"}));
        assert!(!res_dd.auto_approved);
        assert!(res_dd.confidence <= 0.10);

        // 4. Fork 炸弹
        let res_fork = assess_tool_safety("shell", &json!({"command": ":(){ :|:& };:"}));
        assert!(!res_fork.auto_approved);
        assert!(res_fork.confidence <= 0.10);

        // 5. 凭据窃取与修改
        let res_sam =
            assess_tool_safety("shell", &json!({"command": "reg save hklm\\sam sam.bak"}));
        assert!(!res_sam.auto_approved);
        assert!(res_sam.confidence <= 0.10);

        // 6. 危险系统路径写入
        let res_shadow = assess_tool_safety("write_file", &json!({"path": "/etc/shadow"}));
        assert!(!res_shadow.auto_approved);
        assert!(res_shadow.confidence <= 0.10);

        // 7. 越权跨目录逃逸
        let res_traversal =
            assess_tool_safety("write_file", &json!({"path": "../../../etc/passwd"}));
        assert!(!res_traversal.auto_approved);
        assert!(res_traversal.confidence < 0.70);
    }

    #[test]
    fn bold_auto_approval_permits_daily_dev_and_ctf_workflows() {
        // 常规代码编辑
        let write_res = assess_tool_safety("write_file", &json!({"path": "src/main.rs"}));
        assert!(write_res.auto_approved, "工作区代码写入应当大胆自动放行");
        assert!(write_res.confidence >= 0.85);

        let edit_res = assess_tool_safety("edit", &json!({"path": "Cargo.toml"}));
        assert!(edit_res.auto_approved, "工作区配置编辑应当大胆自动放行");
        assert!(edit_res.confidence >= 0.85);

        // 常规编译构建与运行
        let cargo_res = assess_tool_safety("shell", &json!({"command": "cargo build --release"}));
        assert!(cargo_res.auto_approved, "cargo 构建应当自动放行");
        assert!(cargo_res.confidence >= 0.85);

        let py_res = assess_tool_safety(
            "shell",
            &json!({"command": "python exp.py --target 10.10.10.1"}),
        );
        assert!(py_res.auto_approved, "python 脚本执行应当自动放行");
        assert!(py_res.confidence >= 0.85);

        // 常规安全分析与网络工具
        let curl_res = assess_tool_safety(
            "shell",
            &json!({"command": "curl -X POST http://example.com/api"}),
        );
        assert!(curl_res.auto_approved, "curl 请求应当自动放行");
        assert!(curl_res.confidence >= 0.85);

        let nc_res = assess_tool_safety("shell", &json!({"command": "nc 10.10.10.10 1337"}));
        assert!(nc_res.auto_approved, "nc 网络交互应当自动放行");
        assert!(nc_res.confidence >= 0.85);

        // 重定向输出到工作区本地文件
        let redir_res =
            assess_tool_safety("shell", &json!({"command": "python exp.py > output.txt"}));
        assert!(
            redir_res.auto_approved,
            "输出重定向至工作区文件应当自动放行"
        );
        assert!(redir_res.confidence >= 0.80);

        // 清理工作区本地非关键文件
        let rm_res = assess_tool_safety("shell", &json!({"command": "rm -rf target"}));
        assert!(rm_res.auto_approved, "工作区 target 目录清理应当自动放行");
        assert!(rm_res.confidence >= 0.70);

        // CTF 题目协作
        let ctf_res = assess_tool_safety(
            "ctf_challenge",
            &json!({"action": "register", "name": "web1"}),
        );
        assert!(ctf_res.auto_approved, "CTF 题目注册应当自动放行");
        assert!(ctf_res.confidence >= 0.90);
    }
}
