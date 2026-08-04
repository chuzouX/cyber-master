//! `servers.toml` 解析：MCP server 注册表。
//!
//! 文件格式（`~/.cyber/mcp/servers.toml`）：
//! ```toml
//! [[servers]]
//! name = "filesystem"
//! transport = "stdio"        # stdio | sse | http
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
//! env = { FOO = "bar" }
//! timeout_secs = 5
//!
//! [[servers]]
//! name = "remote"
//! transport = "http"
//! url = "https://scanner.internal/mcp"
//! headers = { Authorization = "Bearer ${MCP_TOKEN}" }
//! ```

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::transport::McpTransport;

/// `servers.toml` 顶层结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpServersConfig {
    #[serde(default)]
    pub servers: Vec<McpServerSpec>,
}

/// 单个 MCP server 配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerSpec {
    /// server 唯一名（用于工具命名前缀 `mcp_<name>_<tool>` 与 `/mcp` 展示）。
    pub name: String,
    pub transport: McpTransport,
    /// stdio: 可执行文件名（如 `npx` / `node`）。
    #[serde(default)]
    pub command: Option<String>,
    /// stdio: 命令行参数。
    #[serde(default)]
    pub args: Vec<String>,
    /// stdio: 子进程环境变量（在当前 env 基础上追加）。
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// http/sse: server URL。
    #[serde(default)]
    pub url: Option<String>,
    /// http/sse: 请求头。
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// 连接/握手超时秒数（默认 5）。
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    5
}

impl McpServerSpec {
    /// 默认超时秒数（serde default 用）。
    pub const DEFAULT_TIMEOUT: u64 = 5;

    /// 解析单个 server 配置后补全 timeout_secs（toml 缺省时）。
    pub fn normalized_timeout(&self) -> u64 {
        if self.timeout_secs == 0 {
            Self::DEFAULT_TIMEOUT
        } else {
            self.timeout_secs
        }
    }
}

impl McpServersConfig {
    /// 从 `servers.toml` 文件加载。文件不存在 → 空 config（无 server，不报错）。
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = cyber_core::fsutil::read_utf8(path)?;
        let cfg: Self = toml::from_str(&raw)?;
        Ok(cfg)
    }

    /// 保存到 `servers.toml`（原子写 + `.bak` 备份，与 `save_config` 同模式）。
    pub fn save(&self, path: &Path) -> Result<()> {
        let toml_str = toml::to_string_pretty(self)
            .map_err(|e| crate::error::McpError::Config(e.to_string()))?;
        cyber_core::atomic_write(path, toml_str.as_bytes())?;
        Ok(())
    }

    /// 新增或覆盖 server（按 name 去重，同名替换）。
    pub fn upsert(&mut self, spec: McpServerSpec) {
        if let Some(existing) = self.servers.iter_mut().find(|s| s.name == spec.name) {
            *existing = spec;
        } else {
            self.servers.push(spec);
        }
    }

    /// 按 name 删除，返回是否删除成功。
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.servers.len();
        self.servers.retain(|s| s.name != name);
        self.servers.len() < before
    }

    /// 按 name 查找。
    pub fn find(&self, name: &str) -> Option<&McpServerSpec> {
        self.servers.iter().find(|s| s.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stdio_server() {
        let toml_src = r#"
[[servers]]
name = "filesystem"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
timeout_secs = 10
"#;
        let cfg: McpServersConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.servers.len(), 1);
        let s = &cfg.servers[0];
        assert_eq!(s.name, "filesystem");
        assert_eq!(s.transport, McpTransport::Stdio);
        assert_eq!(s.command.as_deref(), Some("npx"));
        assert_eq!(s.args.len(), 3);
        assert_eq!(s.timeout_secs, 10);
    }

    #[test]
    fn parse_http_server_with_headers() {
        let toml_src = r#"
[[servers]]
name = "remote"
transport = "http"
url = "https://scanner.internal/mcp"
[servers.headers]
Authorization = "Bearer ${MCP_TOKEN}"
"#;
        let cfg: McpServersConfig = toml::from_str(toml_src).unwrap();
        let s = &cfg.servers[0];
        assert_eq!(s.transport, McpTransport::Http);
        assert_eq!(s.url.as_deref(), Some("https://scanner.internal/mcp"));
        assert_eq!(s.headers.get("Authorization").unwrap(), "Bearer ${MCP_TOKEN}");
    }

    #[test]
    fn parse_env_map() {
        let toml_src = r#"
[[servers]]
name = "x"
transport = "stdio"
command = "node"
[servers.env]
DEBUG = "1"
"#;
        let cfg: McpServersConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.servers[0].env.get("DEBUG").unwrap(), "1");
    }

    #[test]
    fn default_timeout_when_missing() {
        let toml_src = r#"
[[servers]]
name = "x"
transport = "stdio"
command = "node"
"#;
        let cfg: McpServersConfig = toml::from_str(toml_src).unwrap();
        let s = &cfg.servers[0];
        assert_eq!(s.timeout_secs, 5, "缺省 timeout 应为 5");
        assert_eq!(s.normalized_timeout(), 5);
    }

    #[test]
    fn zero_timeout_falls_back_to_default() {
        let s = McpServerSpec {
            name: "x".into(),
            transport: McpTransport::Stdio,
            command: Some("n".into()),
            args: vec![],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            timeout_secs: 0,
        };
        assert_eq!(s.normalized_timeout(), 5);
    }

    #[test]
    fn empty_config_when_no_servers() {
        let cfg: McpServersConfig = toml::from_str("").unwrap();
        assert!(cfg.servers.is_empty());
    }

    #[test]
    fn load_nonexistent_file_returns_empty() {
        let p = std::path::PathBuf::from("/nonexistent/cyber_mcp_test_servers.toml");
        let cfg = McpServersConfig::load(&p).unwrap();
        assert!(cfg.servers.is_empty());
    }

    #[test]
    fn load_from_temp_file() {
        let dir = std::env::temp_dir().join(format!(
            "cyber_mcp_cfg_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("servers.toml");
        std::fs::write(
            &path,
            r#"[[servers]]
name = "fs"
transport = "stdio"
command = "npx"
args = ["server"]
"#,
        )
        .unwrap();
        let cfg = McpServersConfig::load(&path).unwrap();
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].name, "fs");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
