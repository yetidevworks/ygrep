use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Global ygrep configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Background watch service
    pub service: ServiceConfig,

    /// Indexing configuration
    pub indexer: IndexerConfig,

    /// Search configuration
    pub search: SearchConfig,

    /// Output formatting
    pub output: OutputConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceConfig {
    /// Rotate the service log once it passes this size, in megabytes
    pub log_max_size_mb: u64,

    /// How often the service re-reads the index registry, in seconds.
    /// Picks up newly built indexes and watch flags toggled elsewhere.
    pub registry_rescan_secs: u64,
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

    /// Skip files whose average line length exceeds this many bytes (0 disables).
    /// Catches bundled JS, minified CSS, and compact data blobs that no one searches.
    pub max_avg_line_length: usize,

    /// zstd level for the index doc store (1-22, or 0 for LZ4).
    /// Higher levels build a smaller index more slowly; search speed is unaffected.
    /// Takes effect when an index is next built with `--rebuild`.
    pub docstore_compression_level: i32,

    /// Compact the index once it exceeds this many segments (0 disables).
    /// Editing files leaves deleted documents behind in their old segments; without
    /// this the index grows without bound even though the code doesn't.
    pub auto_compact_segments: usize,

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

    /// Number of threads used to walk and index a workspace (0 = one per core)
    pub threads: usize,

    /// Writer heap for a full index build, in megabytes.
    /// Indexers that handle one file at a time always use tantivy's 15MB minimum.
    pub writer_heap_mb: usize,
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

    /// Build a text-only index automatically when searching an unindexed workspace
    pub auto_index: bool,
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

    /// Append one line per search to <data_dir>/telemetry/queries.jsonl.
    /// Local only — it feeds the dashboard's query stats and nothing leaves the machine.
    pub telemetry: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            service: ServiceConfig::default(),
            indexer: IndexerConfig::default(),
            search: SearchConfig::default(),
            output: OutputConfig::default(),
        }
    }
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            log_max_size_mb: 5,
            registry_rescan_secs: 30,
        }
    }
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            max_file_size: 10 * 1024 * 1024, // 10MB
            include_extensions: vec![],
            max_avg_line_length: 400,
            docstore_compression_level: crate::index::schema::DEFAULT_DOCSTORE_COMPRESSION_LEVEL,
            auto_compact_segments: 16,
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
                // Apple toolchain
                "**/DerivedData/**".into(),
                "**/Pods/**".into(),
                "**/Carthage/**".into(),
                "**/*.xcarchive/**".into(),
                "**/*.dSYM/**".into(),
                "**/*.framework/**".into(),
                // JS framework build output
                "**/.next/**".into(),
                "**/.nuxt/**".into(),
                "**/.svelte-kit/**".into(),
                "**/.turbo/**".into(),
                "**/.parcel-cache/**".into(),
                "**/.angular/**".into(),
                // Compiled artifacts
                "**/*.a".into(),
                "**/*.rlib".into(),
                "**/*.rmeta".into(),
                "**/*.d".into(),
                "**/*.bc".into(),
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
            chunk_overlap: 0,
            threads: std::thread::available_parallelism()
                .map(|n| n.get().min(8))
                .unwrap_or(2),
            writer_heap_mb: crate::index::writer::DEFAULT_WRITER_HEAP_MB,
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
            auto_index: true,
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
            telemetry: true,
        }
    }
}

fn default_data_dir() -> PathBuf {
    // 1. YGREP_HOME — dedicated override, used as-is
    if let Ok(ygrep_home) = std::env::var("YGREP_HOME") {
        if !ygrep_home.is_empty() {
            return PathBuf::from(ygrep_home);
        }
    }
    // 2. XDG_DATA_HOME/ygrep
    if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
        if !xdg_data.is_empty() {
            return PathBuf::from(xdg_data).join("ygrep");
        }
    }
    // 3. Platform default
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

        // Try XDG_CONFIG_HOME first (even on macOS)
        if let Ok(xdg_config) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg_config.is_empty() {
                let config_path = PathBuf::from(&xdg_config).join("ygrep").join("config.toml");
                if let Ok(content) = std::fs::read_to_string(&config_path) {
                    if let Ok(config) = toml::from_str(&content) {
                        return config;
                    }
                }
            }
        }

        // Try platform default config dir
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
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    Io(#[from] std::io::Error),

    #[error("Failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_sane_values() {
        let config = Config::default();

        // Indexer defaults
        assert!(config.indexer.max_file_size > 0);
        assert!(config.indexer.chunk_size > 0);
        assert!(config.indexer.chunk_overlap < config.indexer.chunk_size);
        assert!(config.indexer.threads > 0);

        // Search weights should be between 0 and 1
        assert!(config.search.bm25_weight >= 0.0 && config.search.bm25_weight <= 1.0);
        assert!(config.search.vector_weight >= 0.0 && config.search.vector_weight <= 1.0);

        // Limits
        assert!(config.search.default_limit > 0);
        assert!(config.search.max_limit >= config.search.default_limit);
    }

    #[test]
    fn test_config_load_returns_defaults_when_no_file() {
        // Config::load() should return defaults when no config file exists
        // (we're in a test environment, so unlikely to have .ygrep.toml in cwd)
        let config = Config::load();
        let default = Config::default();

        assert_eq!(config.indexer.max_file_size, default.indexer.max_file_size);
        assert_eq!(config.search.default_limit, default.search.default_limit);
    }

    #[test]
    fn test_service_defaults() {
        let config = Config::default();
        assert_eq!(config.service.log_max_size_mb, 5);
        assert_eq!(config.service.registry_rescan_secs, 30);
        assert!(config.output.telemetry);
    }

    #[test]
    fn test_retired_daemon_section_still_parses() {
        // Config files written before the service replaced the daemon scaffolding must
        // keep loading rather than failing the whole run.
        let config: Config = toml::from_str(
            r#"
            [daemon]
            socket_path = "/tmp/ygrep.sock"
            idle_timeout = 3600
            max_concurrent_ops = 4

            [service]
            registry_rescan_secs = 90
            "#,
        )
        .expect("an unknown section must be ignored, not rejected");

        assert_eq!(config.service.registry_rescan_secs, 90);
        assert_eq!(config.service.log_max_size_mb, 5);
    }

    #[test]
    fn test_telemetry_can_be_switched_off() {
        let config: Config = toml::from_str("[output]\ntelemetry = false\n").unwrap();
        assert!(!config.output.telemetry);
    }

    #[test]
    fn test_config_load_from_nonexistent_file() {
        let result = Config::load_from(std::path::Path::new("/nonexistent/config.toml"));
        assert!(result.is_err());
    }
}
