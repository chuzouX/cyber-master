//! 文件系统读取辅助：区分 IO 错误与编码错误，并附带文件路径上下文。

use std::path::Path;

use tracing::warn;

use crate::error::{CoreError, Result};

/// 读取文件并以 UTF-8 解码。
///
/// 相比 `std::fs::read_to_string`，本函数区分：
/// - IO 错误（权限/不存在）→ `CoreError::FileRead`，附带路径
/// - 编码错误（非 UTF-8，如记事本 Unicode/ANSI 保存）→ `CoreError::FileEncoding`，附带路径与修复提示
pub fn read_utf8(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|e| {
        warn!(path = %path.display(), error = %e, "文件读取失败（检查权限或路径是否存在）");
        CoreError::FileRead {
            path: path.display().to_string(),
            source: e,
        }
    })?;
    String::from_utf8(bytes).map_err(|e| {
        warn!(
            path = %path.display(),
            error = %e,
            "文件非 UTF-8 编码（记事本 Unicode/ANSI 模式会导致此问题，请用 UTF-8 保存）"
        );
        CoreError::FileEncoding {
            path: path.display().to_string(),
            source: e,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "cyber_fsutil_{}_{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn read_utf8_reads_valid_utf8() {
        let path = tmp_path("ok");
        std::fs::write(&path, "hello 你好").unwrap();
        let s = read_utf8(&path).unwrap();
        assert_eq!(s, "hello 你好");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_utf8_returns_encoding_error_with_path_for_non_utf8() {
        let path = tmp_path("bad");
        // UTF-16 LE BOM + '[' ']' —— 非合法 UTF-8（模拟记事本 Unicode 保存）
        std::fs::write(&path, [0xFF, 0xFE, b'[', 0x00, b']', 0x00]).unwrap();
        match read_utf8(&path) {
            Err(CoreError::FileEncoding { path: p, .. }) => {
                assert!(p.contains("cyber_fsutil_bad"), "错误应附带文件路径: {p}");
            }
            other => panic!("应为 FileEncoding，实际: {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }
}
