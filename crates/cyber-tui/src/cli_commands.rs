//! Non-printing CLI commands. Notices never become model history.
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use color_eyre::eyre::{bail, eyre, Result};
use cyber_agent::{AgentEvent, PermissionBroker};
use cyber_core::{
    Config, CtfCategory, CtfChallenge, EnvVar, MemoryRule, ProviderConfig, ProvidersConfig,
};
use cyber_mcp::McpServersConfig;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::headless::{validate_session_id, HeadlessOutcome, SessionRunner, ViewCtx};
use crate::slash::{self, CommandSpec, SlashCommand};

pub(crate) enum CliAction {
    Output {
        title: String,
        text: String,
    },
    Refresh {
        message: Option<String>,
        reset_usage: bool,
    },
    Form(CommandForm),
    Picker(CommandPicker),
    Cancel,
    Task(CliTask),
    Quit,
    Mode(cyber_agent::PermissionMode),
    TodoVisibility(bool),
    Settings,
    SettingsTab(crate::cli::SettingsTab),
    Jobs(CliJobs),
    Update(CliUpdate),
    Panel(crate::cli::Panel),
}

pub struct CommandForm {
    pub title: String,
    pub fields: Vec<FormField>,
    pub kind: FormKind,
}
pub struct FormField {
    pub name: String,
    pub value: String,
    pub secret: bool,
}
// Deliberately not Debug: the original provider and fields can contain credentials.
pub enum FormKind {
    Provider {
        original_name: Option<String>,
        original: Box<ProviderConfig>,
    },
    MemoryRule {
        index: Option<usize>,
    },
    EnvVar {
        index: Option<usize>,
        original_key: Option<String>,
    },
    CustomTool {
        original_name: Option<String>,
    },
    /// 工具库「AI 智能扫描」表单：目标路径/提示词 + 扫描服务商与模型 + 仅预览。
    ToolboxScan,
}
pub struct CommandPicker {
    pub title: String,
    pub items: Vec<PickerItem>,
    pub kind: PickerKind,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerKind {
    General,
    Sessions,
    Models,
}
pub struct PickerItem {
    pub label: String,
    pub detail: String,
    pub command: String,
}
pub enum CliTask {
    Compact {
        instructions: Option<String>,
    },
    Writeup {
        challenge: Box<CtfChallenge>,
    },
    McpConnect {
        config: McpServersConfig,
    },
    ToolboxScan {
        preview: bool,
        target: Option<String>,
        /// 扫描所用服务商（`None` = 当前默认 Provider）。
        provider: Option<String>,
        /// 扫描所用模型（`None` = 服务商自身配置的模型）。
        model: Option<String>,
    },
}
/// `/bg` 命令族（用户级后台任务入口）。
/// `Shell` 在 busy 时同样可用（数据来自 CliScreen 快照，不触碰 runner）。
pub enum CliJobs {
    Run { prompt: String },
    Shell { command: String },
    List,
    Kill(u64),
    Tail(u64),
}
/// `/update` 子命令：`Check` 仅检查；`Prompt`（无参数）检查后询问；`Apply` 检查后直接更新。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CliUpdate {
    Check,
    Prompt,
    Apply,
}
pub struct CompletionItem {
    pub value: String,
    pub description: String,
}
const EFFORT: CommandSpec = CommandSpec {
    name: "/effort",
    usage: "/effort [low|medium|high|xhigh|auto]",
    desc: "Alias for /think",
};
const SESSION: CommandSpec = CommandSpec {
    name: "/session",
    usage: "/session [id|list|read|new]",
    desc: "打开指令面板",
};
const UPDATE: CommandSpec = CommandSpec {
    name: "/update",
    usage: "/update [check|apply]",
    desc: "检查更新并用安装脚本升级",
};
/// `/about`：打开「关于」页（全屏只读面板；与设置中心「9. 关于」页共用同一内容）。
const ABOUT: CommandSpec = CommandSpec {
    name: "/about",
    usage: "/about",
    desc: "打开关于页（版本 / 运行环境 / 快捷键说明书）",
};
/// `/mode`（无参数）帮助文案。
pub const MODE_HELP: &str = "审批模式选项：\n/mode auto       自动审批（低风险自动放行，高风险弹出确认）\n/mode manual     手动审批（每次调用工具都弹出确认）\n/mode unlimited  无限制（不弹出确认，直接执行）\n快捷键：按 F2 可快速循环切换审批模式。";
pub fn commands() -> Vec<&'static CommandSpec> {
    slash::COMMANDS
        .iter()
        .filter(|c| c.name != "/mode")
        .chain(std::iter::once(&EFFORT))
        .chain(std::iter::once(&SESSION))
        .chain(std::iter::once(&UPDATE))
        .chain(std::iter::once(&ABOUT))
        .collect()
}
fn output(title: &str, text: impl Into<String>) -> CliAction {
    CliAction::Output {
        title: title.into(),
        text: text.into(),
    }
}
fn refresh(message: &str, reset_usage: bool) -> CliAction {
    CliAction::Refresh {
        message: Some(message.into()),
        reset_usage,
    }
}
fn split(value: &str) -> (&str, &str) {
    let (a, b) = value
        .trim()
        .split_once(char::is_whitespace)
        .unwrap_or((value.trim(), ""));
    (a, b.trim())
}

/// `/mode <auto|manual|unlimited>`（不触碰 runner）。
pub fn mode_action(args: &str) -> Result<CliAction> {
    if args.is_empty() {
        return Ok(output("Mode", MODE_HELP));
    }
    let mode = cyber_agent::PermissionMode::parse(args).ok_or_else(|| {
        eyre!("未知审批模式：{args}。可用值：auto(自动)、manual(手动)、unlimited(无限制)")
    })?;
    Ok(CliAction::Mode(mode))
}

/// `/help` 输出（不触碰 runner）。
pub fn help_action() -> CliAction {
    output(
        "Commands",
        commands()
            .iter()
            .map(|c| format!("{}  {}", c.usage, c.desc))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// 把命令错误包装成 `CliAction::Output{title:"Error"}`（与空闲路径 `screen.message("Error", …)` 同文案）。
pub fn error_action(error: &color_eyre::eyre::Report) -> CliAction {
    output("Error", error.to_string())
}

/// 解析 `/bg` 参数为 `CliJobs`（不依赖 runner；busy 时也可用）。
pub(crate) fn parse_bg(args: &str) -> Result<CliJobs> {
    let (sub, rest) = split(args);
    match sub.to_ascii_lowercase().as_str() {
        "shell" => {
            if rest.is_empty() {
                bail!("用法: /bg shell <command>");
            }
            Ok(CliJobs::Shell {
                command: rest.to_string(),
            })
        }
        "run" => {
            if rest.is_empty() {
                bail!("用法: /bg run <prompt>");
            }
            Ok(CliJobs::Run {
                prompt: rest.to_string(),
            })
        }
        "list" | "" => Ok(CliJobs::List),
        "kill" => {
            let id = rest
                .parse::<u64>()
                .map_err(|_| eyre!("用法: /bg kill <id>"))?;
            Ok(CliJobs::Kill(id))
        }
        "tail" => {
            let id = rest
                .parse::<u64>()
                .map_err(|_| eyre!("用法: /bg tail <id>"))?;
            Ok(CliJobs::Tail(id))
        }
        _ => {
            bail!("用法: /bg <run <prompt>|shell <cmd>|list|kill <id>|tail <id>>");
        }
    }
}

/// Private, same-directory publication. Never make public credential backups.
pub(crate) fn persist(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("Missing persistence directory"))?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    {
        let status = std::process::Command::new("icacls")
            .arg(temp.path())
            .args(["/inheritance:r", "/grant:r", "*S-1-3-4:(F)"])
            .output()?;
        if !status.status.success() {
            bail!("Cannot restrict private file ACLs; no data written");
        }
    }
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|e| eyre!("Cannot publish private file: {}", e.error))?;
    Ok(())
}
pub(crate) fn read_optional(path: &Path) -> Result<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}
fn toml_bytes(value: &impl serde::Serialize) -> Result<Vec<u8>> {
    // Serialization errors must not quote values (API keys, endpoints, env).
    toml::to_string_pretty(value)
        .map(String::into_bytes)
        .map_err(|_| eyre!("Cannot serialize configuration"))
}
fn merge_table(old: &mut toml::Value, new: toml::Value) {
    if let (Some(old), toml::Value::Table(new)) = (old.as_table_mut(), &new) {
        for (key, value) in new {
            if let Some(previous) = old.get_mut(key) {
                merge_table(previous, value.clone());
            } else {
                old.insert(key.clone(), value.clone());
            }
        }
    } else {
        *old = new;
    }
}
/// 原始 TOML 中 `[providers.<name>]` 表的可变引用（用于显式清空可选字段）。
fn provider_table_mut<'a>(
    raw: &'a mut toml::Value,
    name: &str,
) -> Option<&'a mut toml::map::Map<String, toml::Value>> {
    raw.get_mut("providers")
        .and_then(|v| v.get_mut(name))
        .and_then(toml::Value::as_table_mut)
}
fn read_configuration(path: &Path) -> Result<toml::Value> {
    let old = read_optional(path)?;
    Ok(if old.is_empty() {
        toml::Value::Table(Default::default())
    } else {
        toml::from_str(
            std::str::from_utf8(&old).map_err(|_| eyre!("Cannot read configuration encoding"))?,
        )
        .map_err(|_| eyre!("Cannot parse configuration; existing data retained"))?
    })
}
fn configuration_bytes(path: &Path, value: &impl serde::Serialize) -> Result<Vec<u8>> {
    let mut raw = read_configuration(path)?;
    let new = toml::Value::try_from(value).map_err(|_| eyre!("Cannot serialize configuration"))?;
    merge_table(&mut raw, new);
    toml_bytes(&raw)
}
fn provider_configuration_bytes(
    runner: &SessionRunner,
    providers: &ProvidersConfig,
    rename: Option<(&str, &str)>,
) -> Result<Vec<u8>> {
    let mut raw = read_configuration(&runner.ctx.paths.providers_file)?;
    if let Some(table) = raw.get_mut("providers").and_then(toml::Value::as_table_mut) {
        if let Some((old, new)) = rename {
            if old != new {
                if let Some(p) = table.remove(old) {
                    table.insert(new.into(), p);
                }
            }
        }
        table.retain(|name, _| providers.providers.contains_key(name));
    }
    let new =
        toml::Value::try_from(providers).map_err(|_| eyre!("Cannot serialize configuration"))?;
    merge_table(&mut raw, new);
    // Only clear a known field when it was explicitly changed. Names such as
    // "price" and "notes" elsewhere may be provider/model IDs or extensions.
    for (name, provider) in &providers.providers {
        let previous_name = rename
            .filter(|(_, new)| *new == name)
            .map(|(old, _)| old)
            .unwrap_or(name);
        if let Some(previous) = runner.ctx.providers.providers.get(previous_name) {
            for (model, config) in &provider.models {
                if config.context_length.is_none()
                    && previous
                        .models
                        .get(model)
                        .is_some_and(|m| m.context_length.is_some())
                {
                    if let Some(table) = raw
                        .get_mut("providers")
                        .and_then(|v| v.get_mut(name))
                        .and_then(|v| v.get_mut("models"))
                        .and_then(|v| v.get_mut(model))
                        .and_then(toml::Value::as_table_mut)
                    {
                        table.remove("context_length");
                    }
                }
            }
            // provider 级高级端点覆盖：显式清空时必须从原始 TOML 删除，
            // 否则 merge_table 会保留陈旧值，下次启动又会被读回来。
            if provider.chat_endpoint.is_none() && previous.chat_endpoint.is_some() {
                if let Some(table) = provider_table_mut(&mut raw, name) {
                    table.remove("chat_endpoint");
                }
            }
            if provider.models_endpoint.is_none() && previous.models_endpoint.is_some() {
                if let Some(table) = provider_table_mut(&mut raw, name) {
                    table.remove("models_endpoint");
                }
            }
            // thinking 同理：显式清空（enabled→未设置，即 Some→None）时必须删除原始键，
            // 否则 merge_table 会保留陈旧值，下次启动又被读回来。
            // 注意顺序：本循环在 merge_table 之后运行，因此只能在「新值为 None」时删除。
            if provider.thinking.is_none() && previous.thinking.is_some() {
                if let Some(table) = provider_table_mut(&mut raw, name) {
                    table.remove("thinking");
                }
            }
        }
    }
    toml_bytes(&raw)
}
fn config_field_bytes(path: &Path, config: &Config, section: &str, field: &str) -> Result<Vec<u8>> {
    let value =
        toml::Value::try_from(config).map_err(|_| eyre!("Cannot serialize configuration"))?;
    let value = value
        .get(section)
        .and_then(|v| v.get(field))
        .ok_or_else(|| eyre!("Missing configuration field"))?
        .clone();
    let patch = toml::Value::Table(toml::map::Map::from_iter([(
        section.into(),
        toml::Value::Table(toml::map::Map::from_iter([(field.into(), value)])),
    )]));
    configuration_bytes(path, &patch)
}
pub(crate) fn persist_config_field(
    path: &Path,
    config: &Config,
    section: &str,
    field: &str,
) -> Result<()> {
    persist(path, &config_field_bytes(path, config, section, field)?)
}

fn save_config(
    runner: &mut SessionRunner,
    config: Config,
    section: &str,
    field: &str,
) -> Result<()> {
    persist_config_field(&runner.ctx.paths.config_file, &config, section, field)?;
    runner.ctx.config = config;
    Ok(())
}
fn save_memory_rule(
    runner: &mut SessionRunner,
    index: Option<usize>,
    rule: Option<&MemoryRule>,
) -> Result<()> {
    let project_path = runner.cwd.join(".cyber/config.toml");
    let project = read_configuration(&project_path)?;
    let path = if project.get("memory").and_then(|v| v.get("rules")).is_some() {
        &project_path
    } else {
        &runner.ctx.paths.config_file
    };
    let mut raw = read_configuration(path)?;
    let memory = raw
        .as_table_mut()
        .ok_or_else(|| eyre!("Invalid configuration table"))?
        .entry("memory")
        .or_insert_with(|| toml::Value::Table(Default::default()));
    let rules = memory
        .as_table_mut()
        .ok_or_else(|| eyre!("Invalid memory configuration"))?
        .entry("rules")
        .or_insert(
            toml::Value::try_from(&runner.ctx.config.memory.rules)
                .map_err(|_| eyre!("Cannot serialize memory rules"))?,
        );
    let items = rules
        .as_array_mut()
        .ok_or_else(|| eyre!("Memory rules must be an array"))?;
    match (index, rule) {
        (Some(index), Some(rule)) => {
            let item = items
                .get_mut(index)
                .ok_or_else(|| eyre!("Unknown memory rule"))?;
            if !item.is_table() {
                bail!("Invalid memory rule table");
            }
            merge_table(
                item,
                toml::Value::try_from(rule).map_err(|_| eyre!("Cannot serialize memory rule"))?,
            );
        }
        (Some(index), None) => {
            if index >= items.len() {
                bail!("Unknown memory rule");
            }
            items.remove(index);
        }
        (None, Some(rule)) => items
            .push(toml::Value::try_from(rule).map_err(|_| eyre!("Cannot serialize memory rule"))?),
        (None, None) => {
            bail!("Missing memory rule operation");
        }
    }
    let updated: Vec<MemoryRule> = rules
        .clone()
        .try_into()
        .map_err(|_| eyre!("Invalid memory rules; existing data retained"))?;
    persist(path, &toml_bytes(&raw)?)?;
    runner.ctx.config.memory.rules = updated;
    Ok(())
}
pub(crate) fn save_selection(
    runner: &mut SessionRunner,
    config: Config,
    providers: ProvidersConfig,
) -> Result<()> {
    save_selection_renamed(runner, config, providers, None, true)
}
fn save_selection_renamed(
    runner: &mut SessionRunner,
    config: Config,
    providers: ProvidersConfig,
    rename: Option<(&str, &str)>,
    select: bool,
) -> Result<()> {
    let cfg = if select || config.agent.default_provider != runner.ctx.config.agent.default_provider
    {
        Some(config_field_bytes(
            &runner.ctx.paths.config_file,
            &config,
            "agent",
            "default_provider",
        )?)
    } else {
        None
    };
    let prov = provider_configuration_bytes(runner, &providers, rename)?;
    let old = read_optional(&runner.ctx.paths.providers_file)?;
    let existed = runner.ctx.paths.providers_file.try_exists()?;
    persist(&runner.ctx.paths.providers_file, &prov)?;
    if let Err(error) = cfg
        .as_deref()
        .map(|cfg| persist(&runner.ctx.paths.config_file, cfg))
        .transpose()
    {
        let rollback = if existed {
            persist(&runner.ctx.paths.providers_file, &old)
        } else {
            std::fs::remove_file(&runner.ctx.paths.providers_file).map_err(Into::into)
        };
        if rollback.is_err() {
            bail!("Configuration publication failed and rollback failed; reload configuration before continuing");
        }
        return Err(error);
    }
    runner.ctx.config = config;
    runner.ctx.providers = providers;
    Ok(())
}

pub(crate) fn stop_subagents(
    background: &cyber_agent::BackgroundRegistry,
    subagents: &cyber_agent::SubagentArchive,
    target: &str,
) -> Result<String> {
    let target = target.trim();
    if target.is_empty() || target.eq_ignore_ascii_case("all") {
        let mut stopped = 0;
        let mut seen_archive_ids = std::collections::HashSet::new();

        // 1. 终止 BackgroundRegistry 中所有运行中的 Subagent 任务
        for job in background.snapshot() {
            if matches!(job.kind, cyber_agent::JobKind::Subagent)
                && matches!(job.status, cyber_agent::JobStatus::Running)
            {
                if background.kill(job.id) {
                    stopped += 1;
                }
                if let Some(run_id) = job.archive_run_id {
                    seen_archive_ids.insert(run_id);
                    subagents.finish(
                        run_id,
                        cyber_agent::SubagentStatus::Killed,
                        None,
                        Some("killed by user".into()),
                    );
                }
            }
        }

        // 2. 终止 SubagentArchive 中处于 Running 状态的独立条目
        for run in subagents.snapshot() {
            if run.status == cyber_agent::SubagentStatus::Running
                && !seen_archive_ids.contains(&run.id)
            {
                subagents.finish(
                    run.id,
                    cyber_agent::SubagentStatus::Killed,
                    None,
                    Some("killed by user".into()),
                );
                stopped += 1;
            }
        }

        if stopped > 0 {
            Ok(format!("已终止 {stopped} 个运行中的子代理"))
        } else {
            Ok("当前没有运行中的子代理".to_string())
        }
    } else {
        let clean_id = target.trim_start_matches('#');
        let target_id = clean_id
            .parse::<u64>()
            .map_err(|_| eyre!("用法: /subagents stop [id|all]"))?;

        let mut found = false;
        for job in background.snapshot() {
            if matches!(job.kind, cyber_agent::JobKind::Subagent)
                && matches!(job.status, cyber_agent::JobStatus::Running)
                && (job.id == target_id || job.archive_run_id == Some(target_id))
            {
                background.kill(job.id);
                if let Some(run_id) = job.archive_run_id {
                    subagents.finish(
                        run_id,
                        cyber_agent::SubagentStatus::Killed,
                        None,
                        Some("killed by user".into()),
                    );
                }
                found = true;
                break;
            }
        }

        if !found {
            for run in subagents.snapshot() {
                if run.id == target_id && run.status == cyber_agent::SubagentStatus::Running {
                    subagents.finish(
                        target_id,
                        cyber_agent::SubagentStatus::Killed,
                        None,
                        Some("killed by user".into()),
                    );
                    found = true;
                    break;
                }
            }
        }

        if found {
            Ok(format!("已终止子代理 #{target_id}"))
        } else {
            Ok(format!("未找到运行中的子代理 #{target_id}"))
        }
    }
}
fn subagents(runner: &mut SessionRunner, args: &str) -> Result<CliAction> {
    let (command, value) = split(args);
    if command.eq_ignore_ascii_case("stop") {
        let message = stop_subagents(
            &runner.registries.background,
            &runner.registries.subagents,
            value,
        )?;
        return Ok(output("Subagents", message));
    }

    let mut config = runner.ctx.config.clone();
    let message = match command.to_ascii_lowercase().as_str() {
        "enable" if value.is_empty() => {
            config.agent.subagents.enabled = true;
            "Subagents enabled; restart to update the tool catalog".to_string()
        }
        "disable" if value.is_empty() => {
            config.agent.subagents.enabled = false;
            "Subagents disabled; restart to update the tool catalog".to_string()
        }
        "max_tasks" => {
            let number = value
                .parse::<u32>()
                .ok()
                .filter(|number| *number > 0)
                .ok_or_else(|| eyre!("max_tasks must be a positive integer"))?;
            config.agent.subagents.max_tasks = number;
            format!("Subagent max_tasks set to {number}")
        }
        "max_parallel" => {
            let number = value
                .parse::<u32>()
                .ok()
                .filter(|number| *number > 0)
                .ok_or_else(|| eyre!("max_parallel must be a positive integer"))?;
            config.agent.subagents.max_parallel = number;
            format!("Subagent max_parallel set to {number}")
        }
        "timeout" => {
            let number = value
                .parse::<u64>()
                .ok()
                .filter(|number| *number > 0)
                .ok_or_else(|| eyre!("timeout must be a positive integer"))?;
            config.agent.subagents.timeout_secs = number;
            format!("Subagent timeout set to {number}s")
        }
        "max_steps" => {
            let number = value
                .parse::<u32>()
                .ok()
                .filter(|number| *number > 0)
                .ok_or_else(|| eyre!("max_steps must be a positive integer"))?;
            config.agent.subagents.max_steps = number;
            format!("Subagent max_steps set to {number}")
        }
        _ => {
            bail!(
                "Usage: /subagents [status|enable|disable|stop [id|all]|max_tasks N|max_parallel N|timeout N|max_steps N]"
            );
        }
    };
    save_config(runner, config, "agent", "subagents")?;
    Ok(refresh(&message, false))
}

