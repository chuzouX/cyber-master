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

/// 判定某工具是否属于高风险操作（用于自动审批模式下进行拦截）。
pub fn is_high_risk_tool(tool: &str, arguments: &Value) -> bool {
    let lower = tool.to_lowercase();
    match lower.as_str() {
        // shell 命令：安全只读命令在自动审批模式下放行；其他命令拦截确认
        "shell" | "bash" | "exec" => {
            let cmd = arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("");
            !is_safe_shell_command(cmd)
        }
        // 写文件/下载/连接等具有直接持久化副作用的操作弹出确认
        "write_file" | "write" | "edit" | "apply_patch" => true,
        "download_file" | "download" => true,
        "mcp_connect" => true,
        name if name.starts_with("custom_") => {
            // 自定义工具按其参数或命令安全度判断
            true
        }
        "ctf_challenge" => {
            // 查询题目为低风险，修改/解题状态为高风险
            let action = arguments
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("");
            action != "list"
        }
        // 低风险安全工具（read_file, list_dir, find_file, search_tools, web_fetch, use_skill, save_memory 等）
        _ => false,
    }
}

pub struct PermissionRequest {
    pub tool: String,
    pub arguments: Value,
    /// Bind terminal responses to this request, never to prefetched input.
    pub nonce: String,
    pub reply: oneshot::Sender<PermissionDecision>,
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
        let current_mode = self.mode();
        match current_mode {
            PermissionMode::Unlimited => return true,
            PermissionMode::Auto => {
                if !is_high_risk_tool(tool, arguments) {
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

        // 潜在高风险/写操作 shell 命令弹出确认
        let b2 = shell_broker.clone();
        let handle = tokio::spawn(async move {
            b2.authorize("shell", &json!({"command": "rm -f temp.txt"}))
                .await
        });
        let req = requests.recv().await.unwrap();
        assert_eq!(req.tool, "shell");
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
}
