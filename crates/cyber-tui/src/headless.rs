//! Single-turn execution shared by headless and the persistent line CLI.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cyber_agent::{
    run_stream_with_permissions, AgentEvent, PermissionBroker, PermissionDecision,
    PermissionRequest, ToolRegistry,
};
use cyber_core::{load_app_context, AppContext, ThinkingIntensity};
use serde_json::json;

use crate::bootstrap::build_registries;
use crate::chat::{append_stream_text, entries_to_messages, ChatEntry};
#[cfg(test)]
use crate::history::load_entries;
use crate::history::{create_session_meta, load_index, SessionIndex};

/// Display-only filtering, safe even when escape sequences span stream pieces.
pub fn terminal_text(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct HeadlessArgs {
    pub prompt: String,
    /// `text` or `json`; an empty value selects text.
    pub format: String,
    pub session: Option<String>,
    pub new: bool,
    pub max_steps: Option<u32>,
    pub think: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Explicit CLI flag, independent of the legacy environment override.
    pub mock: bool,
    /// Explicit caller-supplied exact tool names, allowing any arguments subject
    /// to builtin guards. Empty denies all; never sourced from project or model.
    pub allow_tools: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolCallRecord {
    pub name: String,
    pub arguments: String,
    pub output: String,
    pub is_error: bool,
    #[serde(skip)]
    pub id: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HeadlessOutcome {
    pub session_id: String,
    pub answer: String,
    pub tool_calls: Vec<ToolCallRecord>,
    pub error: Option<String>,
    /// Structured permission failure, not inferred from tool output text.
    pub permission_denied: bool,
}

/// Validate both user-supplied IDs and IDs read from the index before file access.
pub(crate) fn validate_session_id(id: &str) -> color_eyre::Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        || id.eq_ignore_ascii_case("index")
        || matches!(
            id.to_ascii_uppercase().as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        )
    {
        color_eyre::eyre::bail!("Invalid session ID: {id:?}");
    }
    Ok(())
}

pub(crate) fn thinking(
    value: Option<&str>,
    default: ThinkingIntensity,
) -> color_eyre::Result<ThinkingIntensity> {
    match value {
        Some(value) => ThinkingIntensity::from_str(value.trim())
            .ok_or_else(|| color_eyre::eyre::eyre!("Invalid think value: {value:?}")),
        None => Ok(default),
    }
}

pub(crate) struct CliInput {
    pub lines: tokio::sync::mpsc::Receiver<std::io::Result<String>>,
    pub requests: tokio::sync::mpsc::UnboundedReceiver<PermissionRequest>,
}

enum TurnMode<'a> {
    Cli {
        text: bool,
        input: Option<&'a mut CliInput>,
    },
    Ui {
        events: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
        cancel: tokio::sync::oneshot::Receiver<()>,
        steering: Option<cyber_agent::SteeringReceiver>,
    },
}

fn strip_markdown_code_blocks(text: &str) -> String {
    let mut out = String::new();
    let mut in_code = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn strip_markdown_inline(text: &str) -> String {
    let mut res = String::new();
    for c in text.chars() {
        if c == '*' || c == '`' || c == '#' || c == '>' || c == '_' {
            continue;
        }
        res.push(c);
    }
    res.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean_step_line(line: &str) -> String {
    let stripped = strip_markdown_inline(line);
    let trimmed = stripped.trim();
    let rest = trimmed.trim_start_matches(|c: char| {
        c.is_ascii_digit()
            || c == '.'
            || c == '-'
            || c == '*'
            || c == ' '
            || c == '、'
            || c == ')'
            || c == '）'
            || c == '('
            || c == '（'
    });
    let rest = rest
        .trim_start_matches("步骤")
        .trim_start_matches("Step")
        .trim_start();
    let rest = rest.trim_start_matches(|c: char| {
        c.is_ascii_digit()
            || c == '.'
            || c == '、'
            || c == ':'
            || c == '：'
            || c == ' '
            || c == ')'
            || c == '）'
    });
    if rest.is_empty() {
        trimmed.to_string()
    } else {
        rest.to_string()
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let mut truncated: String = s.chars().take(max.saturating_sub(3)).collect();
        truncated.push_str("...");
        truncated
    }
}

fn extract_ctf_solution_flow(answer: &str, challenge: &cyber_core::CtfChallenge) -> String {
    let cleaned = strip_markdown_code_blocks(answer);
    let lines: Vec<&str> = cleaned
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    // 1. 查找显式的解题流程/解题步骤/利用过程标题
    let header_keywords = [
        "解题流程",
        "解题步骤",
        "解题思路",
        "利用过程",
        "漏洞利用",
        "复现步骤",
        "利用流程",
        "攻击链",
    ];
    if let Some(pos) = lines.iter().position(|l| {
        let stripped = strip_markdown_inline(l);
        header_keywords.iter().any(|k| stripped.contains(k))
    }) {
        let mut steps = Vec::new();
        for line in lines.iter().skip(pos + 1) {
            let stripped = strip_markdown_inline(line);
            if stripped.starts_with('#') || header_keywords.iter().any(|k| stripped.contains(k)) {
                break;
            }
            let is_step = line.starts_with(|c: char| c.is_ascii_digit())
                || line.starts_with('-')
                || line.starts_with('*')
                || line.starts_with('（')
                || line.starts_with('(')
                || line.starts_with('①')
                || line.starts_with('②')
                || line.starts_with('③')
                || line.starts_with("步骤")
                || line.starts_with("Step");
            if is_step {
                let clean = clean_step_line(&stripped);
                if clean.len() >= 3 && !clean.to_lowercase().starts_with("flag:") {
                    steps.push(clean);
                }
            } else if steps.is_empty() && stripped.len() >= 8 {
                steps.push(stripped);
                break;
            }
            if steps.len() >= 4 {
                break;
            }
        }
        if !steps.is_empty() {
            if steps.len() == 1 {
                return truncate_chars(&steps[0], 120);
            }
            let indexed: Vec<String> = steps
                .into_iter()
                .enumerate()
                .map(|(i, s)| format!("{}. {}", i + 1, s))
                .collect();
            return truncate_chars(&indexed.join(" -> "), 140);
        }
    }

    // 2. 扫描全文提取结构化编号步骤 (如 1. ... 2. ...)
    let mut numbered = Vec::new();
    for line in &lines {
        let trimmed = line.trim();
        let is_numbered = (trimmed.starts_with(|c: char| c.is_ascii_digit())
            && (trimmed.contains('.') || trimmed.contains('、') || trimmed.contains(')')))
            || trimmed.starts_with("步骤")
            || trimmed.starts_with("Step ")
            || trimmed.starts_with('①')
            || trimmed.starts_with('②')
            || trimmed.starts_with('③');
        if is_numbered {
            let stripped = strip_markdown_inline(trimmed);
            let clean = clean_step_line(&stripped);
            if clean.len() >= 4 && !clean.to_lowercase().starts_with("flag:") {
                numbered.push(clean);
            }
        }
        if numbered.len() >= 4 {
            break;
        }
    }
    if numbered.len() >= 2 {
        let indexed: Vec<String> = numbered
            .into_iter()
            .enumerate()
            .map(|(i, s)| format!("{}. {}", i + 1, s))
            .collect();
        return truncate_chars(&indexed.join(" -> "), 140);
    }

    // 3. 扫描包含关键动作词的叙述性描述（漏洞利用与突破过程）
    let action_keywords = [
        "分析",
        "发现",
        "利用",
        "绕过",
        "注入",
        "读取",
        "获取",
        "提权",
        "逆向",
        "构造",
        "探测",
        "审计",
        "溢出",
        "反序列化",
        "解码",
        "爆破",
        "上传",
        "泄露",
    ];
    let mut actions = Vec::new();
    for line in &lines {
        let stripped = strip_markdown_inline(line);
        if stripped.len() < 8 || stripped.len() > 180 {
            continue;
        }
        if stripped.to_lowercase().starts_with("flag:") || stripped.contains("祝你解题愉快") {
            continue;
        }
        if action_keywords.iter().any(|k| stripped.contains(k)) {
            actions.push(stripped);
            if actions.len() >= 2 {
                break;
            }
        }
    }
    if !actions.is_empty() {
        return truncate_chars(&actions.join("；"), 120);
    }

    // 4. 优先尝试使用题目的 key_points 或 writeup
    if let Some(kp) = challenge
        .key_points
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        return truncate_chars(kp.trim(), 120);
    }
    if let Some(wu) = challenge
        .writeup
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        let wu_clean = strip_markdown_code_blocks(wu);
        let first = wu_clean
            .lines()
            .map(str::trim)
            .find(|l| l.len() >= 10 && !l.starts_with('#'))
            .unwrap_or(wu.trim());
        return truncate_chars(&strip_markdown_inline(first), 120);
    }

    // 5. 提取 answer 中的任意有效首句描述
    if let Some(first_line) = lines.iter().find(|l| {
        let s = strip_markdown_inline(l);
        s.len() >= 10 && !s.to_lowercase().starts_with("flag:")
    }) {
        return truncate_chars(&strip_markdown_inline(first_line), 120);
    }

    // 6. 分类兜底（纯技术流程，不包含工具名称）
    match challenge.category {
        cyber_core::CtfCategory::Web => {
            "经 Web 资产探测、漏洞定位与 Payload 验证成功取得 Flag".into()
        }
        cyber_core::CtfCategory::Pwn => {
            "经二进制防护机制分析、漏洞挖掘与利用链构造成功取得 Flag".into()
        }
        cyber_core::CtfCategory::Reverse => {
            "经逆向反编译算法分析、约束求解与逻辑还原成功取得 Flag".into()
        }
        cyber_core::CtfCategory::Crypto => {
            "经密码体制分析、弱点探测与密钥/明文还原成功取得 Flag".into()
        }
        cyber_core::CtfCategory::Misc => "经多源数据分析、隐写提取与协议解析成功取得 Flag".into(),
    }
}

fn extract_ctf_blocker(answer: &str, challenge: &cyber_core::CtfChallenge) -> String {
    if let Some(kp) = challenge
        .key_points
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        return truncate_chars(kp.trim(), 100);
    }
    let cleaned = strip_markdown_code_blocks(answer);
    let lines: Vec<&str> = cleaned
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let blocker_keywords = [
        "卡点",
        "难点",
        "受阻",
        "失败",
        "防护",
        "限制",
        "WAF",
        "保护",
        "报错",
        "未找到",
        "拦截",
        "过滤",
        "阻碍",
    ];
    for line in &lines {
        let stripped = strip_markdown_inline(line);
        if stripped.len() >= 8 && blocker_keywords.iter().any(|k| stripped.contains(k)) {
            return truncate_chars(&stripped, 100);
        }
    }
    if let Some(last_line) = lines.iter().rev().find(|l| {
        let s = strip_markdown_inline(l);
        s.len() >= 10 && !s.starts_with('#')
    }) {
        return truncate_chars(&strip_markdown_inline(last_line), 100);
    }
    "靶机环境分析中，正在尝试绕过安全防护或探测攻击面".into()
}

fn extract_task_summary(answer: &str) -> String {
    let cleaned = strip_markdown_code_blocks(answer);
    let lines: Vec<&str> = cleaned
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    for line in &lines {
        let stripped = strip_markdown_inline(line);
        if stripped.len() >= 8 && !stripped.starts_with('#') {
            return truncate_chars(&stripped, 80);
        }
    }
    "所有操作已顺利执行并返回结果。".into()
}
/// Built once per CLI lifetime; session and provider selections are mutable.
pub(crate) struct SessionRunner {
    pub ctx: AppContext,
    pub cwd: PathBuf,
    pub index: SessionIndex,
    pub entries: Vec<ChatEntry>,
    pub registries: crate::AppRegistries,
    pub ctf_enabled: bool,
    registry: Arc<ToolRegistry>,
    mock: bool,
}