fn env(runner: &mut SessionRunner, args: &str) -> Result<CliAction> {
    let (command, rest) = split(args);
    if command.eq_ignore_ascii_case("add") {
        return Ok(CliAction::Form(CommandForm {
            title: "Add Environment Variable".into(),
            kind: FormKind::EnvVar {
                index: None,
                original_key: None,
            },
            fields: vec![
                FormField {
                    name: "key".into(),
                    value: String::new(),
                    secret: false,
                },
                FormField {
                    name: "value".into(),
                    value: String::new(),
                    secret: false,
                },
                FormField {
                    name: "sensitive".into(),
                    value: "false".into(),
                    secret: false,
                },
            ],
        }));
    }

    if command.eq_ignore_ascii_case("edit") {
        if rest.is_empty() {
            bail!("Usage: /env edit <key_or_index>");
        }
        let config = &runner.ctx.config;
        let found = if let Ok(idx_1based) = rest.parse::<usize>() {
            if idx_1based >= 1 && idx_1based <= config.env.vars.len() {
                let idx = idx_1based - 1;
                Some((idx, &config.env.vars[idx]))
            } else {
                None
            }
        } else {
            None
        };
        let (found_idx, target) = match found {
            Some(res) => res,
            None => config
                .env
                .vars
                .iter()
                .enumerate()
                .find(|(_, v)| v.key == rest || v.key.eq_ignore_ascii_case(rest))
                .ok_or_else(|| eyre!("Unknown environment variable '{rest}'"))?,
        };
        return Ok(CliAction::Form(CommandForm {
            title: format!("Edit Environment Variable ({})", target.key),
            kind: FormKind::EnvVar {
                index: Some(found_idx),
                original_key: Some(target.key.clone()),
            },
            fields: vec![
                FormField {
                    name: "key".into(),
                    value: target.key.clone(),
                    secret: false,
                },
                FormField {
                    name: "value".into(),
                    value: target.value.clone(),
                    secret: target.sensitive,
                },
                FormField {
                    name: "sensitive".into(),
                    value: target.sensitive.to_string(),
                    secret: false,
                },
            ],
        }));
    }

    let mut config = runner.ctx.config.clone();
    let message = match command.to_ascii_lowercase().as_str() {
        "set" | "set-sensitive" => {
            let (key, value) = split(rest);
            if key.is_empty() || value.is_empty() {
                bail!("Usage: /env set KEY VALUE or /env set-sensitive KEY VALUE");
            }
            let updated = EnvVar {
                key: key.to_string(),
                value: value.to_string(),
                sensitive: command.eq_ignore_ascii_case("set-sensitive"),
            };
            if let Some(existing) = config.env.vars.iter_mut().find(|var| var.key == key) {
                *existing = updated;
            } else {
                config.env.vars.push(updated);
            }
            format!("Environment variable '{key}' saved")
        }
        "remove" if !rest.is_empty() => {
            let before = config.env.vars.len();
            config.env.vars.retain(|var| var.key != rest);
            if config.env.vars.len() == before {
                bail!("Unknown environment variable");
            }
            format!("Environment variable '{rest}' removed")
        }
        _ => {
            bail!("Usage: /env [list|add|edit <key>|set KEY VALUE|set-sensitive KEY VALUE|remove KEY]");
        }
    };
    save_config(runner, config, "env", "vars")?;
    Ok(refresh(&message, false))
}
fn web(runner: &mut SessionRunner, args: &str) -> Result<CliAction> {
    let sub = args.trim().to_ascii_lowercase();
    let (enabled, message) = match sub.as_str() {
        "on" | "enable" | "1" | "true" => (
            true,
            "Web search enabled; web_fetch tool is exposed to model",
        ),
        "off" | "disable" | "0" | "false" => (
            false,
            "Web search disabled; web_fetch tool is hidden from model",
        ),
        _ => {
            bail!("Usage: /web [status|on|off|enable|disable]");
        }
    };
    let mut config = runner.ctx.config.clone();
    config.tools.web_search = enabled;
    save_config(runner, config, "tools", "web_search")?;
    Ok(refresh(message, false))
}

/// `/toolbox`：自定义安全工具库（列表 / 录入 / 编辑 / 删除 / AI 智能扫描）。
fn toolbox(runner: &mut SessionRunner, args: &str) -> Result<CliAction> {
    let (command, rest) = split(args);
    match command.to_ascii_lowercase().as_str() {
        "add" => Ok(CliAction::Form(custom_tool_form(
            None,
            cyber_core::CustomToolConfig::default(),
        ))),
        "edit" => {
            if rest.is_empty() {
                bail!("用法: /toolbox edit <name>");
            }
            let (tools, _) = cyber_core::load_custom_tools(&runner.ctx.paths.tools_dir);
            let target = tools
                .iter()
                .find(|tool| {
                    tool.config.name == rest || tool.config.name.eq_ignore_ascii_case(rest)
                })
                .ok_or_else(|| eyre!("未找到自定义工具 '{rest}'"))?;
            let name = target.config.name.clone();
            Ok(CliAction::Form(custom_tool_form(
                Some(name),
                target.config.clone(),
            )))
        }
        "remove" => {
            if rest.is_empty() {
                bail!("用法: /toolbox remove <name>");
            }
            let (tools, _) = cyber_core::load_custom_tools(&runner.ctx.paths.tools_dir);
            let target = tools
                .iter()
                .find(|tool| {
                    tool.config.name == rest || tool.config.name.eq_ignore_ascii_case(rest)
                })
                .ok_or_else(|| eyre!("未找到自定义工具 '{rest}'"))?;
            let name = target.config.name.clone();
            let path = runner.ctx.paths.tools_dir.join(format!("{name}.toml"));
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            Ok(refresh(
                &format!("已删除自定义工具 {name}（重启后从工具表移除）"),
                false,
            ))
        }
        "scan" => {
            let mut preview = false;
            let mut provider: Option<String> = None;
            let mut model: Option<String> = None;
            let mut target_words: Vec<&str> = Vec::new();
            let mut words = rest.split_whitespace();
            while let Some(token) = words.next() {
                match token {
                    "--preview" => preview = true,
                    "--provider" => {
                        provider = words.next().map(str::to_string).filter(|v| !v.is_empty());
                    }
                    "--model" => {
                        model = words.next().map(str::to_string).filter(|v| !v.is_empty());
                    }
                    other => target_words.push(other),
                }
            }
            let target = target_words.join(" ");
            Ok(CliAction::Task(CliTask::ToolboxScan {
                preview,
                target: (!target.is_empty()).then_some(target),
                provider,
                model,
            }))
        }
        other => {
            bail!("未知子命令：{other}（用法：/toolbox [list|add|edit <name>|remove <name>|scan [目录|提示词] [--preview] [--provider <n>] [--model <m>]]）")
        }
    }
}

fn custom_tool_form(
    original_name: Option<String>,
    tool: cyber_core::CustomToolConfig,
) -> CommandForm {
    let title = match &original_name {
        Some(name) => format!("编辑自定义工具 ({name})"),
        None => "添加自定义工具 (Custom Tool)".to_string(),
    };
    CommandForm {
        title,
        fields: vec![
            FormField {
                name: "tool_name".into(),
                value: tool.name.clone(),
                secret: false,
            },
            FormField {
                name: "tool_description".into(),
                value: tool.description.clone(),
                secret: false,
            },
            FormField {
                name: "tool_command".into(),
                value: tool.command.clone(),
                secret: false,
            },
            FormField {
                name: "tool_tags".into(),
                value: crate::toolbox::format_tags_field(&tool.tags),
                secret: false,
            },
            FormField {
                name: "tool_params".into(),
                value: crate::toolbox::format_params_field(&tool.parameters),
                secret: false,
            },
        ],
        kind: FormKind::CustomTool { original_name },
    }
}

/// 「AI 智能扫描本地安全工具」表单：目标（本地路径/提示词）+ 服务商 + 模型 + 仅预览。
///
/// `provider` / `model` 为入口处的默认值（当前默认服务商与其配置模型）；两者均可改。
pub fn toolbox_scan_form(provider: String, model: String) -> CommandForm {
    CommandForm {
        title: "AI 智能扫描本地安全工具".to_string(),
        fields: vec![
            FormField {
                name: "target".into(),
                value: String::new(),
                secret: false,
            },
            FormField {
                name: "provider".into(),
                value: provider,
                secret: false,
            },
            FormField {
                name: "model".into(),
                value: model,
                secret: false,
            },
            FormField {
                name: "preview".into(),
                value: "false".into(),
                secret: false,
            },
        ],
        kind: FormKind::ToolboxScan,
    }
}

fn vision(runner: &mut SessionRunner, args: &str) -> Result<CliAction> {
    let mut parts = args.trim().splitn(2, char::is_whitespace);
    let sub = parts.next().unwrap_or("").to_lowercase();
    let rest = parts.next().unwrap_or("").trim();

    match sub.as_str() {
        "on" | "enable" | "1" | "true" => {
            let mut config = runner.ctx.config.clone();
            config.agent.vision.enabled = true;
            cyber_core::save_config(&config, &runner.ctx.paths.config_file)?;
            runner.ctx.config = config;
            Ok(refresh("Vision engine enabled", false))
        }
        "off" | "disable" | "0" | "false" => {
            let mut config = runner.ctx.config.clone();
            config.agent.vision.enabled = false;
            cyber_core::save_config(&config, &runner.ctx.paths.config_file)?;
            runner.ctx.config = config;
            Ok(refresh("Vision engine disabled", false))
        }
        "provider" => {
            if rest.is_empty() {
                bail!("Usage: /vision provider <name>");
            }
            let mut config = runner.ctx.config.clone();
            config.agent.vision.provider = rest.to_string();
            cyber_core::save_config(&config, &runner.ctx.paths.config_file)?;
            runner.ctx.config = config;
            Ok(refresh(&format!("Vision provider set to {rest}"), false))
        }
        "model" => {
            if rest.is_empty() {
                let mut items = Vec::new();
                for name in runner.ctx.providers.sorted_names() {
                    let provider = &runner.ctx.providers.providers[&name];
                    let mut models: Vec<_> = provider.models.keys().cloned().collect();
                    models.push(provider.model.clone());
                    models.sort();
                    models.dedup();
                    for model in models.into_iter().filter(|m| !m.is_empty()) {
                        let cap =
                            cyber_core::get_model_vision_capability(provider, &name, &model, None);
                        let badge = cap.badge_text();
                        let label = if badge.is_empty() {
                            model.clone()
                        } else {
                            format!("{model} {badge}")
                        };
                        items.push(PickerItem {
                            label,
                            detail: name.clone(),
                            command: format!("/vision model {name} {model}"),
                        });
                    }
                }
                Ok(CliAction::Picker(CommandPicker {
                    title: "Vision Models".into(),
                    items,
                    kind: PickerKind::Models,
                }))
            } else {
                let (p1, p2) = split(rest);
                let mut config = runner.ctx.config.clone();
                let message = if !p2.is_empty() {
                    config.agent.vision.provider = p1.to_string();
                    config.agent.vision.model = p2.to_string();
                    format!("Vision provider set to {p1}, model set to {p2}")
                } else {
                    config.agent.vision.model = rest.to_string();
                    format!("Vision model set to {rest}")
                };
                cyber_core::save_config(&config, &runner.ctx.paths.config_file)?;
                runner.ctx.config = config;
                Ok(refresh(&message, false))
            }
        }
        "test" => {
            let default_provider = runner.ctx.config.agent.default_provider.clone();
            let provider_cfg = runner
                .ctx
                .providers
                .providers
                .get(&default_provider)
                .cloned()
                .ok_or_else(|| eyre!("Default provider not configured"))?;
            let model = if !rest.is_empty() {
                rest.to_string()
            } else {
                provider_cfg.model.clone()
            };

            let cap = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    cyber_agent::probe_model_vision(&provider_cfg, &model).await
                })
            })?;

            let _ = cyber_core::save_model_vision_capability(
                &default_provider,
                &model,
                cap,
                runner.ctx.providers.providers.get_mut(&default_provider),
            );
            let _ =
                cyber_core::save_providers(&runner.ctx.providers, &runner.ctx.paths.providers_file);

            let out = format!(
                "Probe Test Result:\nProvider: {default_provider}\nModel: {model}\nPayload: 1x1 RGBA PNG Data URI\nCapability: {:?}",
                cap
            );
            Ok(output("Vision Probe", out))
        }
        _ => {
            bail!("Usage: /vision [status|on|off|provider <name>|model <name>|test [model]]");
        }
    }
}

/// `/effort` 归一化为 `/think` 后解析：空闲与回合进行中两条路径共用
/// （修复 busy 期间 `/effort` 被判为 Unknown 的缺陷）。
pub(crate) fn parse_line(line: &str) -> SlashCommand {
    let (name, args) = split(line);
    if name.eq_ignore_ascii_case("/effort") {
        let args = match args.to_ascii_lowercase().as_str() {
            "medium" => "middle",
            "xhigh" => "max",
            _ => args,
        };
        return slash::parse(&format!("/think {args}"));
    }
    slash::parse(line)
}

/// 若 `line` 是 `/update …`，返回解析后的动作；非该命令返回 `None`。
///
/// `Check` 与无参数检查只做网络请求 + 提示，回合进行中亦可用；`Apply` 与无参数确认态
/// 由 `CliScreen` 在执行安装前检查回合状态。
pub(crate) fn update_action(line: &str) -> Option<Result<CliAction>> {
    let (name, args) = split(line);
    if !name.eq_ignore_ascii_case("/update") {
        return None;
    }
    Some(match args.trim().to_ascii_lowercase().as_str() {
        "" => Ok(CliAction::Update(CliUpdate::Prompt)),
        "check" => Ok(CliAction::Update(CliUpdate::Check)),
        "apply" | "yes" | "now" => Ok(CliAction::Update(CliUpdate::Apply)),
        _ => Err(eyre!(
            "Usage: /update [check|apply]（无参数＝检查并询问是否更新）"
        )),
    })
}

/// 若 `line` 是 `/about`，返回打开关于页的动作；非该命令返回 `None`。
///
/// 与 `/update` 一样不经 `slash::parse`：只读、回合进行中亦可用，且不需要 runner。
pub(crate) fn about_action(line: &str) -> Option<Result<CliAction>> {
    let (name, args) = split(line);
    if !name.eq_ignore_ascii_case("/about") {
        return None;
    }
    if args.trim().is_empty() {
        Some(Ok(CliAction::Panel(crate::cli::Panel::About)))
    } else {
        Some(Err(eyre!("Usage: /about")))
    }
}

