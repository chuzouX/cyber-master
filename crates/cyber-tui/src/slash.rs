//! 斜杠命令解析（Chat 输入态）。
//!
//! 用户在输入框输入以 `/` 开头的文本并 Enter 时，App 拦截为斜杠命令（不发送给
//! agent）。支持：`/help` `/clear` `/mode` `/model`（打开面板选 provider+model）
//! `/provider` `/tools` `/cancel` `/quit` `/new` `/sessions` `/max_steps`。
//!
//! 命令名大小写不敏感（`/HELP` 与 `/help` 等价）；参数保留原样。未知命令返回
//! `Unknown`，由 App 层展示提示。
//!
//! 输入 `/` 时自动弹出命令补全菜单（见 `ChatState::slash_menu`）：按前缀过滤
//! `COMMANDS`，Up/Down 选择，Enter/Tab 补全命令名 + 空格，Esc 关闭。`COMMANDS`
//! 同时是命令描述/用法的单一来源，`HELP_TEXT` 与菜单均据此展示。

/// 一条斜杠命令的元信息（补全菜单与帮助的单一来源）。
///
/// `PartialEq` 用于 `update_slash_menu` 中比较新旧过滤结果，避免选中项无谓重置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    /// 命令名（含 `/`，小写），如 `/mode`。用于前缀匹配与补全。
    pub name: &'static str,
    /// 用法串（含参数占位），如 `/mode <name>`。菜单与帮助展示。
    pub usage: &'static str,
    /// 简短描述。
    pub desc: &'static str,
}

/// 全部命令目录（顺序即菜单展示顺序）。
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/help",
        usage: "/help",
        desc: "显示此帮助",
    },
    CommandSpec {
        name: "/clear",
        usage: "/clear",
        desc: "清空对话历史",
    },
    CommandSpec {
        name: "/mode",
        usage: "/mode <name>",
        desc: "切换模式（chat / workflow / dashboard）",
    },
    CommandSpec {
        name: "/model",
        usage: "/model [provider]",
        desc: "打开面板选择 provider + model（带 provider 参数则直接切换）",
    },
    CommandSpec {
        name: "/provider",
        usage: "/provider <sub>",
        desc: "管理服务商：list | add | edit <name> | use <name> | remove <name>",
    },
    CommandSpec {
        name: "/tools",
        usage: "/tools",
        desc: "列出可用工具",
    },
    CommandSpec {
        name: "/skill",
        usage: "/skill <name|list>",
        desc: "查看 Skill 详细说明（list 列出全部）",
    },
    CommandSpec {
        name: "/mcp",
        usage: "/mcp <list|status>",
        desc: "查看 MCP server 连接状态",
    },
    CommandSpec {
        name: "/cancel",
        usage: "/cancel",
        desc: "取消当前生成",
    },
    CommandSpec {
        name: "/compact",
        usage: "/compact [instructions]",
        desc: "手动压缩上下文（可选自定义摘要指令）",
    },
    CommandSpec {
        name: "/ctf",
        usage: "/ctf <enable|disable|add|list|writeup>",
        desc: "CTF 模式管理（enable/disable 开关，add 添加题目，list 列出，writeup 生成报告）",
    },
    CommandSpec {
        name: "/max_steps",
        usage: "/max_steps <N>",
        desc: "查看或设置工具调用步数上限（1-1000）",
    },
    CommandSpec {
        name: "/think",
        usage: "/think [low|middle|high|max|auto]",
        desc: "查看或设置思考强度",
    },
    CommandSpec {
        name: "/new",
        usage: "/new",
        desc: "新建会话",
    },
    CommandSpec {
        name: "/sessions",
        usage: "/sessions <list|read <id|关键词>|new>",
        desc: "会话管理：list 面板 / read 跨会话读取 / new 新建",
    },
    CommandSpec {
        name: "/memory",
        usage: "/memory [list|add <text>|project <text>]",
        desc: "用户记忆：list 查看 / add 追加全局 / project 追加项目级",
    },
    CommandSpec {
        name: "/quit",
        usage: "/quit",
        desc: "退出 Cyber Master",
    },
];