impl SessionRunner {
    pub async fn new(cwd: &Path, mock: bool) -> color_eyre::Result<Self> {
        let ctx = load_app_context(cwd)?;
        let index_path =
            crate::history::session_dir(&ctx.paths.history_dir, cwd).join("index.json");
        let index = if index_path.try_exists()? {
            serde_json::from_slice::<SessionIndex>(&std::fs::read(index_path)?)?
        } else {
            let legacy = ctx
                .paths
                .history_dir
                .join(format!("{}.json", crate::history::cwd_hash(cwd)));
            if legacy.try_exists()? {
                let _: Vec<ChatEntry> = serde_json::from_slice(&std::fs::read(legacy)?)?;
            }
            load_index(&ctx.paths.history_dir, cwd)
        };
        validate_session_id(&index.current)?;
        if index.get(&index.current).is_none() {
            color_eyre::eyre::bail!("Current session is missing from the index");
        }
        for session in &index.sessions {
            validate_session_id(&session.id)?;
        }
        let dir = crate::history::session_dir(&ctx.paths.history_dir, cwd);
        let bytes =
            crate::cli_commands::read_optional(&dir.join(format!("{}.json", index.current)))?;
        let path = dir.join(format!("{}.json", index.current));
        let entries = if !path.try_exists()? {
            Vec::new()
        } else {
            serde_json::from_slice(&bytes)?
        };
        // 非 mock 时正常自动连接已配置的 MCP servers。
        let (registries, errors) =
            build_registries(&ctx.paths, cwd, mock, ctx.config.agent.subagents.enabled).await;
        for error in errors {
            eprintln!("[bootstrap] {}", terminal_text(&error));
        }
        let mut runner = Self {
            ctx,
            cwd: cwd.to_path_buf(),
            index,
            entries,
            registry: registries.tools.clone(),
            registries,
            ctf_enabled: false,
            mock: mock || std::env::var("CYBER_MOCK_PROVIDER").is_ok_and(|v| v == "1"),
        };
        runner.load_challenges(&runner.index.current.clone())?;
        runner.load_todos(&runner.index.current.clone())?;
        Ok(runner)
    }

    pub(crate) fn read_entries(&self, id: &str) -> color_eyre::Result<Vec<ChatEntry>> {
        validate_session_id(id)?;
        let path = crate::history::session_dir(&self.ctx.paths.history_dir, &self.cwd)
            .join(format!("{id}.json"));
        let bytes = crate::cli_commands::read_optional(&path)?;
        if !path.try_exists()? {
            Ok(Vec::new())
        } else {
            Ok(serde_json::from_slice(&bytes)?)
        }
    }

    pub(crate) fn challenges(&self) -> color_eyre::Result<Vec<cyber_core::CtfChallenge>> {
        let shared = self
            .registries
            .ctf_challenges
            .as_ref()
            .ok_or_else(|| color_eyre::eyre::eyre!("CTF registry unavailable"))?;
        Ok(shared
            .lock()
            .map_err(|_| color_eyre::eyre::eyre!("CTF state lock poisoned"))?
            .clone())
    }

    pub(crate) fn replace_challenges(
        &mut self,
        list: Vec<cyber_core::CtfChallenge>,
    ) -> color_eyre::Result<()> {
        let shared = self
            .registries
            .ctf_challenges
            .as_ref()
            .ok_or_else(|| color_eyre::eyre::eyre!("CTF registry unavailable"))?;
        *shared
            .lock()
            .map_err(|_| color_eyre::eyre::eyre!("CTF state lock poisoned"))? = list;
        Ok(())
    }

