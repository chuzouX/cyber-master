//! 斜杠命令解析（Chat 输入态）。
//!
//! 用户在输入框输入以 `/` 开头的文本并 Enter 时，App 拦截为斜杠命令（不发送给
//! agent）。支持：`/help` `/clear` `/mode` `/model` `/tools` `/cancel` `/quit`。
//!
//! 命令名大小写不敏感（`/HELP` 与 `/help` 等价）；参数保留原样。未知命令返回
//! `Unknown`，由 App 层展示提示。

/// 一个已解析的斜杠命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// `/help` — 显示命令帮助。
    Help,
    /// `/clear` — 清空对话历史。
    Clear,
    /// `/mode <name>` — 切换模式（空串表示缺参数）。
    Mode(String),
    /// `/model <provider>` — 切换默认 provider（`/provider use` 的 alias，向后兼容）。
    Model(String),
    /// `/provider <subcommand>` — 管理服务商（list / add / edit / use / remove）。
    /// 空串 = list；子命令参数保留原样由 App 层解析。
    Provider(String),
    /// `/tools` — 列出可用工具。
    Tools,
    /// `/cancel` — 取消当前生成。
    Cancel,
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
        "/cancel" => SlashCommand::Cancel,
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
  /model <provider>  切换默认 provider（/provider use 的 alias）
  /provider <sub>    管理服务商：list | add | edit <name> | use <name> | remove <name>
  /tools             列出可用工具
  /cancel            取消当前生成
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
        for cmd in ["/help", "/clear", "/mode", "/model", "/provider", "/tools", "/cancel", "/quit"] {
            assert!(HELP_TEXT.contains(cmd), "HELP_TEXT 应包含 {cmd}");
        }
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
}
