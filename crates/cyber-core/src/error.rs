use thiserror::Error;

pub type Result<T> = std::result::Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("读取文件失败 {path}: {source}")]
    FileRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("文件 {path} 不是有效的 UTF-8（请用 UTF-8 保存，避免记事本 Unicode/ANSI 模式）: {source}")]
    FileEncoding {
        path: String,
        #[source]
        source: std::string::FromUtf8Error,
    },

    #[error("初始化失败 [{stage}] {path}: {source}")]
    Init {
        stage: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("toml parse: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("toml serialize: {0}")]
    TomlSer(String),

    #[error("yaml parse: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("home directory not found")]
    NoHomeDir,

    #[error("config: {0}")]
    Config(String),
}
