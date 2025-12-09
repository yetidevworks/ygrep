use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum YgrepError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Index error: {0}")]
    Index(#[from] tantivy::TantivyError),

    #[error("Query parse error: {0}")]
    QueryParse(#[from] tantivy::query::QueryParserError),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Workspace not found: {0}")]
    WorkspaceNotFound(PathBuf),

    #[error("Workspace not indexed: {0}")]
    WorkspaceNotIndexed(PathBuf),

    #[error("Invalid path: {0}")]
    InvalidPath(PathBuf),

    #[error("Symlink depth exceeded: {0}")]
    SymlinkDepthExceeded(PathBuf),

    #[error("Circular symlink detected: {0}")]
    CircularSymlink(PathBuf),

    #[error("Daemon connection failed: {0}")]
    DaemonConnection(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Search timeout")]
    Timeout,

    #[error("File too large: {path} ({size} bytes, max {max} bytes)")]
    FileTooLarge { path: PathBuf, size: u64, max: u64 },

    #[error("Unsupported file type: {0}")]
    UnsupportedFileType(String),

    #[error("Index directory error: {0}")]
    IndexDirectory(#[from] tantivy::directory::error::OpenDirectoryError),
}

pub type Result<T> = std::result::Result<T, YgrepError>;
