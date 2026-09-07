use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("{0}")]
    Compatibility(String),
    #[error("cannot determine the current user's home directory")]
    MissingHome,
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    ParseJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize JSON for {path}: {source}")]
    SerializeJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid model catalog: {0}")]
    InvalidCatalog(String),
    #[error("unknown model ID: {0}")]
    UnknownModel(String),
    #[error("invalid request at {path}: {message}")]
    InvalidRequest { path: String, message: String },
    #[error(
        "prompt too long: estimated {estimated_tokens} input tokens exceeds the usable model limit of {limit_tokens}"
    )]
    PromptTooLong {
        estimated_tokens: u64,
        limit_tokens: u64,
    },
    #[error("invalid Responses stream: {0}")]
    InvalidStream(String),
    #[error("authentication error: {0}")]
    Authentication(String),
    #[error("Codex transport error: {0}")]
    CodexTransport(String),
    #[error("invalid command: {0}")]
    InvalidCommand(String),
    #[error("invalid JSONC configuration: {0}")]
    InvalidConfig(String),
    #[error("configuration conflict: {0}")]
    Conflict(String),
    #[error("failed to run {program}: {source}")]
    Process {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, BridgeError>;
