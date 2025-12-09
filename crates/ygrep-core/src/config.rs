use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Global ygrep configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Daemon configuration
    pub daemon: DaemonConfig,

    /// Indexing configuration
    pub indexer: IndexerConfig,

    /// Search configuration
    pub search: SearchConfig,

    /// Output formatting
    pub output: OutputConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Socket path (default: $XDG_RUNTIME_DIR/ygrep/ygrep.sock or ~/.ygrep/ygrep.sock)
    pub socket_path: Option<PathBuf>,

    /// Auto-shutdown after idle time (seconds, 0 = never)
    pub idle_timeout: u64,

    /// Maximum concurrent index operations
    pub max_concurrent_ops: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexerConfig {
    /// Base directory for all index data
    pub data_dir: PathBuf,

    /// Maximum file size to index (bytes)
    pub max_file_size: u64,

    /// File extensions to include (empty = all text files)
    pub include_extensions: Vec<String>,

    /// Additional ignore patterns (glob syntax)
    pub ignore_patterns: Vec<String>,

    /// Follow symlinks
    pub follow_symlinks: bool,

    /// Respect .gitignore files (default: false for code search)
    pub respect_gitignore: bool,

    /// Enable content deduplication
    pub deduplicate: bool,

    /// Chunk size for semantic indexing (lines)
    pub chunk_size: usize,

    /// Chunk overlap (lines)
    pub chunk_overlap: usize,

    /// Number of indexing threads
    pub threads: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// BM25 weight in hybrid search (0.0-1.0)
    pub bm25_weight: f32,

    /// Vector weight in hybrid search (0.0-1.0)
    pub vector_weight: f32,

    /// Default result limit
    pub default_limit: usize,

    /// Maximum results
    pub max_limit: usize,

    /// Minimum score threshold (0.0-1.0)
    pub min_score: f32,

    /// Enable fuzzy matching for BM25
    pub fuzzy_enabled: bool,

    /// Fuzzy distance (1-2)
    pub fuzzy_distance: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    /// AI-optimized format (minimal tokens)
    pub ai_mode: bool,

    /// Include file content snippets
    pub show_content: bool,

    /// Context lines around matches
    pub context_lines: usize,

    /// Maximum output lines per result
    pub max_lines_per_result: usize,

    /// Show scores in output
    pub show_scores: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            daemon: DaemonConfig::default(),
            indexer: IndexerConfig::default(),
            search: SearchConfig::default(),
            output: OutputConfig::default(),
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: None,
            idle_timeout: 3600, // 1 hour
            max_concurrent_ops: 4,
        }
    }
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            max_file_size: 10 * 1024 * 1024, // 10MB
            include_extensions: vec![],
            ignore_patterns: vec![
                // Package managers & dependencies
                "**/node_modules/**".into(),
                "**/vendor/**".into(),
                "**/.venv/**".into(),
                "**/venv/**".into(),
                "**/bower_components/**".into(),

                // Build outputs
                "**/target/**".into(),
                "**/dist/**".into(),
                "**/build/**".into(),
                "**/out/**".into(),
                "**/_build/**".into(),
                "**/bin/**".into(),
                "**/obj/**".into(),

                // Cache directories
                "**/cache/**".into(),
                "**/.cache/**".into(),
                "**/caches/**".into(),
                "**/__pycache__/**".into(),
                "**/.pytest_cache/**".into(),
                "**/.mypy_cache/**".into(),
                "**/.ruff_cache/**".into(),
                "**/.phpunit.cache/**".into(),
                "**/var/cache/**".into(),

                // Log directories
                "**/logs/**".into(),
                "**/log/**".into(),
                "**/*.log".into(),

                // Temp directories
                "**/tmp/**".into(),
                "**/temp/**".into(),
                "**/.tmp/**".into(),

                // Version control
                "**/.git/**".into(),
                "**/.svn/**".into(),
                "**/.hg/**".into(),

                // IDE/Editor
                "**/.idea/**".into(),
                "**/.vscode/**".into(),
                "**/.vs/**".into(),
                "**/*.swp".into(),
                "**/*.swo".into(),

                // Lock files
                "Cargo.lock".into(),
                "package-lock.json".into(),
                "yarn.lock".into(),
                "pnpm-lock.yaml".into(),
                "composer.lock".into(),
                "Gemfile.lock".into(),
                "poetry.lock".into(),

                // Binary/compiled files
                "**/*.pyc".into(),
                "**/*.pyo".into(),
                "**/*.class".into(),
                "**/*.o".into(),
                "**/*.so".into(),
                "**/*.dylib".into(),
                "**/*.dll".into(),
                "**/*.exe".into(),

                // Data files (often large)
                "**/*.sqlite".into(),
                "**/*.sqlite3".into(),
                "**/*.db".into(),

                // Coverage & test artifacts
                "**/coverage/**".into(),
                "**/.coverage/**".into(),
                "**/htmlcov/**".into(),
                "**/.nyc_output/**".into(),

                // Images
                "**/*.svg".into(),
                "**/*.png".into(),
                "**/*.jpg".into(),
                "**/*.jpeg".into(),
                "**/*.gif".into(),
                "**/*.ico".into(),
                "**/*.webp".into(),
                "**/*.bmp".into(),
                "**/*.tiff".into(),
                "**/*.psd".into(),

                // Fonts
                "**/*.woff".into(),
                "**/*.woff2".into(),
                "**/*.ttf".into(),
                "**/*.otf".into(),
                "**/*.eot".into(),

                // Media
                "**/*.mp3".into(),
                "**/*.mp4".into(),
                "**/*.wav".into(),
                "**/*.ogg".into(),
                "**/*.webm".into(),
                "**/*.avi".into(),
                "**/*.mov".into(),

                // Archives
                "**/*.zip".into(),
                "**/*.tar".into(),
                "**/*.gz".into(),
                "**/*.rar".into(),
                "**/*.7z".into(),

                // Documents (usually not code)
                "**/*.pdf".into(),
                "**/*.doc".into(),
                "**/*.docx".into(),
                "**/*.xls".into(),
                "**/*.xlsx".into(),
                "**/*.ppt".into(),
                "**/*.pptx".into(),

                // Minified/bundled files
                "**/*.min.js".into(),
                "**/*.min.css".into(),
                "**/*.bundle.js".into(),
                "**/*.chunk.js".into(),

                // Source maps
                "**/*.map".into(),
            ],
            follow_symlinks: true,
            respect_gitignore: false,
            deduplicate: true,
            chunk_size: 50,
            chunk_overlap: 10,
            threads: std::thread::available_parallelism()
                .map(|n| n.get().min(4))
                .unwrap_or(2),
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            bm25_weight: 0.5,
            vector_weight: 0.5,
            default_limit: 10,
            max_limit: 100,
            min_score: 0.1,
            fuzzy_enabled: true,
            fuzzy_distance: 1,
        }
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            ai_mode: true,
            show_content: true,
            context_lines: 2,
            max_lines_per_result: 10,
            show_scores: false,
        }
    }
}