/// 按前缀过滤命令目录（大小写不敏感）。`prefix` 应已 trim 且以 `/` 开头。
/// 返回 `&'static CommandSpec` 切片引用，供补全菜单复用（无拷贝）。
pub fn filter_commands(prefix: &str) -> Vec<&'static CommandSpec> {
    let p = prefix.to_lowercase();
    COMMANDS.iter().filter(|c| c.name.starts_with(p.as_str())).collect()
}

/// 返回命令的二级参数建议（仅固定参数集命令）。无固定参数的命令返回空。
///
/// 用于 Tab 补全二级参数：用户输入 `/think l` 时过滤出 `low`。
/// `/model` `/max_steps` `/compact` 等无固定参数集的命令返回空（不补全）。
pub fn param_suggestions(cmd: &str) -> Vec<&'static str> {
    match cmd {
        "/think" => vec!["low", "middle", "high", "max", "auto"],
        "/ctf" => vec!["enable", "disable", "add", "list", "writeup"],
        "/mode" => vec!["chat", "workflow", "dashboard"],
        "/provider" => vec!["list", "add", "edit", "use", "remove"],
        "/sessions" => vec!["list", "read", "new"],
        "/memory" => vec!["list", "add", "project"],
        "/mcp" => vec!["list", "status"],
        "/skill" => vec!["list"],
        _ => Vec::new(),
    }
}

/// 一个已解析的斜杠命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// `/help` — 显示命令帮助。
    Help,
    /// `/clear` — 清空对话历史。
    Clear,
    /// `/mode <name>` — 切换模式（空串表示缺参数）。
    Mode(String),
    /// `/model [provider]` — 打开面板选择 provider + model（空串）；或直接切换 provider（向后兼容）。
    Model(String),
    /// `/provider <subcommand>` — 管理服务商（list / add / edit / use / remove）。
    /// 空串 = list；子命令参数保留原样由 App 层解析。
    Provider(String),
    /// `/tools` — 列出可用工具。
    Tools,
    /// `/skill <name|list>` — 查看 Skill 详细说明（list 列出全部）。
    /// 空串 = list；非空 = 注入指定 skill 的 body 为 System 条目。
    Skill(String),
    /// `/mcp <list|status>` — 查看 MCP server 连接状态。
    /// 空串 / list / status 均列出连接状态。
    Mcp(String),
    /// `/cancel` — 取消当前生成。
    Cancel,
    /// `/compact [instructions]` — 手动压缩上下文。
    /// 空串 = 无自定义指令；非空 = 自定义摘要指令。
    Compact(String),
    /// `/ctf <enable|disable|add|list|writeup>` — CTF 模式管理。
    /// enable/disable 开关；add <name> <category> 添加题目；list 列出；writeup <name> 生成报告。
    Ctf(String),
    /// `/max_steps <N>` — 查看或设置工具调用步数上限。空串 = 查看当前值。
    MaxSteps(String),
    /// `/think [level]` — 查看或设置思考强度。空串 = 查看当前值。
    Think(String),
    /// `/new` — 新建会话（保存当前 → 切到空会话）。
    New,
    /// `/sessions <list|read <id|关键词>|new>` — 会话管理。
    /// 空串 / list → 打开 session 面板；read → 跨会话读取；new → 同 `/new`。
    Sessions(String),
    /// `/memory [list|add <text>|project <text>]` — 用户记忆管理。
    /// 空串 / list → 查看记忆；add <text> → 追加全局；project <text> → 追加项目级。
    Memory(String),
    /// `/quit` — 退出。
    Quit,
    /// 未知命令（含原始命令名）。
    Unknown(String),
}

