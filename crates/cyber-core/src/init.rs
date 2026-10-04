use std::io::Write;

use tracing::{debug, info, warn};

use crate::error::{CoreError, Result};
use crate::paths::Paths;

/// 默认配置文件内容（构建时嵌入）。
pub const DEFAULT_CONFIG_TOML: &str = include_str!("../assets/default_config.toml");
pub const DEFAULT_PROVIDERS_TOML: &str = include_str!("../assets/default_providers.toml");
pub const DEFAULT_MCP_SERVERS_TOML: &str = include_str!("../assets/default_mcp_servers.toml");

/// 补齐全局目录和缺失配置文件，不覆盖已有文件。
///
/// Initializers cooperate through a persistent OS-locked file; publication uses
/// same-directory rename, not hard links. The lock is released on process exit.
/// Windows requires a filesystem supporting private ACLs for credential safety.
///
/// 返回 `true` 表示执行了初始化（首次启动），`false` 表示已存在。
pub fn ensure_global_init(paths: &Paths) -> Result<bool> {
    debug!(cyber_home = %paths.cyber_home.display(), "检查 ~/.cyber 目录结构与默认配置");
    create_global_layout(paths)?;
    // Never unlink the lock file: replacing its inode would split concurrent lockers.
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock_path = paths.cyber_home.join(".init.lock");
    let lock = options.open(&lock_path)?;
    fs2::FileExt::lock_exclusive(&lock).map_err(|source| CoreError::Init {
        stage: "lock global initialization",
        path: lock_path.display().to_string(),
        source,
    })?;
    let first_run = !paths.config_file.try_exists()?;
    write_default_files(paths)?;
    if first_run {
        info!("~/.cyber 初始化完成");
    }
    Ok(first_run)
}

fn create_global_layout(paths: &Paths) -> Result<()> {
    for dir in [
        paths.cyber_home.as_path(),
        paths.cache_dir.as_path(),
        paths.skills_dir.as_path(),
        paths.tools_dir.as_path(),
        paths.mcp_dir.as_path(),
        paths.workflows_dir.as_path(),
        paths.sessions_dir.as_path(),
        paths.logs_dir.as_path(),
        paths.reports_dir.as_path(),
        paths.reports_templates_dir.as_path(),
        paths.history_dir.as_path(),
        paths.ctf_dir.as_path(),
        paths.ctf_writeup_dir.as_path(),
    ] {
        debug!(dir = %dir.display(), "创建目录");
        std::fs::create_dir_all(dir).map_err(|e| {
            warn!(dir = %dir.display(), error = %e, "目录创建失败（可能权限不足或路径无效）");
            CoreError::Init {
                stage: "create layout",
                path: dir.display().to_string(),
                source: e,
            }
        })?;
    }
    Ok(())
}

fn write_default_files(paths: &Paths) -> Result<()> {
    let items: [(&std::path::Path, &str); 3] = [
        (paths.config_file.as_path(), DEFAULT_CONFIG_TOML),
        (paths.providers_file.as_path(), DEFAULT_PROVIDERS_TOML),
        (paths.mcp_servers_file.as_path(), DEFAULT_MCP_SERVERS_TOML),
    ];
    for (path, content) in items {
        if path.try_exists()? {
            continue;
        }
        debug!(path = %path.display(), "写入默认配置文件");
        publish_default_file(path, content, restrict_private_permissions).map_err(|e| {
            warn!(path = %path.display(), error = %e, "写入默认配置文件失败（可能权限不足或磁盘满）");
            CoreError::Init {
                stage: "write default config",
                path: path.display().to_string(),
                source: e,
            }
        })?;
    }
    Ok(())
}

