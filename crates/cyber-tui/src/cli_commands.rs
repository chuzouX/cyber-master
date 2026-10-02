//! Non-printing CLI commands. Notices never become model history.
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use color_eyre::eyre::{bail, eyre, Result};
use cyber_agent::{AgentEvent, PermissionBroker};
use cyber_core::{Config, CtfCategory, CtfChallenge, MemoryRule, ProviderConfig, ProvidersConfig};
use cyber_mcp::McpServersConfig;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::headless::{validate_session_id, HeadlessOutcome, SessionRunner};
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
    Task(CliTask),
    Cancel,
    Quit,
    Mode(cyber_agent::PermissionMode),
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
}
pub struct CommandPicker {
    pub title: String,
    pub items: Vec<PickerItem>,
    pub kind: PickerKind,
}
pub enum PickerKind {
    Sessions,
    Models,
}
pub struct PickerItem {
    pub label: String,
    pub detail: String,
    pub command: String,
}
pub enum CliTask {
    Compact { instructions: Option<String> },
    Writeup { challenge: Box<CtfChallenge> },
    McpConnect { config: McpServersConfig },
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
pub fn commands() -> Vec<&'static CommandSpec> {
    slash::COMMANDS
        .iter()
        .filter(|c| c.name != "/mode")
        .chain(std::iter::once(&EFFORT))
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
fn save_config(
    runner: &mut SessionRunner,
    config: Config,
    section: &str,
    field: &str,
) -> Result<()> {
    persist(
        &runner.ctx.paths.config_file,
        &config_field_bytes(&runner.ctx.paths.config_file, &config, section, field)?,
    )?;
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
        (None, None) => bail!("Missing memory rule operation"),
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

pub fn execute(runner: &mut SessionRunner, line: &str) -> Result<CliAction> {
    let (name, args) = split(line);
    if name.eq_ignore_ascii_case("/mode") || name.eq_ignore_ascii_case("/approval") {
        if args.is_empty() {
            return Ok(output(
                "Mode",
                "审批模式选项：\n/mode auto       自动审批（低风险自动放行，高风险弹出确认）\n/mode manual     手动审批（每次调用工具都弹出确认）\n/mode unlimited  无限制（不弹出确认，直接执行）\n快捷键：按 F2 可快速循环切换审批模式。",
            ));
        }
        let mode = cyber_agent::PermissionMode::parse(args).ok_or_else(|| {
            eyre!("未知审批模式：{args}。可用值：auto(自动)、manual(手动)、unlimited(无限制)")
        })?;
        return Ok(CliAction::Mode(mode));
    }
    let parsed = if name.eq_ignore_ascii_case("/effort") {
        let args = match args.to_ascii_lowercase().as_str() {
            "medium" => "middle",
            "xhigh" => "max",
            _ => args,
        };
        slash::parse(&format!("/think {args}"))
    } else {
        slash::parse(line)
    };
    Ok(match parsed {
        SlashCommand::Help => output(
            "Commands",
            commands()
                .iter()
                .map(|c| format!("{}  {}", c.usage, c.desc))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
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
            if args.is_empty() {
                let mut items = Vec::new();
                for name in runner.ctx.providers.sorted_names() {
                    let provider = &runner.ctx.providers.providers[&name];
                    let mut models: Vec<_> = provider.models.keys().cloned().collect();
                    models.push(provider.model.clone());
                    models.sort();
                    models.dedup();
                    for model in models.into_iter().filter(|m| !m.is_empty()) {
                        items.push(PickerItem {
                            label: model.clone(),
                            detail: name.clone(),
                            command: format!("/model {name} {model}"),
                        });
                    }
                }
                CliAction::Picker(CommandPicker {
                    title: "Models".into(),
                    items,
                    kind: PickerKind::Models,
                })
            } else {
                let (provider, model) = split(&args);
                runner.select_model_persisted(provider, (!model.is_empty()).then_some(model))?;
                refresh("Model selected", false)
            }
        }
        SlashCommand::Provider(args) => provider(runner, &args)?,
        SlashCommand::Tools => output(
            "Tools",
            runner
                .registries
                .tools
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
                    runner
                        .registries
                        .skills
                        .iter()
                        .map(|s| format!("{}  {}", s.name(), s.frontmatter.description))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            } else {
                let skill = runner
                    .registries
                    .skills
                    .find(&args)
                    .ok_or_else(|| eyre!("Unknown skill"))?;
                output("Skill", skill.body.clone())
            }
        }
        SlashCommand::Mcp(args) => {
            let config = McpServersConfig::load(&runner.ctx.paths.mcp_servers_file)
                .map_err(|_| eyre!("Cannot read MCP configuration"))?;
            match args.to_ascii_lowercase().as_str() {
                "connect" => {
                    if runner.registries.mcp.is_some() {
                        bail!("MCP already connected; restart before reconnecting");
                    }
                    CliAction::Task(CliTask::McpConnect { config })
                }
                "" | "list" | "status" => {
                    let connected = runner
                        .registries
                        .mcp
                        .as_ref()
                        .map(|m| m.server_names())
                        .unwrap_or_default();
                    output(
                        "MCP",
                        config
                            .servers
                            .iter()
                            .map(|s| {
                                format!(
                                    "{}  {:?}  {}",
                                    s.name,
                                    s.transport,
                                    if connected.contains(&s.name.as_str()) {
                                        "connected"
                                    } else {
                                        "not connected (explicit approval required)"
                                    }
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                }
                _ => bail!("Usage: /mcp list|status|connect"),
            }
        }
        SlashCommand::Compact(args) => CliAction::Task(CliTask::Compact {
            instructions: (!args.is_empty()).then_some(args),
        }),
        SlashCommand::Ctf(args) => ctf(runner, &args)?,
        SlashCommand::MaxSteps(args) => {
            if args.is_empty() {
                output("Max Steps", runner.ctx.config.agent.max_steps.to_string())
            } else {
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
        }
        SlashCommand::Think(args) => {
            if args.is_empty() {
                output(
                    "Thinking",
                    runner.ctx.config.agent.thinking_intensity.as_str(),
                )
            } else {
                let intensity = crate::headless::thinking(
                    Some(&args),
                    runner.ctx.config.agent.thinking_intensity,
                )?;
                let mut config = runner.ctx.config.clone();
                config.agent.thinking_intensity = intensity;
                save_config(runner, config, "agent", "thinking_intensity")?;
                refresh("Thinking intensity saved", false)
            }
        }
        SlashCommand::Sessions(args) => sessions(runner, &args)?,
        SlashCommand::Memory(args) => memory(runner, &args)?,
        SlashCommand::Mode(_) => bail!("/mode is not available in CLI"),
        SlashCommand::Unknown(_) => bail!("Unknown command; use /help"),
    })
}

fn provider(runner: &mut SessionRunner, args: &str) -> Result<CliAction> {
    let (sub, rest) = split(args);
    match sub.to_ascii_lowercase().as_str() {
        "" | "list" => Ok(output(
            "Providers",
            runner
                .ctx
                .providers
                .sorted_names()
                .iter()
                .map(|n| {
                    let p = &runner.ctx.providers.providers[n];
                    // Endpoints can contain credentials. Do not display them.
                    format!(
                        "{}{}  {}  {}",
                        n,
                        if *n == runner.ctx.config.agent.default_provider {
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
        )),
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
                ("endpoint", p.base_url.clone(), true),
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
        "remove" => {
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
        _ => bail!("Usage: /provider list|add|edit <name>|use <name>|remove <name>"),
    }
}

fn sessions(runner: &mut SessionRunner, args: &str) -> Result<CliAction> {
    let (sub, rest) = split(args);
    match sub.to_ascii_lowercase().as_str() {
        "" | "list" => Ok(CliAction::Picker(CommandPicker {
            title: "Sessions".into(),
            kind: PickerKind::Sessions,
            items: runner
                .index
                .sessions
                .iter()
                .map(|s| PickerItem {
                    label: s.title.clone(),
                    detail: format!("{}  {} messages", s.id, s.message_count),
                    command: format!("/sessions {}", s.id),
                })
                .collect(),
        })),
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
        _ => {
            runner.select_session(args.trim())?;
            Ok(refresh("Session selected", true))
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
            "list" => Ok(output(
                "Memory Rules",
                runner
                    .ctx
                    .config
                    .memory
                    .rules
                    .iter()
                    .enumerate()
                    .map(|(i, r)| format!("{}. {} {} {}", i + 1, r.enabled, r.scope, r.prompt))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )),
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
            _ => bail!("Usage: /memory rule [add|list|edit <index>|delete <index>]"),
        };
    }
    let global = runner.ctx.paths.memory_file.clone();
    let project = runner.cwd.join(".cyber/memory.md");
    if sub.is_empty() || sub.eq_ignore_ascii_case("list") {
        let mut text = String::new();
        for (name, path) in [("global", &global), ("project", &project)] {
            let content = String::from_utf8(read_optional(path)?)?;
            for (i, line) in content
                .lines()
                .filter_map(|l| l.strip_prefix("- "))
                .enumerate()
            {
                text.push_str(&format!("{name} {}. {line}\n", i + 1));
            }
        }
        return Ok(output("Memory", text));
    }
    let sub = sub.to_ascii_lowercase();
    let (path, index, content) = if sub == "add" || sub == "project" {
        (if sub == "add" { global } else { project }, None, rest)
    } else {
        let (scope, rest) = split(rest);
        let path = match scope.to_ascii_lowercase().as_str() {
            "global" => global,
            "project" => project,
            _ => bail!("Scope must be global or project"),
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
        _ => bail!("Unknown memory command"),
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
        "" | "status" => Ok(output(
            "CTF",
            if runner.ctf_enabled {
                "enabled"
            } else {
                "disabled"
            },
        )),
        "list" => Ok(output(
            "CTF",
            runner
                .challenges()?
                .iter()
                .map(|c| format!("{} [{}] {}", c.name, c.category, c.status.label()))
                .collect::<Vec<_>>()
                .join("\n"),
        )),
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
        _ => bail!("Usage: /ctf enable|disable|add <name> <category>|list|writeup <name>"),
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
                _ => bail!("Enabled must be true or false"),
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
    }
}

pub fn suggestions(runner: Option<&SessionRunner>, input: &str) -> Vec<CompletionItem> {
    let input = input.trim_start();
    if !input.starts_with('/') {
        return Vec::new();
    }
    if !input.contains(char::is_whitespace) {
        return commands()
            .into_iter()
            .filter(|c| c.name.starts_with(&input.to_ascii_lowercase()))
            .map(|c| CompletionItem {
                value: format!("{} ", c.name),
                description: c.desc.into(),
            })
            .collect();
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
        values.push("connect".into());
    }
    if cmd == "/sessions" && head.is_empty() {
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
            || (cmd == "/provider"
                && ["use", "edit", "remove"]
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
        if cmd == "/sessions"
            && (head.is_empty()
                || ["read", "delete"]
                    .iter()
                    .any(|s| head.eq_ignore_ascii_case(s)))
        {
            values.extend(runner.index.sessions.iter().map(|s| s.id.clone()));
        }
        if cmd == "/skill" && head.is_empty() {
            values.extend(runner.registries.skills.iter().map(|s| s.name().to_owned()));
        }
    }
    values.sort();
    values.dedup();
    values
        .into_iter()
        .filter(|s| s.to_lowercase().starts_with(&prefix.to_lowercase()))
        .map(|s| CompletionItem {
            value: if head.is_empty() {
                format!("{cmd} {s} ")
            } else {
                format!("{cmd} {head} {s} ")
            },
            description: cmd.clone(),
        })
        .collect()
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
    async fn catalog_dispatches_all_sixteen_commands_and_effort_case_insensitively() {
        let mut runner = test_runner().await;
        assert_eq!(commands().len(), 17);
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
                assert_eq!(
                    field.secret,
                    ["endpoint", "apikey"].contains(&field.name.as_str()),
                    "{}",
                    field.name
                );
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
            .filter(|f| ["endpoint", "apikey"].contains(&f.name.as_str()))
            .all(|f| f.secret));
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
}
