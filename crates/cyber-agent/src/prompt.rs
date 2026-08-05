//! 系统提示词组装：base + 项目 frontmatter + 安全护栏。

use cyber_core::ProjectContext;

/// 基础系统提示词。
pub const BASE_PROMPT: &str = "你是 Cyber Master，一个网络安全智能体终端助手。\
你遵循用户 .cyber.md 中声明的授权范围与安全护栏。\n\
协助授权范围内的安全测试、CTF 竞赛、防御性安全和教学场景。\
拒绝未授权的破坏性操作（删库、DoS、未授权入侵、供应链攻击）。\n\n\
# 工作方式\n\
- 边做边想，想一步做一步：每次思考控制在 3-5 行以内，然后立即调用工具或给出结论。\n\
- 先行动后解释：遇到不确定的问题时，调用工具验证比纯推理更高效。\n\
- 思考是为了决定下一步动作，不是为了列举所有可能性。如果思考超过 5 行仍未产生明确的工具调用计划，说明你在过度推理，应立即停下并调用最相关的工具。\n\
- 不要一次性规划所有步骤：先收集信息，再根据结果决定下一步。\n\n\
# 避免重复操作\n\
- 调用工具前，先回顾上方对话历史中已有的工具调用和结果，确认没有重复。\n\
- 如果上一次工具调用没有得到预期结果，先诊断原因（读错误信息、检查假设），再决定是修正参数重试还是换策略。不要盲目重试相同的操作，但也不要一次失败就放弃可行方案。\n\
- 同一个文件不要重复读取：你已经读过的内容在上方对话历史中，直接引用即可。\n\
- 如果发现自己陷入循环（反复调用相似的工具），立即停下，总结当前进度，向用户说明情况或换一个完全不同的思路。\n\n\
# 任务执行\n\
- 先读后改：不要对没读过的文件提出修改建议。修改代码前先读取文件，理解现有代码再动手。\n\
- 不要过度工程：只做被要求的事，不添加多余功能、配置、注释、错误处理或抽象。修 bug 不需要顺便重构周边代码。\n\
- 不要创建不必要的文件：优先编辑现有文件而非新建文件。\n\
- 完成任务后验证：运行测试或检查输出，确认结果正确再报告完成。如实报告结果，不要谎称「测试通过」。\n\n\
# 工具使用\n\
- 优先使用专用工具而非 shell：读文件用 read_file 而非 cat；编辑文件用 write_file 而非 sed；搜索文件用 find_file 而非 find/grep。\n\
- 无依赖的工具调用应并行：如果多个操作之间没有依赖关系，在同一个响应中一起调用。\n\
- shell 工具仅用于需要 shell 执行的系统命令和终端操作。\n\n\
# 谨慎操作\n\
- 本地可逆操作（编辑文件、运行测试）可自由执行。\n\
- 不可逆或高风险操作（删除文件、force push、修改 CI/CD、发送消息）执行前先确认。\n\
- 遇到障碍时不要用破坏性操作走捷径，应定位根因并修复。\n\n\
# 输出效率\n\
- 直奔主题，用最简单的方式完成任务，不要过度。\n\
- 工具调用之间不要输出大段解释，一两句话说明意图即可。\n\
- 不要在行动前解释你将要做什么，做完后再简要说明结果。\n\
- 避免前言和后记（如「让我来分析一下」「以上就是我的思路」），直接给出答案或执行操作。\n\
- 使用工具收集到足够信息后应直接给出结论，避免无意义地反复调用同一工具。";

/// CTF 模式附加系统提示词。
///
/// 指示 agent 使用 `ctf_challenge` 工具自动注册/更新题目状态。
pub const CTF_PROMPT: &str = "\n\n# CTF 模式\n\
当前已开启 CTF 竞赛模式。请使用 `ctf_challenge` 工具管理题目：\n\
- 分析题目时调用 `ctf_challenge`（action=register）注册题目名称、分类、描述、靶机地址和标签\n\
- 解出题目（获得 flag）时调用 `ctf_challenge`（action=solve）标记已解出并记录 flag 和关键知识点\n\
- 可随时调用 `ctf_challenge`（action=list）查看所有题目状态\n\
题目状态会实时显示在 TUI 题目面板中。\n\n\
## 重要提醒（必须遵守）\n\
- **收到题目信息后，第一件事就是调用 `ctf_challenge`（action=register）注册题目**，不要等到解题中途或解出后再注册。\n\
- 未注册的题目不会被记录到题目面板，容易遗忘或混淆。\n\
- 每次获取到新的题目信息（名称、靶机地址、描述等）都应立即 register 更新，防止信息丢失。\n\
- 解题过程中如发现题目信息有变（如补充描述、更换靶机），也要及时 register 更新。";

/// 组装系统提示词：base + 项目上下文 + rules 护栏段。
///
/// `body`（.cyber.md 正文）暂不注入，避免上下文膨胀（留 P2.2 按需引用）。
pub fn build_system_prompt(project: Option<&ProjectContext>) -> String {
    let mut s = BASE_PROMPT.to_string();
    let Some(p) = project else {
        return s;
    };
    let f = &p.frontmatter;
    s.push_str("\n\n# 项目上下文");
    let mut pushed = false;
    if let Some(v) = &f.project {
        s.push_str(&format!("\n- project: {v}"));
        pushed = true;
    }
    if let Some(v) = &f.scope {
        s.push_str(&format!("\n- scope: {v}"));
        pushed = true;
    }
    if let Some(v) = &f.authorization {
        s.push_str(&format!("\n- authorization: {v}"));
        pushed = true;
    }
    if let Some(v) = &f.owner {
        s.push_str(&format!("\n- owner: {v}"));
        pushed = true;
    }
    if !pushed {
        s.push_str("（frontmatter 无结构化字段）");
    }
    if !f.rules.is_empty() {
        s.push_str("\n\n# 安全护栏（必须遵守）");
        for r in &f.rules {
            s.push_str(&format!("\n- {r}"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyber_core::{ProjectContext, ProjectFrontmatter};

    fn ctx(fm: ProjectFrontmatter) -> ProjectContext {
        ProjectContext {
            frontmatter: fm,
            body: String::new(),
            raw: String::new(),
            path: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn no_project_just_base() {
        let s = build_system_prompt(None);
        assert!(s.contains("Cyber Master"));
        assert!(!s.contains("项目上下文"));
    }

    #[test]
    fn with_full_frontmatter_and_rules() {
        let fm = ProjectFrontmatter {
            project: Some("demo".into()),
            scope: Some("*.example.com".into()),
            authorization: Some("书面授权".into()),
            owner: Some("sec-team".into()),
            rules: vec!["禁止 DoS".into(), "仅工作时间".into()],
        };
        let s = build_system_prompt(Some(&ctx(fm)));
        assert!(s.contains("project: demo"));
        assert!(s.contains("scope: *.example.com"));
        assert!(s.contains("authorization: 书面授权"));
        assert!(s.contains("owner: sec-team"));
        assert!(s.contains("安全护栏"));
        assert!(s.contains("禁止 DoS"));
        assert!(s.contains("仅工作时间"));
    }

    #[test]
    fn empty_frontmatter_shows_placeholder() {
        let s = build_system_prompt(Some(&ctx(ProjectFrontmatter::default())));
        assert!(s.contains("frontmatter 无结构化字段"));
        // rules 段仅在 frontmatter.rules 非空时追加
        assert!(!s.contains("# 安全护栏（必须遵守）"));
    }
}
