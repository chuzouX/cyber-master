//! cyber-core: 配置、路径、错误、项目上下文。
//!
//! P1 范围：配置层加载（全局 `~/.cyber` + 项目级 `.cyber/` + `.cyber.md`）。

pub mod config;
pub mod ctf;
pub mod custom_tool;
pub mod error;
pub mod fsutil;
pub mod init;
pub mod loader;
pub mod memory;
pub mod paths;
pub mod project;
pub mod providers;
pub mod todo;
pub mod update;

pub use config::{
    Config, EnvConfig, EnvVar, MemoryConfig, MemoryRule, ThinkingIntensity, VisionConfig,
};
pub use ctf::{current_time_str, CtfCategory, CtfChallenge, CtfStatus};
pub use custom_tool::{
    load_custom_tools, save_custom_tool, CustomToolConfig, CustomToolParam, LoadedCustomTool,
};
pub use error::{CoreError, Result};
pub use loader::{
    atomic_write, load_app_context, load_providers, save_config, save_providers, AppContext,
};
pub use memory::{MemoryEntry, MemoryScope, MemoryStore};
pub use paths::Paths;
pub use project::{ProjectContext, ProjectFrontmatter};
pub use providers::{
    get_model_vision_capability, is_deepseek_provider, is_deepseek_vision_model, resolve_api_key,
    save_model_vision_capability, CapabilityStore, ModelConfig, PriceConfig, ProviderConfig,
    ProviderPreset, ProvidersConfig, VisionCapability, CONTEXT_LENGTH_PRESETS, DEFAULT_MAX_TOKENS,
    DEFAULT_OUTPUT_TOKEN_CAP, PROVIDER_KINDS, PROVIDER_PRESETS,
};
pub use todo::{TodoItem, TodoStatus};
pub use update::{
    check_for_updates, is_newer, ReleaseInfo, CNB_RELEASES_URL, CNB_REPO, GITHUB_REPO,
};