/// 解析一行输入为斜杠命令。输入应以 `/` 开头（调用前保证）；内部会 trim。
/// 命令名转小写匹配，参数保留原样并 trim。
pub fn parse(line: &str) -> SlashCommand {
    let trimmed = line.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let cmd_raw = parts.next().unwrap_or("");
    let args = parts.next().unwrap_or("").trim();
    let cmd = cmd_raw.to_lowercase();
    match cmd.as_str() {
        "/help" => SlashCommand::Help,
        "/clear" => SlashCommand::Clear,
        "/mode" => SlashCommand::Mode(args.to_string()),
        "/model" => SlashCommand::Model(args.to_string()),
        "/provider" => SlashCommand::Provider(args.to_string()),
        "/tools" => SlashCommand::Tools,
        "/skill" => SlashCommand::Skill(args.to_string()),
        "/mcp" => SlashCommand::Mcp(args.to_string()),
        "/cancel" => SlashCommand::Cancel,
        "/compact" => SlashCommand::Compact(args.to_string()),
        "/ctf" => SlashCommand::Ctf(args.to_string()),
        "/max_steps" => SlashCommand::MaxSteps(args.to_string()),
        "/think" => SlashCommand::Think(args.to_string()),
        "/new" => SlashCommand::New,
        "/sessions" => SlashCommand::Sessions(args.to_string()),
        "/memory" => SlashCommand::Memory(args.to_string()),
        "/quit" => SlashCommand::Quit,
        _ => SlashCommand::Unknown(cmd_raw.to_string()),
    }
}

