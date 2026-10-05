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
        usage: "/model [name]",
        desc: "打开模型面板（双栏浏览与切换核心模型；亦可带参数快速切换）",
    },
    CommandSpec {
        name: "/provider",
        usage: "/provider [sub]",
        desc: "打开服务商面板（支持浏览配置全部 Provider，添加、编辑与删除）",
    },
    CommandSpec {
        name: "/subagents",
        usage: "/subagents [status|enable|disable|stop [id|all]|max_tasks N|max_parallel N|timeout N|max_steps N]",
        desc: "查看、配置或终止批量子 agent",
    },
    CommandSpec {
        name: "/env",
        usage: "/env [list|add|edit <key>|set KEY VALUE|set-sensitive KEY VALUE|remove KEY]",
        desc: "管理注入工具子进程的环境变量",
    },
    CommandSpec {
        name: "/web",
        usage: "/web [status|on|off|enable|disable]",
        desc: "开启或禁用联网搜索与抓取功能（web_fetch）",
    },
    CommandSpec {
        name: "/image",
        usage: "/image <path> [prompt]",
        desc: "传入本地图片或 URL（自动转为 [image:1] 占位符并提交给视觉模型分析）",
    },
    CommandSpec {
        name: "/vision",
        usage: "/vision [status|on|off|provider <name>|model <name>|test [model]]",
        desc: "查看或配置自适应识图引擎（多模态能力探测与图生文降级）",
    },
    CommandSpec {
        name: "/paste",
        usage: "/paste",
        desc: "粘贴系统剪贴板中的图片并生成 [image:1] 占位符（Windows 快捷键：Alt+V）",
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
        usage: "/mcp [panel|list|status|connect]",
        desc: "打开全屏 MCP 管理面板（支持测活、配置与工具查看）",
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
        usage: "/memory [list|add <text>|project <text>|edit <scope> <index> <text>|delete <scope> <index>|rule]",
        desc: "用户记忆：list 查看 / add 追加全局 / project 追加项目级",
    },
    CommandSpec {
        name: "/todo",
        usage: "/todo [list|add <title>|done <id>|clear|close|open]",
        desc: "结构化任务清单：list 查看 / add 添加 / done 完成 / clear 清空 / close 收起 / open 展开",
    },
    CommandSpec {
        name: "/bg",
        usage: "/bg <run <prompt>|shell <cmd>|list|kill <id>|tail <id>>",
        desc: "后台任务：运行子代理/脚本、查看状态、终止",
    },
    CommandSpec {
        name: "/settings",
        usage: "/settings",
        desc: "打开设置中心面板",
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
    COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(p.as_str()))
        .collect()
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
        "/sessions" | "/session" => vec!["list", "read", "new"],
        "/subagents" => vec![
            "status",
            "enable",
            "disable",
            "stop",
            "max_tasks",
            "max_parallel",
            "timeout",
            "max_steps",
        ],
        "/env" => vec!["list", "add", "edit", "set", "set-sensitive", "remove"],
        "/web" => vec!["status", "on", "off", "enable", "disable"],
        "/vision" => vec!["status", "on", "off", "provider", "model", "test"],
        "/memory" => vec!["list", "add", "project", "edit", "delete", "rule"],
        "/mcp" => vec!["list", "status"],
        "/skill" => vec!["list"],
        "/todo" => vec!["list", "add", "done", "clear", "close", "open"],
        "/bg" => vec!["run", "shell", "list", "kill", "tail"],
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
    /// `/subagents [...]` — 查看或配置批量子 agent。
    Subagents(String),
    /// `/env [...]` — 管理工具子进程环境变量。
    Env(String),
    /// `/web [status|on|off|enable|disable]` — 开启/禁用联网搜索功能。
    Web(String),
    /// `/vision [subcommand]` — 自适应识图引擎配置与能力探针。
    Vision(String),
    /// `/tools` — 列出可用工具。
    Tools,
    /// `/skill <name|list>` — 查看 Skill 详细说明（list 列出全部）。
    /// 空串 = list；非空 = 注入指定 skill 的 body 为 System 条目。
    Skill(String),
    /// `/mcp [panel|list|status|connect]` — 打开全屏 MCP 管理面板（亦支持 list/status/connect）。
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
    /// `/image <path> [prompt]` — 传入本地图片或 URL 进行视觉多模态分析。
    Image(String),
    /// `/new` — 新建会话（保存当前 → 切到空会话）。
    New,
    /// `/sessions <list|read <id|关键词>|new>` — 会话管理。
    /// 空串 / list → 打开 session 面板；read → 跨会话读取；new → 同 `/new`。
    Sessions(String),
    /// `/memory [list|add <text>|project <text>|edit <scope> <index> <text>|delete <scope> <index>|rule]` — 用户记忆管理。
    /// 空串 / list → 查看记忆；add <text> → 追加全局；project <text> → 追加项目级。
    Memory(String),
    /// `/todo [list|add <title>|done <id>|clear]` — 结构化任务清单管理。
    Todo(String),
    /// `/bg <run <prompt>|shell <cmd>|list|kill <id>|tail <id>>` — 后台任务管理。
    Bg(String),
    /// `/settings` — 打开设置中心面板。
    Settings,
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
        "/model" | "/models" => SlashCommand::Model(args.to_string()),
        "/provider" | "/providers" => SlashCommand::Provider(args.to_string()),
        "/subagents" => SlashCommand::Subagents(args.to_string()),
        "/env" => SlashCommand::Env(args.to_string()),
        "/web" => SlashCommand::Web(args.to_string()),
        "/vision" => SlashCommand::Vision(args.to_string()),
        "/tools" => SlashCommand::Tools,
        "/skill" => SlashCommand::Skill(args.to_string()),
        "/mcp" => SlashCommand::Mcp(args.to_string()),
        "/cancel" => SlashCommand::Cancel,
        "/compact" => SlashCommand::Compact(args.to_string()),
        "/ctf" => SlashCommand::Ctf(args.to_string()),
        "/max_steps" => SlashCommand::MaxSteps(args.to_string()),
        "/think" => SlashCommand::Think(args.to_string()),
        "/new" => SlashCommand::New,
        "/sessions" | "/session" => SlashCommand::Sessions(args.to_string()),
        "/memory" => SlashCommand::Memory(args.to_string()),
        "/todo" => SlashCommand::Todo(args.to_string()),
        "/bg" => SlashCommand::Bg(args.to_string()),
        "/settings" => SlashCommand::Settings,
        "/quit" => SlashCommand::Quit,
        "/image" => SlashCommand::Image(args.to_string()),
        "/paste" => SlashCommand::Image("paste".into()),
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
  /subagents [sub]   子 agent：status | enable | disable | stop [id|all] | max_tasks N | max_parallel N | timeout N | max_steps N
  /env [sub]         环境变量：list | set KEY VALUE | set-sensitive KEY VALUE | remove KEY
  /web [status|on|off] 联网搜索：查看状态或开启/禁用 web_fetch 功能
  /tools             列出可用工具
  /skill <name|list> 查看 Skill 详细说明（list 列出全部）
  /mcp [sub]         打开全屏 MCP 管理面板（支持测活、配置与工具查看）
  /cancel            取消当前生成
  /compact [instr]   手动压缩上下文（可选自定义摘要指令）
  /max_steps <N>     查看或设置工具调用步数上限（1-1000）
  /image <path> [prompt] 视觉分析：传入图片或 URL 提交给视觉多模态模型
  /paste             粘贴剪贴板图片：生成 [image:1] 占位符（快捷键 Alt+V / Ctrl+V）
  /think [level]     查看或设置思考强度（low / middle / high / max / auto）
  /new               新建会话
  /sessions <sub>    会话管理：list（面板）| read <id|关键词>（跨读）| new
  /memory <sub>      记忆管理：list | add | project | edit | delete | rule
  /todo <sub>        任务管理：list | add <title> | done <id> | clear | close | open
  /bg <sub>          后台任务：run <prompt> | shell <cmd> | list | kill <id> | tail <id>
  /settings          打开设置中心面板
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
    fn parse_settings() {
        assert_eq!(parse("/settings"), SlashCommand::Settings);
        assert_eq!(parse("/settings  "), SlashCommand::Settings);
    }

    #[test]
    fn parse_case_insensitive() {
        assert_eq!(parse("/HELP"), SlashCommand::Help);
        assert_eq!(parse("/Clear"), SlashCommand::Clear);
        assert_eq!(parse("/QUIT"), SlashCommand::Quit);
    }

    #[test]
    fn parse_image() {
        assert_eq!(
            parse("/image ./test.png"),
            SlashCommand::Image("./test.png".into())
        );
        assert_eq!(
            parse("/image ./test.png 请分析"),
            SlashCommand::Image("./test.png 请分析".into())
        );
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
    fn parse_vision_command() {
        assert_eq!(parse("/vision"), SlashCommand::Vision(String::new()));
        assert_eq!(parse("/vision on"), SlashCommand::Vision("on".into()));
        assert_eq!(
            parse("/vision test gpt-4o"),
            SlashCommand::Vision("test gpt-4o".into())
        );
    }
    #[test]
    fn parse_model_no_arg() {
        assert_eq!(parse("/model"), SlashCommand::Model(String::new()));
        assert_eq!(parse("/models"), SlashCommand::Model(String::new()));
        assert_eq!(
            parse("/models gpt-4o"),
            SlashCommand::Model("gpt-4o".into())
        );
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
        assert_eq!(
            parse("/max_steps 100"),
            SlashCommand::MaxSteps("100".into())
        );
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
    fn parse_todo() {
        assert_eq!(parse("/todo"), SlashCommand::Todo(String::new()));
        assert_eq!(parse("/todo list"), SlashCommand::Todo("list".into()));
        assert_eq!(
            parse("/todo add Scan ports"),
            SlashCommand::Todo("add Scan ports".into())
        );
        assert_eq!(parse("/todo done 1"), SlashCommand::Todo("done 1".into()));
        assert_eq!(parse("/todo clear"), SlashCommand::Todo("clear".into()));
        assert_eq!(parse("/todo close"), SlashCommand::Todo("close".into()));
        assert_eq!(parse("/todo open"), SlashCommand::Todo("open".into()));
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
        for cmd in [
            "/help",
            "/clear",
            "/mode",
            "/model",
            "/provider",
            "/tools",
            "/skill",
            "/mcp",
            "/subagents",
            "/env",
            "/cancel",
            "/compact",
            "/max_steps",
            "/think",
            "/new",
            "/sessions",
            "/memory",
            "/quit",
        ] {
            assert!(HELP_TEXT.contains(cmd), "HELP_TEXT 应包含 {cmd}");
        }
    }

    #[test]
    fn parse_memory_no_arg() {
        assert_eq!(parse("/memory"), SlashCommand::Memory(String::new()));
    }

    #[test]
    fn parse_memory_with_arg() {
        assert_eq!(
            parse("/memory add 记住这个"),
            SlashCommand::Memory("add 记住这个".into())
        );
        assert_eq!(
            parse("/memory project 项目约定"),
            SlashCommand::Memory("project 项目约定".into())
        );
    }

    #[test]
    fn parse_provider_no_arg() {
        assert_eq!(parse("/provider"), SlashCommand::Provider(String::new()));
        assert_eq!(parse("/provider   "), SlashCommand::Provider(String::new()));
        assert_eq!(parse("/providers"), SlashCommand::Provider(String::new()));
        assert_eq!(
            parse("/providers list"),
            SlashCommand::Provider("list".into())
        );
    }
    #[test]
    fn parse_provider_subcommands() {
        assert_eq!(
            parse("/provider list"),
            SlashCommand::Provider("list".into())
        );
        assert_eq!(parse("/provider add"), SlashCommand::Provider("add".into()));
        assert_eq!(
            parse("/provider use openai"),
            SlashCommand::Provider("use openai".into())
        );
        assert_eq!(
            parse("/provider edit anthropic"),
            SlashCommand::Provider("edit anthropic".into())
        );
        assert_eq!(
            parse("/provider remove ollama"),
            SlashCommand::Provider("remove ollama".into())
        );
    }

    #[test]
    fn parse_provider_case_insensitive() {
        assert_eq!(parse("/PROVIDER"), SlashCommand::Provider(String::new()));
        assert_eq!(
            parse("/Provider List"),
            SlashCommand::Provider("List".into())
        );
    }

    #[test]
    fn parse_subagents_and_env_commands() {
        assert_eq!(
            parse("/subagents max_parallel 3"),
            SlashCommand::Subagents("max_parallel 3".into())
        );
        assert_eq!(
            parse("/ENV set-sensitive TOKEN secret value"),
            SlashCommand::Env("set-sensitive TOKEN secret value".into())
        );
        assert_eq!(parse("/web on"), SlashCommand::Web("on".into()));
        assert_eq!(parse("/WEB OFF"), SlashCommand::Web("OFF".into()));
    }

    #[test]
    fn parse_skill_no_arg_is_list() {
        assert_eq!(parse("/skill"), SlashCommand::Skill(String::new()));
        assert_eq!(parse("/skill   "), SlashCommand::Skill(String::new()));
    }

    #[test]
    fn parse_skill_with_name() {
        assert_eq!(
            parse("/skill src-recon"),
            SlashCommand::Skill("src-recon".into())
        );
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
        for name in [
            "/help",
            "/clear",
            "/mode",
            "/model",
            "/provider",
            "/tools",
            "/skill",
            "/mcp",
            "/cancel",
            "/compact",
            "/max_steps",
            "/think",
            "/new",
            "/sessions",
            "/quit",
        ] {
            assert!(
                COMMANDS.iter().any(|c| c.name == name),
                "COMMANDS 应包含 {name}"
            );
        }
    }

    #[test]
    fn param_suggestions_for_known_commands() {
        assert_eq!(
            param_suggestions("/think"),
            vec!["low", "middle", "high", "max", "auto"]
        );
        assert_eq!(
            param_suggestions("/ctf"),
            vec!["enable", "disable", "add", "list", "writeup"]
        );
        assert_eq!(
            param_suggestions("/mode"),
            vec!["chat", "workflow", "dashboard"]
        );
        assert_eq!(
            param_suggestions("/provider"),
            vec!["list", "add", "edit", "use", "remove"]
        );
        assert_eq!(param_suggestions("/sessions"), vec!["list", "read", "new"]);
        assert_eq!(param_suggestions("/mcp"), vec!["list", "status"]);
        assert_eq!(param_suggestions("/skill"), vec!["list"]);
        assert_eq!(
            param_suggestions("/subagents"),
            vec![
                "status",
                "enable",
                "disable",
                "stop",
                "max_tasks",
                "max_parallel",
                "timeout",
                "max_steps"
            ]
        );
        assert_eq!(
            param_suggestions("/env"),
            vec!["list", "add", "edit", "set", "set-sensitive", "remove"]
        );
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
        assert_eq!(parse("/session"), SlashCommand::Sessions(String::new()));
    }

    #[test]
    fn parse_sessions_with_subcommand() {
        assert_eq!(
            parse("/sessions list"),
            SlashCommand::Sessions("list".into())
        );
        assert_eq!(
            parse("/sessions read abc"),
            SlashCommand::Sessions("read abc".into())
        );
        assert_eq!(parse("/sessions new"), SlashCommand::Sessions("new".into()));
    }
}
