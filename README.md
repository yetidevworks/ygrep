# ygrep

A fast, local, indexed code search tool optimized for AI coding assistants. Written in Rust using Tantivy for full-text indexing.

## Features

- **Literal text matching** - Works like grep, special characters included (`$variable`, `{% block`, `->get(`, `@decorator`)
- **Code-aware tokenizer** - Preserves `$`, `@`, `#` as part of tokens (essential for PHP, Shell, Python, etc.)
- **Fast indexed search** - Tantivy-powered BM25 ranking, instant results
- **File watching** - Incremental index updates on file changes
- **Optional semantic search** - HNSW vector index with local embeddings (bge-small-en-v1.5)
- **Symlink handling** - Follows symlinks with cycle detection
- **AI-optimized output** - Clean, minimal output with file paths and line numbers

## Installation

### Homebrew (macOS/Linux)

```bash
brew install yetidevworks/ygrep/ygrep
```

### From Source

```bash
# Using cargo
cargo install --path crates/ygrep-cli

# Or build release
cargo build --release
cp target/release/ygrep ~/.cargo/bin/
```

## Quick Start

### 1. Install for your AI tool

```bash
ygrep install claude-code    # Claude Code
ygrep install opencode       # OpenCode
ygrep install codex          # Codex
ygrep install droid          # Factory Droid
```

### 2. Index your project

```bash
ygrep index
```

### 3. Search

```bash
ygrep "search query"         # Shorthand
ygrep search "search query"  # Explicit
```

That's it! The AI tool will now use ygrep for code searches.

## Usage

### Searching

```bash
# Basic search (returns up to 100 results by default)
ygrep "$variable"                  # PHP/Shell variables
ygrep "{% block content"           # Twig templates
ygrep "->get("                     # Method calls
ygrep "@decorator"                 # Python decorators

# With options
ygrep search "error" -n 20         # Limit results
ygrep search "config" -e rs -e toml # Filter by extension
ygrep search "api" -p src/         # Filter by path

# Output formats
ygrep search "query" -f ai         # AI-optimized (default)
ygrep search "query" -f json       # JSON output
ygrep search "query" -f pretty     # Human-readable
```

### Indexing

```bash
ygrep index                        # Index current directory
ygrep index --rebuild              # Force rebuild (required after ygrep updates)
ygrep index --embeddings           # Include semantic embeddings (slower)
ygrep index /path/to/project       # Index specific directory
```

### File Watching

```bash
ygrep watch                        # Watch current directory
ygrep watch /path/to/project       # Watch specific directory
```

### Status

```bash
ygrep status                       # Show index status
ygrep status --detailed            # Detailed statistics
```

### Index Management

```bash
ygrep indexes list                 # List all indexes with sizes
ygrep indexes clean                # Remove orphaned indexes (freed disk space)
ygrep indexes remove <hash>        # Remove specific index by hash
ygrep indexes remove /path/to/dir  # Remove index by workspace path
```

### Semantic Search (Optional)

Enable semantic/vector search for better results on natural language queries:

```bash
# Index with embeddings (one-time, slower)
ygrep index --embeddings

# Search automatically uses hybrid mode when embeddings exist
ygrep "authentication flow"        # Uses BM25 + vector search

# Force text-only search
ygrep search "auth" --text-only
```

Semantic search uses the `bge-small-en-v1.5` model (~50MB, downloaded on first use).

## AI Tool Integration

ygrep integrates with popular AI coding assistants:

### Claude Code

```bash
ygrep install claude-code          # Install plugin
ygrep uninstall claude-code        # Uninstall plugin
```

After installation, restart Claude Code. The plugin:
- Runs `ygrep index` on session start
- Provides a skill that teaches Claude to use ygrep for searches
- Invoke with `/ygrep` then ask Claude to search

### OpenCode

```bash
ygrep install opencode             # Install tool
ygrep uninstall opencode           # Uninstall tool
```

### Codex

```bash
ygrep install codex                # Install skill
ygrep uninstall codex              # Uninstall skill
```

### Factory Droid

```bash
ygrep install droid                # Install hooks and skill
ygrep uninstall droid              # Uninstall
```

## Example Output

AI-optimized output format:

```
# 5 results

src/config.rs:45-67
  45: pub struct Config {
  46:     pub data_dir: PathBuf,
  47:     pub max_file_size: u64,

src/main.rs:12-28
  12: fn main() -> Result<()> {
  13:     let config = Config::load()?;
  14:     let workspace = Workspace::open(&config)?;
```

## How It Works

1. **Indexing**: Walks directory tree, indexes text files with Tantivy using a code-aware tokenizer
2. **Tokenizer**: Custom tokenizer preserves code characters (`$`, `@`, `#`, `-`, `_`) as part of tokens
3. **Search**: BM25-ranked search with optional semantic/vector search
4. **Results**: Returns matching files with line numbers and context

## Configuration

Index data stored in:
- macOS: `~/Library/Application Support/ygrep/indexes/`
- Linux: `~/.local/share/ygrep/indexes/`

## Upgrading

```bash
# Via Homebrew
brew upgrade ygrep

# Then rebuild indexes to use latest tokenizer
ygrep index --rebuild
```

## License

MIT