/// 只读指令分派：不写盘、不改会话，回合进行中（runner 已被 `take()` 走）亦可立即执行。
///
/// `Ok(None)` = 该输入需要 runner（写盘 / 改会话），由调用方排队到回合结束后执行。
/// 与 `execute` 共用本函数，保证同一指令在空闲与 busy 下输出逐字节一致。
pub fn readonly_action(view: &ViewCtx<'_>, line: &str) -> Result<Option<CliAction>> {
    let (name, args) = split(line);
    if name.eq_ignore_ascii_case("/mode") || name.eq_ignore_ascii_case("/approval") {
        return mode_action(args).map(Some);
    }
    if let Some(action) = update_action(line) {
        return action.map(Some);
    }
    if let Some(action) = about_action(line) {
        return action.map(Some);
    }
    let action = match parse_line(line) {
        SlashCommand::Help => help_action(),
        SlashCommand::Settings => CliAction::Settings,
        SlashCommand::Tools => output(
            "Tools",
            view.tools
                .all_schemas()
                .iter()
                .map(|s| format!("{}  {}", s.name, s.description))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        SlashCommand::Skill(args) => {
            if args.is_empty() || args.eq_ignore_ascii_case("list") {
                output(
                    "Skills",
                    view.skills
                        .iter()
                        .map(|s| format!("{}  {}", s.name(), s.frontmatter.description))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            } else {
                let skill = view
                    .skills
                    .find(&args)
                    .ok_or_else(|| eyre!("Unknown skill"))?;
                output("Skill", skill.body.clone())
            }
        }
        SlashCommand::Think(args) if args.trim().is_empty() => {
            output("Thinking", view.config.agent.thinking_intensity.as_str())
        }
        SlashCommand::MaxSteps(args) if args.trim().is_empty() => {
            output("Max Steps", view.config.agent.max_steps.to_string())
        }
        SlashCommand::Subagents(args) => {
            let (command, _) = split(&args);
            if command.is_empty() || command.eq_ignore_ascii_case("status") {
                let config = &view.config.agent.subagents;
                output(
                    "Subagents",
                    format!(
                        "enabled={}\nmax_tasks={}\nmax_parallel={}\ntimeout_secs={}\nmax_steps={}",
                        config.enabled,
                        config.effective_max_tasks(),
                        config.effective_max_parallel(),
                        config.effective_timeout_secs(),
                        config.effective_max_steps()
                    ),
                )
            } else {
                return Ok(None);
            }
        }
        SlashCommand::Env(args) => {
            let (command, _) = split(&args);
            if command.is_empty() || command.eq_ignore_ascii_case("list") {
                let mut vars = view.config.env.vars.clone();
                vars.sort_by(|left, right| left.key.cmp(&right.key));
                let text = if vars.is_empty() {
                    "No environment variables configured".to_string()
                } else {
                    vars.into_iter()
                        .map(|var| {
                            if var.sensitive {
                                format!("{}=<sensitive>", var.key)
                            } else {
                                format!("{}={}", var.key, var.value)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                output("Environment", text)
            } else {
                return Ok(None);
            }
        }
        SlashCommand::Web(args) => {
            let sub = args.trim().to_ascii_lowercase();
            if sub.is_empty() || sub == "status" {
                let status_str = if view.config.tools.web_search {
                    "enabled"
                } else {
                    "disabled"
                };
                output(
                    "Web Search",
                    format!("web_search is currently {status_str}"),
                )
            } else {
                return Ok(None);
            }
        }
        SlashCommand::Vision(args) => {
            let sub = args.split_whitespace().next().unwrap_or("").to_lowercase();
            if sub.is_empty() || sub == "status" {
                let status_str = if view.config.agent.vision.enabled {
                    "enabled"
                } else {
                    "disabled"
                };
                let provider = if view.config.agent.vision.provider.is_empty() {
                    "auto"
                } else {
                    &view.config.agent.vision.provider
                };
                let model = if view.config.agent.vision.model.is_empty() {
                    "auto"
                } else {
                    &view.config.agent.vision.model
                };
                let out = format!(
                    "Vision Engine: {status_str}\nProvider: {provider}\nModel: {model}\nDetail: {}\nPrompt: {}",
                    view.config.agent.vision.detail, view.config.agent.vision.prompt
                );
                output("Vision Engine", out)
            } else {
                return Ok(None);
            }
        }
        SlashCommand::Toolbox(args) => {
            let (command, _) = split(&args);
            if command.is_empty() || command.eq_ignore_ascii_case("list") {
                let (tools, _) = cyber_core::load_custom_tools(&view.paths.tools_dir);
                let text = if tools.is_empty() {
                    "（暂无自定义工具；/toolbox add 手动录入，/toolbox scan 自动扫描）".to_string()
                } else {
                    tools
                        .iter()
                        .map(|tool| {
                            format!(
                                "{} — {}  ({})",
                                tool.config.name, tool.config.description, tool.config.command
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                output("自定义工具库", text)
            } else {
                return Ok(None);
            }
        }
        SlashCommand::Mcp(args) => {
            let lower = args.trim().to_ascii_lowercase();
            match lower.as_str() {
                "" | "panel" | "add" | "edit" | "delete" => {
                    CliAction::Panel(crate::cli::Panel::Mcp)
                }
                "list" | "status" => {
                    let config = McpServersConfig::load(&view.paths.mcp_servers_file)
                        .map_err(|_| eyre!("Cannot read MCP configuration"))?;
                    output(
                        "MCP",
                        config
                            .servers
                            .iter()
                            .map(|s| {
                                let status_str = match view.mcp.and_then(|m| m.tool_count(&s.name)) {
                                    Some(0) => "connected (0 tools - hint: if SSE server, set transport = \"sse\")".to_string(),
                                    Some(n) => format!("connected ({n} tools)"),
                                    None => "not connected (explicit approval required)".to_string(),
                                };
                                format!(
                                    "{}  {:?}  {}",
                                    s.name,
                                    s.transport,
                                    status_str,
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                }
                _ => return Ok(None),
            }
        }
        SlashCommand::Model(args) if args.trim().is_empty() => {
            let mut items = Vec::new();
            let current_default = &view.config.agent.default_provider;
            let current_prov_cfg = view.providers.providers.get(current_default);
            let current_model = current_prov_cfg.map(|p| p.model.as_str()).unwrap_or("");

            for name in view.providers.sorted_names() {
                let provider = &view.providers.providers[&name];
                let is_default_prov = &name == current_default;
                let mut models: Vec<_> = provider.models.keys().cloned().collect();
                if !provider.model.is_empty() && !models.contains(&provider.model) {
                    models.push(provider.model.clone());
                }
                models.sort();
                models.dedup();
                for model in models.into_iter().filter(|m| !m.is_empty()) {
                    let is_active = is_default_prov && model == current_model;
                    let cap =
                        cyber_core::get_model_vision_capability(provider, &name, &model, None);
                    let badge = cap.badge_text();
                    let active_badge = if is_active { " ✓ 当前" } else { "" };
                    let label = if badge.is_empty() {
                        format!("{model}{active_badge}")
                    } else {
                        format!("{model} {badge}{active_badge}")
                    };
                    items.push(PickerItem {
                        label,
                        detail: if is_default_prov {
                            format!("{name} ★ 默认")
                        } else {
                            name.clone()
                        },
                        command: format!("/model {name} {model}"),
                    });
                }
            }
            if let Some(pos) = items.iter().position(|i| i.label.contains("✓ 当前")) {
                let active_item = items.remove(pos);
                items.insert(0, active_item);
            }
            CliAction::Picker(CommandPicker {
                title: "Models".into(),
                items,
                kind: PickerKind::Models,
            })
        }
        SlashCommand::Sessions(args) => {
            let (sub, _) = split(&args);
            if sub.is_empty() || sub.eq_ignore_ascii_case("list") {
                let current_title = view
                    .current_meta()
                    .map(|m| m.title.as_str())
                    .unwrap_or("默认会话");
                CliAction::Picker(CommandPicker {
                    title: format!("Sessions · {current_title}"),
                    kind: PickerKind::Sessions,
                    items: view
                        .sessions
                        .iter()
                        .map(|s| PickerItem {
                            label: s.title.clone(),
                            detail: format!("{}  {} messages", s.id, s.message_count),
                            command: format!("/sessions {}", s.id),
                        })
                        .collect(),
                })
            } else {
                return Ok(None);
            }
        }
        SlashCommand::Provider(args) => {
            let (sub, rest) = split(&args);
            match sub.to_ascii_lowercase().as_str() {
                "" | "panel" | "dashboard" => {
                    CliAction::SettingsTab(crate::cli::SettingsTab::Providers)
                }
                "list" => output(
                    "Providers",
                    view.providers
                        .sorted_names()
                        .iter()
                        .map(|n| {
                            let p = &view.providers.providers[n];
                            format!(
                                "{}{}  {}  {}",
                                n,
                                if *n == view.config.agent.default_provider {
                                    " (default)"
                                } else {
                                    ""
                                },
                                p.kind,
                                p.model
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                "models" => {
                    let name = rest.trim();
                    if name.is_empty() {
                        output(
                            "Provider Models",
                            view.providers
                                .sorted_names()
                                .iter()
                                .map(|n| {
                                    let p = &view.providers.providers[n];
                                    let mut m: Vec<_> = p.models.keys().cloned().collect();
                                    if !p.model.is_empty() && !m.contains(&p.model) {
                                        m.push(p.model.clone());
                                    }
                                    format!("{}: {}", n, m.join(", "))
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        )
                    } else if let Some(p) = view.providers.providers.get(name) {
                        let mut m: Vec<_> = p.models.keys().cloned().collect();
                        if !p.model.is_empty() && !m.contains(&p.model) {
                            m.push(p.model.clone());
                        }
                        output(&format!("Models for {name}"), m.join("\n"))
                    } else {
                        bail!("Unknown provider '{name}'");
                    }
                }
                _ => return Ok(None),
            }
        }
        SlashCommand::Ctf(args) => {
            let (sub, _) = split(&args);
            match sub.to_ascii_lowercase().as_str() {
                "status" => output(
                    "CTF",
                    if view.ctf_enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                ),
                "list" => {
                    let shared = view
                        .ctf_challenges
                        .ok_or_else(|| eyre!("CTF registry unavailable"))?;
                    let list = shared
                        .lock()
                        .map_err(|_| eyre!("CTF state lock poisoned"))?
                        .clone();
                    output(
                        "CTF",
                        list.iter()
                            .map(|c| format!("{} [{}] {}", c.name, c.category, c.status.label()))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                }
                _ => return Ok(None),
            }
        }
        SlashCommand::Memory(args) => {
            let (sub, rest) = split(&args);
            if sub.eq_ignore_ascii_case("rule") {
                let (op, _) = split(rest);
                if !op.eq_ignore_ascii_case("list") {
                    return Ok(None);
                }
                output(
                    "Memory Rules",
                    view.config
                        .memory
                        .rules
                        .iter()
                        .enumerate()
                        .map(|(i, r)| format!("{}. {} {} {}", i + 1, r.enabled, r.scope, r.prompt))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            } else if sub.is_empty() || sub.eq_ignore_ascii_case("list") {
                let project = cyber_core::Paths::project_memory_file(view.cwd);
                let mut text = String::new();
                for (name, path) in [("global", &view.paths.memory_file), ("project", &project)] {
                    let content = String::from_utf8(read_optional(path)?)?;
                    for (i, line) in content
                        .lines()
                        .filter_map(|l| l.strip_prefix("- "))
                        .enumerate()
                    {
                        text.push_str(&format!("{name} {}. {line}\n", i + 1));
                    }
                }
                output("Memory", text)
            } else {
                return Ok(None);
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(action))
}

pub fn execute(runner: &mut SessionRunner, line: &str) -> Result<CliAction> {
    // 只读指令优先：与回合进行中（busy）路径共用 `readonly_action`，输出逐字节一致。
    let readonly = readonly_action(&runner.view(), line);
    match readonly {
        Ok(Some(action)) => return Ok(action),
        Err(error) => return Err(error),
        Ok(None) => {}
    }
    let parsed = parse_line(line);
    Ok(match parsed {
        // 只读变体在上方已由 `readonly_action` 消费；此分支仅为穷尽性，
        // 若被命中说明分派表与实现已不一致。
        SlashCommand::Help | SlashCommand::Tools | SlashCommand::Skill(_) => {
            bail!("Command must be dispatched by readonly_action");
        }
        SlashCommand::Clear => {
            runner.entries.clear();
            runner.save()?;
            refresh("History cleared", true)
        }
        SlashCommand::Cancel => CliAction::Cancel,
        SlashCommand::Quit => {
            runner.save()?;
            CliAction::Quit
        }
        SlashCommand::New => {
            runner.create_session()?;
            refresh("New session", true)
        }
        SlashCommand::Model(args) => {
            let (first, second) = split(&args);
            if !second.is_empty() {
                runner.select_model_persisted(first, Some(second))?;
                refresh("Model selected", false)
            } else if runner.ctx.providers.providers.contains_key(first) {
                runner.select_model_persisted(first, None)?;
                refresh("Model selected", false)
            } else {
                let target = first.to_ascii_lowercase();
                let mut matched = Vec::new();
                for name in runner.ctx.providers.sorted_names() {
                    let p = &runner.ctx.providers.providers[&name];
                    if p.model.to_ascii_lowercase() == target
                        || p.models.keys().any(|m| m.to_ascii_lowercase() == target)
                    {
                        matched.push((name, first.to_string()));
                    }
                }

                if matched.len() == 1 {
                    let (prov, mdl) = &matched[0];
                    runner.select_model_persisted(prov, Some(mdl))?;
                    refresh("Model selected", false)
                } else if matched.is_empty() {
                    bail!("Unknown provider");
                } else {
                    let items = matched
                        .into_iter()
                        .map(|(prov, mdl)| PickerItem {
                            label: mdl.clone(),
                            detail: prov.clone(),
                            command: format!("/model {prov} {mdl}"),
                        })
                        .collect();
                    CliAction::Picker(CommandPicker {
                        title: format!("Select Provider for '{first}'"),
                        items,
                        kind: PickerKind::Models,
                    })
                }
            }
        }
        SlashCommand::Provider(args) => provider(runner, &args)?,
        SlashCommand::Subagents(args) => subagents(runner, &args)?,
        SlashCommand::Env(args) => env(runner, &args)?,
        SlashCommand::Web(args) => web(runner, &args)?,
        SlashCommand::Vision(args) => vision(runner, &args)?,
        SlashCommand::Toolbox(args) => toolbox(runner, &args)?,
        SlashCommand::Mcp(args) => {
            let config = McpServersConfig::load(&runner.ctx.paths.mcp_servers_file)
                .map_err(|_| eyre!("Cannot read MCP configuration"))?;
            let lower = args.trim().to_ascii_lowercase();
            match lower.as_str() {
                "connect" => {
                    if runner.registries.mcp.is_some() {
                        bail!("MCP already connected; restart before reconnecting");
                    }
                    CliAction::Task(CliTask::McpConnect { config })
                }
                _ => {
                    bail!("Usage: /mcp list|status|connect|panel");
                }
            }
        }
        SlashCommand::Compact(args) => CliAction::Task(CliTask::Compact {
            instructions: (!args.is_empty()).then_some(args),
        }),
        SlashCommand::Ctf(args) => ctf(runner, &args)?,
        SlashCommand::MaxSteps(args) => {
            let steps: u32 = args
                .parse()
                .map_err(|_| eyre!("Max steps must be 1-1000"))?;
            if !(1..=1000).contains(&steps) {
                bail!("Max steps must be 1-1000");
            }
            let mut config = runner.ctx.config.clone();
            config.agent.max_steps = steps;
            save_config(runner, config, "agent", "max_steps")?;
            refresh("Max steps saved", false)
        }
        SlashCommand::Think(args) => {
            let intensity =
                crate::headless::thinking(Some(&args), runner.ctx.config.agent.thinking_intensity)?;
            let mut config = runner.ctx.config.clone();
            config.agent.thinking_intensity = intensity;
            save_config(runner, config, "agent", "thinking_intensity")?;
            refresh("Thinking intensity saved", false)
        }
        SlashCommand::Sessions(args) => sessions(runner, &args)?,
        SlashCommand::Memory(args) => memory(runner, &args)?,
        SlashCommand::Todo(args) => todo_cmd(runner, &args)?,
        SlashCommand::Bg(args) => CliAction::Jobs(parse_bg(&args)?),
        SlashCommand::Image(args) => {
            let trimmed = args.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("paste") {
                let mut cb_res = arboard::Clipboard::new();
                for _ in 0..5 {
                    if cb_res.is_ok() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(15));
                    cb_res = arboard::Clipboard::new();
                }
                if let Ok(mut cb) = cb_res {
                    if let Ok(img) = cb.get_image() {
                        let width = img.width as u32;
                        let height = img.height as u32;
                        let png_bytes = crate::chat::encode_rgba_to_png(width, height, &img.bytes);
                        let cache_dir = cyber_core::Paths::detect()
                            .map(|p| p.cyber_home)
                            .unwrap_or_else(|_| std::path::PathBuf::from("."))
                            .join("cache")
                            .join("images");
                        let _ = std::fs::create_dir_all(&cache_dir);
                        let timestamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis();
                        let file_name = format!("clip_{}_{}.png", timestamp, 1);
                        let file_path = cache_dir.join(&file_name);
                        if let Ok(()) = std::fs::write(&file_path, png_bytes) {
                            let prompt =
                                format!("[image: {}] 请详细分析此图片", file_path.display());
                            runner.entries.push(crate::chat::ChatEntry::User(prompt));
                            return Ok(output(
                                "Image",
                                format!("已从系统剪贴板捕获图片并转存为 {file_name}，生成标识符 [image:1]。"),
                            ));
                        }
                    }
                }
                output(
                    "Image",
                    "剪贴板中未检测到图片。\n提示：\n1. 请在截图后直接按 Alt+V（Windows推荐）或 Ctrl+V；\n2. 或使用 /image <path> [prompt] 指定本地图片/URL路径。",
                )
            } else {
                let mut parts = trimmed.splitn(2, char::is_whitespace);
                let path_str = parts.next().unwrap_or("").trim();
                let prompt_str = parts.next().map(|s| s.trim()).filter(|s| !s.is_empty());
                let prompt = match prompt_str {
                    Some(p) => format!("[image: {path_str}] {p}"),
                    None => format!("[image: {path_str}] 请详细分析此图片"),
                };
                runner.entries.push(crate::chat::ChatEntry::User(prompt));
                output(
                    "Image",
                    format!("已加载图片附件: {path_str}，标识符 [image:1] 已附加至会话。"),
                )
            }
        }
        SlashCommand::Settings => CliAction::Settings,
        SlashCommand::Mode(_) => {
            bail!("/mode is not available in CLI");
        }
        SlashCommand::Unknown(_) => {
            bail!("Unknown command; use /help");
        }
    })
}

fn provider(runner: &mut SessionRunner, args: &str) -> Result<CliAction> {
    let (sub, rest) = split(args);
    match sub.to_ascii_lowercase().as_str() {
        "wizard" | "preset" | "presets" => {
            let mut items = Vec::new();
            for preset in cyber_core::PROVIDER_PRESETS {
                let label = if preset.default_model.is_empty() {
                    format!("{} (自定义)", preset.name)
                } else {
                    format!("{} ({})", preset.name, preset.default_model)
                };
                let detail = if preset.base_url.is_empty() {
                    preset.description.to_string()
                } else {
                    format!("{} · {}", preset.base_url, preset.description)
                };
                items.push(PickerItem {
                    label,
                    detail,
                    command: format!("/provider add-preset {}", preset.id),
                });
            }
            items.push(PickerItem {
                label: "🛠️ 自定义服务商接入 (自行选择协议类型)...".into(),
                detail: "支持 openai-compatible, anthropic, ollama, openai 等自建网关或代理".into(),
                command: "/provider add-custom".into(),
            });
            Ok(CliAction::Picker(CommandPicker {
                title: "Add Provider from Preset or Custom".into(),
                items,
                kind: PickerKind::General,
            }))
        }
        "add-custom" => {
            let items = vec![
                PickerItem {
                    label: "openai-compatible (通用兼容协议 - vLLM/OneAPI/中转/FastChat)".into(),
                    detail: "http://localhost:8000/v1 · Bearer 鉴权".into(),
                    command: "/provider add-with-kind openai-compatible".into(),
                },
                PickerItem {
                    label: "anthropic (Anthropic Claude 原生 Messages API 协议)".into(),
                    detail: "https://api.anthropic.com/v1 · x-api-key 鉴权".into(),
                    command: "/provider add-with-kind anthropic".into(),
                },
                PickerItem {
                    label: "ollama (Ollama 本地/局域网私有化运行实例)".into(),
                    detail: "http://127.0.0.1:11434 · 默认免密".into(),
                    command: "/provider add-with-kind ollama".into(),
                },
                PickerItem {
                    label: "openai (OpenAI 官方接口标准规范)".into(),
                    detail: "https://api.openai.com/v1 · Bearer 鉴权".into(),
                    command: "/provider add-with-kind openai".into(),
                },
            ];
            Ok(CliAction::Picker(CommandPicker {
                title: "Select Protocol Kind".into(),
                items,
                kind: PickerKind::General,
            }))
        }
        "add-preset" => {
            let preset_id = rest.trim();
            let preset = cyber_core::PROVIDER_PRESETS
                .iter()
                .find(|p| p.id.eq_ignore_ascii_case(preset_id))
                .ok_or_else(|| eyre!("Unknown preset '{preset_id}'"))?;
            let p = ProviderConfig {
                kind: preset.kind.to_string(),
                base_url: preset.base_url.to_string(),
                model: preset.default_model.to_string(),
                api_key: if preset.id == "ollama" {
                    String::new()
                } else {
                    format!("${{{}}}", preset.env_var_suggestion)
                },
                ..Default::default()
            };
            let values = [
                ("name", preset.id.to_string(), false),
                ("kind", p.kind.clone(), false),
                ("endpoint", p.base_url.clone(), false),
                ("apikey", p.api_key.clone(), true),
                ("model", p.model.clone(), false),
                ("maxtokens", p.max_tokens.to_string(), false),
                ("temperature", p.temperature.to_string(), false),
                (
                    "context_length",
                    p.effective_context_length()
                        .map(|n| n.to_string())
                        .unwrap_or_default(),
                    false,
                ),
                (
                    "chat_endpoint",
                    p.chat_endpoint.clone().unwrap_or_default(),
                    false,
                ),
                (
                    "models_endpoint",
                    p.models_endpoint.clone().unwrap_or_default(),
                    false,
                ),
                (
                    "thinking_type",
                    p.thinking
                        .as_ref()
                        .and_then(|t| t.r#type.clone())
                        .unwrap_or_default(),
                    false,
                ),
                (
                    "thinking_effort",
                    p.thinking
                        .as_ref()
                        .and_then(|t| t.effort.clone())
                        .unwrap_or_default(),
                    false,
                ),
            ];
            Ok(CliAction::Form(CommandForm {
                title: format!("Add Provider ({})", preset.name),
                fields: values
                    .into_iter()
                    .map(|(name, value, secret)| FormField {
                        name: name.into(),
                        value,
                        secret,
                    })
                    .collect(),
                kind: FormKind::Provider {
                    original_name: None,
                    original: Box::new(p),
                },
            }))
        }
        "add-with-kind" => {
            let kind = rest.trim().to_ascii_lowercase();
            if !cyber_core::PROVIDER_KINDS.contains(&kind.as_str()) {
                bail!("Invalid provider kind '{kind}'");
            }
            let (default_endpoint, default_model, default_key) = match kind.as_str() {
                "anthropic" => (
                    "https://api.anthropic.com/v1",
                    "claude-3-5-sonnet-20241022",
                    "${ANTHROPIC_API_KEY}",
                ),
                "ollama" => ("http://127.0.0.1:11434", "llama3.3", ""),
                "openai" => ("https://api.openai.com/v1", "gpt-4o", "${OPENAI_API_KEY}"),
                _ => ("http://localhost:8000/v1", "", ""),
            };
            let p = ProviderConfig {
                kind: kind.clone(),
                base_url: default_endpoint.to_string(),
                model: default_model.to_string(),
                api_key: default_key.to_string(),
                ..Default::default()
            };
            let values = [
                ("name", String::new(), false),
                ("kind", kind, false),
                ("endpoint", p.base_url.clone(), false),
                ("apikey", p.api_key.clone(), true),
                ("model", p.model.clone(), false),
                ("maxtokens", p.max_tokens.to_string(), false),
                ("temperature", p.temperature.to_string(), false),
                (
                    "context_length",
                    p.effective_context_length()
                        .map(|n| n.to_string())
                        .unwrap_or_default(),
                    false,
                ),
                (
                    "chat_endpoint",
                    p.chat_endpoint.clone().unwrap_or_default(),
                    false,
                ),
                (
                    "models_endpoint",
                    p.models_endpoint.clone().unwrap_or_default(),
                    false,
                ),
                (
                    "thinking_type",
                    p.thinking
                        .as_ref()
                        .and_then(|t| t.r#type.clone())
                        .unwrap_or_default(),
                    false,
                ),
                (
                    "thinking_effort",
                    p.thinking
                        .as_ref()
                        .and_then(|t| t.effort.clone())
                        .unwrap_or_default(),
                    false,
                ),
            ];
            Ok(CliAction::Form(CommandForm {
                title: "Add Custom Provider".into(),
                fields: values
                    .into_iter()
                    .map(|(name, value, secret)| FormField {
                        name: name.into(),
                        value,
                        secret,
                    })
                    .collect(),
                kind: FormKind::Provider {
                    original_name: None,
                    original: Box::new(p),
                },
            }))
        }
        "add" | "edit" => {
            let edit = sub.eq_ignore_ascii_case("edit");
            let p = if edit {
                runner
                    .ctx
                    .providers
                    .providers
                    .get(rest)
                    .ok_or_else(|| eyre!("Unknown provider"))?
                    .clone()
            } else {
                ProviderConfig::default()
            };
            let values = [
                ("name", rest.to_owned(), false),
                ("kind", p.kind.clone(), false),
                ("endpoint", p.base_url.clone(), false),
                ("apikey", p.api_key.clone(), true),
                ("model", p.model.clone(), false),
                ("maxtokens", p.max_tokens.to_string(), false),
                ("temperature", p.temperature.to_string(), false),
                (
                    "context_length",
                    p.effective_context_length()
                        .map(|n| n.to_string())
                        .unwrap_or_default(),
                    false,
                ),
                (
                    "chat_endpoint",
                    p.chat_endpoint.clone().unwrap_or_default(),
                    false,
                ),
                (
                    "models_endpoint",
                    p.models_endpoint.clone().unwrap_or_default(),
                    false,
                ),
                (
                    "thinking_type",
                    p.thinking
                        .as_ref()
                        .and_then(|t| t.r#type.clone())
                        .unwrap_or_default(),
                    false,
                ),
                (
                    "thinking_effort",
                    p.thinking
                        .as_ref()
                        .and_then(|t| t.effort.clone())
                        .unwrap_or_default(),
                    false,
                ),
            ];
            Ok(CliAction::Form(CommandForm {
                title: "Provider".into(),
                fields: values
                    .into_iter()
                    .map(|(name, value, secret)| FormField {
                        name: name.into(),
                        value,
                        secret,
                    })
                    .collect(),
                kind: FormKind::Provider {
                    original_name: edit.then(|| rest.into()),
                    original: Box::new(p),
                },
            }))
        }
        "use" => {
            runner.select_model_persisted(rest, None)?;
            Ok(refresh("Provider selected", false))
        }
        "remove" | "delete" => {
            let mut providers = runner.ctx.providers.clone();
            if providers.remove(rest).is_none() {
                bail!("Unknown provider");
            }
            let mut config = runner.ctx.config.clone();
            if config.agent.default_provider == rest {
                config.agent.default_provider = providers
                    .sorted_names()
                    .into_iter()
                    .next()
                    .unwrap_or_default();
            }
            providers.default_provider = config.agent.default_provider.clone();
            save_selection_renamed(runner, config, providers, None, false)?;
            Ok(refresh("Provider removed", false))
        }
        _ => {
            bail!("Usage: /provider list|add|edit <name>|use <name>|remove <name>");
        }
    }
}
/// `/todo` 核心实现（不触碰 runner）。返回 `(动作, 是否改动了清单)`。
pub fn todo_action(
    todos: &std::sync::Arc<std::sync::Mutex<Vec<cyber_core::TodoItem>>>,
    args: &str,
) -> Result<(CliAction, bool)> {
    let (sub, rest) = split(args);
    let mut todos = todos
        .lock()
        .map_err(|_| color_eyre::eyre::eyre!("Failed to acquire todo lock"))?;
    match sub.to_ascii_lowercase().as_str() {
        "" | "list" => {
            if todos.is_empty() {
                Ok((
                    output("Todo", "当前没有任务，可用 /todo add <title> 添加"),
                    false,
                ))
            } else {
                let mut lines = String::from("📋 任务清单：");
                for t in todos.iter() {
                    let status_mark = match t.status {
                        cyber_core::TodoStatus::Pending => "[ ]",
                        cyber_core::TodoStatus::InProgress => "[>]",
                        cyber_core::TodoStatus::Completed => "[x]",
                        cyber_core::TodoStatus::Failed => "[!]",
                    };
                    lines.push_str(&format!("\n  {} #{} {}", status_mark, t.id, t.title));
                }
                Ok((output("Todo", lines), false))
            }
        }
        "add" => {
            if rest.is_empty() {
                bail!("用法：/todo add <任务标题>");
            }
            let next_num = todos
                .iter()
                .filter_map(|t| t.id.parse::<usize>().ok())
                .max()
                .unwrap_or(0)
                + 1;
            let id_str = next_num.to_string();
            todos.push(cyber_core::TodoItem::new(
                id_str.clone(),
                rest,
                cyber_core::TodoStatus::Pending,
            ));
            drop(todos);
            Ok((
                output("Todo", format!("已添加任务 #{id_str}：{rest}")),
                true,
            ))
        }
        "done" => {
            if rest.is_empty() {
                bail!("用法：/todo done <任务编号>");
            }
            if let Some(item) = todos.iter_mut().find(|t| t.id == rest) {
                item.status = cyber_core::TodoStatus::Completed;
                drop(todos);
                Ok((output("Todo", format!("任务 #{rest} 已标记为完成")), true))
            } else {
                bail!("未找到编号为 #{rest} 的任务");
            }
        }
        "close" | "hide" => {
            drop(todos);
            Ok((CliAction::TodoVisibility(false), false))
        }
        "open" | "show" => {
            drop(todos);
            Ok((CliAction::TodoVisibility(true), false))
        }
        "clear" => {
            todos.clear();
            drop(todos);
            Ok((output("Todo", "任务清单已清空"), true))
        }
        other => {
            bail!(
                "未知子命令：{other}（用法：/todo [list|add <title>|done <id>|clear|close|open]）"
            );
        }
    }
}

fn todo_cmd(runner: &mut SessionRunner, args: &str) -> Result<CliAction> {
    let (action, mutated) = todo_action(&runner.registries.todos, args)?;
    if mutated {
        runner.save()?;
    }
    Ok(action)
}

fn sessions(runner: &mut SessionRunner, args: &str) -> Result<CliAction> {
    let (sub, rest) = split(args);
    match sub.to_ascii_lowercase().as_str() {
        "new" => {
            runner.create_session()?;
            Ok(refresh("New session", true))
        }
        "delete" => {
            runner.delete_session(rest)?;
            Ok(refresh("Session deleted", true))
        }
        "read" => {
            let matches: Vec<_> = runner
                .index
                .sessions
                .iter()
                .filter(|s| s.id == rest || s.title.to_lowercase().contains(&rest.to_lowercase()))
                .collect();
            if rest.is_empty() || matches.len() != 1 {
                return Ok(output(
                    "Sessions",
                    matches
                        .iter()
                        .map(|s| {
                            format!(
                                "{}  {}  {} messages{}",
                                s.id,
                                s.title,
                                if s.id == runner.index.current {
                                    runner.entries.len()
                                } else {
                                    s.message_count
                                },
                                if s.id == runner.index.current {
                                    " (current)"
                                } else {
                                    ""
                                }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ));
            }
            let entries = runner.read_entries(&matches[0].id)?;
            Ok(output(
                "Session",
                entries
                    .iter()
                    .filter_map(|e| match e {
                        crate::chat::ChatEntry::User(t) => Some(format!("User: {t}")),
                        crate::chat::ChatEntry::Assistant(t) => Some(format!("Assistant: {t}")),
                        crate::chat::ChatEntry::System(t) => Some(format!("Notice: {t}")),
                        crate::chat::ChatEntry::Thinking(t) => Some(format!("Thinking: {t}")),
                        crate::chat::ChatEntry::ToolCall {
                            name, arguments, ..
                        } => Some(format!("Tool {name}: {arguments}")),
                        crate::chat::ChatEntry::ToolResult { name, output, .. } => {
                            Some(format!("Result {name}: {output}"))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ))
        }
        _ if !sub.is_empty() && !sub.eq_ignore_ascii_case("list") => {
            runner.select_session(args.trim())?;
            Ok(refresh("Session selected", true))
        }
        _ => {
            // `""` / `list` 由 `cli_commands::readonly_action` 分派为会话面板。
            bail!("Usage: /sessions [list|read <id|关键词>|new|delete <id>|<id>]");
        }
    }
}

fn memory(runner: &mut SessionRunner, args: &str) -> Result<CliAction> {
    let (sub, rest) = split(args);
    if sub.eq_ignore_ascii_case("rule") {
        let (op, index) = split(rest);
        let index = if index.is_empty() {
            None
        } else {
            Some(
                index
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .ok_or_else(|| eyre!("Rule index starts at 1"))?,
            )
        };
        let rule = index
            .map(|i| {
                runner
                    .ctx
                    .config
                    .memory
                    .rules
                    .get(i)
                    .ok_or_else(|| eyre!("Unknown rule"))
            })
            .transpose()?;
        return match op.to_ascii_lowercase().as_str() {
            "delete" => {
                let i = index.ok_or_else(|| eyre!("Usage: /memory rule delete <index>"))?;
                save_memory_rule(runner, Some(i), None)?;
                Ok(refresh("Memory rule deleted", false))
            }
            "" | "add" | "edit" => {
                if op.eq_ignore_ascii_case("edit") && index.is_none() {
                    bail!("Usage: /memory rule edit <index>");
                }
                Ok(CliAction::Form(CommandForm {
                    title: "Memory Rule".into(),
                    kind: FormKind::MemoryRule { index },
                    fields: vec![
                        FormField {
                            name: "enabled".into(),
                            value: rule.map(|r| r.enabled).unwrap_or(true).to_string(),
                            secret: false,
                        },
                        FormField {
                            name: "scope".into(),
                            value: rule
                                .map(|r| r.scope.clone())
                                .unwrap_or_else(|| "both".into()),
                            secret: false,
                        },
                        FormField {
                            name: "prompt".into(),
                            value: rule.map(|r| r.prompt.clone()).unwrap_or_default(),
                            secret: false,
                        },
                    ],
                }))
            }
            _ => {
                bail!("Usage: /memory rule [add|list|edit <index>|delete <index>]");
            }
        };
    }
    let global = runner.ctx.paths.memory_file.clone();
    let project = runner.cwd.join(".cyber/memory.md");
    let sub = sub.to_ascii_lowercase();
    let (path, index, content) = if sub == "add" || sub == "project" {
        (if sub == "add" { global } else { project }, None, rest)
    } else {
        let (scope, rest) = split(rest);
        let path = match scope.to_ascii_lowercase().as_str() {
            "global" => global,
            "project" => project,
            _ => {
                bail!("Scope must be global or project");
            }
        };
        let (index, content) = split(rest);
        (
            path,
            Some(
                index
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .ok_or_else(|| eyre!("Memory index starts at 1"))?,
            ),
            content,
        )
    };
    let source = String::from_utf8(read_optional(&path)?)?;
    let mut lines: Vec<_> = source.lines().map(str::to_owned).collect();
    let content = content
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    match sub.as_str() {
        "add" | "project" => {
            if content.is_empty() {
                bail!("Memory content is required");
            }
            lines.push(format!("- {content}"));
        }
        "edit" | "delete" | "remove" => {
            let position = lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.starts_with("- "))
                .nth(index.unwrap())
                .map(|(i, _)| i)
                .ok_or_else(|| eyre!("Unknown memory index"))?;
            if sub == "edit" {
                if content.is_empty() {
                    bail!("Memory content is required");
                }
                lines[position] = format!("- {content}");
            } else {
                if !content.is_empty() {
                    bail!("Usage: /memory delete <scope> <index>");
                }
                lines.remove(position);
            }
        }
        _ => {
            bail!("Unknown memory command");
        }
    }
    persist(&path, format!("{}\n", lines.join("\n")).as_bytes())?;
    Ok(refresh("Memory saved", false))
}

pub(crate) fn validate_challenge_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || ['/', '\\', ':', '*', '?', '"', '<', '>', '|'].contains(&c))
    {
        bail!("Invalid challenge path component");
    }
    let stem = name.split('.').next().unwrap_or("");
    if validate_session_id(stem).is_err() && stem.is_ascii() && !stem.contains([' ', '-']) {
        bail!("Invalid challenge path component");
    }
    Ok(())
}
fn ctf(runner: &mut SessionRunner, args: &str) -> Result<CliAction> {
    let (sub, rest) = split(args);
    match sub.to_ascii_lowercase().as_str() {
        "enable" | "disable" => {
            runner.ctf_enabled = sub.eq_ignore_ascii_case("enable");
            Ok(refresh("CTF mode updated", false))
        }
        "" | "panel" | "open" => {
            runner.ctf_enabled = true;
            Ok(CliAction::Panel(crate::cli::Panel::Ctf))
        }
        "add" => {
            let (name, category) = split(rest);
            validate_challenge_name(name)?;
            let category = CtfCategory::from_str(category)
                .ok_or_else(|| eyre!("Category must be misc/web/reverse/pwn/crypto"))?;
            let mut list = runner.challenges()?;
            if list.iter().any(|c| c.name == name) {
                bail!("Challenge already exists");
            }
            list.push(CtfChallenge::new(name.into(), category));
            runner.replace_challenges(list)?;
            runner.save()?;
            Ok(refresh("Challenge added", false))
        }
        "writeup" => {
            validate_challenge_name(rest)?;
            let challenge = runner
                .challenges()?
                .into_iter()
                .find(|c| c.name == rest)
                .ok_or_else(|| eyre!("Unknown challenge"))?;
            if !challenge.is_solved() {
                bail!("Challenge must be solved before generating a writeup");
            }
            Ok(CliAction::Task(CliTask::Writeup {
                challenge: Box::new(challenge),
            }))
        }
        _ => {
            bail!("Usage: /ctf [panel|open]|enable|disable|status|add <name> <category>|list|writeup <name>");
        }
    }
}

pub fn submit_form(runner: &mut SessionRunner, form: &CommandForm) -> Result<CliAction> {
    let field = |name: &str| -> Result<&str> {
        form.fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.value.trim())
            .ok_or_else(|| eyre!("Missing form field: {name}"))
    };
    match &form.kind {
        FormKind::Provider {
            original_name,
            original,
        } => {
            let name = field("name")?;
            if name.is_empty() || name.split_whitespace().count() != 1 || name.contains(['/', '\\'])
            {
                bail!("Invalid provider name");
            }
            if original_name.as_deref() != Some(name)
                && runner.ctx.providers.providers.contains_key(name)
            {
                bail!("Provider name already exists");
            }
            let mut p = (**original).clone();
            p.kind = field("kind")?.to_ascii_lowercase();
            if !cyber_core::PROVIDER_KINDS.contains(&p.kind.as_str()) {
                bail!("Invalid provider kind");
            }
            p.base_url = field("endpoint")?.into();
            p.api_key = field("apikey")?.into();
            p.model = field("model")?.into();
            if p.base_url.is_empty() || p.model.is_empty() || p.model.contains(char::is_whitespace)
            {
                bail!("Endpoint and model are required (model must not contain spaces)");
            }
            p.max_tokens = field("maxtokens")?
                .parse()
                .map_err(|_| eyre!("Invalid max tokens"))?;
            if p.max_tokens == 0 {
                bail!("Max tokens must be positive");
            }
            p.temperature = field("temperature")?
                .parse()
                .map_err(|_| eyre!("Invalid temperature"))?;
            if !p.temperature.is_finite() || !(0.0..=2.0).contains(&p.temperature) {
                bail!("Temperature must be 0-2");
            }
            let context = field("context_length")?;
            let context = if context.is_empty() {
                None
            } else {
                Some(
                    context
                        .parse::<u32>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| eyre!("Invalid context length"))?,
                )
            };
            p.models.entry(p.model.clone()).or_default().context_length = context;
            let opt_field = |name: &str| -> &str {
                form.fields
                    .iter()
                    .find(|f| f.name == name)
                    .map(|f| f.value.trim())
                    .unwrap_or("")
            };
            let chat_ep = opt_field("chat_endpoint");
            p.chat_endpoint = if chat_ep.is_empty() {
                None
            } else {
                Some(chat_ep.to_string())
            };
            let models_ep = opt_field("models_endpoint");
            p.models_endpoint = if models_ep.is_empty() {
                None
            } else {
                Some(models_ep.to_string())
            };
            let thinking_type = field("thinking_type")?.trim().to_ascii_lowercase();
            if !thinking_type.is_empty()
                && !matches!(thinking_type.as_str(), "enabled" | "disabled")
            {
                bail!("Invalid thinking type (expected enabled/disabled)");
            }
            let thinking_effort = field("thinking_effort")?.trim().to_ascii_lowercase();
            if !thinking_effort.is_empty()
                && !matches!(thinking_effort.as_str(), "low" | "medium" | "high")
            {
                bail!("Invalid thinking effort (expected low/medium/high)");
            }
            p.thinking = if thinking_type.is_empty() && thinking_effort.is_empty() {
                None
            } else {
                Some(cyber_core::ThinkingConfig {
                    r#type: (!thinking_type.is_empty()).then_some(thinking_type),
                    effort: (!thinking_effort.is_empty()).then_some(thinking_effort),
                })
            };
            let mut providers = runner.ctx.providers.clone();
            let mut config = runner.ctx.config.clone();
            if let Some(old) = original_name {
                providers.remove(old);
                if config.agent.default_provider == *old {
                    config.agent.default_provider = name.into();
                }
            }
            if config.agent.default_provider.is_empty() {
                config.agent.default_provider = name.into();
            }
            providers.upsert(name, p);
            // 首次向导/新机器：默认服务商没有可用凭据时，直接采用刚配置好的这一个，
            // 否则用户填完 API Key 仍会因默认项不可用而无法离开向导。
            if config.agent.default_provider != name
                && !cyber_core::setup::configured(&config, &providers)
            {
                let mut candidate = config.clone();
                candidate.agent.default_provider = name.into();
                if cyber_core::setup::configured(&candidate, &providers) {
                    config.agent.default_provider = name.into();
                }
            }
            providers.default_provider = config.agent.default_provider.clone();
            save_selection_renamed(
                runner,
                config,
                providers,
                original_name.as_deref().map(|old| (old, name)),
                false,
            )?;
            Ok(refresh("Provider saved", false))
        }
        FormKind::MemoryRule { index } => {
            let enabled = match field("enabled")?.to_ascii_lowercase().as_str() {
                "true" => true,
                "false" => false,
                _ => {
                    bail!("Enabled must be true or false");
                }
            };
            let scope = field("scope")?.to_ascii_lowercase();
            if !["global", "project", "both"].contains(&scope.as_str()) {
                bail!("Scope must be global, project or both");
            }
            let prompt = field("prompt")?;
            if prompt.is_empty() {
                bail!("Rule prompt is required");
            }
            let rule = MemoryRule {
                enabled,
                scope,
                prompt: prompt.into(),
            };
            save_memory_rule(runner, *index, Some(&rule))?;
            Ok(refresh("Memory rule saved", false))
        }
        FormKind::EnvVar {
            index,
            original_key,
        } => {
            let key = field("key")?;
            if key.is_empty() {
                bail!("Environment variable key cannot be empty");
            }
            if key.contains('=') || key.contains(char::is_whitespace) {
                bail!("Environment variable key cannot contain '=' or whitespace");
            }
            let value = form
                .fields
                .iter()
                .find(|f| f.name == "value")
                .map(|f| f.value.as_str())
                .unwrap_or("");
            let sensitive_str = field("sensitive")?.to_ascii_lowercase();
            let sensitive = matches!(sensitive_str.as_str(), "true" | "1" | "yes");

            let mut config = runner.ctx.config.clone();

            for (i, v) in config.env.vars.iter().enumerate() {
                let is_self = match (index, original_key) {
                    (Some(idx), _) if *idx == i => true,
                    (_, Some(orig)) if v.key == *orig => true,
                    _ => false,
                };
                if !is_self && v.key == key {
                    bail!("Environment variable '{key}' already exists");
                }
            }

            let new_var = EnvVar {
                key: key.to_string(),
                value: value.to_string(),
                sensitive,
            };

            if let Some(idx) = index {
                if *idx < config.env.vars.len() {
                    config.env.vars[*idx] = new_var;
                } else {
                    config.env.vars.push(new_var);
                }
            } else if let Some(orig) = original_key {
                if let Some(existing) = config.env.vars.iter_mut().find(|v| v.key == *orig) {
                    *existing = new_var;
                } else {
                    config.env.vars.push(new_var);
                }
            } else {
                config.env.vars.push(new_var);
            }

            save_config(runner, config, "env", "vars")?;
            Ok(refresh("Environment variable saved", false))
        }
        FormKind::CustomTool { original_name } => {
            let name = field("tool_name")?.to_string();
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                bail!("自定义工具名只能包含字母、数字、下划线、连字符");
            }
            let command = field("tool_command")?.to_string();
            if command.is_empty() {
                bail!("自定义工具命令不能为空");
            }
            let path = runner.ctx.paths.tools_dir.join(format!("{name}.toml"));
            if original_name.as_deref() != Some(name.as_str()) && path.exists() {
                bail!("自定义工具 {name} 已存在");
            }
            let config = cyber_core::CustomToolConfig {
                name: name.clone(),
                description: field("tool_description")?.to_string(),
                command,
                tags: crate::toolbox::parse_tags_field(field("tool_tags")?),
                parameters: crate::toolbox::parse_params_field(field("tool_params")?)?,
            };
            cyber_core::custom_tool::save_custom_tool(&runner.ctx.paths.tools_dir, &config)
                .map_err(|error| eyre!("{error}"))?;
            Ok(CliAction::Refresh {
                message: Some(format!("已保存自定义工具 {name}（重启后生效）")),
                reset_usage: false,
            })
        }
        FormKind::ToolboxScan => {
            let provider = field("provider")?.to_string();
            if !provider.is_empty() && !runner.ctx.providers.providers.contains_key(&provider) {
                bail!("未知服务商：{provider}（请在服务商管理中先配置）");
            }
            let model = field("model")?.to_string();
            let preview = match field("preview")?.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => true,
                "false" | "0" | "no" | "" => false,
                _ => bail!("仅预览 (preview) 只能是 true / false"),
            };
            let target = field("target")?.to_string();
            Ok(CliAction::Task(CliTask::ToolboxScan {
                preview,
                target: (!target.is_empty()).then_some(target),
                provider: (!provider.is_empty()).then_some(provider),
                model: (!model.is_empty()).then_some(model),
            }))
        }
    }
}

pub fn panel_guide(cmd: &str) -> Option<&'static str> {
    match cmd.to_ascii_lowercase().as_str() {
        "/mcp" => Some("打开 MCP 面板 (服务器/工具控制中心)"),
        "/provider" | "/providers" => Some("打开服务商面板 (查看与配置全部 Provider)"),
        "/model" | "/models" => Some("打开模型面板 (双栏浏览与切换核心模型)"),
        "/settings" => Some("打开设置中心面板 (Agent/界面/并发/存储)"),
        "/ctf" => Some("打开 CTF 题目面板 (靶场题单与解题工具)"),
        "/subagents" => Some("打开子代理转录面板 (实时查看执行流)"),
        "/bg" => Some("打开后台任务面板 (查看与管理后台进程)"),
        "/sessions" => Some("打开会话管理面板 (浏览与切换历史会话)"),
        _ => None,
    }
}

pub fn is_panel_command(cmd: &str) -> bool {
    panel_guide(cmd).is_some()
}

pub fn subcommand_description(cmd: &str, sub: &str) -> Option<&'static str> {
    let cmd = cmd.to_ascii_lowercase();
    let sub = sub.to_ascii_lowercase();
    match (cmd.as_str(), sub.as_str()) {
        ("/mcp", "connect") => Some("测试并建立 MCP 服务器连接"),
        ("/mcp", "list") => Some("查看已配置 MCP 服务器列表"),
        ("/mcp", "status") => Some("查看 MCP 服务器运行状态"),
        ("/mcp", "add") => Some("添加新 MCP 服务器配置"),
        ("/mcp", "edit") => Some("编辑 MCP 服务器配置"),
        ("/mcp", "delete") => Some("删除指定 MCP 服务器"),
        ("/mcp", "panel") => Some("打开全屏 MCP 管理面板"),

        ("/provider" | "/providers", "add") => Some("添加模型服务商 (向导或自定义协议)"),
        ("/provider" | "/providers", "edit") => Some("编辑服务商参数 (端点、密钥、模型)"),
        ("/provider" | "/providers", "delete" | "remove") => Some("删除指定模型服务商"),
        ("/provider" | "/providers", "list") => Some("列出所有已配置服务商及默认项"),
        ("/provider" | "/providers", "use") => Some("切换全局默认模型服务商"),
        ("/provider" | "/providers", "models") => Some("查看或拉取服务商模型清单"),
        ("/provider" | "/providers", "wizard" | "preset" | "presets") => {
            Some("从主流预设向导添加服务商")
        }

        ("/sessions" | "/session", "list") => Some("浏览与切换历史会话"),
        ("/sessions" | "/session", "read") => Some("跨会话读取历史上下文"),
        ("/sessions" | "/session", "new") => Some("新建空白会话"),
        ("/sessions" | "/session", "delete") => Some("删除指定历史会话"),

        ("/ctf", "enable") => Some("开启 CTF 靶场辅助解题模式"),
        ("/ctf", "disable") => Some("关闭 CTF 靶场模式"),
        ("/ctf", "add") => Some("添加新 CTF 靶场题目"),
        ("/ctf", "list") => Some("查看 CTF 靶场题单与解题状态"),
        ("/ctf", "writeup") => Some("生成当前题目 Writeup 报告"),

        ("/subagents", "status") => Some("查看批量子代理运行状态"),
        ("/subagents", "enable") => Some("开启批量子代理并发执行"),
        ("/subagents", "disable") => Some("禁用子代理功能"),
        ("/subagents", "stop") => Some("终止指定的子代理任务"),

        ("/bg", "list") => Some("查看全部后台任务与进程"),
        ("/bg", "run") => Some("在后台异步运行 Prompt 任务"),
        ("/bg", "shell") => Some("在后台启动持久化命令进程"),
        ("/bg", "kill") => Some("终止指定后台任务"),
        ("/bg", "tail") => Some("追踪后台任务实时输出"),
        _ => None,
    }
}

pub fn suggestions(runner: Option<&SessionRunner>, input: &str) -> Vec<CompletionItem> {
    let input = input.trim_start();
    if !input.starts_with('/') {
        return Vec::new();
    }
    if !input.contains(char::is_whitespace) {
        let current_session_title = runner
            .and_then(|r| r.index.current_meta())
            .map(|m| m.title.clone());
        let input_lower = input.to_ascii_lowercase();
        let mut items: Vec<CompletionItem> = commands()
            .into_iter()
            .filter(|c| c.name.starts_with(&input_lower))
            .map(|c| {
                let description = if let Some(guide) = panel_guide(c.name) {
                    match (
                        (c.name == "/session" || c.name == "/sessions"),
                        &current_session_title,
                    ) {
                        (true, Some(title)) => format!("{guide} · {title}"),
                        _ => guide.to_string(),
                    }
                } else if c.name == "/session" || c.name == "/sessions" {
                    if let Some(title) = &current_session_title {
                        format!("{} · {}", c.desc, title)
                    } else {
                        c.desc.into()
                    }
                } else {
                    c.desc.into()
                };
                CompletionItem {
                    value: format!("{} ", c.name),
                    description,
                }
            })
            .collect();

        // 对完全匹配的面板主命令（如 /mcp、/provider、/model），在排序后置顶至 items[0]
        if let Some(pos) = items.iter().position(|item| {
            let name = item.value.trim().to_ascii_lowercase();
            name == input_lower && is_panel_command(&name)
        }) {
            let exact = items.remove(pos);
            items.insert(0, exact);
        }
        return items;
    }
    let (cmd, args) = split(input);
    let cmd = cmd.to_ascii_lowercase();
    let (head, prefix) = if input.ends_with(char::is_whitespace) {
        (args, "")
    } else {
        args.rsplit_once(char::is_whitespace).unwrap_or(("", args))
    };
    let mut values: Vec<String> = if head.is_empty() {
        (if cmd == "/effort" {
            vec!["low", "medium", "high", "xhigh", "auto"]
        } else {
            slash::param_suggestions(&cmd)
        })
        .into_iter()
        .map(str::to_owned)
        .collect()
    } else {
        Vec::new()
    };
    if cmd == "/effort" && head.is_empty() && prefix.len() >= 2 {
        values.extend(
            ["middle", "max"]
                .into_iter()
                .filter(|value| value.starts_with(&prefix.to_ascii_lowercase()))
                .map(str::to_owned),
        );
    }
    if cmd == "/mcp" && head.is_empty() {
        values.extend(["connect", "add", "edit", "delete"].map(str::to_owned));
    }
    if (cmd == "/provider" || cmd == "/providers") && head.is_empty() {
        values.extend(["delete", "models"].map(str::to_owned));
    }
    if (cmd == "/sessions" || cmd == "/session") && head.is_empty() {
        values.push("delete".into());
    }
    if cmd == "/memory" && head.eq_ignore_ascii_case("rule") {
        values.extend(["add", "list", "edit", "delete"].map(str::to_owned));
    }
    if cmd == "/memory"
        && ["edit", "delete", "remove"]
            .iter()
            .any(|s| head.eq_ignore_ascii_case(s))
    {
        values.extend(["global", "project"].map(str::to_owned));
    }
    if let Some(runner) = runner {
        if (cmd == "/model" && head.is_empty())
            || ((cmd == "/provider" || cmd == "/providers")
                && ["use", "edit", "remove", "delete"]
                    .iter()
                    .any(|s| head.eq_ignore_ascii_case(s)))
        {
            values.extend(runner.ctx.providers.sorted_names());
        }
        if cmd == "/model" && !head.is_empty() {
            if let Some(p) = runner.ctx.providers.providers.get(head) {
                values.extend(p.models.keys().cloned());
                values.push(p.model.clone());
            }
        }
        if (cmd == "/sessions" || cmd == "/session")
            && (head.is_empty()
                || ["read", "delete"]
                    .iter()
                    .any(|s| head.eq_ignore_ascii_case(s)))
        {
            values.extend(runner.index.sessions.iter().map(|s| s.id.clone()));
        }
        if cmd == "/env"
            && ["set", "set-sensitive", "remove", "edit"]
                .iter()
                .any(|value| head.eq_ignore_ascii_case(value))
        {
            values.extend(runner.ctx.config.env.vars.iter().map(|var| var.key.clone()));
        }
        if cmd == "/skill" && head.is_empty() {
            values.extend(runner.registries.skills.iter().map(|s| s.name().to_owned()));
        }
    }
    values.sort();
    values.dedup();

    let prefix_lower = prefix.to_ascii_lowercase();
    let mut items: Vec<CompletionItem> = values
        .into_iter()
        .filter(|s| s.to_ascii_lowercase().starts_with(&prefix_lower))
        .map(|s| {
            let description = if (cmd == "/sessions" || cmd == "/session") && !head.is_empty() {
                runner
                    .and_then(|r| r.index.get(&s))
                    .map(|m| m.title.clone())
                    .unwrap_or_else(|| cmd.clone())
            } else if let Some(desc) = subcommand_description(&cmd, &s) {
                desc.to_string()
            } else if cmd == "/sessions" || cmd == "/session" {
                runner
                    .and_then(|r| r.index.get(&s))
                    .map(|m| m.title.clone())
                    .unwrap_or_else(|| {
                        subcommand_description(&cmd, &s)
                            .map(str::to_string)
                            .unwrap_or_else(|| cmd.clone())
                    })
            } else if cmd == "/model" && head.is_empty() {
                format!("切换至 {s} 服务商核心模型")
            } else if cmd == "/model" && !head.is_empty() {
                format!("选择 {head} 服务商下的 {s} 模型")
            } else {
                cmd.clone()
            };
            CompletionItem {
                value: if head.is_empty() {
                    format!("{cmd} {s} ")
                } else {
                    format!("{cmd} {head} {s} ")
                },
                description,
            }
        })
        .collect();

    if head.is_empty() && is_panel_command(&cmd) {
        let prefix_matches = prefix.is_empty()
            || cmd.trim_start_matches('/').starts_with(&prefix_lower)
            || cmd.starts_with(&prefix_lower)
            || "panel".starts_with(&prefix_lower);
        if prefix_matches {
            if let Some(guide_desc) = panel_guide(&cmd) {
                items.insert(
                    0,
                    CompletionItem {
                        value: cmd.clone(),
                        description: guide_desc.to_string(),
                    },
                );
            }
        }
    }

    items
}
pub async fn run_task(
    runner: &mut SessionRunner,
    task: CliTask,
    permissions: Arc<PermissionBroker>,
    events: UnboundedSender<AgentEvent>,
    cancel: oneshot::Receiver<()>,
) -> HeadlessOutcome {
    runner.run_cli_task(task, permissions, events, cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{entries_to_messages, ChatEntry};
    use crate::headless::tests::test_runner;

    fn form(action: CliAction) -> CommandForm {
        match action {
            CliAction::Form(form) => form,
            _ => panic!("expected form"),
        }
    }
    fn set(form: &mut CommandForm, name: &str, value: &str) {
        form.fields
            .iter_mut()
            .find(|f| f.name == name)
            .unwrap()
            .value = value.into();
    }
    fn task(action: CliAction) -> CliTask {
        match action {
            CliAction::Task(task) => task,
            _ => panic!("expected task"),
        }
    }

    #[tokio::test]
    async fn toolbox_scan_form_submits_task_with_target_provider_and_model() {
        let mut runner = test_runner().await;
        let mut form = toolbox_scan_form("openai".into(), "gpt-4o-mini".into());
        set(&mut form, "target", "D:/tools");
        set(&mut form, "model", "gpt-4o");
        set(&mut form, "preview", "true");
        match submit_form(&mut runner, &form).unwrap() {
            CliAction::Task(CliTask::ToolboxScan {
                preview,
                target,
                provider,
                model,
            }) => {
                assert!(preview);
                assert_eq!(target.as_deref(), Some("D:/tools"));
                assert_eq!(provider.as_deref(), Some("openai"));
                assert_eq!(model.as_deref(), Some("gpt-4o"));
            }
            _ => panic!("expected toolbox scan task"),
        }

        // 留空目标/模型 → None（各取默认值），preview 默认 false
        let mut blank = toolbox_scan_form("openai".into(), String::new());
        match submit_form(&mut runner, &blank).unwrap() {
            CliAction::Task(CliTask::ToolboxScan {
                preview,
                target,
                provider,
                model,
            }) => {
                assert!(!preview);
                assert!(target.is_none());
                assert_eq!(provider.as_deref(), Some("openai"));
                assert!(model.is_none());
            }
            _ => panic!("expected toolbox scan task"),
        }

        // 未知服务商 / 非布尔 preview 必须被拒绝
        set(&mut blank, "provider", "nope");
        assert!(submit_form(&mut runner, &blank).is_err());
        set(&mut blank, "provider", "openai");
        set(&mut blank, "preview", "maybe");
        assert!(submit_form(&mut runner, &blank).is_err());
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn adding_a_provider_adopts_it_as_default_only_when_the_current_default_is_unusable() {
        let mut runner = test_runner().await;
        runner
            .ctx
            .providers
            .providers
            .get_mut("openai")
            .unwrap()
            .api_key
            .clear();
        assert!(!cyber_core::setup::configured(
            &runner.ctx.config,
            &runner.ctx.providers
        ));

        let mut add = form(execute(&mut runner, "/provider add local").unwrap());
        set(&mut add, "endpoint", "http://localhost:11434");
        set(&mut add, "model", "test");
        set(&mut add, "kind", "ollama");
        submit_form(&mut runner, &add).unwrap();
        assert_eq!(runner.ctx.config.agent.default_provider, "local");
        assert!(cyber_core::setup::configured(
            &runner.ctx.config,
            &runner.ctx.providers
        ));

        // 默认项已可用时，新增其他服务商不得抢走默认。
        let mut extra = form(execute(&mut runner, "/provider add extra").unwrap());
        set(&mut extra, "endpoint", "http://localhost:11435");
        set(&mut extra, "model", "test2");
        set(&mut extra, "kind", "ollama");
        submit_form(&mut runner, &extra).unwrap();
        assert_eq!(runner.ctx.config.agent.default_provider, "local");
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn toolbox_forms_save_edit_list_and_remove_custom_tools() {
        let mut runner = test_runner().await;
        let tools_dir = runner.ctx.paths.tools_dir.clone();

        // add → 表单落盘（params / tags 编解码）
        let mut add_form = form(execute(&mut runner, "/toolbox add").unwrap());
        set(&mut add_form, "tool_name", "myprobe");
        set(&mut add_form, "tool_description", "自定义探测");
        set(
            &mut add_form,
            "tool_command",
            "myprobe -u {target} -p {port}",
        );
        set(&mut add_form, "tool_tags", "custom, web ,");
        set(
            &mut add_form,
            "tool_params",
            "target|r|目标|127.0.0.1;port|o||80",
        );
        assert!(matches!(
            submit_form(&mut runner, &add_form).unwrap(),
            CliAction::Refresh {
                reset_usage: false,
                ..
            }
        ));
        let path = tools_dir.join("myprobe.toml");
        assert!(path.exists());
        let (tools, errors) = cyber_core::load_custom_tools(&tools_dir);
        assert!(errors.is_empty());
        let saved = tools
            .iter()
            .find(|tool| tool.config.name == "myprobe")
            .unwrap();
        assert_eq!(saved.config.parameters.len(), 2);
        assert_eq!(saved.config.parameters[0].name, "target");
        assert!(saved.config.parameters[0].required);
        assert_eq!(
            saved.config.parameters[0].default.as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(saved.config.parameters[1].name, "port");
        assert!(!saved.config.parameters[1].required);
        assert_eq!(saved.config.parameters[1].default.as_deref(), Some("80"));
        assert_eq!(saved.config.tags, vec!["custom", "web"]);

        // edit → 字段初值回填为等价格式
        let edit = form(execute(&mut runner, "/toolbox edit myprobe").unwrap());
        assert_eq!(
            edit.fields
                .iter()
                .find(|field| field.name == "tool_params")
                .unwrap()
                .value,
            "target|r|目标|127.0.0.1;port|o||80"
        );
        assert_eq!(
            edit.fields
                .iter()
                .find(|field| field.name == "tool_tags")
                .unwrap()
                .value,
            "custom,web"
        );

        // list → 列出名称与命令
        let listing = match execute(&mut runner, "/toolbox list").unwrap() {
            CliAction::Output { text, .. } => text,
            _ => panic!("expected output"),
        };
        assert!(listing.contains("myprobe"));
        assert!(listing.contains("myprobe -u {target} -p {port}"));

        // 重名 / 非法名 / 空命令必须被拒绝
        let mut duplicate = form(execute(&mut runner, "/toolbox add").unwrap());
        set(&mut duplicate, "tool_name", "myprobe");
        set(&mut duplicate, "tool_command", "x");
        assert!(submit_form(&mut runner, &duplicate).is_err());
        let mut traversal = form(execute(&mut runner, "/toolbox add").unwrap());
        set(&mut traversal, "tool_name", "../evil");
        set(&mut traversal, "tool_command", "x");
        assert!(submit_form(&mut runner, &traversal).is_err());
        let mut empty_command = form(execute(&mut runner, "/toolbox add").unwrap());
        set(&mut empty_command, "tool_name", "other");
        assert!(submit_form(&mut runner, &empty_command).is_err());

        // scan 解析为任务（--preview + 目标 + 可选 provider/model）
        match execute(&mut runner, "/toolbox scan --preview D:/tools").unwrap() {
            CliAction::Task(CliTask::ToolboxScan {
                preview,
                target,
                provider,
                model,
            }) => {
                assert!(preview);
                assert_eq!(target.as_deref(), Some("D:/tools"));
                assert!(provider.is_none());
                assert!(model.is_none());
            }
            _ => panic!("expected toolbox scan task"),
        }
        match execute(&mut runner, "/toolbox scan").unwrap() {
            CliAction::Task(CliTask::ToolboxScan {
                preview,
                target,
                provider,
                model,
            }) => {
                assert!(!preview);
                assert!(target.is_none());
                assert!(provider.is_none());
                assert!(model.is_none());
            }
            _ => panic!("expected toolbox scan task"),
        }
        match execute(
            &mut runner,
            "/toolbox scan --provider demo --model demo-model D:/tools",
        )
        .unwrap()
        {
            CliAction::Task(CliTask::ToolboxScan {
                preview,
                target,
                provider,
                model,
            }) => {
                assert!(!preview);
                assert_eq!(target.as_deref(), Some("D:/tools"));
                assert_eq!(provider.as_deref(), Some("demo"));
                assert_eq!(model.as_deref(), Some("demo-model"));
            }
            _ => panic!("expected toolbox scan task"),
        }
        assert!(execute(&mut runner, "/toolbox bogus").is_err());

        // remove → 文件删除
        assert!(matches!(
            execute(&mut runner, "/toolbox remove myprobe").unwrap(),
            CliAction::Refresh { .. }
        ));
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn toolbox_list_reports_empty_library() {
        let mut runner = test_runner().await;
        let text = match execute(&mut runner, "/toolbox").unwrap() {
            CliAction::Output { text, .. } => text,
            _ => panic!("expected output"),
        };
        assert!(text.contains("暂无自定义工具"));
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn catalog_dispatches_all_commands_and_effort_case_insensitively() {
        let mut runner = test_runner().await;
        assert_eq!(commands().len(), 30);
        assert!(commands().iter().any(|c| c.name == "/settings"));
        assert!(!commands().iter().any(|c| c.name == "/mode"));
        for command in commands() {
            if let Err(error) = execute(&mut runner, &command.name.to_uppercase()) {
                panic!("{}: {error}", command.name);
            }
        }
        assert!(execute(&mut runner, "/mode chat").is_err());
        execute(&mut runner, "/EfFoRt HIGH").unwrap();
        assert_eq!(runner.ctx.config.agent.thinking_intensity.as_str(), "high");
        assert!(execute(&mut runner, "/max_steps 0").is_err());
        execute(&mut runner, "/max_steps 500").unwrap();
        assert_eq!(runner.ctx.config.agent.max_steps, 500);
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn effort_aliases_catalog_and_completion_preserve_the_command_interface() {
        use cyber_core::ThinkingIntensity::{Auto, High, Low, Max, Middle};
        let mut runner = test_runner().await;
        for (value, expected) in [
            ("low", Low),
            ("MEDIUM", Middle),
            ("high", High),
            ("XHIGH", Max),
            ("auto", Auto),
            ("middle", Middle),
            ("max", Max),
        ] {
            let action = execute(&mut runner, &format!("/EfFoRt {value}")).unwrap();
            assert!(matches!(
                action,
                CliAction::Refresh {
                    reset_usage: false,
                    ..
                }
            ));
            assert_eq!(runner.ctx.config.agent.thinking_intensity, expected);
            let saved: Config =
                toml::from_str(&std::fs::read_to_string(&runner.ctx.paths.config_file).unwrap())
                    .unwrap();
            assert_eq!(saved.agent.thinking_intensity, expected);
        }
        for value in ["low", "middle", "mid", "high", "max", "auto"] {
            execute(&mut runner, &format!("/think {value}")).unwrap();
        }
        assert!(execute(&mut runner, "/effort invalid").is_err());
        assert_eq!(
            commands()
                .iter()
                .find(|c| c.name == "/effort")
                .unwrap()
                .usage,
            "/effort [low|medium|high|xhigh|auto]"
        );
        let values: Vec<_> = suggestions(None, "/effort ")
            .into_iter()
            .map(|s| s.value)
            .collect();
        assert_eq!(
            values,
            vec![
                "/effort auto ",
                "/effort high ",
                "/effort low ",
                "/effort medium ",
                "/effort xhigh "
            ]
        );
        assert_eq!(suggestions(None, "/EFFORT X")[0].value, "/effort xhigh ");
        assert_eq!(suggestions(None, "/effort ma")[0].value, "/effort max ");
        assert_eq!(suggestions(None, "/effort mi")[0].value, "/effort middle ");
        assert_eq!(
            suggestions(None, "/think m")
                .into_iter()
                .map(|s| s.value)
                .collect::<Vec<_>>(),
            vec!["/think max ", "/think middle "]
        );
        assert!(matches!(
            execute(&mut runner, "/effort").unwrap(),
            CliAction::Output { .. }
        ));
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn session_command_and_completion_displays_session_title() {
        let mut runner = test_runner().await;
        let s_id = runner.index.current.clone();
        if let Some(m) = runner.index.get_mut(&s_id) {
            m.title = "测试渗透会话".into();
        }

        // 1. Completion when typing "/session" shows title behind description
        let comps = suggestions(Some(&runner), "/session");
        let session_comp = comps.iter().find(|c| c.value == "/session ").unwrap();
        assert!(session_comp.description.contains("测试渗透会话"));
        assert!(session_comp.description.starts_with("打开指令面板"));

        // 2. Completion when typing "/session " shows session title in description
        let arg_comps = suggestions(Some(&runner), "/session ");
        let item_comp = arg_comps.iter().find(|c| c.value.contains(&s_id)).unwrap();
        assert_eq!(item_comp.description, "测试渗透会话");

        // 3. Executing "/session" opens Picker with title containing session title
        let action = execute(&mut runner, "/session").unwrap();
        let CliAction::Picker(picker) = action else {
            panic!("expected picker");
        };
        assert!(picker.title.contains("测试渗透会话"));
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn saves_only_target_global_fields_without_copying_project_overrides() {
        let mut runner = test_runner().await;
        let global: toml::Value = toml::from_str(
            r#"
[agent]
default_provider = "openai"
max_steps = 500
thinking_intensity = "low"
auto_tool_call = false
notes = "global extension"
[ui]
theme = "global-theme"
[tools]
shell_timeout_secs = 15
[env]
vars = [{key = "GLOBAL", value = "retained", sensitive = false}]
[memory]
rules = [{enabled = true, scope = "global", prompt = "global rule"}]
price = "global extension"
"#,
        )
        .unwrap();
        persist(&runner.ctx.paths.config_file, &toml_bytes(&global).unwrap()).unwrap();
        let project_path = runner.cwd.join(".cyber/config.toml");
        let project = b"[agent]\ndefault_provider = 'ollama'\nmax_steps = 25\nauto_tool_call = true\n[ui]\ntheme = 'project-theme'\n[env]\nvars = [{key = 'PROJECT', value = 'project-only', sensitive = true}]\n";
        persist(&project_path, project).unwrap();
        let mut effective = global.clone();
        merge_table(
            &mut effective,
            toml::from_str(std::str::from_utf8(project).unwrap()).unwrap(),
        );
        runner.ctx.config = effective.try_into().unwrap();
        execute(&mut runner, "/effort medium").unwrap();
        let mut expected = global.clone();
        expected["agent"]["thinking_intensity"] = toml::Value::String("middle".into());
        assert_eq!(
            read_configuration(&runner.ctx.paths.config_file).unwrap(),
            expected
        );
        assert_eq!(runner.ctx.config.agent.max_steps, 25);
        assert_eq!(runner.ctx.config.ui.theme, "project-theme");
        assert_eq!(runner.ctx.config.env.vars[0].key, "PROJECT");

        let mut edit = form(execute(&mut runner, "/provider edit openai").unwrap());
        set(&mut edit, "apikey", "private-marker");
        submit_form(&mut runner, &edit).unwrap();
        assert_eq!(
            read_configuration(&runner.ctx.paths.config_file).unwrap(),
            expected
        );
        execute(&mut runner, "/provider remove anthropic").unwrap();
        assert_eq!(
            read_configuration(&runner.ctx.paths.config_file).unwrap(),
            expected
        );
        // An explicit selection is saved even if it already matches the project override.
        execute(&mut runner, "/model ollama test").unwrap();
        expected["agent"]["default_provider"] = toml::Value::String("ollama".into());
        assert_eq!(
            read_configuration(&runner.ctx.paths.config_file).unwrap(),
            expected
        );
        assert_eq!(runner.ctx.config.agent.max_steps, 25);
        execute(&mut runner, "/max_steps 500").unwrap();
        assert_eq!(runner.ctx.config.agent.max_steps, 500);
        assert_eq!(
            read_configuration(&runner.ctx.paths.config_file).unwrap(),
            expected
        );
        execute(&mut runner, "/think high").unwrap();
        expected["agent"]["thinking_intensity"] = toml::Value::String("high".into());
        assert_eq!(
            read_configuration(&runner.ctx.paths.config_file).unwrap(),
            expected
        );
        assert_eq!(runner.ctx.config.agent.max_steps, 500);

        let mut rule = form(execute(&mut runner, "/memory rule edit 1").unwrap());
        set(&mut rule, "prompt", "updated global rule");
        submit_form(&mut runner, &rule).unwrap();
        expected["memory"]["rules"].as_array_mut().unwrap()[0]["prompt"] =
            toml::Value::String("updated global rule".into());
        assert_eq!(
            read_configuration(&runner.ctx.paths.config_file).unwrap(),
            expected
        );
        execute(&mut runner, "/memory rule delete 1").unwrap();
        expected["memory"]["rules"] = toml::Value::Array(vec![]);
        assert_eq!(
            read_configuration(&runner.ctx.paths.config_file).unwrap(),
            expected
        );
        assert_eq!(std::fs::read(project_path).unwrap(), project);
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn provider_model_extensions_survive_save_switch_rename_and_explicit_context_clear() {
        let mut runner = test_runner().await;
        let raw: toml::Value = toml::from_str(
            r#"
default_provider = "openai"
price = "root extension"
notes = "root notes"
[providers.openai]
kind = "openai"
base_url = "https://example.test"
api_key = "private-marker"
model = "main"
notes = "provider extension"
alias = "provider alias extension"
context_length = 9876
[providers.openai.price]
input_per_m = 1.0
notes = "price extension"
price = "nested price extension"
[providers.openai.models.main]
context_length = 12345
alias = "main alias"
notes = "main notes"
chat_endpoint = "model extension"
models_endpoint = "other model extension"
[providers.openai.models.main.price]
output_per_m = 2.0
alias = "model price extension"
temperature = 0.9
[providers.openai.models.price]
notes = "model named price"
[providers.openai.models.notes]
notes = "model named notes"
"#,
        )
        .unwrap();
        persist(&runner.ctx.paths.providers_file, &toml_bytes(&raw).unwrap()).unwrap();
        runner.ctx.providers = raw.clone().try_into().unwrap();
        let mut edit = form(execute(&mut runner, "/provider edit openai").unwrap());
        set(&mut edit, "maxtokens", "8192");
        submit_form(&mut runner, &edit).unwrap();
        execute(&mut runner, "/model openai notes").unwrap();
        execute(&mut runner, "/model openai main").unwrap();
        let mut edit = form(execute(&mut runner, "/provider edit openai").unwrap());
        set(&mut edit, "name", "renamed");
        set(&mut edit, "context_length", "");
        submit_form(&mut runner, &edit).unwrap();
        let saved = read_configuration(&runner.ctx.paths.providers_file).unwrap();
        assert_eq!(saved["price"], raw["price"]);
        assert_eq!(saved["notes"], raw["notes"]);
        assert!(saved["providers"].get("openai").is_none());
        let mut expected = raw["providers"]["openai"].clone();
        expected
            .as_table_mut()
            .unwrap()
            .insert("max_tokens".into(), toml::Value::Integer(8192));
        expected.as_table_mut().unwrap().insert(
            "temperature".into(),
            toml::Value::try_from(&runner.ctx.providers.providers["renamed"]).unwrap()
                ["temperature"]
                .clone(),
        );
        expected["models"]["main"]
            .as_table_mut()
            .unwrap()
            .remove("context_length");
        assert_eq!(saved["providers"]["renamed"], expected);
        assert!(runner.ctx.providers.providers["renamed"].models["main"]
            .context_length
            .is_none());
        assert_eq!(runner.ctx.config.agent.max_steps, 500);
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn notices_and_session_reads_do_not_inject_model_history() {
        let mut runner = test_runner().await;
        runner.entries.push(ChatEntry::User("old question".into()));
        runner.save().unwrap();
        let before = serde_json::to_value(&runner.entries).unwrap();
        for command in [
            "/help",
            "/tools",
            "/provider list",
            "/skill list",
            "/mcp status",
            "/sessions read old",
            "/memory list",
        ] {
            assert!(matches!(
                execute(&mut runner, command).unwrap(),
                CliAction::Output { .. }
            ));
            assert_eq!(serde_json::to_value(&runner.entries).unwrap(), before);
        }
        if let CliAction::Output { text, .. } = execute(&mut runner, "/tools").unwrap() {
            assert!(text.contains("ctf_challenge"));
        }
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn provider_forms_preserve_models_unknown_fields_and_private_credentials() {
        let mut runner = test_runner().await;
        let mut edit = form(execute(&mut runner, "/provider edit openai").unwrap());
        assert!(
            edit.fields
                .iter()
                .find(|f| f.name == "apikey")
                .unwrap()
                .secret
        );
        for name in [
            "name",
            "kind",
            "endpoint",
            "apikey",
            "model",
            "maxtokens",
            "temperature",
            "context_length",
        ] {
            assert!(edit.fields.iter().any(|f| f.name == name));
        }
        let mut raw = toml::Value::try_from(&runner.ctx.providers).unwrap();
        raw["providers"]["openai"]["models"]
            .as_table_mut()
            .unwrap()
            .insert(
                "gpt-4o".into(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "custom_extension".into(),
                    toml::Value::String("keep".into()),
                )])),
            );
        persist(&runner.ctx.paths.providers_file, &toml_bytes(&raw).unwrap()).unwrap();
        set(&mut edit, "apikey", "private-key-marker");
        set(&mut edit, "context_length", "12345");
        submit_form(&mut runner, &edit).unwrap();
        let saved = std::fs::read_to_string(&runner.ctx.paths.providers_file).unwrap();
        assert!(saved.contains("custom_extension"));
        assert!(saved.contains("private-key-marker"));
        assert!(!runner
            .ctx
            .paths
            .providers_file
            .with_extension("toml.bak")
            .exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&runner.ctx.paths.providers_file)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        runner.entries.push(ChatEntry::User("keep state".into()));
        execute(&mut runner, "/model openai gpt-4o").unwrap();
        assert!(matches!(&runner.entries[0],ChatEntry::User(t) if t == "keep state"));
        let config: Config =
            toml::from_str(&std::fs::read_to_string(&runner.ctx.paths.config_file).unwrap())
                .unwrap();
        let providers: ProvidersConfig =
            toml::from_str(&std::fs::read_to_string(&runner.ctx.paths.providers_file).unwrap())
                .unwrap();
        assert_eq!(config.agent.default_provider, providers.default_provider);
        execute(&mut runner, "/provider remove openai").unwrap();
        assert_ne!(runner.ctx.config.agent.default_provider, "openai");
        assert_eq!(
            runner.ctx.providers.default_provider,
            runner.ctx.config.agent.default_provider
        );
        let mut add = form(execute(&mut runner, "/provider add local").unwrap());
        set(&mut add, "endpoint", "http://localhost:11434");
        set(&mut add, "model", "test");
        set(&mut add, "kind", "ollama");
        submit_form(&mut runner, &add).unwrap();
        execute(&mut runner, "/provider use local").unwrap();
        assert_eq!(runner.ctx.config.agent.default_provider, "local");
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn provider_form_exposes_and_persists_advanced_endpoint_overrides() {
        let mut runner = test_runner().await;

        // 1. edit 表单必须包含两个高级端点字段
        let mut edit = form(execute(&mut runner, "/provider edit openai").unwrap());
        for name in ["chat_endpoint", "models_endpoint"] {
            let f = edit
                .fields
                .iter()
                .find(|f| f.name == name)
                .unwrap_or_else(|| panic!("missing field {name}"));
            assert!(!f.secret, "{name} must render as plain text");
        }

        // 2. 填写自定义端点后提交 → 落盘并被运行时读取
        set(&mut edit, "chat_endpoint", "https://gw.test/v1/chat");
        set(&mut edit, "models_endpoint", "https://gw.test/v1/list");
        submit_form(&mut runner, &edit).unwrap();
        let cfg = &runner.ctx.providers.providers["openai"];
        assert_eq!(
            cfg.chat_endpoint.as_deref(),
            Some("https://gw.test/v1/chat")
        );
        assert_eq!(
            cfg.models_endpoint.as_deref(),
            Some("https://gw.test/v1/list")
        );
        assert_eq!(cfg.chat_endpoint(), "https://gw.test/v1/chat");
        let saved = std::fs::read_to_string(&runner.ctx.paths.providers_file).unwrap();
        assert!(saved.contains("chat_endpoint"));
        assert!(saved.contains("https://gw.test/v1/list"));

        // 3. 留空 → 回退到 Some(None)，且默认端点恢复正常
        let mut cleared = form(execute(&mut runner, "/provider edit openai").unwrap());
        assert_eq!(
            cleared
                .fields
                .iter()
                .find(|f| f.name == "chat_endpoint")
                .unwrap()
                .value,
            "https://gw.test/v1/chat"
        );
        set(&mut cleared, "chat_endpoint", "");
        set(&mut cleared, "models_endpoint", "   ");
        submit_form(&mut runner, &cleared).unwrap();
        let cfg = &runner.ctx.providers.providers["openai"];
        assert!(cfg.chat_endpoint.is_none(), "blank must clear override");
        assert!(cfg.models_endpoint.is_none(), "blank must clear override");
        assert_eq!(
            cfg.chat_endpoint(),
            "https://api.openai.com/v1/chat/completions"
        );
        // 磁盘上必须真正删除陈旧覆盖值，否则下次启动会重新读回
        let saved = std::fs::read_to_string(&runner.ctx.paths.providers_file).unwrap();
        assert!(
            !saved.contains("https://gw.test/v1/chat"),
            "stale chat_endpoint must be removed from file: {saved}"
        );
        assert!(
            !saved.contains("https://gw.test/v1/list"),
            "stale models_endpoint must be removed from file: {saved}"
        );
        let reparsed: ProvidersConfig = toml::from_str(&saved).unwrap();
        let reloaded = &reparsed.providers["openai"];
        assert!(reloaded.chat_endpoint.is_none());
        assert!(reloaded.models_endpoint.is_none());

        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn provider_form_exposes_and_persists_thinking_config() {
        let mut runner = test_runner().await;

        // 1. edit 表单必须包含两个 thinking 字段（非密钥字段）
        let mut edit = form(execute(&mut runner, "/provider edit openai").unwrap());
        for name in ["thinking_type", "thinking_effort"] {
            let f = edit
                .fields
                .iter()
                .find(|f| f.name == name)
                .unwrap_or_else(|| panic!("missing field {name}"));
            assert!(!f.secret, "{name} must render as plain text");
        }
        assert_eq!(
            runner.ctx.providers.providers["openai"].thinking, None,
            "默认未设置时不得下发思考参数"
        );

        // 2. 设置后提交 → 运行时与磁盘都必须生效
        set(&mut edit, "thinking_type", "enabled");
        set(&mut edit, "thinking_effort", "low");
        submit_form(&mut runner, &edit).unwrap();
        assert_eq!(
            runner.ctx.providers.providers["openai"].thinking,
            Some(cyber_core::ThinkingConfig {
                r#type: Some("enabled".into()),
                effort: Some("low".into()),
            })
        );
        let saved = std::fs::read_to_string(&runner.ctx.paths.providers_file).unwrap();
        assert!(saved.contains("[providers.openai.thinking]"), "{saved}");
        assert!(saved.contains("type = \"enabled\""), "{saved}");
        assert!(saved.contains("effort = \"low\""), "{saved}");

        // 3. 两项都清空 → None，且磁盘上的陈旧键必须被删除
        let mut cleared = form(execute(&mut runner, "/provider edit openai").unwrap());
        assert_eq!(
            cleared
                .fields
                .iter()
                .find(|f| f.name == "thinking_type")
                .unwrap()
                .value,
            "enabled"
        );
        set(&mut cleared, "thinking_type", "");
        set(&mut cleared, "thinking_effort", "  ");
        submit_form(&mut runner, &cleared).unwrap();
        assert!(runner.ctx.providers.providers["openai"].thinking.is_none());
        let saved = std::fs::read_to_string(&runner.ctx.paths.providers_file).unwrap();
        assert!(
            !saved.contains("thinking"),
            "陈旧 thinking 必须删除: {saved}"
        );
        let reparsed: ProvidersConfig = toml::from_str(&saved).unwrap();
        assert!(reparsed.providers["openai"].thinking.is_none());

        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn invalid_thinking_values_are_rejected() {
        let mut runner = test_runner().await;
        let mut edit = form(execute(&mut runner, "/provider edit openai").unwrap());
        set(&mut edit, "thinking_type", "yes");
        assert!(submit_form(&mut runner, &edit).is_err());

        let mut edit = form(execute(&mut runner, "/provider edit openai").unwrap());
        set(&mut edit, "thinking_effort", "extreme");
        assert!(submit_form(&mut runner, &edit).is_err());

        // 非法值不得污染运行时配置
        assert!(runner.ctx.providers.providers["openai"].thinking.is_none());
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn failed_two_file_publication_rolls_back_without_changing_running_selection() {
        let mut runner = test_runner().await;
        persist(
            &runner.ctx.paths.providers_file,
            &toml_bytes(&runner.ctx.providers).unwrap(),
        )
        .unwrap();
        let before = std::fs::read(&runner.ctx.paths.providers_file).unwrap();
        runner.ctx.paths.config_file = runner.cwd.join("blocked");
        std::fs::create_dir_all(&runner.ctx.paths.config_file).unwrap();
        assert!(execute(&mut runner, "/model ollama test").is_err());
        assert_eq!(
            std::fs::read(&runner.ctx.paths.providers_file).unwrap(),
            before
        );
        assert_eq!(runner.ctx.config.agent.default_provider, "openai");
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn memory_and_rules_have_strict_scopes_and_complete_crud() {
        let mut runner = test_runner().await;
        execute(&mut runner, "/memory add first").unwrap();
        execute(&mut runner, "/memory project second").unwrap();
        assert!(execute(&mut runner, "/memory edit typo 1 unsafe").is_err());
        execute(&mut runner, "/memory edit GLOBAL 1 updated").unwrap();
        execute(&mut runner, "/memory delete project 1").unwrap();
        assert!(execute(&mut runner, "/memory delete global 0").is_err());
        let mut rule = form(execute(&mut runner, "/memory rule").unwrap());
        set(&mut rule, "prompt", "remember preferences");
        set(&mut rule, "scope", "typo");
        assert!(submit_form(&mut runner, &rule).is_err());
        set(&mut rule, "scope", "project");
        submit_form(&mut runner, &rule).unwrap();
        let count = runner.ctx.config.memory.rules.len();
        let mut rule = form(execute(&mut runner, &format!("/memory rule edit {count}")).unwrap());
        set(&mut rule, "enabled", "false");
        set(&mut rule, "prompt", "updated rule");
        submit_form(&mut runner, &rule).unwrap();
        assert!(!runner.ctx.config.memory.rules.last().unwrap().enabled);
        assert!(matches!(
            execute(&mut runner, "/memory rule list").unwrap(),
            CliAction::Output { .. }
        ));
        execute(&mut runner, &format!("/memory rule delete {count}")).unwrap();
        assert_eq!(runner.ctx.config.memory.rules.len(), count - 1);
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn pickers_completion_and_session_delete_are_safe() {
        let mut runner = test_runner().await;
        if let CliAction::Picker(p) = execute(&mut runner, "/model").unwrap() {
            assert!(matches!(p.kind, PickerKind::Models));
            assert!(!p.items.is_empty());
            assert!(p.items.iter().all(|i| i.command.starts_with("/model ")));
        } else {
            panic!("expected model picker");
        }
        for (input, expected) in [
            ("/THINK h", "/think high "),
            ("/provider use op", "/provider use openai "),
            ("/mcp c", "/mcp connect "),
            ("/memory rule e", "/memory rule edit "),
        ] {
            assert!(
                suggestions(Some(&runner), input)
                    .iter()
                    .any(|c| c.value == expected),
                "{input}"
            );
        }
        assert!(suggestions(None, "/mo").iter().all(|c| c.value != "/mode "));
        let id = runner.index.current.clone();
        assert!(execute(&mut runner, &format!("/sessions delete {id}")).is_err());
        execute(&mut runner, "/new").unwrap();
        if let CliAction::Picker(p) = execute(&mut runner, "/sessions list").unwrap() {
            assert!(matches!(p.kind, PickerKind::Sessions));
            assert!(p
                .items
                .iter()
                .any(|i| i.command == format!("/sessions {id}")));
        }
        assert!(execute(&mut runner, "/sessions ../escape").is_err());
        execute(&mut runner, &format!("/sessions {id}")).unwrap();
        execute(&mut runner, &format!("/sessions delete {id}")).unwrap();
        assert!(runner.index.get(&id).is_none());
        let _ = std::fs::remove_dir_all(runner.cwd);
    }
    #[tokio::test]
    async fn enhanced_model_and_provider_preset_wizard_operations() {
        let mut runner = test_runner().await;

        // 1. /model 无参数返回模型列表，且当前生效模型置顶
        if let CliAction::Picker(p) = execute(&mut runner, "/model").unwrap() {
            assert!(matches!(p.kind, PickerKind::Models));
            assert!(p.items[0].label.contains("✓ 当前"));
        }

        // 2. /model <model_name> 自动跨服务商匹配模型
        let res = execute(&mut runner, "/model qwen2.5:32b").unwrap();
        assert!(matches!(res, CliAction::Refresh { .. }));
        assert_eq!(runner.ctx.config.agent.default_provider, "ollama");

        // 3. /provider 与 /providers 打开服务商管理看板
        let action1 = execute(&mut runner, "/provider").unwrap();
        assert!(matches!(
            action1,
            CliAction::SettingsTab(crate::cli::SettingsTab::Providers)
        ));
        let action2 = execute(&mut runner, "/providers").unwrap();
        assert!(matches!(
            action2,
            CliAction::SettingsTab(crate::cli::SettingsTab::Providers)
        ));

        // 4. /provider wizard 弹出预设模板与自定义向导
        if let CliAction::Picker(p) = execute(&mut runner, "/provider wizard").unwrap() {
            assert!(p
                .items
                .iter()
                .any(|i| i.command == "/provider add-preset deepseek"));
            assert!(p.items.iter().any(|i| i.command == "/provider add-custom"));
        } else {
            panic!("expected wizard picker");
        }

        // 5. /provider add-custom 弹出协议规范选择器
        if let CliAction::Picker(p) = execute(&mut runner, "/provider add-custom").unwrap() {
            assert!(p
                .items
                .iter()
                .any(|i| i.command == "/provider add-with-kind openai-compatible"));
            assert!(p
                .items
                .iter()
                .any(|i| i.command == "/provider add-with-kind anthropic"));
            assert!(p
                .items
                .iter()
                .any(|i| i.command == "/provider add-with-kind ollama"));
        } else {
            panic!("expected protocol kind picker");
        }

        // 6. /provider add-preset deepseek 打开预填官方节点的表单
        if let CliAction::Form(f) = execute(&mut runner, "/provider add-preset deepseek").unwrap() {
            assert_eq!(
                f.fields
                    .iter()
                    .find(|field| field.name == "endpoint")
                    .unwrap()
                    .value,
                "https://api.deepseek.com"
            );
            assert_eq!(
                f.fields
                    .iter()
                    .find(|field| field.name == "model")
                    .unwrap()
                    .value,
                "deepseek-chat"
            );
        } else {
            panic!("expected preset form");
        }

        // 7. /provider add-with-kind anthropic 打开预填 Anthropic 协议的表单
        if let CliAction::Form(f) =
            execute(&mut runner, "/provider add-with-kind anthropic").unwrap()
        {
            assert_eq!(
                f.fields
                    .iter()
                    .find(|field| field.name == "kind")
                    .unwrap()
                    .value,
                "anthropic"
            );
            assert_eq!(
                f.fields
                    .iter()
                    .find(|field| field.name == "endpoint")
                    .unwrap()
                    .value,
                "https://api.anthropic.com/v1"
            );
        } else {
            panic!("expected kind form");
        }

        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn ctf_is_shared_session_isolated_and_rejects_traversal_and_unsolved_writeups() {
        let mut runner = test_runner().await;
        for name in ["../evil", "..\\evil", "C:evil", "CON", "NUL.txt"] {
            assert!(validate_challenge_name(name).is_err(), "{name}");
        }
        execute(&mut runner, "/ctf ENABLE").unwrap();
        assert!(runner.ctf_enabled);
        let original = runner.index.current.clone();
        execute(&mut runner, "/ctf add example web").unwrap();
        assert_eq!(runner.challenges().unwrap().len(), 1);
        assert!(execute(&mut runner, "/ctf add bad unknown").is_err());
        assert!(execute(&mut runner, "/ctf writeup example").is_err());
        execute(&mut runner, "/new").unwrap();
        assert!(runner.challenges().unwrap().is_empty());
        execute(&mut runner, &format!("/sessions {original}")).unwrap();
        assert_eq!(runner.challenges().unwrap()[0].name, "example");
        execute(&mut runner, "/ctf disable").unwrap();
        assert!(!runner.ctf_enabled);
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn compact_replaces_model_history_only_after_summary_and_cancel_keeps_history() {
        let mut runner = test_runner().await;
        runner.entries = vec![
            ChatEntry::User("original".into()),
            ChatEntry::Assistant("old answer".into()),
        ];
        let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel, cancel) = oneshot::channel();
        let action = task(execute(&mut runner, "/compact focus on facts").unwrap());
        let outcome = run_task(
            &mut runner,
            action,
            Arc::new(PermissionBroker::deny_all()),
            events,
            cancel,
        )
        .await;
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(entries_to_messages(&runner.entries).len(), 1);
        assert!(
            matches!(runner.entries.last(),Some(ChatEntry::TurnSummary { status,.. }) if status == "done")
        );
        let mut summary = None;
        while let Some(event) = rx.recv().await {
            if let AgentEvent::Compacted { summary: text, .. } = event {
                summary = Some(text);
            }
        }
        assert_eq!(
            entries_to_messages(&runner.entries)[0].content,
            summary.unwrap()
        );
        let before = entries_to_messages(&runner.entries)[0].content.clone();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (tx, cancel) = oneshot::channel();
        tx.send(()).unwrap();
        let outcome = run_task(
            &mut runner,
            CliTask::Compact { instructions: None },
            Arc::new(PermissionBroker::deny_all()),
            events,
            cancel,
        )
        .await;
        assert!(outcome.error.unwrap().contains("Cancelled"));
        assert_eq!(entries_to_messages(&runner.entries)[0].content, before);
        let saved = runner.read_entries(&runner.index.current).unwrap();
        assert!(
            matches!(saved.last(),Some(ChatEntry::TurnSummary { status,.. }) if status == "cancelled")
        );
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn mcp_requires_exact_config_nonce_approval_and_cancel_closes_pending_request() {
        let mut runner = test_runner().await;
        let config = McpServersConfig::default();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (_tx, cancel) = oneshot::channel();
        let outcome = run_task(
            &mut runner,
            CliTask::McpConnect {
                config: config.clone(),
            },
            Arc::new(PermissionBroker::deny_all()),
            events,
            cancel,
        )
        .await;
        assert!(outcome.permission_denied);
        assert!(runner.registries.mcp.is_none());
        let (broker, mut requests) = PermissionBroker::interactive();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (tx, cancel) = oneshot::channel();
        let expected = serde_json::to_value(&config).unwrap();
        let (outcome, request) = tokio::join!(
            run_task(
                &mut runner,
                CliTask::McpConnect {
                    config: config.clone()
                },
                Arc::new(broker),
                events,
                cancel
            ),
            async {
                let request = requests.recv().await.unwrap();
                assert_eq!(request.tool, "mcp_connect");
                assert_eq!(request.arguments, expected);
                assert!(!request.nonce.is_empty());
                assert_eq!(
                    request.decision_for("once"),
                    cyber_agent::PermissionDecision::Deny
                );
                tx.send(()).unwrap();
                request
            }
        );
        assert!(request.reply.is_closed());
        assert!(outcome.error.unwrap().contains("Cancelled"));
        assert!(runner.registries.mcp.is_none());
        let (broker, mut requests) = PermissionBroker::interactive();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (_tx, cancel) = oneshot::channel();
        let (outcome, ()) = tokio::join!(
            run_task(
                &mut runner,
                CliTask::McpConnect { config },
                Arc::new(broker),
                events,
                cancel
            ),
            async {
                requests
                    .recv()
                    .await
                    .unwrap()
                    .reply
                    .send(cyber_agent::PermissionDecision::AllowOnce)
                    .unwrap();
            }
        );
        assert!(outcome
            .error
            .as_deref()
            .unwrap()
            .contains("no servers started"));
        assert!(runner.registries.mcp.is_none());
        assert!(runner
            .registries
            .tools
            .all_schemas()
            .iter()
            .any(|s| s.name == "ctf_challenge"));
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn corrupt_session_or_failed_save_is_not_reported_as_success() {
        let mut runner = test_runner().await;
        runner.save().unwrap();
        let id = runner.index.current.clone();
        let path = crate::history::session_dir(&runner.ctx.paths.history_dir, &runner.cwd)
            .join(format!("{id}.json"));
        std::fs::write(&path, b"broken").unwrap();
        assert!(runner.read_entries(&id).is_err());
        runner.ctx.paths.history_dir = runner.cwd.join("file-instead-of-dir");
        std::fs::write(&runner.ctx.paths.history_dir, b"blocked").unwrap();
        assert!(execute(&mut runner, "/clear").is_err());
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn writeup_uses_loaded_skill_and_saves_project_artifact_and_challenge() {
        let mut runner = test_runner().await;
        execute(&mut runner, "/ctf add solved web").unwrap();
        let mut list = runner.challenges().unwrap();
        list[0].status = cyber_core::CtfStatus::Solved;
        runner.replace_challenges(list).unwrap();
        let skill_dir = runner.ctx.paths.skills_dir.join("ctf-writeup");
        persist(&skill_dir.join("SKILL.md"),b"---\nname: ctf-writeup\ndescription: Writeup guide\n---\nUse reproducible steps and explain the vulnerability.").unwrap();
        let (skills, errors) =
            cyber_skills::SkillRegistry::load_all(&runner.ctx.paths.skills_dir, None);
        assert!(errors.is_empty(), "{errors:?}");
        runner.registries.skills = Arc::new(skills);
        assert!(matches!(
            execute(&mut runner, "/skill ctf-writeup").unwrap(),
            CliAction::Output { .. }
        ));
        assert!(runner.entries.is_empty());
        let action = task(execute(&mut runner, "/ctf writeup solved").unwrap());
        let report = runner
            .writeup_directory(&runner.challenges().unwrap()[0])
            .unwrap()
            .join("writeup.md");
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (_tx, cancel) = oneshot::channel();
        let outcome = run_task(
            &mut runner,
            action,
            Arc::new(PermissionBroker::deny_all()),
            events,
            cancel,
        )
        .await;
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let saved = std::fs::read_to_string(&report).unwrap();
        assert_eq!(saved, outcome.answer);
        assert_eq!(
            runner.challenges().unwrap()[0].writeup.as_deref(),
            Some(saved.as_str())
        );
        let entries = runner.read_entries(&runner.index.current).unwrap();
        assert!(
            matches!(entries.last(),Some(ChatEntry::TurnSummary { status,.. }) if status == "done")
        );
        let action = task(execute(&mut runner, "/ctf writeup solved").unwrap());
        let (events, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let (tx, cancel) = oneshot::channel();
        let (outcome, ()) = tokio::join!(
            run_task(
                &mut runner,
                action,
                Arc::new(PermissionBroker::deny_all()),
                events,
                cancel
            ),
            async {
                while let Some(event) = observed.recv().await {
                    if matches!(event, AgentEvent::Token(_)) {
                        tx.send(()).unwrap();
                        break;
                    }
                }
            }
        );
        assert!(outcome.error.unwrap().contains("Cancelled"));
        assert!(!outcome.answer.is_empty());
        assert_eq!(std::fs::read_to_string(&report).unwrap(), saved);
        let entries = runner.read_entries(&runner.index.current).unwrap();
        assert!(
            matches!(entries.last(),Some(ChatEntry::TurnSummary { status,.. }) if status == "cancelled")
        );
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn approved_mcp_connection_failure_is_reported_without_exposing_config() {
        let mut runner = test_runner().await;
        let config = McpServersConfig {
            servers: vec![cyber_mcp::McpServerSpec {
                name: "failure".into(),
                transport: cyber_mcp::McpTransport::Stdio,
                command: Some("cyber-nonexistent-executable-for-test".into()),
                args: vec![],
                env: Default::default(),
                url: None,
                headers: Default::default(),
                timeout_secs: 1,
            }],
        };
        let (broker, mut requests) = PermissionBroker::interactive();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (_tx, cancel) = oneshot::channel();
        let (outcome, ()) = tokio::join!(
            run_task(
                &mut runner,
                CliTask::McpConnect { config },
                Arc::new(broker),
                events,
                cancel
            ),
            async {
                requests
                    .recv()
                    .await
                    .unwrap()
                    .reply
                    .send(cyber_agent::PermissionDecision::AllowOnce)
                    .unwrap();
            }
        );
        assert!(!outcome.permission_denied);
        let error = outcome.error.unwrap();
        assert!(error.contains("failed"));
        assert!(!error.contains("executable"));
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn memory_rule_crud_preserves_raw_extensions_and_project_rule_ownership() {
        let mut runner = test_runner().await;
        let raw: toml::Value = toml::from_str(
            r#"
[agent]
max_steps = 500
[memory]
notes = "retained"
rules = [
  {enabled = true, scope = "global", prompt = "first", vendor = { notes = "keep", weight = 2 }},
  {enabled = false, scope = "project", prompt = "second", unknown = ["a", "b"]},
]
"#,
        )
        .unwrap();
        persist(&runner.ctx.paths.config_file, &toml_bytes(&raw).unwrap()).unwrap();
        runner.ctx.config = raw.clone().try_into().unwrap();
        let mut edit = form(execute(&mut runner, "/memory rule edit 1").unwrap());
        set(&mut edit, "enabled", "false");
        set(&mut edit, "scope", "both");
        set(&mut edit, "prompt", "changed");
        submit_form(&mut runner, &edit).unwrap();
        let saved = read_configuration(&runner.ctx.paths.config_file).unwrap();
        assert_eq!(
            saved["memory"]["rules"][0]["vendor"],
            raw["memory"]["rules"][0]["vendor"]
        );
        assert_eq!(saved["memory"]["rules"][1], raw["memory"]["rules"][1]);
        let mut add = form(execute(&mut runner, "/memory rule add").unwrap());
        set(&mut add, "prompt", "third");
        submit_form(&mut runner, &add).unwrap();
        let appended = read_configuration(&runner.ctx.paths.config_file).unwrap();
        assert_eq!(appended["memory"]["rules"][0], saved["memory"]["rules"][0]);
        assert_eq!(appended["memory"]["rules"][1], saved["memory"]["rules"][1]);
        execute(&mut runner, "/memory rule delete 1").unwrap();
        let deleted = read_configuration(&runner.ctx.paths.config_file).unwrap();
        assert_eq!(deleted["memory"]["rules"][0], raw["memory"]["rules"][1]);
        assert_eq!(deleted["agent"]["max_steps"].as_integer(), Some(500));
        let global_before = std::fs::read(&runner.ctx.paths.config_file).unwrap();
        let project_path = runner.cwd.join(".cyber/config.toml");
        let project: toml::Value = toml::from_str("[memory]\nrules = [{enabled = true, scope = 'project', prompt = 'project rule', extension = 'project extension'}]\n").unwrap();
        persist(&project_path, &toml_bytes(&project).unwrap()).unwrap();
        runner.ctx.config.memory.rules = project["memory"]["rules"].clone().try_into().unwrap();
        let mut edit = form(execute(&mut runner, "/memory rule edit 1").unwrap());
        set(&mut edit, "prompt", "project updated");
        submit_form(&mut runner, &edit).unwrap();
        assert_eq!(
            std::fs::read(&runner.ctx.paths.config_file).unwrap(),
            global_before
        );
        assert_eq!(
            read_configuration(&project_path).unwrap()["memory"]["rules"][0]["extension"],
            project["memory"]["rules"][0]["extension"]
        );
        assert_eq!(runner.ctx.config.memory.rules[0].prompt, "project updated");
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn sessions_read_without_arguments_lists_metadata_even_for_one_session() {
        let mut runner = test_runner().await;
        runner
            .entries
            .push(ChatEntry::User("body-marker-not-in-list".into()));
        let current = runner.index.current.clone();
        let CliAction::Output { title, text } = execute(&mut runner, "/sessions read").unwrap()
        else {
            panic!("expected metadata output");
        };
        assert_eq!(title, "Sessions");
        assert!(text.contains(&current));
        assert!(text.contains("1 messages (current)"));
        assert!(!text.contains("body-marker-not-in-list"));
        runner.save().unwrap();
        let CliAction::Output { title, text } =
            execute(&mut runner, &format!("/sessions read {current}")).unwrap()
        else {
            panic!("expected body output");
        };
        assert_eq!(title, "Session");
        assert!(text.contains("User: body-marker-not-in-list"));
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn same_named_challenges_in_two_sessions_have_isolated_writeups_and_keep_legacy_reports()
    {
        let mut runner = test_runner().await;
        persist(
            &runner.ctx.paths.skills_dir.join("ctf-writeup/SKILL.md"),
            b"---\nname: ctf-writeup\ndescription: guide\n---\nWrite reproducible steps.",
        )
        .unwrap();
        let (skills, errors) =
            cyber_skills::SkillRegistry::load_all(&runner.ctx.paths.skills_dir, None);
        assert!(errors.is_empty());
        runner.registries.skills = Arc::new(skills);
        let legacy = runner.cwd.join(".cyber/ctf/web/repeated/writeup.md");
        persist(&legacy, b"legacy report stays here").unwrap();
        let mut reports = Vec::new();
        for _ in 0..2 {
            execute(&mut runner, "/ctf add repeated web").unwrap();
            let mut challenges = runner.challenges().unwrap();
            challenges[0].status = cyber_core::CtfStatus::Solved;
            let directory = runner.writeup_directory(&challenges[0]).unwrap();
            runner.replace_challenges(challenges).unwrap();
            let action = task(execute(&mut runner, "/ctf writeup repeated").unwrap());
            let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
            let (_tx, cancel) = oneshot::channel();
            let outcome = run_task(
                &mut runner,
                action,
                Arc::new(PermissionBroker::deny_all()),
                events,
                cancel,
            )
            .await;
            assert!(outcome.error.is_none(), "{:?}", outcome.error);
            assert!(outcome.answer.contains(&format!(
                "Project artifact directory: {}",
                directory.display()
            )));
            reports.push((directory.join("writeup.md"), outcome.answer));
            execute(&mut runner, "/new").unwrap();
        }
        assert_ne!(reports[0].0, reports[1].0);
        for (path, content) in reports {
            assert_eq!(std::fs::read_to_string(path).unwrap(), content);
        }
        assert_eq!(std::fs::read(&legacy).unwrap(), b"legacy report stays here");
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn provider_form_credential_fields_are_secret_without_changing_saved_urls() {
        let mut runner = test_runner().await;
        for endpoint in [
            "https://user:password@example.test/v1",
            "https://example.test/v1?api_key=private-query",
            "https://example.test/private-path-key/v1",
        ] {
            runner
                .ctx
                .providers
                .providers
                .get_mut("openai")
                .unwrap()
                .base_url = endpoint.into();
            let edit = form(execute(&mut runner, "/provider edit openai").unwrap());
            for field in &edit.fields {
                assert_eq!(field.secret, field.name == "apikey", "{}", field.name);
            }
            assert_eq!(
                edit.fields
                    .iter()
                    .find(|f| f.name == "endpoint")
                    .unwrap()
                    .value,
                endpoint
            );
            submit_form(&mut runner, &edit).unwrap();
            let providers: ProvidersConfig = read_configuration(&runner.ctx.paths.providers_file)
                .unwrap()
                .try_into()
                .unwrap();
            assert_eq!(providers.providers["openai"].base_url, endpoint);
        }
        let add = form(execute(&mut runner, "/provider add").unwrap());
        assert!(add
            .fields
            .iter()
            .filter(|f| f.name == "apikey")
            .all(|f| f.secret));
        assert!(
            !add.fields
                .iter()
                .find(|f| f.name == "endpoint")
                .unwrap()
                .secret
        );
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn zero_connection_mcp_attempts_remain_retryable_and_never_report_success() {
        let mut runner = test_runner().await;
        let base = runner.registries.tools.clone();
        let failed = McpServersConfig {
            servers: vec![cyber_mcp::McpServerSpec {
                name: "missing".into(),
                transport: cyber_mcp::McpTransport::Stdio,
                command: Some("cyber-test-missing-server".into()),
                args: vec![],
                env: Default::default(),
                url: None,
                headers: Default::default(),
                timeout_secs: 1,
            }],
        };
        for config in [McpServersConfig::default(), failed.clone(), failed] {
            let (events, mut observed) = tokio::sync::mpsc::unbounded_channel();
            let (_tx, cancel) = oneshot::channel();
            let outcome = run_task(
                &mut runner,
                CliTask::McpConnect { config },
                Arc::new(PermissionBroker::explicit_tools(["mcp_connect"])),
                events,
                cancel,
            )
            .await;
            assert!(outcome
                .error
                .as_deref()
                .unwrap()
                .contains("no servers started"));
            assert!(runner.registries.mcp.is_none());
            assert!(Arc::ptr_eq(&base, &runner.registries.tools));
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&crate::headless::outcome_to_json(
                    &outcome
                ))
                .unwrap()["success"],
                false
            );
            assert!(
                matches!(runner.entries.last(),Some(ChatEntry::TurnSummary { status,.. }) if status == "error")
            );
            let mut error_event = false;
            while let Some(event) = observed.recv().await {
                error_event |= matches!(event, AgentEvent::Error(_));
            }
            assert!(error_event);
            assert!(matches!(
                execute(&mut runner, "/mcp connect").unwrap(),
                CliAction::Task(CliTask::McpConnect { .. })
            ));
        }
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn subagents_and_env_commands_persist_mask_and_complete() {
        let mut runner = test_runner().await;

        execute(&mut runner, "/subagents max_parallel 3").unwrap();
        execute(&mut runner, "/subagents timeout 45").unwrap();
        execute(&mut runner, "/subagents disable").unwrap();
        assert_eq!(runner.ctx.config.agent.subagents.max_parallel, 3);
        assert_eq!(runner.ctx.config.agent.subagents.timeout_secs, 45);
        assert!(!runner.ctx.config.agent.subagents.enabled);
        assert!(execute(&mut runner, "/subagents max_tasks 0").is_err());

        // /subagents stop 命令验证：空闲、单任务按 ID 停止、批量停止与非法参数
        let action = execute(&mut runner, "/subagents stop").unwrap();
        assert!(
            matches!(action, CliAction::Output { ref text, .. } if text.contains("当前没有运行中的子代理"))
        );
        assert!(execute(&mut runner, "/subagents stop invalid").is_err());

        let (job_id, _kill_rx) = runner
            .registries
            .background
            .start(cyber_agent::JobKind::Subagent, "worker".into());
        let run_id = runner.registries.subagents.start("worker");
        runner.registries.background.link_archive(job_id, run_id);

        let action = execute(&mut runner, &format!("/subagents stop {run_id}")).unwrap();
        assert!(
            matches!(action, CliAction::Output { ref text, .. } if text.contains(&format!("已终止子代理 #{run_id}")))
        );
        assert_eq!(
            runner.registries.subagents.snapshot()[0].status,
            cyber_agent::SubagentStatus::Killed
        );

        let (_job_id2, _rx) = runner
            .registries
            .background
            .start(cyber_agent::JobKind::Subagent, "worker2".into());
        let action_all = execute(&mut runner, "/subagents stop all").unwrap();
        assert!(
            matches!(action_all, CliAction::Output { ref text, .. } if text.contains("已终止 1 个运行中的子代理"))
        );
        execute(&mut runner, "/env set PUBLIC visible value").unwrap();
        execute(&mut runner, "/env set-sensitive TOKEN private value").unwrap();
        let listed = execute(&mut runner, "/env list").unwrap();
        let CliAction::Output { text, .. } = listed else {
            panic!("expected env output");
        };
        assert!(text.contains("PUBLIC=visible value"));
        assert!(text.contains("TOKEN=<sensitive>"));
        assert!(!text.contains("private value"));

        let completions = suggestions(Some(&runner), "/env remove TO");
        assert!(completions
            .iter()
            .any(|item| item.value == "/env remove TOKEN "));

        let persisted: Config =
            toml::from_str(&std::fs::read_to_string(&runner.ctx.paths.config_file).unwrap())
                .unwrap();
        assert_eq!(persisted.agent.subagents.max_parallel, 3);
        assert_eq!(persisted.agent.subagents.timeout_secs, 45);
        assert!(!persisted.agent.subagents.enabled);
        assert!(persisted
            .env
            .vars
            .iter()
            .any(|var| var.key == "TOKEN" && var.sensitive && var.value == "private value"));

        execute(&mut runner, "/env remove TOKEN").unwrap();
        assert!(!runner
            .ctx
            .config
            .env
            .vars
            .iter()
            .any(|var| var.key == "TOKEN"));

        // /web 命令验证：关闭与开启，以及持久化
        assert!(runner.ctx.config.tools.web_search);
        execute(&mut runner, "/web off").unwrap();
        assert!(!runner.ctx.config.tools.web_search);
        let persisted_disabled: Config =
            toml::from_str(&std::fs::read_to_string(&runner.ctx.paths.config_file).unwrap())
                .unwrap();
        assert!(!persisted_disabled.tools.web_search);

        execute(&mut runner, "/web on").unwrap();
        assert!(runner.ctx.config.tools.web_search);
        let persisted_enabled: Config =
            toml::from_str(&std::fs::read_to_string(&runner.ctx.paths.config_file).unwrap())
                .unwrap();
        assert!(persisted_enabled.tools.web_search);

        let web_status = execute(&mut runner, "/web status").unwrap();
        let CliAction::Output {
            text: status_text, ..
        } = web_status
        else {
            panic!("expected web status output");
        };
        assert!(status_text.contains("enabled"));
        let _ = std::fs::remove_dir_all(runner.cwd);
    }
    #[tokio::test]
    async fn slash_update_parses_subcommands_without_touching_the_network() {
        let mut runner = test_runner().await;
        for (line, expected) in [
            ("/update", CliUpdate::Prompt),
            ("/update check", CliUpdate::Check),
            ("/UPDATE APPLY", CliUpdate::Apply),
            ("/update now", CliUpdate::Apply),
            ("/update yes", CliUpdate::Apply),
        ] {
            match execute(&mut runner, line).unwrap() {
                CliAction::Update(kind) => assert_eq!(kind, expected, "{line}"),
                _ => panic!("expected Update action for {line}"),
            }
        }
        // `CliAction` 故意不实现 `Debug`（表单含凭据），因此不能用 `unwrap_err`。
        let error = match execute(&mut runner, "/update bogus") {
            Ok(_) => panic!("expected /update usage error"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("Usage: /update"), "{error}");
        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn slash_vision_config_and_status() {
        let mut runner = test_runner().await;
        assert!(runner.ctx.config.agent.vision.enabled);
        let status = execute(&mut runner, "/vision status").unwrap();
        let CliAction::Output { text, .. } = status else {
            panic!("expected vision status output");
        };
        assert!(text.contains("enabled"));

        execute(&mut runner, "/vision off").unwrap();
        assert!(!runner.ctx.config.agent.vision.enabled);
        execute(&mut runner, "/vision on").unwrap();
        assert!(runner.ctx.config.agent.vision.enabled);

        execute(&mut runner, "/vision provider custom-prov").unwrap();
        assert_eq!(runner.ctx.config.agent.vision.provider, "custom-prov");
        execute(&mut runner, "/vision model custom-model").unwrap();
        assert_eq!(runner.ctx.config.agent.vision.model, "custom-model");

        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn slash_vision_model_picker_and_set() {
        let mut runner = test_runner().await;
        // 1. 无参数时弹出 picker
        let action = execute(&mut runner, "/vision model").unwrap();
        match action {
            CliAction::Picker(p) => {
                assert_eq!(p.title, "Vision Models");
                assert!(!p.items.is_empty());
                assert!(p
                    .items
                    .iter()
                    .all(|i| i.command.starts_with("/vision model ")));
            }
            _ => panic!("expected Picker for /vision model"),
        }

        // 2. 带参数时直接设置
        execute(&mut runner, "/vision model openai gpt-4o").unwrap();
        assert_eq!(runner.ctx.config.agent.vision.provider, "openai");
        assert_eq!(runner.ctx.config.agent.vision.model, "gpt-4o");

        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn command_panel_guides_and_top_shortcut_entries() {
        let runner = test_runner().await;

        // 1. 无空格输入 /mcp：第一项置顶 /mcp，描述为打开 MCP 面板
        let mcp_comps = suggestions(Some(&runner), "/mcp");
        assert!(!mcp_comps.is_empty());
        assert_eq!(mcp_comps[0].value.trim(), "/mcp");
        assert!(mcp_comps[0].description.contains("打开 MCP 面板"));

        // 2. 带空格输入 /mcp ：第 0 项为 /mcp 面板引导项，后续为带描述的 connect、list、status
        let mcp_sub_comps = suggestions(Some(&runner), "/mcp ");
        assert!(!mcp_sub_comps.is_empty());
        assert_eq!(mcp_sub_comps[0].value, "/mcp");
        assert!(mcp_sub_comps[0].description.contains("打开 MCP 面板"));

        let connect_item = mcp_sub_comps
            .iter()
            .find(|c| c.value == "/mcp connect ")
            .expect("should have /mcp connect");
        assert_eq!(connect_item.description, "测试并建立 MCP 服务器连接");

        let list_item = mcp_sub_comps
            .iter()
            .find(|c| c.value == "/mcp list ")
            .expect("should have /mcp list");
        assert_eq!(list_item.description, "查看已配置 MCP 服务器列表");

        let status_item = mcp_sub_comps
            .iter()
            .find(|c| c.value == "/mcp status ")
            .expect("should have /mcp status");
        assert_eq!(status_item.description, "查看 MCP 服务器运行状态");

        // 3. 带前缀 /mcp c：精准过滤子命令，引导项不强行占用
        let mcp_c_comps = suggestions(Some(&runner), "/mcp c");
        assert_eq!(mcp_c_comps.len(), 1);
        assert_eq!(mcp_c_comps[0].value, "/mcp connect ");
        assert_eq!(mcp_c_comps[0].description, "测试并建立 MCP 服务器连接");

        // 4. 带空格输入 /provider ：第 0 项为 /provider 面板引导项
        let provider_comps = suggestions(Some(&runner), "/provider ");
        assert!(!provider_comps.is_empty());
        assert_eq!(provider_comps[0].value, "/provider");
        assert!(provider_comps[0].description.contains("打开服务商面板"));
        let prov_add = provider_comps
            .iter()
            .find(|c| c.value == "/provider add ")
            .expect("should have /provider add");
        assert_eq!(prov_add.description, "添加模型服务商 (向导或自定义协议)");

        // 5. 带空格输入 /model ：第 0 项为 /model 面板引导项
        let model_comps = suggestions(Some(&runner), "/model ");
        assert!(!model_comps.is_empty());
        assert_eq!(model_comps[0].value, "/model");
        assert!(model_comps[0].description.contains("打开模型面板"));

        // 6. 带空格输入 /settings ：第 0 项为 /settings 引导项
        let settings_comps = suggestions(Some(&runner), "/settings ");
        assert!(!settings_comps.is_empty());
        assert_eq!(settings_comps[0].value, "/settings");
        assert!(settings_comps[0].description.contains("打开设置中心面板"));

        let _ = std::fs::remove_dir_all(runner.cwd);
    }

    #[tokio::test]
    async fn env_form_add_edit_and_submit_lifecycle() {
        let mut runner = test_runner().await;

        // 1. /env add opens form
        let action = execute(&mut runner, "/env add").unwrap();
        let mut add_form = form(action);
        assert_eq!(add_form.title, "Add Environment Variable");
        assert!(matches!(
            add_form.kind,
            FormKind::EnvVar {
                index: None,
                original_key: None
            }
        ));

        // Submitting with empty key should fail
        set(&mut add_form, "key", "");
        assert!(submit_form(&mut runner, &add_form).is_err());

        // Submitting with invalid key (contains '=') should fail
        set(&mut add_form, "key", "INVALID=KEY");
        assert!(submit_form(&mut runner, &add_form).is_err());

        // Submitting valid new env var
        set(&mut add_form, "key", "MY_API_KEY");
        set(&mut add_form, "value", "secret-token-123");
        set(&mut add_form, "sensitive", "true");
        submit_form(&mut runner, &add_form).unwrap();

        assert_eq!(runner.ctx.config.env.vars.len(), 1);
        assert_eq!(runner.ctx.config.env.vars[0].key, "MY_API_KEY");
        assert_eq!(runner.ctx.config.env.vars[0].value, "secret-token-123");
        assert!(runner.ctx.config.env.vars[0].sensitive);

        // Submitting duplicate key should fail
        let action_dup = execute(&mut runner, "/env add").unwrap();
        let mut dup_form = form(action_dup);
        set(&mut dup_form, "key", "MY_API_KEY");
        set(&mut dup_form, "value", "another-token");
        assert!(submit_form(&mut runner, &dup_form).is_err());

        // 2. /env edit by key
        let action_edit = execute(&mut runner, "/env edit MY_API_KEY").unwrap();
        let mut edit_form = form(action_edit);
        assert_eq!(edit_form.title, "Edit Environment Variable (MY_API_KEY)");
        assert_eq!(
            edit_form
                .fields
                .iter()
                .find(|f| f.name == "value")
                .unwrap()
                .value,
            "secret-token-123"
        );

        // Edit value & keep same key
        set(&mut edit_form, "value", "updated-token-456");
        set(&mut edit_form, "sensitive", "false");
        submit_form(&mut runner, &edit_form).unwrap();

        assert_eq!(runner.ctx.config.env.vars.len(), 1);
        assert_eq!(runner.ctx.config.env.vars[0].key, "MY_API_KEY");
        assert_eq!(runner.ctx.config.env.vars[0].value, "updated-token-456");
        assert!(!runner.ctx.config.env.vars[0].sensitive);

        // 3. /env edit by 1-based index
        let action_idx = execute(&mut runner, "/env edit 1").unwrap();
        let edit_idx_form = form(action_idx);
        assert_eq!(
            edit_idx_form.title,
            "Edit Environment Variable (MY_API_KEY)"
        );

        // 4. /env edit unknown key fails
        assert!(execute(&mut runner, "/env edit NON_EXISTENT").is_err());

        let _ = std::fs::remove_dir_all(runner.cwd);
    }
}
