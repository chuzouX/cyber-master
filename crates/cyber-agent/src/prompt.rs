//! 系统提示词组装：base + 项目 frontmatter + 安全护栏。

use cyber_core::{ProjectContext, ThinkingIntensity};

/// Skill 摘要：名称 + 一行描述，用于注入系统提示词的 skill 索引段落。
#[derive(Debug, Clone)]
pub struct SkillSummary {
    /// Skill 名称（不含 `skill_` 前缀）。
    pub name: String,
    /// 一行简介（frontmatter.description）。
    pub description: String,
}

/// 根据思考强度返回对应的"工作方式"段落。
fn thinking_section(intensity: ThinkingIntensity) -> &'static str {
    match intensity {
        ThinkingIntensity::Low => "# 工作方式\n\
- 直接执行，不要输出思考过程。先行动后解释。\n\
- 遇到不确定的问题时，调用工具验证比纯推理更高效。\n\
- 不要一次性规划所有步骤：先收集信息，再根据结果决定下一步。",
        ThinkingIntensity::Middle => "# 工作方式\n\
- 边做边想，想一步做一步：每次思考控制在 3-5 行以内，然后立即调用工具或给出结论。\n\
- 先行动后解释：遇到不确定的问题时，调用工具验证比纯推理更高效。\n\
- 思考是为了决定下一步动作，不是为了列举所有可能性。如果思考超过 5 行仍未产生明确的工具调用计划，说明你在过度推理，应立即停下并调用最相关的工具。\n\
- 不要一次性规划所有步骤：先收集信息，再根据结果决定下一步。",
        ThinkingIntensity::High => "# 工作方式\n\
- 允许 10-15 行的深入思考，分析问题根因后再行动。\n\
- 思考应包含：问题分析 → 可能的方案 → 最优选择 → 执行计划。\n\
- 遇到复杂问题时，先充分分析再调用工具，避免盲目试错。\n\
- 思考结束后必须立即行动（调用工具或给出结论），不要只思考不行动。",
        ThinkingIntensity::Max => "# 工作方式\n\
- 充分思考，无行数限制。复杂问题应深入分析所有可能性后再行动。\n\
- 思考应包含：问题根因分析 → 方案对比 → 风险评估 → 最优选择 → 详细执行计划。\n\
- 简单问题也允许简短思考，但不强制。\n\
- 思考结束后必须立即行动（调用工具或给出结论）。",
        ThinkingIntensity::Auto => unreachable!("Auto 应在调用前被 resolve"),
    }
}

/// 基础系统提示词（静态部分，不含"工作方式"段落——该部分由 thinking_section 动态注入）。
pub const BASE_PROMPT_STATIC: &str = "你是 Cyber Master，一个网络安全智能体终端助手。\
你遵循用户 .cyber.md 中声明的授权范围与安全护栏。\n\
协助授权范围内的安全测试、CTF 竞赛、防御性安全和教学场景。\
拒绝未授权的破坏性操作（删库、DoS、未授权入侵、供应链攻击）。\n\n\
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
# Skill 使用（重要）\n\
- Skill 是经过实战验证的方法论和操作手册。遇到安全测试、CTF 解题、漏洞利用等任务时，**先调用相关 skill 工具获取方法论**，再执行操作。\n\
- Skill 工具命名为 `skill_<name>`，调用后返回详细使用说明（渐进式披露）。调用成本极低（无参数），但能避免大量试错。\n\
- 下方「可用 Skill」段落列出了所有 skill 的名称和简介。开始任务前扫描该列表，匹配到相关 skill 时**必须先调用**。\n\
- 不要跳过 skill 直接用 curl/Python 操作——skill 中包含的关键步骤、检查点和常见坑能节省大量时间。\n\
- 调用 skill 后按其指引执行；skill 引用的 .md 资源文件可用 read_file 读取获取更多细节。\n\n\
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

/// 运行时环境信息段落：注入 OS 类型和 shell 语法提示，避免 agent 盲猜平台。
///
/// agent 在不知道平台时会先试 Unix 命令（pwd/ls）失败后再试 Windows（cmd /C），
/// 浪费多轮工具调用。提前告知平台可消除试错。
fn env_info_section() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let (platform, shell_hint) = if cfg!(target_os = "windows") {
        (
            "Windows",
            "shell 工具用 `cmd /C` 执行命令。路径用反斜杠 `\\`（如 `C:\\Users\\...`）。命令示例：`dir`、`type file.txt`、`cd /d C:\\path`。",
        )
    } else if cfg!(target_os = "macos") {
        (
            "macOS",
            "shell 工具用 `sh -c` 执行命令。路径用正斜杠 `/`。命令示例：`ls`、`cat file.txt`、`pwd`。",
        )
    } else {
        (
            "Linux/Unix",
            "shell 工具用 `sh -c` 执行命令。路径用正斜杠 `/`。命令示例：`ls`、`cat file.txt`、`pwd`。",
        )
    };
    format!(
        "# 运行环境\n\
- 平台：{platform}（{os}/{arch}）\n\
- {shell_hint}"
    )
}