/// `/help` 输出文本。
pub const HELP_TEXT: &str = "\
可用斜杠命令：
  /help              显示此帮助
  /clear             清空对话历史
  /mode <name>       切换模式（chat / workflow / dashboard）
  /model [provider]  打开面板选择 provider + model（带 provider 参数则直接切换）
  /provider <sub>    管理服务商：list | add | edit <name> | use <name> | remove <name>
  /tools             列出可用工具
  /skill <name|list> 查看 Skill 详细说明（list 列出全部）
  /mcp <list|status> 查看 MCP server 连接状态
  /cancel            取消当前生成
  /compact [instr]   手动压缩上下文（可选自定义摘要指令）
  /max_steps <N>     查看或设置工具调用步数上限（1-1000）
  /think [level]     查看或设置思考强度（low / middle / high / max / auto）
  /new               新建会话
  /sessions <sub>    会话管理：list（面板）| read <id|关键词>（跨读）| new
  /memory <sub>      用户记忆：list 查看 | add <text> 全局 | project <text> 项目级
  /quit              退出 Cyber Master";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_help() {
        assert_eq!(parse("/help"), SlashCommand::Help);
        assert_eq!(parse("/help  "), SlashCommand::Help); // 尾空格 trim
    }

    #[test]
    fn parse_case_insensitive() {
        assert_eq!(parse("/HELP"), SlashCommand::Help);
        assert_eq!(parse("/Clear"), SlashCommand::Clear);
        assert_eq!(parse("/QUIT"), SlashCommand::Quit);
    }

    #[test]
    fn parse_mode_with_arg() {
        assert_eq!(parse("/mode chat"), SlashCommand::Mode("chat".into()));
        assert_eq!(
            parse("/mode workflow"),
            SlashCommand::Mode("workflow".into())
        );
    }

    #[test]
    fn parse_mode_no_arg() {
        assert_eq!(parse("/mode"), SlashCommand::Mode(String::new()));
        assert_eq!(parse("/mode   "), SlashCommand::Mode(String::new()));
    }

    #[test]
    fn parse_model_with_arg() {
        assert_eq!(parse("/model ollama"), SlashCommand::Model("ollama".into()));
    }

    #[test]
    fn parse_model_no_arg() {
        assert_eq!(parse("/model"), SlashCommand::Model(String::new()));
    }

    #[test]
    fn parse_clear_tools_cancel_quit() {
        assert_eq!(parse("/clear"), SlashCommand::Clear);
        assert_eq!(parse("/tools"), SlashCommand::Tools);
        assert_eq!(parse("/cancel"), SlashCommand::Cancel);
        assert_eq!(parse("/quit"), SlashCommand::Quit);
        assert_eq!(parse("/new"), SlashCommand::New);
    }

    #[test]
    fn parse_max_steps_no_arg() {
        assert_eq!(parse("/max_steps"), SlashCommand::MaxSteps(String::new()));
    }

    #[test]
    fn parse_max_steps_with_number() {
        assert_eq!(parse("/max_steps 100"), SlashCommand::MaxSteps("100".into()));
    }

    #[test]
    fn parse_think_no_arg() {
        assert_eq!(parse("/think"), SlashCommand::Think(String::new()));
    }

    #[test]
    fn parse_think_with_level() {
        assert_eq!(parse("/think high"), SlashCommand::Think("high".into()));
    }

    #[test]
    fn parse_compact_no_arg() {
        assert_eq!(parse("/compact"), SlashCommand::Compact(String::new()));
        assert_eq!(parse("/compact   "), SlashCommand::Compact(String::new()));
    }

    #[test]
    fn parse_compact_with_instructions() {
        assert_eq!(
            parse("/compact 关注安全相关内容"),
            SlashCommand::Compact("关注安全相关内容".into())
        );
        // 多词指令保留原样
        assert_eq!(
            parse("/compact focus on code changes and tests"),
            SlashCommand::Compact("focus on code changes and tests".into())
        );
    }

    #[test]
    fn parse_unknown() {
        match parse("/foobar") {
            SlashCommand::Unknown(name) => assert_eq!(name, "/foobar"),
            other => panic!("期望 Unknown，得到 {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_preserves_original_case() {
        match parse("/FooBar") {
            SlashCommand::Unknown(name) => assert_eq!(name, "/FooBar"),
            other => panic!("期望 Unknown，得到 {other:?}"),
        }
    }

    #[test]
    fn parse_non_slash_is_unknown() {
        // 调用前应保证以 / 开头，但即便传入普通文本也归类 Unknown（防御）
        match parse("hello") {
            SlashCommand::Unknown(name) => assert_eq!(name, "hello"),
            other => panic!("期望 Unknown，得到 {other:?}"),
        }
    }

    #[test]
    fn help_text_lists_all_commands() {
        for cmd in ["/help", "/clear", "/mode", "/model", "/provider", "/tools", "/skill", "/mcp", "/cancel", "/compact", "/max_steps", "/think", "/new", "/sessions", "/memory", "/quit"] {
            assert!(HELP_TEXT.contains(cmd), "HELP_TEXT 应包含 {cmd}");
        }
    }

    #[test]
    fn parse_memory_no_arg() {
        assert_eq!(parse("/memory"), SlashCommand::Memory(String::new()));
    }

    #[test]
    fn parse_memory_with_arg() {
        assert_eq!(parse("/memory add 记住这个"), SlashCommand::Memory("add 记住这个".into()));
        assert_eq!(parse("/memory project 项目约定"), SlashCommand::Memory("project 项目约定".into()));
    }

    #[test]
    fn parse_provider_no_arg() {
        assert_eq!(parse("/provider"), SlashCommand::Provider(String::new()));
        assert_eq!(parse("/provider   "), SlashCommand::Provider(String::new()));
    }

    #[test]
    fn parse_provider_subcommands() {
        assert_eq!(parse("/provider list"), SlashCommand::Provider("list".into()));
        assert_eq!(parse("/provider add"), SlashCommand::Provider("add".into()));
        assert_eq!(parse("/provider use openai"), SlashCommand::Provider("use openai".into()));
        assert_eq!(parse("/provider edit anthropic"), SlashCommand::Provider("edit anthropic".into()));
        assert_eq!(parse("/provider remove ollama"), SlashCommand::Provider("remove ollama".into()));
    }

    #[test]
    fn parse_provider_case_insensitive() {
        assert_eq!(parse("/PROVIDER"), SlashCommand::Provider(String::new()));
        assert_eq!(parse("/Provider List"), SlashCommand::Provider("List".into()));
    }

    #[test]
    fn parse_skill_no_arg_is_list() {
        assert_eq!(parse("/skill"), SlashCommand::Skill(String::new()));
        assert_eq!(parse("/skill   "), SlashCommand::Skill(String::new()));
    }

    #[test]
    fn parse_skill_with_name() {
        assert_eq!(parse("/skill src-recon"), SlashCommand::Skill("src-recon".into()));
        assert_eq!(parse("/skill list"), SlashCommand::Skill("list".into()));
    }

    #[test]
    fn parse_mcp_no_arg() {
        assert_eq!(parse("/mcp"), SlashCommand::Mcp(String::new()));
    }

    #[test]
    fn parse_mcp_with_subcommand() {
        assert_eq!(parse("/mcp list"), SlashCommand::Mcp("list".into()));
        assert_eq!(parse("/mcp status"), SlashCommand::Mcp("status".into()));
    }

    // ---- 命令补全目录（filter_commands）----

    #[test]
    fn filter_empty_prefix_returns_all() {
        let all = filter_commands("/");
        assert_eq!(all.len(), COMMANDS.len(), "仅 `/` 应返回全部命令");
    }

    #[test]
    fn filter_specific_prefix() {
        let m = filter_commands("/mo");
        let names: Vec<&str> = m.iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["/mode", "/model"], "/mo 应匹配 mode 与 model");
    }

    #[test]
    fn filter_case_insensitive() {
        let m = filter_commands("/HELP");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "/help");
    }

    #[test]
    fn filter_no_match_returns_empty() {
        assert!(filter_commands("/zzz").is_empty());
    }

    #[test]
    fn filter_full_name_matches_single() {
        let m = filter_commands("/clear");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "/clear");
    }

    #[test]
    fn commands_catalog_covers_all_parsed_commands() {
        // 目录应覆盖每个可解析命令名
        for name in ["/help", "/clear", "/mode", "/model", "/provider", "/tools", "/skill", "/mcp", "/cancel", "/compact", "/max_steps", "/think", "/new", "/sessions", "/quit"] {
            assert!(
                COMMANDS.iter().any(|c| c.name == name),
                "COMMANDS 应包含 {name}"
            );
        }
    }

    #[test]
    fn param_suggestions_for_known_commands() {
        assert_eq!(param_suggestions("/think"), vec!["low", "middle", "high", "max", "auto"]);
        assert_eq!(param_suggestions("/ctf"), vec!["enable", "disable", "add", "list", "writeup"]);
        assert_eq!(param_suggestions("/mode"), vec!["chat", "workflow", "dashboard"]);
        assert_eq!(param_suggestions("/provider"), vec!["list", "add", "edit", "use", "remove"]);
        assert_eq!(param_suggestions("/sessions"), vec!["list", "read", "new"]);
        assert_eq!(param_suggestions("/mcp"), vec!["list", "status"]);
        assert_eq!(param_suggestions("/skill"), vec!["list"]);
    }

    #[test]
    fn param_suggestions_empty_for_paramless_commands() {
        assert!(param_suggestions("/help").is_empty());
        assert!(param_suggestions("/clear").is_empty());
        assert!(param_suggestions("/quit").is_empty());
        assert!(param_suggestions("/model").is_empty());
        assert!(param_suggestions("/max_steps").is_empty());
        assert!(param_suggestions("/compact").is_empty());
        assert!(param_suggestions("/unknown").is_empty());
    }

    #[test]
    fn parse_sessions_no_arg() {
        assert_eq!(parse("/sessions"), SlashCommand::Sessions(String::new()));
    }

    #[test]
    fn parse_sessions_with_subcommand() {
        assert_eq!(parse("/sessions list"), SlashCommand::Sessions("list".into()));
        assert_eq!(parse("/sessions read abc"), SlashCommand::Sessions("read abc".into()));
        assert_eq!(parse("/sessions new"), SlashCommand::Sessions("new".into()));
    }
}