// Caller holds .init.lock throughout existence checks and publication.
fn publish_default_file(
    path: &std::path::Path,
    content: &str,
    restrict: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let tmp = path.with_extension(format!(
        "init-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // If create_new fails, this temp belongs to somebody else; do not remove it.
    let mut file = options.open(&tmp)?;
    let result = (|| -> std::io::Result<()> {
        restrict(&tmp)?;
        let backup = path.with_extension("toml.bak");
        let data = match std::fs::read(&backup) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => content.as_bytes().to_vec(),
            Err(e) => return Err(e),
        };
        file.write_all(&data)?;
        file.sync_all()?;
        drop(file);
        // Also preserve a file created by a non-initializer during preparation.
        if path.try_exists()? {
            return Ok(());
        }
        std::fs::rename(&tmp, path)
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

fn restrict_private_permissions(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        // The empty file may inherit public access. Restrict it before copying any backup bytes.
        let output = std::process::Command::new("icacls")
            .arg(path)
            .args(["/inheritance:r", "/grant:r", "*S-1-3-4:(F)"])
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Cannot restrict configuration ACLs; no data written. Use CYBER_HOME on a filesystem supporting private ACLs (such as NTFS).",
            ));
        }
    }
    #[cfg(not(windows))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;

    #[test]
    fn ensure_init_creates_layout_and_default_files() {
        let dir = std::env::temp_dir().join(format!(
            "cyber_init_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::at(dir.clone()).unwrap();

        let first = ensure_global_init(&paths).unwrap();
        assert!(first, "首次应执行初始化");
        assert!(paths.config_file.exists());
        assert!(paths.providers_file.exists());
        assert!(paths.mcp_servers_file.exists());
        assert!(paths.skills_dir.exists());
        assert!(paths.tools_dir.exists());
        assert!(paths.workflows_dir.exists());
        assert!(paths.logs_dir.exists());

        let second = ensure_global_init(&paths).unwrap();
        assert!(!second, "config.toml 已存在时不应重复初始化");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_paths(label: &str) -> Paths {
        Paths::at(std::env::temp_dir().join(format!(
            "cyber_init_{label}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )))
        .unwrap()
    }

    #[test]
    fn missing_config_does_not_overwrite_existing_providers_or_mcp() {
        let paths = test_paths("preserve");
        create_global_layout(&paths).unwrap();
        std::fs::write(&paths.providers_file, "custom provider with credentials").unwrap();
        std::fs::write(&paths.mcp_servers_file, "custom MCP").unwrap();
        assert!(ensure_global_init(&paths).unwrap());
        assert_eq!(
            std::fs::read_to_string(&paths.providers_file).unwrap(),
            "custom provider with credentials"
        );
        assert_eq!(
            std::fs::read_to_string(&paths.mcp_servers_file).unwrap(),
            "custom MCP"
        );
        std::fs::remove_dir_all(paths.cyber_home).unwrap();
    }

    #[test]
    fn existing_config_repairs_incomplete_layout_and_recovers_backup() {
        let paths = test_paths("recover");
        std::fs::create_dir_all(&paths.cyber_home).unwrap();
        std::fs::write(&paths.config_file, "custom config").unwrap();
        std::fs::write(
            paths.providers_file.with_extension("toml.bak"),
            "saved providers",
        )
        .unwrap();
        assert!(!ensure_global_init(&paths).unwrap());
        assert_eq!(
            std::fs::read_to_string(&paths.config_file).unwrap(),
            "custom config"
        );
        assert_eq!(
            std::fs::read_to_string(&paths.providers_file).unwrap(),
            "saved providers"
        );
        assert!(paths.mcp_servers_file.exists());
        assert!(paths.logs_dir.is_dir());
        #[cfg(windows)]
        {
            let output = std::process::Command::new("icacls")
                .arg(&paths.providers_file)
                .output()
                .unwrap();
            assert!(output.status.success());
            assert!(
                !String::from_utf8_lossy(&output.stdout).contains("(I)"),
                "Recovered credentials must not inherit public access"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&paths.providers_file)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(paths.cyber_home).unwrap();
    }

    #[test]
    fn permission_failure_does_not_copy_backup_data_or_publish_file() {
        let paths = test_paths("permission_failure");
        create_global_layout(&paths).unwrap();
        let backup = paths.providers_file.with_extension("toml.bak");
        std::fs::write(&backup, "private credential").unwrap();
        let error = publish_default_file(&paths.providers_file, DEFAULT_PROVIDERS_TOML, |temp| {
            assert_eq!(std::fs::metadata(temp)?.len(), 0);
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected ACL failure",
            ))
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!paths.providers_file.exists());
        assert_eq!(
            std::fs::read_to_string(backup).unwrap(),
            "private credential"
        );
        assert!(!std::fs::read_dir(&paths.cyber_home)
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".init-")));
        std::fs::remove_dir_all(paths.cyber_home).unwrap();
    }

    #[test]
    fn concurrent_initializers_publish_complete_files() {
        let paths = test_paths("concurrent");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let paths = paths.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    ensure_global_init(&paths).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&paths.config_file).unwrap(),
            DEFAULT_CONFIG_TOML
        );
        assert_eq!(
            std::fs::read_to_string(&paths.providers_file).unwrap(),
            DEFAULT_PROVIDERS_TOML
        );
        assert_eq!(
            std::fs::read_to_string(&paths.mcp_servers_file).unwrap(),
            DEFAULT_MCP_SERVERS_TOML
        );
        std::fs::remove_dir_all(paths.cyber_home).unwrap();
    }

    #[test]
    fn initialization_child_process() {
        let Some(home) = std::env::var_os("CYBER_INIT_CHILD_TEST_HOME") else {
            return;
        };
        let paths = Paths::at(home.into()).unwrap();
        ensure_global_init(&paths).unwrap();
    }

    #[test]
    fn independent_process_initializers_preserve_existing_files() {
        let paths = test_paths("processes");
        create_global_layout(&paths).unwrap();
        std::fs::write(&paths.providers_file, "custom provider").unwrap();
        let mut children: Vec<_> = (0..3)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "init::tests::initialization_child_process"])
                    .env("CYBER_INIT_CHILD_TEST_HOME", &paths.cyber_home)
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(
            std::fs::read_to_string(&paths.providers_file).unwrap(),
            "custom provider"
        );
        assert_eq!(
            std::fs::read_to_string(&paths.config_file).unwrap(),
            DEFAULT_CONFIG_TOML
        );
        assert_eq!(
            std::fs::read_to_string(&paths.mcp_servers_file).unwrap(),
            DEFAULT_MCP_SERVERS_TOML
        );
        // A crash leaves the file itself, but no stale directory/PID lock to block recovery.
        assert!(paths.cyber_home.join(".init.lock").exists());
        assert!(!ensure_global_init(&paths).unwrap());
        std::fs::remove_dir_all(paths.cyber_home).unwrap();
    }
}