/// CTF 模式附加系统提示词。
///
/// 指示 agent 使用 `ctf_challenge` 工具自动注册/更新题目状态，并规范测试方法论优先级。
pub const CTF_PROMPT: &str = "\n\n# CTF 模式\n\
当前已开启 CTF 竞赛模式。请使用 `ctf_challenge` 工具管理题目：\n\
- 分析题目时调用 `ctf_challenge`（action=register）注册题目名称、分类、描述、靶机地址和标签\n\
- 解出题目（获得 flag）时调用 `ctf_challenge`（action=solve）标记题目已解出并记录 flag 和关键知识点\n\
- 可随时调用 `ctf_challenge`（action=list）查看所有题目状态\n\
题目状态会实时显示在 TUI 题目面板中。\n\n\
## 重要提醒（必须遵守）\n\
- **收到题目信息后，第一件事就是调用 `ctf_challenge`（action=register）注册题目**，不要等到解题中途或解出后再注册。\n\
- 未注册的题目不会被记录到题目面板，容易遗忘或混淆。\n\
- 每次获取到新的题目信息（名称、靶机地址、描述等）都应立即 register 更新，防止信息丢失。\n\
- 解题过程中如发现题目信息有变（如补充描述、更换靶机），也要及时 register 更新。\n\n\
## 测试优先级（必须遵守）\n\
CTF 解题按以下优先级推进，**严禁跳级**：\n\
1. **信息收集**：先从题目描述、靶机响应、页面源码、HTTP 头、注释、robots.txt 等提取线索。每个线索都可能直接指向漏洞点。\n\
2. **Skill 知识库**：根据线索匹配调用对应 `skill_<name>` 工具获取方法论。skill 中包含该类漏洞的检查清单和利用路径，按其指引执行。\n\
3. **工具测试**：基于前两步的线索和 skill 指引，用已有工具进行针对性测试。\n\
4. **脚本/爆破**：仅当前三步均未突破时才考虑。且必须基于已有线索缩小范围，不做盲目爆破。\n\n\
**禁止的行为：**\n\
- 在信息收集不充分时直接启动爆破/fuzz（如未查看页面源码就跑 dirsearch）\n\
- 跳过 skill 知识库直接写脚本测试\n\
- 用自写脚本替代已有工具——已有工具更成熟、字典更全、效率更高\n\n\
## 工具使用规范\n\
- **目录扫描**用 `shell` 运行 `dirsearch`（已安装），不要自写 Python 脚本扫目录。命令示例：`dirsearch -u <url> -x 404 --exclude-sizes=0B`\n\
- **端口扫描**用 `shell` 运行 `nmap`，不要自写脚本。\n\
- **HTTP 请求**优先用 `web_fetch` 或 `shell` 运行 `curl`，不要自写脚本发请求。\n\
- 仅当已有工具无法满足特定需求时才写脚本（如需要特定协议交互、链式利用、自定义 payload 生成）。";

/// 组装系统提示词：thinking_section + base + 项目上下文 + rules 护栏段 + skill 索引。
///
/// `intensity` 应为已 resolve 的值（非 Auto）。`body`（.cyber.md 正文）暂不注入。
/// `skills` 为 `(name, description)` 列表，非空时追加「可用 Skill」段落到提示词末尾。
pub fn build_system_prompt(
    project: Option<&ProjectContext>,
    intensity: ThinkingIntensity,
    skills: &[SkillSummary],
) -> String {
    let mut s = String::new();
    s.push_str(thinking_section(intensity));
    s.push_str("\n\n");
    s.push_str(BASE_PROMPT_STATIC);
    s.push_str("\n\n");
    s.push_str(&env_info_section());
    // Skill 索引：非空时追加，让 agent 一眼看到有哪些 skill 可用
    if !skills.is_empty() {
        s.push_str("\n\n# 可用 Skill\n");
        s.push_str("开始任务前扫描此列表，匹配到相关 skill 时先调用 `skill_<name>` 获取方法论：\n");
        for sk in skills {
            s.push_str(&format!("- skill_{}: {}\n", sk.name, sk.description));
        }
    }
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
    use cyber_core::{ProjectContext, ProjectFrontmatter, ThinkingIntensity};

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
        let s = build_system_prompt(None, ThinkingIntensity::Middle, &[]);
        assert!(s.contains("Cyber Master"));
        assert!(!s.contains("项目上下文"));
    }

    #[test]
    fn env_info_section_is_injected() {
        let s = build_system_prompt(None, ThinkingIntensity::Middle, &[]);
        assert!(s.contains("# 运行环境"), "应包含运行环境段落");
        assert!(s.contains("平台："), "应包含平台信息");
        if cfg!(target_os = "windows") {
            assert!(s.contains("Windows"), "Windows 上应标注 Windows");
            assert!(s.contains("cmd /C"), "Windows 上应提示 cmd /C");
        } else {
            assert!(s.contains("sh -c"), "非 Windows 应提示 sh -c");
        }
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
        let s = build_system_prompt(Some(&ctx(fm)), ThinkingIntensity::Middle, &[]);
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
        let s = build_system_prompt(Some(&ctx(ProjectFrontmatter::default())), ThinkingIntensity::Middle, &[]);
        assert!(s.contains("frontmatter 无结构化字段"));
        // rules 段仅在 frontmatter.rules 非空时追加
        assert!(!s.contains("# 安全护栏（必须遵守）"));
    }

    #[test]
    fn skill_index_injected_when_non_empty() {
        let skills = vec![
            SkillSummary { name: "hack".into(), description: "黑客攻击总入口".into() },
            SkillSummary { name: "sqli".into(), description: "SQL 注入攻击".into() },
        ];
        let s = build_system_prompt(None, ThinkingIntensity::Middle, &skills);
        assert!(s.contains("# 可用 Skill"), "应包含 skill 索引段落");
        assert!(s.contains("skill_hack"), "应列出 skill_hack");
        assert!(s.contains("黑客攻击总入口"), "应含 skill 描述");
        assert!(s.contains("skill_sqli"), "应列出 skill_sqli");
        assert!(s.contains("SQL 注入攻击"));
    }

    #[test]
    fn skill_index_omitted_when_empty() {
        let s = build_system_prompt(None, ThinkingIntensity::Middle, &[]);
        assert!(!s.contains("# 可用 Skill"), "空 skill 列表不应生成索引段落");
    }
}