    pub(crate) fn todos(&self) -> Vec<cyber_core::TodoItem> {
        self.registries
            .todos
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub(crate) fn replace_todos(&mut self, list: Vec<cyber_core::TodoItem>) {
        if let Ok(mut g) = self.registries.todos.lock() {
            *g = list;
        }
    }

    fn load_todos(&mut self, id: &str) -> color_eyre::Result<()> {
        validate_session_id(id)?;
        let items = crate::history::load_todos(&self.ctx.paths.history_dir, &self.cwd, id);
        self.replace_todos(items);
        Ok(())
    }

    pub(crate) fn writeup_directory(
        &self,
        challenge: &cyber_core::CtfChallenge,
    ) -> color_eyre::Result<PathBuf> {
        validate_session_id(&self.index.current)?;
        validate_session_id(&challenge.id)?;
        crate::cli_commands::validate_challenge_name(&challenge.name)?;
        let directory = self
            .cwd
            .join(".cyber/ctf/sessions")
            .join(&self.index.current)
            .join(&challenge.id)
            .join(challenge.category.as_str())
            .join(&challenge.name);
        let mut ancestor = Some(directory.as_path());
        while let Some(path) = ancestor {
            match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    color_eyre::eyre::bail!("Writeup path contains a symbolic link");
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            if path == self.cwd {
                break;
            }
            ancestor = path.parent();
        }
        Ok(directory)
    }

    /// 启动后台子代理（`/bg run`）：复用当前 runner 的 provider/工具/记忆上下文。
    /// 立即返回 job id；任务 detached 运行，完成后由 CLI 注入当前会话。
    pub(crate) fn start_background_subagent(&self, prompt: String) -> u64 {
        cyber_agent::background::spawn_background_subagent(
            self.ctx.config.clone(),
            self.ctx.providers.clone(),
            self.ctx.project.clone(),
            self.mock,
            self.cwd.clone(),
            self.registry.clone(),
            self.ctf_enabled,
            self.ctx.config.agent.thinking_intensity,
            self.memory_prompt().unwrap_or_default(),
            prompt,
            Some(Arc::clone(&self.registries.subagents)),
            Arc::clone(&self.registries.background),
        )
    }

    fn memory_prompt(&self) -> color_eyre::Result<String> {
        let mut memory = String::new();
        for (scope, path) in [
            ("global", self.ctx.paths.memory_file.clone()),
            ("project", self.cwd.join(".cyber/memory.md")),
        ] {
            let content = String::from_utf8(crate::cli_commands::read_optional(&path)?)?;
            if !content.trim().is_empty() {
                memory.push_str(&format!("[{scope} memory]\n{content}\n"));
            }
        }
        let rules: Vec<_> = self
            .ctx
            .config
            .memory
            .rules
            .iter()
            .filter(|r| {
                r.enabled
                    && !r.prompt.trim().is_empty()
                    && ["global", "project", "both"].contains(&r.scope.as_str())
            })
            .collect();
        if !rules.is_empty() {
            memory.push_str("\nMemory-writing guidance (advisory prompt preferences, not enforced tool permissions or sandbox rules):\n");
            for rule in rules {
                let scope = if rule.scope == "both" {
                    "global and project"
                } else {
                    &rule.scope
                };
                memory.push_str(&format!("- [{scope}] {}\n", rule.prompt.trim()));
            }
        }
        Ok(memory)
    }

    fn commit_manual_compaction(&mut self, turn: &mut TurnHistory, cancelled: bool) {
        if !cancelled && turn.error.is_none() && turn.compacted {
            self.entries = std::mem::take(&mut turn.entries);
        }
    }

    fn load_challenges(&mut self, id: &str) -> color_eyre::Result<()> {
        validate_session_id(id)?;
        let mut list = Vec::new();
        for path in [
            self.ctx.paths.ctf_dir.join("challenges.json"),
            self.ctx
                .paths
                .ctf_dir
                .join("sessions")
                .join(format!("{id}.json")),
        ] {
            let bytes = crate::cli_commands::read_optional(&path)?;
            if path.try_exists()? {
                let items: Vec<cyber_core::CtfChallenge> = serde_json::from_slice(&bytes)?;
                let global_file = path.file_name().is_some_and(|n| n == "challenges.json");
                for item in items {
                    if (!global_file || item.is_global)
                        && !list
                            .iter()
                            .any(|c: &cyber_core::CtfChallenge| c.id == item.id)
                    {
                        list.push(item);
                    }
                }
            }
        }
        self.replace_challenges(list)
    }

    pub(crate) fn save(&mut self) -> color_eyre::Result<()> {
        use crate::cli_commands::persist;
        validate_session_id(&self.index.current)?;
        let dir = crate::history::session_dir(&self.ctx.paths.history_dir, &self.cwd);
        let list = self.challenges()?;
        let (global, local): (Vec<_>, Vec<_>) = list.into_iter().partition(|c| c.is_global);
        persist(
            &self.ctx.paths.ctf_dir.join("challenges.json"),
            &serde_json::to_vec_pretty(&global)?,
        )?;
        persist(
            &self
                .ctx
                .paths
                .ctf_dir
                .join("sessions")
                .join(format!("{}.json", self.index.current)),
            &serde_json::to_vec_pretty(&local)?,
        )?;
        let challenge_list = self.challenges().unwrap_or_default();
        if let Some(meta) = self.index.get_mut(&self.index.current.clone()) {
            meta.message_count = self.entries.len();
            meta.updated_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if meta.title == "新会话" || meta.title == "默认会话" || meta.title.is_empty() {
                let new_title =
                    crate::history::derive_session_title(&self.entries, &challenge_list);
                if new_title != "新会话" {
                    meta.title = new_title;
                }
            }
        }
        persist(
            &dir.join(format!("{}.json", self.index.current)),
            &serde_json::to_vec_pretty(&self.entries)?,
        )?;
        let todos = self.todos();
        let _ = crate::history::save_todos(
            &self.ctx.paths.history_dir,
            &self.cwd,
            &self.index.current,
            &todos,
        );
        persist(
            &dir.join("index.json"),
            &serde_json::to_vec_pretty(&self.index)?,
        )?;
        Ok(())
    }

    fn save_turn_outcome(&mut self, error: &mut Option<String>) {
        if let Err(failure) = self.save() {
            let message = format!("History persistence failed: {failure}");
            *error = Some(match error.take() {
                Some(previous) => format!("{previous}\n{message}"),
                None => message,
            });
            if let Some(ChatEntry::TurnSummary { status, .. }) = self.entries.last_mut() {
                if status == "done" {
                    *status = "error".into();
                    // Entries may have been published before a later index failure.
                    // Correct that record if possible; the outcome still reports failure.
                    if let Ok(bytes) = serde_json::to_vec_pretty(&self.entries) {
                        let path =
                            crate::history::session_dir(&self.ctx.paths.history_dir, &self.cwd)
                                .join(format!("{}.json", self.index.current));
                        let _ = crate::cli_commands::persist(&path, &bytes);
                    }
                }
            }
        }
    }

    pub(crate) fn create_session(&mut self) -> color_eyre::Result<()> {
        self.save()?;
        let meta = create_session_meta();
        self.load_challenges(&meta.id)?;
        self.load_todos(&meta.id)?;
        self.index.current = meta.id.clone();
        self.index.sessions.push(meta);
        self.entries.clear();
        self.save()
    }

    pub(crate) fn delete_session(&mut self, id: &str) -> color_eyre::Result<()> {
        validate_session_id(id)?;
        if self.index.get(id).is_none() {
            color_eyre::eyre::bail!("Unknown session");
        }
        if self.index.sessions.len() <= 1 {
            color_eyre::eyre::bail!("Cannot delete the last session");
        }
        self.save()?;
        if self.index.current == id {
            let next = self
                .index
                .sessions
                .iter()
                .find(|s| s.id != id)
                .unwrap()
                .id
                .clone();
            self.select_session(&next)?;
        }
        for path in [
            crate::history::session_dir(&self.ctx.paths.history_dir, &self.cwd)
                .join(format!("{id}.json")),
            self.ctx
                .paths
                .ctf_dir
                .join("sessions")
                .join(format!("{id}.json")),
        ] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.index.sessions.retain(|s| s.id != id);
        self.save()
    }

    pub(crate) fn select_model_persisted(
        &mut self,
        provider: &str,
        model: Option<&str>,
    ) -> color_eyre::Result<()> {
        let mut providers = self.ctx.providers.clone();
        let mut config = self.ctx.config.clone();
        let p = providers
            .providers
            .get_mut(provider)
            .ok_or_else(|| color_eyre::eyre::eyre!("Unknown provider"))?;
        if let Some(model) = model {
            if model.is_empty() || model.contains(char::is_whitespace) {
                color_eyre::eyre::bail!("Invalid model");
            }
            p.model = model.into();
        }
        config.agent.default_provider = provider.into();
        providers.default_provider = provider.into();
        crate::cli_commands::save_selection(self, config, providers)
    }

    pub fn select_session(&mut self, id: &str) -> color_eyre::Result<()> {
        validate_session_id(id)?;
        if self.index.get(id).is_none() {
            color_eyre::eyre::bail!("Unknown session: {id}");
        }
        let entries = self.read_entries(id)?;
        self.save()?;
        self.load_challenges(id)?;
        self.load_todos(id)?;
        self.entries = entries;
        self.index.current = id.to_owned();
        self.save()?;
        Ok(())
    }

    pub fn select_model(&mut self, provider: &str, model: Option<&str>) -> color_eyre::Result<()> {
        let cfg = self
            .ctx
            .providers
            .providers
            .get_mut(provider)
            .ok_or_else(|| color_eyre::eyre::eyre!("Unknown provider: {provider}"))?;
        if let Some(model) = model {
            cfg.model = model.to_owned();
        }
        self.ctx.config.agent.default_provider = provider.to_owned();
        Ok(())
    }

    pub(crate) fn generate_turn_summary(
        &self,
        status: &str,
        answer: &str,
        _tool_calls: &[ToolCallRecord],
        error: Option<&str>,
    ) -> String {
        let challenges = self.challenges().unwrap_or_default();
        let has_challenges = self.ctf_enabled || !challenges.is_empty();

        if has_challenges {
            // 1. 如果有题目解出来，总结题目的真实解题流程（杜绝仅列出工具名）
            if let Some(solved) = challenges
                .iter()
                .find(|c| c.status == cyber_core::CtfStatus::Solved)
            {
                let flag_str = solved.flag.as_deref().unwrap_or("已获取");
                let flow = extract_ctf_solution_flow(answer, solved);
                return format!(
                    "题目【{}】已解出 (Flag: {flag_str})，解题流程：{flow}",
                    solved.name
                );
            }

            // 2. 有题目但没解出来 -> 说明实际卡点与防护限制
            if let Some(in_progress) = challenges
                .iter()
                .find(|c| c.status == cyber_core::CtfStatus::InProgress)
            {
                let blocker = if let Some(err) = error {
                    err.to_string()
                } else if status == "cancelled" {
                    "解题过程被用户取消".to_string()
                } else {
                    extract_ctf_blocker(answer, in_progress)
                };
                return format!("题目【{}】尚未解出，卡点：{blocker}", in_progress.name);
            }
        }

        // 3. 如果不是题目则直接总结对话和任务完成情况（描述成果，不列举底层工具）
        if status == "cancelled" {
            return "任务已取消，已保存当前对话与执行历史。".into();
        }
        if let Some(err) = error {
            return format!("任务执行未完成：{err}");
        }
        if !answer.trim().is_empty() {
            let brief = extract_task_summary(answer);
            return format!("任务完成：{brief}");
        }
        "任务已完成，所有操作已执行完毕并返回结果。".into()
    }

    pub(crate) async fn run_cli_task(
        &mut self,
        task: crate::cli_commands::CliTask,
        permissions: Arc<PermissionBroker>,
        events: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
        mut cancel: tokio::sync::oneshot::Receiver<()>,
    ) -> HeadlessOutcome {
        use crate::cli_commands::{persist, validate_challenge_name, CliTask};
        let started = Instant::now();
        let mut turn = TurnHistory::new(String::new());
        turn.entries.clear();
        let mut cancelled = false;
        let mut denied = false;
        let mut writeup = None;
        match task {
            CliTask::McpConnect { config } => {
                let _ = events.send(AgentEvent::Started);
                let arguments = serde_json::to_value(&config).unwrap_or(serde_json::Value::Null);
                let approved;
                if self.registries.mcp.is_some() {
                    approved = false;
                    turn.error = Some("MCP already connected; restart before reconnecting".into());
                } else {
                    tokio::select! {
                        biased;
                        _ = &mut cancel => { cancelled = true; approved = false; turn.error = Some("Cancelled MCP connection".into()); }
                        _ = events.closed() => { cancelled = true; approved = false; turn.error = Some("UI event observer closed".into()); }
                        allowed = permissions.authorize("mcp_connect", &arguments) => { approved = allowed && !arguments.is_null(); }
                    }
                }
                if !approved && !cancelled && turn.error.is_none() {
                    denied = true;
                    turn.error = Some("MCP connection denied; no servers started".into());
                }
                if approved {
                    let mut handle =
                        tokio::spawn(
                            async move { cyber_mcp::McpRegistry::connect_all(&config).await },
                        );
                    let _abort_on_drop = AbortOnDrop(handle.abort_handle());
                    let result = tokio::select! {
                        biased;
                        _ = &mut cancel => { cancelled = true; turn.error = Some("Cancelled MCP connection".into()); handle.abort(); handle.await }
                        _ = events.closed() => { cancelled = true; turn.error = Some("UI event observer closed".into()); handle.abort(); handle.await }
                        result = &mut handle => result,
                    };
                    if let Ok(result) = result {
                        let (mcp, tools, errors) = result;
                        if cancelled {
                            drop(tools);
                            mcp.shutdown_all().await;
                        } else if mcp.is_empty() {
                            turn.error = Some(if errors.is_empty() {
                                "No MCP servers configured; no servers started".into()
                            } else {
                                format!(
                                    "MCP connection failed for {} server(s); no servers started",
                                    errors.len()
                                )
                            });
                            drop(tools);
                            mcp.shutdown_all().await;
                        } else {
                            // Preserve all existing tools, guards, hidden skills and shared CTF state.
                            let base = self.registry.clone();
                            let mut registry = ToolRegistry::new();
                            let visible: Vec<_> =
                                base.schemas().into_iter().map(|s| s.name).collect();
                            for schema in base.all_schemas() {
                                if schema.name == "search_tools" {
                                    continue;
                                }
                                let hidden = !visible.contains(&schema.name);
                                let tool = Box::new(SharedCliTool {
                                    schema,
                                    base: base.clone(),
                                });
                                if hidden {
                                    registry.register_hidden(tool);
                                } else {
                                    registry.register(tool);
                                }
                            }
                            for tool in tools {
                                registry.register(Box::new(tool));
                            }
                            registry.register(Box::new(cyber_agent::SearchToolsTool::new(
                                registry.catalog(),
                            )));
                            self.registry = Arc::new(registry);
                            self.registries.tools = self.registry.clone();
                            self.registries.mcp = Some(Arc::new(mcp));
                            if !errors.is_empty() {
                                // Do not expose connection errors containing URLs/credentials.
                                turn.error = Some(format!(
                                    "MCP connection failed for {} server(s)",
                                    errors.len()
                                ));
                            }
                            turn.answer = "MCP connection attempt completed".into();
                        }
                    } else if !cancelled {
                        turn.error = Some("MCP connection task failed".into());
                    }
                }
                if let Some(error) = &turn.error {
                    let _ = events.send(AgentEvent::Error(error.clone()));
                }
                let _ = events.send(AgentEvent::Done);
            }
            CliTask::ToolboxScan { preview, target } => {
                let _ = events.send(AgentEvent::Started);
                let provider = self
                    .ctx
                    .providers
                    .providers
                    .get(&self.ctx.config.agent.default_provider)
                    .cloned();
                let report = match provider {
                    None => Err(color_eyre::eyre::eyre!(
                        "未配置默认 Provider；请先运行 cyber setup 或 /provider"
                    )),
                    Some(cfg)
                        if cfg.kind != "ollama" && cfg.resolved_api_key().trim().is_empty() =>
                    {
                        Err(color_eyre::eyre::eyre!(
                            "当前 Provider [{}] 缺少有效 API Key",
                            self.ctx.config.agent.default_provider
                        ))
                    }
                    Some(cfg) => {
                        crate::toolbox::run_scan(
                            &cfg,
                            &self.ctx.paths.tools_dir,
                            target.as_deref(),
                            preview,
                            &events,
                            &mut cancel,
                        )
                        .await
                    }
                };
                match report {
                    Ok(report) => {
                        let text = crate::toolbox::format_report(&report);
                        turn.answer = text.clone();
                        let _ = events.send(AgentEvent::Notice(text));
                    }
                    Err(error) => {
                        turn.error = Some(error.to_string());
                    }
                }
                let _ = events.send(AgentEvent::Done);
            }
            task => {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let config = self.ctx.config.clone();
                let providers = self.ctx.providers.clone();
                let handle = match task {
                    CliTask::Compact { instructions } => {
                        tokio::spawn(cyber_agent::run_compact_stream(
                            config,
                            providers,
                            self.ctx.project.clone(),
                            entries_to_messages(&self.entries),
                            instructions,
                            tx,
                            0,
                            self.mock,
                        ))
                    }
                    CliTask::Writeup { challenge } => {
                        let result = validate_challenge_name(&challenge.name).and_then(|_| {
                            if !challenge.is_solved() {
                                color_eyre::eyre::bail!("Challenge must be solved");
                            }
                            let skill =
                                self.registries.skills.find("ctf-writeup").ok_or_else(|| {
                                    color_eyre::eyre::eyre!("ctf-writeup skill unavailable")
                                })?;
                            Ok((skill.body.clone(), self.writeup_directory(&challenge)?))
                        });
                        match result {
                            Ok((guide, directory)) => {
                                let context = format!("Write a complete Markdown writeup using this solved challenge and the conversation.\n{}\nConversation:\n{}\nProject artifact directory: {}", serde_json::to_string(&challenge).unwrap_or_default(), entries_to_messages(&self.entries).iter().map(|m| m.content.as_str()).collect::<Vec<_>>().join("\n"), directory.display());
                                turn.entries.push(ChatEntry::User(format!(
                                    "Generate writeup for {}",
                                    challenge.name
                                )));
                                writeup = Some(challenge);
                                tokio::spawn(cyber_agent::run_writeup_stream(
                                    config, providers, guide, context, tx, 0, self.mock,
                                ))
                            }
                            Err(error) => {
                                turn.error = Some(error.to_string());
                                tokio::spawn(async move {
                                    let _ = tx.send((0, AgentEvent::Error(error.to_string())));
                                    let _ = tx.send((0, AgentEvent::Done));
                                })
                            }
                        }
                    }
                    CliTask::McpConnect { .. } => unreachable!(),
                    CliTask::ToolboxScan { .. } => unreachable!(),
                };
                cancelled =
                    collect_task_events(handle, &mut rx, &events, &mut cancel, &mut turn).await;
                if let Some(challenge) = writeup {
                    if !cancelled && turn.error.is_none() && turn.answer.trim().is_empty() {
                        turn.error = Some("Writeup generation returned no content".into());
                    }
                    if !cancelled && turn.error.is_none() {
                        let result = (|| -> color_eyre::Result<()> {
                            let path = self.writeup_directory(&challenge)?.join("writeup.md");
                            let mut list = self.challenges()?;
                            let item = list.iter_mut().find(|c| c.id == challenge.id).ok_or_else(
                                || color_eyre::eyre::eyre!("Challenge no longer exists"),
                            )?;
                            persist(&path, turn.answer.as_bytes())?;
                            item.writeup = Some(turn.answer.clone());
                            self.replace_challenges(list)?;
                            Ok(())
                        })();
                        if let Err(error) = result {
                            turn.error = Some(error.to_string());
                        }
                    }
                    self.entries.extend(turn.entries);
                } else {
                    self.commit_manual_compaction(&mut turn, cancelled);
                }
                // Partial summaries must never replace the resumable model history.
            }
        }
        let status = if cancelled {
            "cancelled"
        } else if turn.error.is_some() {
            "error"
        } else {
            "done"
        };
        let summary = self.generate_turn_summary(
            status,
            &turn.answer,
            &turn.tool_calls,
            turn.error.as_deref(),
        );
        self.entries.push(ChatEntry::TurnSummary {
            elapsed_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            finished_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            status: status.into(),
            summary: Some(summary),
        });
        self.save_turn_outcome(&mut turn.error);
        HeadlessOutcome {
            session_id: self.index.current.clone(),
            answer: turn.answer,
            tool_calls: turn.tool_calls,
            error: turn.error,
            permission_denied: denied,
        }
    }

    fn explicit_permissions(
        &self,
        tool_names: &[String],
    ) -> color_eyre::Result<Arc<PermissionBroker>> {
        let schemas = self.registry.all_schemas();
        for name in tool_names {
            if name.eq_ignore_ascii_case("all")
                || name.contains(['*', '?', '[', ']'])
                || !schemas.iter().any(|schema| schema.name == *name)
            {
                color_eyre::eyre::bail!("Invalid allowed tool: {name:?}; use an exact registered tool name, without wildcards or all");
            }
        }
        Ok(Arc::new(PermissionBroker::explicit_tools(tool_names)))
    }

    /// One turn, with real event-order persistence and cancellation-safe history.
    pub async fn run_turn(
        &mut self,
        prompt: String,
        text: bool,
        intensity: ThinkingIntensity,
        permissions: Arc<PermissionBroker>,
        input: Option<&mut CliInput>,
    ) -> HeadlessOutcome {
        self.run_turn_inner(
            prompt,
            intensity,
            permissions,
            TurnMode::Cli { text, input },
        )
        .await
    }

    /// Non-printing turn; the UI owns permission requests and cancellation.
    pub(crate) async fn run_turn_ui(
        &mut self,
        prompt: String,
        intensity: ThinkingIntensity,
        permissions: Arc<PermissionBroker>,
        events: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
        cancel: tokio::sync::oneshot::Receiver<()>,
        steering: Option<cyber_agent::SteeringReceiver>,
    ) -> HeadlessOutcome {
        self.run_turn_inner(
            prompt,
            intensity,
            permissions,
            TurnMode::Ui {
                events,
                cancel,
                steering,
            },
        )
        .await
    }

    async fn run_turn_inner(
        &mut self,
        prompt: String,
        intensity: ThinkingIntensity,
        permissions: Arc<PermissionBroker>,
        mode: TurnMode<'_>,
    ) -> HeadlessOutcome {
        let started_at = Instant::now();
        let (text, mut input, observer, mut cancel, steering) = match mode {
            TurnMode::Cli { text, input } => (text, input, None, None, None),
            TurnMode::Ui {
                events,
                cancel,
                steering,
            } => (false, None, Some(events), Some(cancel), steering),
        };
        let memory = match self.memory_prompt() {
            Ok(memory) => memory,
            Err(error) => {
                return HeadlessOutcome {
                    session_id: self.index.current.clone(),
                    answer: String::new(),
                    tool_calls: Vec::new(),
                    error: Some(format!("Cannot load memory: {error}")),
                    permission_denied: false,
                }
            }
        };
        let history = entries_to_messages(&self.entries);
        let denials_before = permissions.denial_count();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(run_stream_with_permissions(
            self.ctx.config.clone(),
            self.ctx.providers.clone(),
            self.ctx.project.clone(),
            prompt.clone(),
            history,
            tx,
            0,
            self.mock,
            self.cwd.clone(),
            self.registry.clone(),
            self.ctf_enabled,
            intensity,
            memory,
            permissions.clone(),
            steering,
            Some(Arc::clone(&self.registries.subagents)),
            Some(Arc::clone(&self.registries.background)),
        ));
        let mut turn = TurnHistory::new(prompt);
        let mut pending: Option<PermissionRequest> = None;
        let mut cancelled = false;
        let mut requests_open = true;
        // Register once: recreating ctrl_c futures must not leave signal gaps.
        let interrupt = tokio::signal::ctrl_c();
        tokio::pin!(interrupt);
        loop {
            let (lines, requests) = match input.as_deref_mut() {
                Some(input) => (Some(&mut input.lines), Some(&mut input.requests)),
                None => (None, None),
            };
            tokio::select! {
                biased;
                _ = async {
                    match cancel.as_mut() {
                        Some(cancel) => { let _ = cancel.await; }
                        None => std::future::pending().await,
                    }
                }, if !cancelled => {
                    cancelled = true;
                    turn.error = Some("Cancelled; completed history retained. Tool side effects may already have occurred.".into());
                    handle.abort();
                }
                _ = async {
                    match observer.as_ref() {
                        Some(observer) => observer.closed().await,
                        None => std::future::pending().await,
                    }
                }, if !cancelled => {
                    cancelled = true;
                    turn.error = Some("UI event observer closed; completed history retained. Tool side effects may already have occurred.".into());
                    handle.abort();
                }
                signal = &mut interrupt, if !cancelled => {
                    cancelled = true;
                    turn.error = Some(match signal {
                        Ok(()) => "Cancelled; completed history retained. Tool side effects may already have occurred.".into(),
                        Err(e) => format!("Cannot listen for Ctrl+C: {e}"),
                    });
                    pending.take();
                    handle.abort();
                    // Drain already-emitted results before completing the history.
                }
                _ = tokio::signal::ctrl_c(), if cancelled => {
                    // 已处于取消状态时再次收到 Ctrl+C：强制立即退出，绝不卡死
                    break;
                }
                _ = tokio::time::sleep(Duration::from_millis(1500)), if cancelled => {
                    // 取消后排空缓冲最多等待 1.5 秒，超时强制退出
                    break;
                }
                event = rx.recv() => {
                    match event {
                        Some((_, event)) => {
                            let done = matches!(event, AgentEvent::Done);
                            if let Some(observer) = &observer {
                                if observer.send(event.clone()).is_err() && !cancelled {
                                    cancelled = true;
                                    turn.error = Some("UI event observer closed; completed history retained. Tool side effects may already have occurred.".into());
                                    handle.abort();
                                }
                            }
                            turn.record(event, text);
                            if done { break; }
                        }
                        None => break,
                    }
                }
                request = async {
                    match requests {
                        Some(requests) => requests.recv().await,
                        None => std::future::pending().await,
                    }
                }, if pending.is_none() && !cancelled && requests_open => {
                    if let Some(request) = request {
                        eprintln!("{}", terminal_text(&format!("\n[approval] tool={} arguments={}\nReply: once {} / session {} / deny\nSession scope: this tool + exact JSON arguments only.", request.tool, request.arguments, request.nonce, request.nonce)));
                        pending = Some(request);
                    } else {
                        requests_open = false;
                    }
                }
                line = async {
                    match lines {
                        Some(lines) => lines.recv().await,
                        None => std::future::pending().await,
                    }
                }, if pending.is_some() && !cancelled => {
                    if let Some(request) = pending.take() {
                        let decision = match line {
                            Some(Ok(line)) => request.decision_for(&line),
                            _ => PermissionDecision::Deny,
                        };
                        let _ = request.reply.send(decision);
                    }
                }
            }
        }
        if let Err(error) = handle.await {
            if !error.is_cancelled() {
                turn.error = Some(format!("Agent task failed: {error}"));
            }
        }
        if let Some(input) = input {
            // Cancelled requests must not be presented as next-turn approvals.
            while input.requests.try_recv().is_ok() {}
        }
        turn.finish_pending();
        let permission_denied = permissions.denial_count() != denials_before;
        if permission_denied {
            turn.error
                .get_or_insert_with(|| "Tool execution denied by permission policy.".into());
        }
        if turn.compacted {
            self.entries.clear();
        }
        self.entries.extend(turn.entries);
        if observer.is_some() {
            let status = if cancelled {
                "cancelled"
            } else if turn.error.is_some() {
                "error"
            } else {
                "done"
            };
            let summary = self.generate_turn_summary(
                status,
                &turn.answer,
                &turn.tool_calls,
                turn.error.as_deref(),
            );
            self.entries.push(ChatEntry::TurnSummary {
                elapsed_ms: started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                finished_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                status: status.into(),
                summary: Some(summary),
            });
        }
        self.save_turn_outcome(&mut turn.error);
        HeadlessOutcome {
            session_id: self.index.current.clone(),
            answer: turn.answer,
            tool_calls: turn.tool_calls,
            error: turn.error,
            permission_denied,
        }
    }
}

async fn collect_task_events(
    handle: tokio::task::JoinHandle<()>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<(u64, AgentEvent)>,
    events: &tokio::sync::mpsc::UnboundedSender<AgentEvent>,
    cancel: &mut tokio::sync::oneshot::Receiver<()>,
    turn: &mut TurnHistory,
) -> bool {
    let _abort_on_drop = AbortOnDrop(handle.abort_handle());
    let mut cancelled = false;
    loop {
        tokio::select! {
            biased;
            _ = &mut *cancel, if !cancelled => { cancelled = true; turn.error = Some("Cancelled; completed history retained".into()); handle.abort(); }
            _ = events.closed(), if !cancelled => { cancelled = true; turn.error = Some("UI event observer closed".into()); handle.abort(); }
            event = rx.recv() => match event {
                Some((_,event)) => { let _ = events.send(event.clone()); turn.record(event,false); }
                None => break,
            }
        }
    }
    if let Err(error) = handle.await {
        if !error.is_cancelled() {
            turn.error = Some(format!("Task failed: {error}"));
        }
    }
    cancelled
}

struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct SharedCliTool {
    schema: cyber_agent::ToolSchema,
    base: Arc<ToolRegistry>,
}
impl cyber_agent::Tool for SharedCliTool {
    fn schema(&self) -> cyber_agent::ToolSchema {
        self.schema.clone()
    }
    fn run<'a>(
        &'a self,
        input: serde_json::Value,
        ctx: &'a cyber_agent::ToolCtx,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = cyber_agent::Result<cyber_agent::ToolOutput>>
                + Send
                + 'a,
        >,
    > {
        self.base.execute(&self.schema.name, input, ctx)
    }
    fn run_streaming<'a>(
        &'a self,
        input: serde_json::Value,
        ctx: &'a cyber_agent::ToolCtx,
        progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = cyber_agent::Result<cyber_agent::ToolOutput>>
                + Send
                + 'a,
        >,
    > {
        self.base
            .execute_streaming(&self.schema.name, input, ctx, progress)
    }
}

struct TurnHistory {
    entries: Vec<ChatEntry>,
    answer: String,
    tool_calls: Vec<ToolCallRecord>,
    error: Option<String>,
    compacted: bool,
    pending_calls: Vec<usize>,
    leading_answer: String,
    leading_reasoning: String,
}

impl TurnHistory {
    fn new(prompt: String) -> Self {
        Self {
            entries: vec![ChatEntry::User(prompt)],
            answer: String::new(),
            tool_calls: Vec::new(),
            error: None,
            compacted: false,
            pending_calls: Vec::new(),
            leading_answer: String::new(),
            leading_reasoning: String::new(),
        }
    }

