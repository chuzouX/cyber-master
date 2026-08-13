//! cyber-core: 配置、路径、错误、项目上下文。
//!
//! P1 范围：配置层加载（全局 `~/.cyber` + 项目级 `.cyber/` + `.cyber.md`）。

pub mod config;
pub mod ctf;
pub mod error;
pub mod fsutil;
pub mod init;
pub mod loader;
pub mod memory;
pub mod paths;
pub mod project;
pub mod providers;

pub use config::{Config, EnvConfig, EnvVar, ThinkingIntensity};
pub use ctf::{current_time_str, CtfCategory, CtfChallenge, CtfStatus};
pub use error::{CoreError, Result};
pub use loader::{atomic_write, load_app_context, save_config, save_providers, AppContext};
pub use memory::{MemoryScope, MemoryStore};
pub use paths::Paths;
pub use project::{ProjectContext, ProjectFrontmatter};
pub use providers::{resolve_api_key, ModelConfig, PriceConfig, ProviderConfig, ProvidersConfig, PROVIDER_KINDS};