fn default_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("ygrep")
}

impl Config {
    /// Load config from default locations (in order of precedence):
    /// 1. $PWD/.ygrep.toml
    /// 2. $XDG_CONFIG_HOME/ygrep/config.toml
    /// 3. ~/.config/ygrep/config.toml
    /// 4. Built-in defaults
    pub fn load() -> Self {
        // Try project-level config
        if let Ok(content) = std::fs::read_to_string(".ygrep.toml") {
            if let Ok(config) = toml::from_str(&content) {
                return config;
            }
        }

        // Try user-level config
        if let Some(config_dir) = dirs::config_dir() {
            let config_path = config_dir.join("ygrep").join("config.toml");
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                if let Ok(config) = toml::from_str(&content) {
                    return config;
                }
            }
        }

        // Fall back to defaults
        Self::default()
    }

    /// Load config from a specific file
    pub fn load_from(path: &std::path::Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config = toml::from_str(&content)?;
        Ok(config)
    }

    /// Get the socket path, using default if not specified
    pub fn socket_path(&self) -> PathBuf {
        self.daemon.socket_path.clone().unwrap_or_else(default_socket_path)
    }
}

fn default_socket_path() -> PathBuf {
    if let Some(runtime_dir) = dirs::runtime_dir() {
        runtime_dir.join("ygrep").join("ygrep.sock")
    } else if let Some(home) = dirs::home_dir() {
        home.join(".ygrep").join("ygrep.sock")
    } else {
        PathBuf::from("/tmp/ygrep/ygrep.sock")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    Io(#[from] std::io::Error),

    #[error("Failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
}