    fn record_text(&mut self, token: &str, thinking: bool) {
        let has_entry = self
            .entries
            .iter()
            .rev()
            .take_while(|entry| matches!(entry, ChatEntry::Assistant(_) | ChatEntry::Thinking(_)))
            .any(|entry| matches!(entry, ChatEntry::Thinking(_)) == thinking);
        let leading = if thinking {
            &mut self.leading_reasoning
        } else {
            &mut self.leading_answer
        };
        // Preserve indentation without creating a title for a whitespace-only stream.
        if !has_entry && token.trim().is_empty() {
            leading.push_str(token);
        } else if leading.is_empty() {
            append_stream_text(&mut self.entries, token, thinking);
        } else {
            leading.push_str(token);
            let content = std::mem::take(leading);
            append_stream_text(&mut self.entries, &content, thinking);
        }
    }

    fn record(&mut self, event: AgentEvent, text: bool) {
        if matches!(
            event,
            AgentEvent::Started
                | AgentEvent::ToolCall { .. }
                | AgentEvent::ToolProgress { .. }
                | AgentEvent::ToolResult { .. }
                | AgentEvent::Compacting { .. }
                | AgentEvent::Compacted { .. }
                | AgentEvent::SteeringReceived(_)
                | AgentEvent::Error(_)
                | AgentEvent::Retry { .. }
                | AgentEvent::Done
        ) {
            self.leading_answer.clear();
            self.leading_reasoning.clear();
        }
        match event {
            AgentEvent::Token(token) => {
                self.answer.push_str(&token);
                self.record_text(&token, false);
                if text {
                    print!("{}", terminal_text(&token));
                    let _ = std::io::stdout().flush();
                }
            }
            AgentEvent::Reasoning(token) => {
                self.record_text(&token, true);
                if text {
                    eprint!("{}", terminal_text(&token));
                }
            }
            AgentEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                if text {
                    eprintln!("{}", terminal_text(&format!("\n[tool] {name} {arguments}")));
                }
                self.entries.push(ChatEntry::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
                self.pending_calls.push(self.tool_calls.len());
                self.tool_calls.push(ToolCallRecord {
                    id,
                    name,
                    arguments,
                    output: String::new(),
                    is_error: false,
                });
            }
            AgentEvent::ToolProgress { name, chunk, .. } => {
                if text {
                    eprint!("{}", terminal_text(&format!("[{name}] {chunk}")));
                }
            }
            AgentEvent::ToolResult {
                id,
                name,
                output,
                is_error,
            } => {
                if text {
                    eprintln!(
                        "{}",
                        terminal_text(&format!(
                            "[tool] {name}: {}\n{output}",
                            if is_error { "error" } else { "done" }
                        ))
                    );
                }
                if let Some(position) = self
                    .pending_calls
                    .iter()
                    .rposition(|&index| self.tool_calls[index].id == id)
                {
                    let index = self.pending_calls.remove(position);
                    let call = &mut self.tool_calls[index];
                    call.output = output.clone();
                    call.is_error = is_error;
                }
                self.entries.push(ChatEntry::ToolResult {
                    id,
                    name,
                    output,
                    is_error,
                });
            }
            AgentEvent::Notice(msg) => {
                self.entries.push(ChatEntry::System(msg.clone()));
                if text {
                    eprintln!("{}", terminal_text(&format!("\n[notice] {msg}")));
                }
            }
            AgentEvent::Error(error) => {
                if text {
                    eprintln!("{}", terminal_text(&format!("\n[error] {error}")));
                }
                self.error = Some(error);
            }
            AgentEvent::Compacted { summary, .. } => {
                self.finish_pending();
                self.entries = vec![ChatEntry::User(summary)];
                self.compacted = true;
            }
            AgentEvent::Retry {
                attempt,
                max_retries,
                delay_secs,
                error,
            } if text => {
                eprintln!(
                    "{}",
                    terminal_text(&format!(
                        "\n[retry {attempt}/{max_retries} ({delay_secs}s)] {error}"
                    ))
                );
            }
            _ => {}
        }
    }

    fn finish_pending(&mut self) {
        self.leading_answer.clear();
        self.leading_reasoning.clear();
        // Never invent a successful empty result for a cancelled tool call.
        for index in self.pending_calls.drain(..) {
            let call = &mut self.tool_calls[index];
            call.output =
                "Tool interrupted; completion unknown. Check side effects before retrying.".into();
            call.is_error = true;
            self.entries.push(ChatEntry::ToolResult {
                id: call.id.clone(),
                name: call.name.clone(),
                output: call.output.clone(),
                is_error: true,
            });
        }
    }
}

pub async fn run_headless(cwd: &Path, args: HeadlessArgs) -> HeadlessOutcome {
    let result = async {
        if !matches!(args.format.as_str(), "" | "text" | "json") {
            color_eyre::eyre::bail!("Invalid output format: {}", args.format);
        }
        // Reject malformed explicit options before bootstrap or session writes.
        if let Some(id) = args.session.as_deref() {
            validate_session_id(id)?;
        }
        if args.think.is_some() {
            thinking(args.think.as_deref(), ThinkingIntensity::Auto)?;
        }
        let mut runner = SessionRunner::new(cwd, args.mock).await?;
        runner.registries.question_broker.set_headless();
        let permissions = runner.explicit_permissions(&args.allow_tools)?;
        let intensity = thinking(
            args.think.as_deref(),
            runner.ctx.config.agent.thinking_intensity,
        )?;
        if let Some(provider) = &args.provider {
            runner.select_model(provider, args.model.as_deref())?;
        } else if let Some(model) = &args.model {
            let provider = runner.ctx.config.agent.default_provider.clone();
            runner.select_model(&provider, Some(model))?;
        }
        if let Some(steps) = args.max_steps {
            runner.ctx.config.agent.max_steps = steps;
        }
        if args.new {
            runner.create_session()?;
        } else if let Some(id) = &args.session {
            runner.select_session(id)?;
        }
        Ok::<_, color_eyre::Report>(
            runner
                .run_turn(
                    args.prompt,
                    args.format != "json",
                    intensity,
                    permissions,
                    None,
                )
                .await,
        )
    }
    .await;
    result.unwrap_or_else(|error| HeadlessOutcome {
        session_id: String::new(),
        answer: String::new(),
        tool_calls: Vec::new(),
        error: Some(error.to_string()),
        permission_denied: false,
    })
}

pub fn outcome_to_json(outcome: &HeadlessOutcome) -> String {
    json!({
        "session_id": outcome.session_id,
        "success": outcome.error.is_none(),
        "answer": outcome.answer,
        "tool_calls": outcome.tool_calls,
        "error": outcome.error,
        "permission_denied": outcome.permission_denied,
    })
    .to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use cyber_agent::PermissionMode;

    #[test]
    fn rejects_traversal_and_index_ids() {
        for id in [
            "",
            "../escape",
            "..\\escape",
            "C:evil",
            "/root",
            "index",
            "INDEX",
            "a.b",
            "CON",
            "nul",
            "com1",
        ] {
            assert!(validate_session_id(id).is_err(), "{id}");
        }
        assert!(validate_session_id("abc123_test-1").is_ok());
    }

    #[test]
    fn rejects_invalid_think() {
        assert!(thinking(Some("typo"), ThinkingIntensity::Auto).is_err());
        assert!(thinking(None, ThinkingIntensity::Auto).is_ok());
    }

    #[test]
    fn preserves_text_tool_text_order_and_completed_results() {
        let mut turn = TurnHistory::new("prompt".into());
        turn.record(AgentEvent::Token("before".into()), false);
        turn.record(
            AgentEvent::ToolCall {
                id: "1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            },
            false,
        );
        turn.record(
            AgentEvent::ToolResult {
                id: "1".into(),
                name: "shell".into(),
                output: "ok".into(),
                is_error: false,
            },
            false,
        );
        turn.record(AgentEvent::Token("after".into()), false);
        turn.finish_pending();
        assert!(matches!(&turn.entries[1], ChatEntry::Assistant(t) if t == "before"));
        assert!(matches!(&turn.entries[2], ChatEntry::ToolCall { .. }));
        assert!(matches!(
            &turn.entries[3],
            ChatEntry::ToolResult {
                is_error: false,
                ..
            }
        ));
        assert!(matches!(&turn.entries[4], ChatEntry::Assistant(t) if t == "after"));
        let messages = entries_to_messages(&turn.entries);
        assert_eq!(messages[1].content, "before");
        assert_eq!(messages[1].tool_calls.len(), 1);
        assert_eq!(messages.last().unwrap().content, "after");
    }

    #[test]
    fn interleaved_stream_merges_text_and_ignores_empty_entry_starters() {
        let mut turn = TurnHistory::new("prompt".into());
        for event in [
            AgentEvent::Token("".into()),
            AgentEvent::Reasoning(" \n".into()),
            AgentEvent::Token(" \t".into()),
        ] {
            turn.record(event, false);
        }
        assert_eq!(turn.entries.len(), 1);
        for event in [
            AgentEvent::Token("hello".into()),
            AgentEvent::Reasoning("think".into()),
            AgentEvent::Token(" ".into()),
            AgentEvent::Reasoning("\n".into()),
            AgentEvent::Token("world".into()),
            AgentEvent::Reasoning("more".into()),
            AgentEvent::Token("".into()),
        ] {
            turn.record(event, false);
        }
        assert_eq!(turn.entries.len(), 3);
        assert!(matches!(&turn.entries[1], ChatEntry::Assistant(t) if t == " \thello world"));
        assert!(matches!(&turn.entries[2], ChatEntry::Thinking(t) if t == " \nthink\nmore"));
        let messages = entries_to_messages(&turn.entries);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].content, " \thello world");

        turn.record(
            AgentEvent::ToolCall {
                id: "1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            },
            false,
        );
        turn.record(AgentEvent::Token("during".into()), false);
        turn.record(
            AgentEvent::ToolResult {
                id: "1".into(),
                name: "shell".into(),
                output: "ok".into(),
                is_error: false,
            },
            false,
        );
        turn.record(AgentEvent::Reasoning("next".into()), false);
        turn.record(AgentEvent::Token("after".into()), false);
        assert!(matches!(&turn.entries[3], ChatEntry::ToolCall { .. }));
        assert!(matches!(&turn.entries[4], ChatEntry::Assistant(t) if t == "during"));
        assert!(matches!(&turn.entries[5], ChatEntry::ToolResult { .. }));
        assert!(matches!(&turn.entries[7], ChatEntry::Assistant(t) if t == "after"));
        turn.record(
            AgentEvent::Compacted {
                summary: "summary".into(),
                before_tokens: 100,
                after_tokens: 1,
            },
            false,
        );
        turn.record(AgentEvent::Token("fresh".into()), false);
        assert_eq!(turn.entries.len(), 2);
        assert!(matches!(&turn.entries[1], ChatEntry::Assistant(t) if t == "fresh"));
    }

    #[test]
    fn leading_answer_whitespace_preserves_markdown_in_history() {
        let mut turn = TurnHistory::new("prompt".into());
        turn.record(AgentEvent::Token("  ".into()), false);
        assert_eq!(turn.entries.len(), 1);
        turn.record(AgentEvent::Token("**bold answer**".into()), false);
        turn.finish_pending();
        assert_eq!(turn.answer, "  **bold answer**");
        assert!(matches!(&turn.entries[1], ChatEntry::Assistant(t) if t == &turn.answer));
        assert_eq!(entries_to_messages(&turn.entries)[1].content, turn.answer);
    }

    #[test]
    fn leading_whitespace_survives_reasoning_interleaving_without_reordering_entries() {
        let mut turn = TurnHistory::new("prompt".into());
        for event in [
            AgentEvent::Token("  ".into()),
            AgentEvent::Reasoning("\t".into()),
            AgentEvent::Reasoning("think".into()),
            AgentEvent::Token("\n".into()),
            AgentEvent::Reasoning(" ".into()),
        ] {
            turn.record(event, false);
        }
        assert_eq!(turn.entries.len(), 2);
        assert!(matches!(&turn.entries[1], ChatEntry::Thinking(t) if t == "\tthink "));
        for event in [
            AgentEvent::Token("**answer**".into()),
            AgentEvent::Reasoning("more".into()),
            AgentEvent::Token(" \t".into()),
            AgentEvent::Reasoning("\n".into()),
            AgentEvent::Token("text".into()),
        ] {
            turn.record(event, false);
        }
        turn.finish_pending();
        assert_eq!(turn.entries.len(), 3);
        assert!(matches!(&turn.entries[1], ChatEntry::Thinking(t) if t == "\tthink more\n"));
        assert!(
            matches!(&turn.entries[2], ChatEntry::Assistant(t) if t == "  \n**answer** \ttext")
        );
        assert_eq!(entries_to_messages(&turn.entries)[1].content, turn.answer);
    }

    #[test]
    fn leading_whitespace_does_not_cross_boundaries_or_create_blank_entries() {
        for boundary in [
            AgentEvent::ToolCall {
                id: "1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            },
            AgentEvent::ToolResult {
                id: "1".into(),
                name: "shell".into(),
                output: "ok".into(),
                is_error: false,
            },
            AgentEvent::ToolProgress {
                id: "1".into(),
                name: "shell".into(),
                chunk: "output".into(),
            },
            AgentEvent::Compacting { is_auto: true },
            AgentEvent::Compacted {
                summary: "summary".into(),
                before_tokens: 100,
                after_tokens: 1,
            },
            AgentEvent::Error("error".into()),
            AgentEvent::Done,
        ] {
            let mut turn = TurnHistory::new("prompt".into());
            turn.record(AgentEvent::Token("  ".into()), false);
            turn.record(AgentEvent::Reasoning("\t".into()), false);
            turn.record(boundary, false);
            turn.record(AgentEvent::Reasoning("think".into()), false);
            turn.record(AgentEvent::Token("answer".into()), false);
            assert!(matches!(turn.entries.last(), Some(ChatEntry::Assistant(t)) if t == "answer"));
            assert!(
                matches!(&turn.entries[turn.entries.len() - 2], ChatEntry::Thinking(t) if t == "think")
            );
        }
        let mut turn = TurnHistory::new("prompt".into());
        turn.record(AgentEvent::Token("  ".into()), false);
        turn.record(AgentEvent::Reasoning("\n".into()), false);
        turn.finish_pending();
        assert_eq!(turn.entries.len(), 1);
        assert!(turn.leading_answer.is_empty());
        assert!(turn.leading_reasoning.is_empty());
    }

    #[test]
    fn interrupted_call_gets_explicit_unknown_result() {
        let mut turn = TurnHistory::new("prompt".into());
        turn.record(
            AgentEvent::ToolCall {
                id: "1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            },
            false,
        );
        turn.finish_pending();
        assert!(turn.tool_calls[0].is_error);
        assert!(turn.tool_calls[0].output.contains("unknown"));
    }

    pub(crate) async fn test_runner() -> SessionRunner {
        let cwd = tempfile::tempdir().unwrap().keep();
        let paths = cyber_core::Paths::at(cwd.clone()).unwrap();
        let index = load_index(&paths.history_dir, &cwd);
        let (registries, _) = build_registries(&paths, &cwd, true, true).await;
        SessionRunner {
            ctx: AppContext {
                config: cyber_core::Config::default(),
                providers: cyber_core::ProvidersConfig::default_template(),
                project: None,
                paths,
                is_first_run: false,
            },
            cwd,
            index,
            entries: Vec::new(),
            registry: registries.tools.clone(),
            registries,
            ctf_enabled: false,
            mock: true,
        }
    }

    fn assert_saved_history(runner: &SessionRunner) -> Vec<ChatEntry> {
        let saved = load_entries(
            &runner.ctx.paths.history_dir,
            &runner.cwd,
            &runner.index.current,
        );
        assert_eq!(
            serde_json::to_value(&saved).unwrap(),
            serde_json::to_value(&runner.entries).unwrap()
        );
        saved
    }

    #[tokio::test]
    async fn ui_turn_does_not_print_to_stdout_or_stderr() {
        const CHILD: &str = "CYBER_UI_SILENT_TEST_CHILD";
        const PROMPT: &str = "ui-silent-output-marker";
        if std::env::var_os(CHILD).is_some() {
            let mut runner = test_runner().await;
            runner.ctx.config.agent.auto_tool_call = false;
            let (events, _observed) = tokio::sync::mpsc::unbounded_channel();
            let (_cancel, cancel) = tokio::sync::oneshot::channel();
            let outcome = runner
                .run_turn_ui(
                    PROMPT.into(),
                    ThinkingIntensity::Auto,
                    Arc::new(PermissionBroker::deny_all()),
                    events,
                    cancel,
                    None,
                )
                .await;
            assert!(outcome.answer.contains(PROMPT));
            assert!(outcome.error.is_none());
            assert!(
                matches!(assert_saved_history(&runner).last(), Some(ChatEntry::TurnSummary { status, .. }) if status == "done")
            );
            runner.ctx.config.agent.auto_tool_call = true;
            let (events, _observed) = tokio::sync::mpsc::unbounded_channel();
            let (_cancel, cancel) = tokio::sync::oneshot::channel();
            let outcome = runner
                .run_turn_ui(
                    "list files".into(),
                    ThinkingIntensity::Auto,
                    Arc::new(PermissionBroker::deny_all()),
                    events,
                    cancel,
                    None,
                )
                .await;
            assert!(outcome.permission_denied);
            assert!(
                matches!(assert_saved_history(&runner).last(), Some(ChatEntry::TurnSummary { status, .. }) if status == "error")
            );
            let _ = std::fs::remove_dir_all(&runner.cwd);
            return;
        }
        // A separate process captures both streams without global test interference.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "headless::tests::ui_turn_does_not_print_to_stdout_or_stderr",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains(PROMPT));
        assert!(output.stderr.is_empty(), "{output:?}");
    }

    #[tokio::test]
    async fn ui_observer_receives_stream_and_external_denial_with_compaction() {
        let mut runner = test_runner().await;
        runner.entries = vec![ChatEntry::User("old-marker".into())];
        runner.ctx.config.agent.max_steps = 1;
        let provider = runner
            .ctx
            .providers
            .providers
            .get_mut(&runner.ctx.config.agent.default_provider)
            .unwrap();
        provider
            .models
            .entry(provider.model.clone())
            .or_default()
            .context_length = Some(5000);
        let (broker, mut requests) = PermissionBroker::interactive();
        broker.set_mode(PermissionMode::Manual);
        let (events, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel, cancel) = tokio::sync::oneshot::channel();
        let (outcome, ()) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            tokio::join!(
                runner.run_turn_ui(
                    "list files".into(),
                    ThinkingIntensity::Auto,
                    Arc::new(broker),
                    events,
                    cancel,
                    None,
                ),
                async {
                    let request = requests.recv().await.unwrap();
                    request.reply.send(PermissionDecision::Deny).unwrap();
                }
            )
        })
        .await
        .expect("UI must leave permission requests to its caller");
        assert!(outcome.permission_denied);
        assert!(outcome.error.is_some());
        let mut replay = TurnHistory::new("list files".into());
        let mut started = false;
        let mut done = false;
        let mut context = false;
        while let Some(event) = observed.recv().await {
            started |= matches!(event, AgentEvent::Started);
            done |= matches!(event, AgentEvent::Done);
            context |= matches!(event, AgentEvent::ContextUpdate { .. });
            replay.record(event, false);
        }
        assert!(started && done && context);
        assert!(replay.compacted);
        assert_eq!(replay.answer, outcome.answer);
        assert_eq!(outcome.tool_calls.len(), 1);
        assert!(outcome.tool_calls[0].output.contains("Permission denied"));
        let saved = assert_saved_history(&runner);
        assert_eq!(
            serde_json::to_value(&saved[..saved.len() - 1]).unwrap(),
            serde_json::to_value(&replay.entries).unwrap()
        );
        assert!(!saved
            .iter()
            .any(|entry| matches!(entry, ChatEntry::User(t) if t == "old-marker")));
        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    async fn assert_ui_interruption_saves_emitted_history(close_observer: bool) {
        let mut runner = test_runner().await;
        let (broker, mut requests) = PermissionBroker::interactive();
        broker.set_mode(PermissionMode::Manual);
        let broker = Arc::new(broker);
        let (events, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let (cancel_tx, cancel) = tokio::sync::oneshot::channel();
        let mut cancel_tx = Some(cancel_tx);
        let (outcome, request) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(
                runner.run_turn_ui(
                    "list files".into(),
                    ThinkingIntensity::Auto,
                    broker.clone(),
                    events,
                    cancel,
                    None,
                ),
                async {
                    // The agent emitted text and a call, then blocked on approval.
                    let request = requests.recv().await.unwrap();
                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                    if close_observer {
                        observed.close();
                    } else {
                        cancel_tx.take().unwrap().send(()).unwrap();
                    }
                    request
                }
            )
        })
        .await
        .expect("UI interruption must abort an agent blocked on permission");
        assert!(request.reply.is_closed());
        assert!(!outcome.permission_denied);
        assert_eq!(broker.denial_count(), 0);
        let error = outcome.error.unwrap();
        assert!(error.contains(if close_observer {
            "observer closed"
        } else {
            "Cancelled"
        }));
        assert!(!outcome.answer.is_empty());
        assert_eq!(outcome.tool_calls.len(), 1);
        assert!(outcome.tool_calls[0].is_error);
        assert!(outcome.tool_calls[0].output.contains("completion unknown"));
        let saved = assert_saved_history(&runner);
        assert!(
            matches!(saved.last(), Some(ChatEntry::TurnSummary { elapsed_ms, finished_at, status, .. })
            if *elapsed_ms >= 40 && *finished_at > 0 && status == "cancelled")
        );
        assert!(matches!(&saved[1], ChatEntry::Assistant(t) if t == &outcome.answer));
        assert!(matches!(&saved[2], ChatEntry::ToolCall { .. }));
        assert!(matches!(
            &saved[3],
            ChatEntry::ToolResult { is_error: true, .. }
        ));
        if !close_observer {
            let mut replay = TurnHistory::new("list files".into());
            while let Some(event) = observed.recv().await {
                replay.record(event, false);
            }
            replay.finish_pending();
            assert_eq!(
                serde_json::to_value(&saved[..saved.len() - 1]).unwrap(),
                serde_json::to_value(&replay.entries).unwrap()
            );
        }
        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    #[tokio::test]
    async fn ui_cancel_drains_and_saves_emitted_history() {
        assert_ui_interruption_saves_emitted_history(false).await;
    }

    #[tokio::test]
    async fn ui_closed_observer_aborts_and_saves_emitted_history() {
        assert_ui_interruption_saves_emitted_history(true).await;
    }

    #[tokio::test]
    async fn multiple_turns_reuse_registry_and_tui_history_and_switch_sessions() {
        let mut runner = test_runner().await;
        runner.ctx.config.agent.auto_tool_call = false;
        let registry = runner.registry.clone();
        let original = runner.index.current.clone();
        for prompt in ["first", "second"] {
            let outcome = runner
                .run_turn(
                    prompt.into(),
                    false,
                    ThinkingIntensity::Auto,
                    Arc::new(PermissionBroker::deny_all()),
                    None,
                )
                .await;
            assert!(outcome.error.is_none());
            assert!(outcome.answer.contains(prompt));
        }
        assert!(Arc::ptr_eq(&registry, &runner.registry));
        let saved = load_entries(&runner.ctx.paths.history_dir, &runner.cwd, &original);
        assert_eq!(saved.len(), 4);
        assert_eq!(entries_to_messages(&saved).len(), 4);
        runner.create_session().unwrap();
        assert!(runner.entries.is_empty());
        assert_ne!(runner.index.current, original);
        runner.select_session(&original).unwrap();
        assert_eq!(runner.entries.len(), 4);
        assert!(runner.select_session("../outside").is_err());
        assert_eq!(runner.index.current, original);
        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    #[tokio::test]
    async fn noninteractive_mock_tool_loop_records_denial_in_real_event_order() {
        let mut runner = test_runner().await;
        let args = HeadlessArgs::default();
        assert!(args.allow_tools.is_empty());
        let permissions = runner.explicit_permissions(&args.allow_tools).unwrap();
        let outcome = runner
            .run_turn(
                "list files".into(),
                false,
                ThinkingIntensity::Auto,
                permissions,
                None,
            )
            .await;
        assert!(outcome.error.is_some());
        assert!(outcome.permission_denied);
        let json: serde_json::Value = serde_json::from_str(&outcome_to_json(&outcome)).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["permission_denied"], true);
        assert_eq!(outcome.tool_calls.len(), 1);
        assert!(outcome.tool_calls[0].is_error);
        assert!(outcome.tool_calls[0].output.contains("Permission denied"));
        assert!(matches!(&runner.entries[1], ChatEntry::Assistant(_)));
        assert!(matches!(&runner.entries[2], ChatEntry::ToolCall { .. }));
        assert!(matches!(
            &runner.entries[3],
            ChatEntry::ToolResult { is_error: true, .. }
        ));
        assert!(matches!(&runner.entries[4], ChatEntry::Assistant(_)));
        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    #[tokio::test]
    async fn invalid_headless_options_fail_before_loading_context() {
        let outcome = run_headless(
            Path::new("."),
            HeadlessArgs {
                think: Some("typo".into()),
                ..Default::default()
            },
        )
        .await;
        assert!(outcome.error.unwrap().contains("Invalid think"));
        let outcome = run_headless(
            Path::new("."),
            HeadlessArgs {
                session: Some("../outside".into()),
                ..Default::default()
            },
        )
        .await;
        assert!(outcome.error.unwrap().contains("Invalid session"));
    }

    #[tokio::test]
    async fn prefetched_bare_approval_never_authorizes_a_tool() {
        let mut runner = test_runner().await;
        let (broker, requests) = PermissionBroker::interactive();
        broker.set_mode(PermissionMode::Manual);
        let broker = Arc::new(broker);
        let (tx, lines) = tokio::sync::mpsc::channel(1);
        let mut input = CliInput { lines, requests };
        for response in ["once", "session"] {
            runner.create_session().unwrap();
            // This line is queued before the agent creates the approval request.
            tx.send(Ok(response.to_owned())).await.unwrap();
            let outcome = runner
                .run_turn(
                    "list files".into(),
                    false,
                    ThinkingIntensity::Auto,
                    broker.clone(),
                    Some(&mut input),
                )
                .await;
            assert!(outcome.permission_denied);
            assert!(outcome.error.is_some());
            assert!(outcome.tool_calls[0].is_error);
        }
        assert_eq!(broker.denial_count(), 2);
        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    #[tokio::test]
    async fn recoverable_tool_error_is_not_a_permission_failure() {
        struct FailingTool;
        impl cyber_agent::Tool for FailingTool {
            fn schema(&self) -> cyber_agent::ToolSchema {
                cyber_agent::ToolSchema {
                    name: "list_dir".into(),
                    description: "test tool error".into(),
                    parameters: json!({"type": "object"}),
                    tags: vec![],
                }
            }
            fn run<'a>(
                &'a self,
                _: serde_json::Value,
                _: &'a cyber_agent::ToolCtx,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = cyber_agent::Result<cyber_agent::ToolOutput>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(async {
                    Ok(cyber_agent::ToolOutput {
                        content: "Permission denied from an ordinary tool, not the broker".into(),
                        is_error: true,
                    })
                })
            }
        }
        let mut runner = test_runner().await;
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(FailingTool));
        runner.registry = Arc::new(registry);
        let (broker, mut requests) = PermissionBroker::interactive();
        broker.set_mode(PermissionMode::Manual);
        let broker = Arc::new(broker);
        let responder = tokio::spawn(async move {
            let request = requests.recv().await.unwrap();
            request.reply.send(PermissionDecision::AllowOnce).unwrap();
        });
        let outcome = runner
            .run_turn(
                "list files".into(),
                false,
                ThinkingIntensity::Auto,
                broker.clone(),
                None,
            )
            .await;
        responder.await.unwrap();
        assert!(outcome.error.is_none());
        assert!(!outcome.permission_denied);
        assert_eq!(broker.denial_count(), 0);
        assert!(outcome.tool_calls[0].is_error);
        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    #[test]
    fn compaction_replaces_resume_history_without_restoring_old_calls() {
        let mut turn = TurnHistory::new("original prompt".into());
        turn.record(AgentEvent::Token("old text".into()), false);
        turn.record(
            AgentEvent::ToolCall {
                id: "1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            },
            false,
        );
        turn.record(
            AgentEvent::ToolResult {
                id: "1".into(),
                name: "shell".into(),
                output: "completed".into(),
                is_error: false,
            },
            false,
        );
        turn.record(
            AgentEvent::ToolCall {
                id: "2".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            },
            false,
        );
        turn.record(
            AgentEvent::Compacted {
                summary: "summary".into(),
                before_tokens: 100,
                after_tokens: 10,
            },
            false,
        );
        turn.record(AgentEvent::Token("new text".into()), false);
        turn.record(
            AgentEvent::ToolCall {
                id: "3".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            },
            false,
        );
        turn.finish_pending();
        assert!(turn.compacted);
        assert_eq!(turn.tool_calls.len(), 3);
        assert_eq!(turn.tool_calls[0].output, "completed");
        assert!(!turn.tool_calls[0].is_error);
        assert!(turn.tool_calls[1].is_error);
        assert!(turn.tool_calls[2].is_error);
        assert_eq!(turn.entries.len(), 4);
        assert!(matches!(&turn.entries[0], ChatEntry::User(t) if t == "summary"));
        assert!(!turn.entries.iter().any(|entry| matches!(entry, ChatEntry::ToolCall { id, .. } | ChatEntry::ToolResult { id, .. } if id == "1" || id == "2")));
        let messages = entries_to_messages(&turn.entries);
        assert_eq!(messages[0].content, "summary");
        assert_eq!(messages[1].content, "new text");
        assert_eq!(messages[1].tool_calls[0].id, "3");
        // A second compaction must not restore the previous pending call either.
        turn.record(
            AgentEvent::Compacted {
                summary: "new summary".into(),
                before_tokens: 100,
                after_tokens: 10,
            },
            false,
        );
        turn.finish_pending();
        assert_eq!(turn.entries.len(), 1);
        assert_eq!(turn.tool_calls[0].output, "completed");
    }

    #[test]
    fn reused_call_id_does_not_hide_an_interrupted_call() {
        let mut turn = TurnHistory::new("prompt".into());
        turn.record(
            AgentEvent::ToolCall {
                id: "same".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            },
            false,
        );
        turn.record(
            AgentEvent::ToolResult {
                id: "same".into(),
                name: "shell".into(),
                output: String::new(),
                is_error: false,
            },
            false,
        );
        turn.record(
            AgentEvent::ToolCall {
                id: "same".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            },
            false,
        );
        turn.finish_pending();
        assert!(!turn.tool_calls[0].is_error);
        assert!(turn.tool_calls[0].output.is_empty());
        assert!(turn.tool_calls[1].is_error);
        assert!(turn.tool_calls[1].output.contains("unknown"));
        assert_eq!(turn.entries.len(), 5);
        turn.finish_pending();
        assert_eq!(turn.entries.len(), 5);
    }

    #[tokio::test]
    async fn automatic_compaction_persists_only_summary_and_following_events() {
        let mut runner = test_runner().await;
        runner.entries = vec![
            ChatEntry::User("old-marker".into()),
            ChatEntry::Assistant("old-answer".into()),
        ];
        runner.ctx.config.agent.max_steps = 1;
        let provider_name = runner.ctx.config.agent.default_provider.clone();
        let provider = runner
            .ctx
            .providers
            .providers
            .get_mut(&provider_name)
            .unwrap();
        provider
            .models
            .entry(provider.model.clone())
            .or_default()
            .context_length = Some(5000);
        let broker = Arc::new(PermissionBroker::deny_all());
        let outcome = runner
            .run_turn(
                "compact this".into(),
                false,
                ThinkingIntensity::Auto,
                broker.clone(),
                None,
            )
            .await;
        assert!(outcome.permission_denied);
        assert_eq!(outcome.tool_calls.len(), 1);
        let saved = load_entries(
            &runner.ctx.paths.history_dir,
            &runner.cwd,
            &runner.index.current,
        );
        assert!(
            matches!(&saved[0], ChatEntry::User(summary) if summary.contains(&cyber_agent::compact_prompt(None)))
        );
        assert!(!saved.iter().any(
            |entry| matches!(entry, ChatEntry::User(t) if t == "old-marker" || t == "compact this")
        ));
        assert!(!saved
            .iter()
            .any(|entry| matches!(entry, ChatEntry::Assistant(t) if t == "old-answer")));
        assert_eq!(saved.len(), runner.entries.len());
        let summary_json = serde_json::to_value(&saved[0]).unwrap();
        let provider = runner
            .ctx
            .providers
            .providers
            .get_mut(&provider_name)
            .unwrap();
        provider
            .models
            .entry(provider.model.clone())
            .or_default()
            .context_length = Some(128_000);
        runner.ctx.config.agent.auto_tool_call = false;
        let outcome = runner
            .run_turn("next".into(), false, ThinkingIntensity::Auto, broker, None)
            .await;
        assert!(!outcome.permission_denied);
        assert!(outcome.error.is_none());
        assert_eq!(
            serde_json::to_value(&runner.entries[0]).unwrap(),
            summary_json
        );
        assert_eq!(runner.entries.len(), saved.len() + 2);
        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    #[tokio::test]
    async fn explicit_headless_allowlist_executes_only_the_named_tool() {
        let mut runner = test_runner().await;
        let args = HeadlessArgs {
            allow_tools: vec!["list_dir".into(), "list_dir".into()],
            ..Default::default()
        };
        let permissions = runner.explicit_permissions(&args.allow_tools).unwrap();
        let outcome = runner
            .run_turn(
                "list files".into(),
                false,
                ThinkingIntensity::Auto,
                permissions.clone(),
                None,
            )
            .await;
        assert!(outcome.error.is_none());
        assert!(!outcome.permission_denied);
        assert_eq!(outcome.tool_calls.len(), 1);
        assert_eq!(outcome.tool_calls[0].name, "list_dir");
        assert!(!outcome.tool_calls[0].is_error);
        let registry = ToolRegistry::with_permissions(runner.registry.clone(), permissions.clone());
        let ctx = cyber_agent::ToolCtx::new(runner.cwd.clone(), vec![], None, vec![]);
        let output = registry
            .execute(
                "write_file",
                json!({"path": "not-authorized.txt", "content": "no"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.is_error);
        assert_eq!(permissions.denial_count(), 1);
        assert!(!runner.cwd.join("not-authorized.txt").exists());
        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    #[tokio::test]
    async fn unknown_and_nonexact_allowed_tool_names_fail_validation() {
        let runner = test_runner().await;
        for name in [
            "unknown_tool",
            "*",
            "all",
            "ALL",
            "list_*",
            "list_?ir",
            "[list_dir]",
            "LIST_DIR",
        ] {
            assert!(
                runner.explicit_permissions(&[name.to_owned()]).is_err(),
                "{name}"
            );
        }
        assert!(runner
            .explicit_permissions(&["list_dir".into(), "unknown_tool".into()])
            .is_err());
        assert!(runner.entries.is_empty());
        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    #[tokio::test]
    async fn explicit_headless_authorization_does_not_bypass_builtin_guards() {
        let runner = test_runner().await;
        let permissions = runner.explicit_permissions(&["write_file".into()]).unwrap();
        let registry = ToolRegistry::with_permissions(runner.registry.clone(), permissions.clone());
        let ctx = cyber_agent::ToolCtx::new(runner.cwd.clone(), vec![], None, vec![]);
        let output = registry
            .execute(
                "write_file",
                json!({"path": "../outside.txt", "content": "no"}),
                &ctx,
            )
            .await;
        assert!(output.is_err());
        // Approval passed; the tool's own path guard rejected the operation.
        assert_eq!(permissions.denial_count(), 0);
        let _ = std::fs::remove_dir_all(&runner.cwd);
    }

    #[tokio::test]
    async fn queued_manual_summary_never_commits_after_cancel_or_error() {
        for cancel_first in [true, false] {
            let mut runner = test_runner().await;
            runner.entries = vec![
                ChatEntry::User("original history".into()),
                ChatEntry::Assistant("original answer".into()),
            ];
            let before = serde_json::to_value(&runner.entries).unwrap();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            tx.send((
                0,
                AgentEvent::Compacted {
                    summary: "queued summary".into(),
                    before_tokens: 100,
                    after_tokens: 10,
                },
            ))
            .unwrap();
            if !cancel_first {
                tx.send((0, AgentEvent::Error("failed after summary".into())))
                    .unwrap();
            }
            tx.send((0, AgentEvent::Done)).unwrap();
            drop(tx);
            let handle = tokio::spawn(async {});
            let (events, mut observed) = tokio::sync::mpsc::unbounded_channel();
            let (cancel_tx, mut cancel) = tokio::sync::oneshot::channel();
            if cancel_first {
                cancel_tx.send(()).unwrap();
            }
            let mut turn = TurnHistory::new(String::new());
            turn.entries.clear();
            let cancelled =
                collect_task_events(handle, &mut rx, &events, &mut cancel, &mut turn).await;
            assert_eq!(cancelled, cancel_first);
            assert!(turn.compacted);
            assert!(turn.error.is_some());
            assert!(observed.try_recv().is_ok());
            runner.commit_manual_compaction(&mut turn, cancelled);
            runner.save().unwrap();
            assert_eq!(serde_json::to_value(&runner.entries).unwrap(), before);
            assert_eq!(
                serde_json::to_value(runner.read_entries(&runner.index.current).unwrap()).unwrap(),
                before
            );
            let _ = std::fs::remove_dir_all(runner.cwd);
        }
    }

    #[tokio::test]
    async fn two_tool_registered_challenges_survive_id_deduplicating_reload() {
        let mut runner = test_runner().await;
        let ctx = cyber_agent::ToolCtx::new(runner.cwd.clone(), vec![], None, vec![]);
        for name in ["first", "second"] {
            let output = runner
                .registries
                .tools
                .execute(
                    "ctf_challenge",
                    json!({"action":"register","name":name,"category":"web"}),
                    &ctx,
                )
                .await
                .unwrap();
            assert!(!output.is_error);
        }
        let challenges = runner.challenges().unwrap();
        assert_eq!(challenges.len(), 2);
        assert_ne!(challenges[0].id, challenges[1].id);
        let original = runner.index.current.clone();
        runner.save().unwrap();
        runner.create_session().unwrap();
        runner.select_session(&original).unwrap();
        let reloaded = runner.challenges().unwrap();
        assert_eq!(reloaded.len(), 2);
        assert_eq!(
            reloaded.iter().map(|c| &c.id).collect::<Vec<_>>(),
            challenges.iter().map(|c| &c.id).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn next_turn_system_request_includes_only_enabled_scoped_memory_guidance() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut runner = test_runner().await;
        runner.mock = false;
        runner.ctx.config.agent.auto_tool_call = false;
        runner.ctx.config.memory.rules = vec![
            cyber_core::MemoryRule {
                enabled: true,
                scope: "global".into(),
                prompt: "global-guidance-marker".into(),
            },
            cyber_core::MemoryRule {
                enabled: true,
                scope: "project".into(),
                prompt: "project-guidance-marker".into(),
            },
            cyber_core::MemoryRule {
                enabled: true,
                scope: "both".into(),
                prompt: "both-guidance-marker".into(),
            },
            cyber_core::MemoryRule {
                enabled: false,
                scope: "both".into(),
                prompt: "disabled-guidance-marker".into(),
            },
            cyber_core::MemoryRule {
                enabled: true,
                scope: "invalid".into(),
                prompt: "invalid-guidance-marker".into(),
            },
        ];
        crate::cli_commands::persist(&runner.ctx.paths.memory_file, b"- global-memory-marker\n")
            .unwrap();
        crate::cli_commands::persist(
            &runner.cwd.join(".cyber/memory.md"),
            b"- project-memory-marker\n",
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider = runner.ctx.providers.providers.get_mut("openai").unwrap();
        provider.base_url = format!("http://{}", listener.local_addr().unwrap());
        provider.api_key = "test-only-key".into();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            let body = loop {
                let n = socket.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buffer[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break serde_json::from_slice::<serde_json::Value>(
                            &request[end + 4..end + 4 + length],
                        )
                        .unwrap();
                    }
                }
            };
            let response =
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).as_bytes()).await.unwrap();
            body
        });
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            runner.run_turn(
                "test memory guidance".into(),
                false,
                ThinkingIntensity::Auto,
                Arc::new(PermissionBroker::deny_all()),
                None,
            ),
        )
        .await
        .unwrap();
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let request = server.await.unwrap();
        let system = request["messages"][0]["content"].as_str().unwrap();
        for marker in [
            "global-memory-marker",
            "project-memory-marker",
            "[global] global-guidance-marker",
            "[project] project-guidance-marker",
            "[global and project] both-guidance-marker",
            "advisory prompt preferences",
        ] {
            assert!(system.contains(marker), "{marker}");
        }
        assert!(!system.contains("disabled-guidance-marker"));
        assert!(!system.contains("invalid-guidance-marker"));
        // Guidance remains advisory; the approved tool retains its existing guards.
        let ctx = cyber_agent::ToolCtx::new(runner.cwd.clone(), vec![], None, vec![]);
        let registry = ToolRegistry::with_permissions(
            runner.registry.clone(),
            Arc::new(PermissionBroker::explicit_tools(["save_memory"])),
        );
        let output = registry
            .execute(
                "save_memory",
                json!({"scope":"global","content":"approved memory write"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn cancelling_slow_mcp_handshake_aborts_and_joins_without_waiting_for_timeout() {
        use tokio::io::AsyncReadExt;
        let mut runner = test_runner().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = cyber_mcp::McpServersConfig {
            servers: vec![cyber_mcp::McpServerSpec {
                name: "slow".into(),
                transport: cyber_mcp::McpTransport::Http,
                command: None,
                args: vec![],
                env: Default::default(),
                url: Some(format!("http://{}/mcp", listener.local_addr().unwrap())),
                headers: Default::default(),
                timeout_secs: 3600,
            }],
        };
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel_tx, cancel) = tokio::sync::oneshot::channel();
        let (outcome, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(
                runner.run_cli_task(
                    crate::cli_commands::CliTask::McpConnect { config },
                    Arc::new(PermissionBroker::explicit_tools(["mcp_connect"])),
                    events,
                    cancel
                ),
                async {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut buffer = [0u8; 4096];
                    assert!(socket.read(&mut buffer).await.unwrap() > 0);
                    cancel_tx.send(()).unwrap();
                    while socket.read(&mut buffer).await.unwrap() > 0 {}
                }
            )
        })
        .await
        .expect("MCP cancellation must not await the 3600s handshake timeout");
        assert!(outcome.error.unwrap().contains("Cancelled"));
        assert!(runner.registries.mcp.is_none());
        assert!(
            matches!(runner.entries.last(),Some(ChatEntry::TurnSummary { status,.. }) if status == "cancelled")
        );
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn writeup_directory_rejects_symlink_or_junction_redirection() {
        let runner = test_runner().await;
        let challenge = cyber_core::CtfChallenge::new("safe".into(), cyber_core::CtfCategory::Web);
        let outside = tempfile::tempdir().unwrap();
        let link = runner.cwd.join(".cyber/ctf/sessions");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link.to_string_lossy().replace('/', "\\"))
                .arg(outside.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
        assert!(runner.writeup_directory(&challenge).is_err());
        #[cfg(windows)]
        std::fs::remove_dir(&link).unwrap();
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[test]
    fn terminal_filter_removes_c0_c1_and_is_safe_for_split_vt_sequences() {
        let controls: String = (0..=0x9f)
            .filter_map(char::from_u32)
            .filter(|c| c.is_control())
            .collect();
        assert_eq!(terminal_text(&controls), "\t\n");
        let pieces = [
            "safe\t\n",
            "\x1b",
            "]52;c;clipboard",
            "\x07",
            "\u{009b}",
            "2J",
            "\u{009d}",
            "title",
            "\u{009c}",
            "\r\x08\x7f",
            "text",
        ];
        let filtered = pieces.iter().map(|s| terminal_text(s)).collect::<String>();
        assert_eq!(filtered, terminal_text(&pieces.concat()));
        assert_eq!(filtered, "safe\t\n]52;c;clipboard2Jtitletext");
        assert!(filtered
            .chars()
            .all(|c| !c.is_control() || matches!(c, '\n' | '\t')));
    }

    #[test]
    fn headless_display_filters_vt_controls_without_mutating_records() {
        const CHILD: &str = "CYBER_HEADLESS_VT_FILTER_CHILD";
        if let Ok(mode) = std::env::var(CHILD) {
            let text = mode == "text";
            let mut turn = TurnHistory::new("prompt".into());
            let tokens = ["display-answer\x1b", "[31m\u{009d}title\x07\r\x08\t\n"];
            let reasoning = "display-reasoning\u{009b}2J\x1b]52;c;hidden\u{009c}";
            let name = "display-tool\x1b[2J\u{009b}";
            let args = "display-args\x1b]0;title\x07";
            let result = "display-result\u{009d}52;c;clipboard\x07";
            let error = "display-error\x1b[31m\u{009b}2J";
            for token in tokens {
                turn.record(AgentEvent::Token(token.into()), text);
            }
            turn.record(AgentEvent::Reasoning(reasoning.into()), text);
            turn.record(
                AgentEvent::ToolCall {
                    id: "id".into(),
                    name: name.into(),
                    arguments: args.into(),
                },
                text,
            );
            turn.record(
                AgentEvent::ToolProgress {
                    id: "id".into(),
                    name: name.into(),
                    chunk: "display-progress\x1b\u{009d}\x07".into(),
                },
                text,
            );
            turn.record(
                AgentEvent::ToolResult {
                    id: "id".into(),
                    name: name.into(),
                    output: result.into(),
                    is_error: true,
                },
                text,
            );
            turn.record(AgentEvent::Error(error.into()), text);
            turn.record(AgentEvent::Done, text);
            assert_eq!(turn.answer, tokens.concat());
            assert!(turn
                .entries
                .iter()
                .any(|e| matches!(e,ChatEntry::Thinking(text) if text == reasoning)));
            assert_eq!(turn.tool_calls[0].name, name);
            assert_eq!(turn.tool_calls[0].arguments, args);
            assert_eq!(turn.tool_calls[0].output, result);
            let outcome = HeadlessOutcome {
                session_id: "test".into(),
                answer: turn.answer,
                tool_calls: turn.tool_calls,
                error: turn.error,
                permission_denied: false,
            };
            let json: serde_json::Value = serde_json::from_str(&outcome_to_json(&outcome)).unwrap();
            assert_eq!(json["answer"], tokens.concat());
            assert_eq!(json["tool_calls"][0]["name"], name);
            assert_eq!(json["tool_calls"][0]["arguments"], args);
            assert_eq!(json["tool_calls"][0]["output"], result);
            assert_eq!(json["error"], error);
            assert_eq!(json["success"], false);
            return;
        }
        for mode in ["text", "json"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "headless::tests::headless_display_filters_vt_controls_without_mutating_records",
                "--nocapture",
                "--color",
                "never",
            ])
            .env(CHILD, mode)
            .output()
            .unwrap();
            assert!(output.status.success(), "{output:?}");
            for bytes in [&output.stdout, &output.stderr] {
                let text = String::from_utf8(bytes.clone()).unwrap();
                assert!(
                    text.chars()
                        .all(|c| !c.is_control() || matches!(c, '\n' | '\t')),
                    "{text:?}"
                );
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(stdout.contains("display-answer"), mode == "text");
            for marker in [
                "display-reasoning",
                "display-tool",
                "display-args",
                "display-progress",
                "display-result",
                "display-error",
            ] {
                assert_eq!(stderr.contains(marker), mode == "text", "{marker}");
            }
        }
    }

    #[tokio::test]
    async fn persistence_failure_corrects_summary_and_preserves_existing_errors_and_cancellation() {
        for status in ["done", "error", "cancelled"] {
            let mut runner = test_runner().await;
            runner.entries = vec![
                ChatEntry::User("retained".into()),
                ChatEntry::TurnSummary {
                    elapsed_ms: 1,
                    finished_at: 1,
                    status: status.into(),
                    summary: Some("任务完成".into()),
                },
            ];
            let dir = crate::history::session_dir(&runner.ctx.paths.history_dir, &runner.cwd);
            std::fs::remove_file(dir.join("index.json")).unwrap();
            std::fs::create_dir_all(dir.join("index.json")).unwrap();
            let mut error = match status {
                "error" => Some("original provider failure".into()),
                "cancelled" => Some("Cancelled".into()),
                _ => None,
            };
            runner.save_turn_outcome(&mut error);
            let message = error.as_deref().unwrap();
            assert!(message.contains("History persistence failed"));
            if status == "error" {
                assert!(message.contains("original provider failure"));
            }
            if status == "cancelled" {
                assert!(message.contains("Cancelled"));
            }
            let expected = if status == "done" { "error" } else { status };
            assert!(
                matches!(runner.entries.last(),Some(ChatEntry::TurnSummary { status,.. }) if status == expected)
            );
            assert!(
                matches!(runner.read_entries(&runner.index.current).unwrap().last(),Some(ChatEntry::TurnSummary { status,.. }) if status == expected)
            );
            let outcome = HeadlessOutcome {
                session_id: runner.index.current.clone(),
                answer: String::new(),
                tool_calls: vec![],
                error,
                permission_denied: false,
            };
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&outcome_to_json(&outcome)).unwrap()
                    ["success"],
                false
            );
            let _ = std::fs::remove_dir_all(runner.cwd);
        }
    }

    #[tokio::test]
    async fn partial_mcp_success_is_retained_and_cannot_be_connected_twice() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut runner = test_runner().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 4096];
                let request = loop {
                    let n = socket.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = std::str::from_utf8(&bytes[..end]).unwrap();
                        let length: usize = header
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap();
                        if bytes.len() >= end + 4 + length {
                            break serde_json::from_slice::<serde_json::Value>(
                                &bytes[end + 4..end + 4 + length],
                            )
                            .unwrap();
                        }
                    }
                };
                let result = match request["method"].as_str().unwrap() {
                    "initialize" => {
                        json!({"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"test","version":"1"}})
                    }
                    "tools/list" => json!({"tools":[]}),
                    "notifications/initialized" => {
                        socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                        continue;
                    }
                    _ => panic!("unexpected MCP request"),
                };
                let body = json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        });
        let config = cyber_mcp::McpServersConfig {
            servers: vec![
                cyber_mcp::McpServerSpec {
                    name: "live".into(),
                    transport: cyber_mcp::McpTransport::Http,
                    command: None,
                    args: vec![],
                    env: Default::default(),
                    url: Some(server_url),
                    headers: Default::default(),
                    timeout_secs: 3,
                },
                cyber_mcp::McpServerSpec {
                    name: "missing".into(),
                    transport: cyber_mcp::McpTransport::Stdio,
                    command: Some("cyber-test-missing-server".into()),
                    args: vec![],
                    env: Default::default(),
                    url: None,
                    headers: Default::default(),
                    timeout_secs: 1,
                },
            ],
        };
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (_tx, cancel) = tokio::sync::oneshot::channel();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            runner.run_cli_task(
                crate::cli_commands::CliTask::McpConnect { config },
                Arc::new(PermissionBroker::explicit_tools(["mcp_connect"])),
                events,
                cancel,
            ),
        )
        .await
        .unwrap();
        assert!(outcome.error.as_deref().unwrap().contains("failed for 1"));
        let connected = runner.registries.mcp.as_ref().unwrap().clone();
        assert_eq!(connected.len(), 1);
        assert_eq!(connected.server_names(), vec!["live"]);
        assert!(crate::cli_commands::execute(&mut runner, "/mcp connect").is_err());
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (_tx, cancel) = tokio::sync::oneshot::channel();
        let repeat = runner
            .run_cli_task(
                crate::cli_commands::CliTask::McpConnect {
                    config: Default::default(),
                },
                Arc::new(PermissionBroker::deny_all()),
                events,
                cancel,
            )
            .await;
        assert!(repeat
            .error
            .as_deref()
            .unwrap()
            .contains("already connected"));
        assert!(!repeat.permission_denied);
        assert!(Arc::ptr_eq(
            &connected,
            runner.registries.mcp.as_ref().unwrap()
        ));
        connected.shutdown_all().await;
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(runner.cwd);
    }
}
