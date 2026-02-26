# ygrep

A fast, local, indexed code search tool optimized for AI coding assistants. Written in Rust using Tantivy for full-text indexing.

![ygrep screenshot](ygrep-screenshot.png)

## Features

- **Literal text matching** - Works like grep by default, special characters included (`$variable`, `{% block`, `->get(`, `@decorator`)
- **Regex support** - Use `-r` flag for regex patterns (`fn\s+main`, `TODO|FIXME`)
- **Code-aware tokenizer** - Preserves `$`, `@`, `#` as part of tokens (essential for PHP, Shell, Python, etc.)
- **Subtoken matching** - camelCase and snake_case identifiers are split into subtokens, so `send` also finds `sendCampaign`, `send_email`, etc.
- **Multi-word AND queries** - `"campaign sending"` returns results where all terms appear in the file, not just exact adjacent phrases
- **Filename search** - Search matches file paths too, not just content
- **Fast indexed search** - Tantivy-powered BM25 ranking, instant results
- **Incremental indexing** - Only re-indexes changed files based on mtime; no-op runs complete in ~10ms
- **Non-blocking AI hooks** - Background indexing on session start, never slows down your AI tool
- **Interactive dashboard** - TUI for managing indexes, toggling watchers, and viewing live activity
- **File watching** - Incremental index updates on file changes
- **Optional semantic search** - HNSW vector index with local semantic model (all-MiniLM-L6-v2)
- **Symlink handling** - Follows symlinks with cycle detection
- **AI-optimized output** - Clean, minimal output with file paths and line numbers

## Installation

### Homebrew (macOS/Linux)

```bash
brew install yetidevworks/ygrep/ygrep
```

### From Source

```bash
# Using cargo (full features, requires ONNX Runtime)
cargo install --path crates/ygrep-cli

# Text search only (no ONNX dependency, faster build)
cargo install --path crates/ygrep-cli --no-default-features

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
```

### 2. Index your project

```bash
ygrep index                    # Fast text-only index
ygrep index --semantic         # With semantic search (better natural language queries)
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
# Basic search (literal text matching by default)
ygrep "$variable"                  # PHP/Shell variables
ygrep "{% block content"           # Twig templates
ygrep "->get("                     # Method calls
ygrep "@decorator"                 # Python decorators

# Regex search (use -r or --regex)
ygrep search "fn\s+\w+" -r         # Function definitions
ygrep search "TODO|FIXME" -r       # Multiple patterns
ygrep search "^import" -r          # Line anchors

# Subtoken matching (automatic with indexed search)
ygrep "send"                       # Also finds sendCampaign, send_email, etc.
ygrep "config load"                # AND match: files containing both terms

# Case-sensitive search (default is case-insensitive)
ygrep "Config" -s                  # Only matches exact case "Config"
ygrep search "IOException" -s      # Exact case match

# Context lines around matches
ygrep "error" -A 3                 # 3 lines after each match
ygrep "error" -B 2                 # 2 lines before each match
ygrep "error" -K 3                 # 3 lines before and after each match

# With options
ygrep search "error" -n 20         # Limit results
ygrep search "config" -e rs -e toml # Filter by extension
ygrep search "api" -p src/         # Filter by path

# Verbose mode (debug search pipeline)
ygrep "error" -e php -p src/ -v    # Shows per-stage filtering counts on stderr

# Output formats (AI format is default)
ygrep search "query"               # AI-optimized (default)
ygrep search "query" --json        # JSON output
ygrep search "query" --pretty      # Human-readable
```

### Indexing

```bash
ygrep index                        # Incremental update (only changed files)
ygrep index --rebuild              # Force full rebuild from scratch
ygrep index --semantic             # Build semantic index (sticky - remembered)
ygrep index --text                 # Build text-only index (sticky - remembered)
ygrep index /path/to/project       # Index specific directory
```

Indexing is **incremental by default** - only files with changed modification times are re-indexed. A no-op run (nothing changed) completes in ~10ms. Use `--rebuild` to force a full re-index.

The `--semantic` and `--text` flags are **sticky** - once set, subsequent `ygrep index` commands (without flags) will remember and use the same mode. This also applies to `ygrep watch`.

When upgrading ygrep to a new version with schema changes, the index is automatically rebuilt on the next `ygrep index` run.

### File Watching

```bash
ygrep watch                        # Watch current directory (honors stored mode)
ygrep watch /path/to/project       # Watch specific directory
```

File watching automatically uses the same mode (text or semantic) as the original index.

### Status

```bash
ygrep status                       # Show index status
ygrep status --detailed            # Detailed statistics
```

### Index Management

```bash
ygrep indexes list                 # List all indexes with sizes and type
ygrep indexes clean                # Remove orphaned indexes (freed disk space)
ygrep indexes remove <hash>        # Remove specific index by hash
ygrep indexes remove /path/to/dir  # Remove index by workspace path
```

Example output:
```
# 2 indexes (24.0 MB)

1bb65a32a7aa44ba  319.4 KB  [text]
  /path/to/project

c4f2ba4712ed98e7  23.7 MB  [semantic]
  /path/to/another-project
```

### Dashboard

Interactive TUI for monitoring and managing all your indexes in one place:

```bash
ygrep dashboard
```

The dashboard shows a table of all indexes with workspace path, size, file count, last indexed time, and watch state. Below the table is a live activity log showing real-time file indexing events.

**Key bindings:**

| Key | Action |
|-----|--------|
| `j/k` or `↑/↓` | Navigate entries |
| `w` | Toggle watch (off/active) |
| `r` | Re-index workspace |
| `d` | Delete index (with confirmation) |
| `s` | Cycle sort column (Name, Size, Files, Indexed, Watch) |
| `S` | Toggle sort order (ascending/descending) |
| `/` | Filter by workspace name |
| `Tab` | Switch focus between table and activity log |
| `?` | Help overlay |
| `q` | Quit |

### Updating

```bash
ygrep update                       # Check and install latest version
ygrep update --check               # Just check, don't install
```

ygrep automatically checks for updates once per day (in the background, after search) and shows a hint when a new version is available:

```
ygrep v3.2.0 available (current: v3.1.6). Run `ygrep update` to upgrade.
```

If installed via Homebrew or cargo, `ygrep update` will suggest the appropriate command (`brew upgrade ygrep` or `cargo install ygrep-cli`) instead of self-updating.

### Semantic Search (Optional)

Enable semantic search for better results on natural language queries:

```bash
# Build semantic index (one-time, slower - mode is remembered)
ygrep index --semantic

# Search automatically uses hybrid mode when semantic index exists
ygrep "authentication flow"        # Uses BM25 + semantic search

# Force text-only search (single query, doesn't change index mode)
ygrep search "auth" --text-only

# Future index/watch commands remember the mode
ygrep index                        # Still semantic
ygrep watch                        # Watches with semantic indexing

# Convert back to text-only index
ygrep index --text
```

Semantic search uses the `all-MiniLM-L6-v2` model (~25MB, downloaded on first use).

**Note:** Semantic search requires ONNX Runtime and is only available on certain platforms:
- ✅ macOS ARM64 (Apple Silicon)
- ✅ Linux x86_64
- ❌ macOS x86_64 (Intel) (text search only)
- ❌ Windows x86_64 (text search only)
- ❌ Linux ARM64/ARMv7/musl (text search only)

On unsupported platforms, ygrep works normally with BM25 text search - the `--semantic` flag will print a warning.

## AI Tool Integration

ygrep integrates with popular AI coding assistants:

### Claude Code

```bash
ygrep install claude-code          # Install plugin
ygrep uninstall claude-code        # Uninstall plugin
```

After installation, restart Claude Code. The plugin:
- Runs `ygrep index` in the background on session start (non-blocking)
- Provides a skill that teaches Claude to prefer ygrep over built-in search

**Important:** At the start of each session, run `/ygrep` to load the skill. This tells Claude to use ygrep for code searches instead of its built-in Grep/Glob tools. Without loading the skill, Claude will default to its slower built-in search.

### OpenCode

```bash
ygrep install opencode             # Install skill
ygrep uninstall opencode           # Uninstall skill
```

### Codex

```bash
ygrep install codex                # Install skill
ygrep uninstall codex              # Uninstall skill
```

## Example Output

### AI Format (Default)

Optimized for AI assistants - single line header with score and match type:

```
# 5 results (3 text + 2 semantic)

src/config.rs:45 (85%) +
  pub struct Config {

src/main.rs:12 (72%) ~
  fn main() -> Result<()> {

src/lib.rs:100 (65%)
  let workspace = Workspace::open(&config)?;
```

**Format:** `path:line (score%) [match_indicator]`
- `+` = Hybrid match (both text AND semantic)
- `~` = Semantic only (no exact text match)
- No indicator = Text only

### JSON Format

Full metadata with `--json`:

```json
{
  "hits": [...],
  "total": 5,
  "query_time_ms": 42,
  "text_hits": 3,
  "semantic_hits": 2
}
```

Each hit includes `match_type`: `"Text"`, `"Semantic"`, or `"Hybrid"`.

### Pretty Format

Human-readable with `--pretty`:

```
# 5 results (3 text + 2 semantic)

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
2. **Incremental updates**: Compares file modification times against the index using fast columnar fields; only changed, new, or deleted files are processed
3. **Tokenizer**: Custom tokenizer preserves code characters (`$`, `@`, `#`, `-`, `_`) as part of tokens, and emits subtokens for camelCase and snake_case identifiers
4. **Search**: BM25-ranked literal search (default) or regex matching with `-r` flag, plus optional semantic search
5. **Results**: Returns matching files with line numbers and context

## Development

### Running Tests

```bash
cargo test --workspace              # Run all tests
cargo test -p ygrep-core            # Run core library tests only
cargo test -p ygrep-core -- search  # Run tests matching "search"
```

### Code Quality

```bash
cargo fmt --all -- --check          # Check formatting
cargo clippy --workspace --all-targets  # Lint
```

### Building

```bash
cargo build --release               # Build release binary
cargo install --path crates/ygrep-cli   # Install to ~/.cargo/bin/
```

## Configuration

Index data stored in:
- macOS: `~/Library/Application Support/ygrep/indexes/`
- Linux: `~/.local/share/ygrep/indexes/`

## Upgrading

```bash
# Self-update (downloads latest release binary)
ygrep update

# Via Homebrew
brew upgrade ygrep

# Indexes auto-rebuild when schema changes are detected
ygrep index

# If upgrading to v3.0.6+, rebuild is required for filename search
ygrep index --rebuild
```

## Windows Build Prerequisites: C++ SDK & Build Tools

Building this project on Windows requires **MSVC Build Tools** and the **Windows SDK** because several dependencies compile native C/C++ code.

### Install Rust

```bash
winget install Rustlang.Rustup
```

### Install MSVC Build Tools

Install **Visual Studio Build Tools 2022** (or latest) with the following workloads:
- **"Desktop development with C++"** — includes MSVC compiler and Windows SDK
- Alternatively, install the individual components: **MSVC v143+ C++ build tools** and **Windows 10/11 SDK**

Download: https://visualstudio.microsoft.com/visual-cpp-build-tools/

### Dependencies that require C/C++ compilation

| Crate | Why | Used by |
|-------|-----|---------|
| `ort-sys` | ONNX Runtime C++ bindings | `ort` → `fastembed` (ML inference) |
| `onig_sys` | Oniguruma regex engine (C library) | `onig` |
| `zstd-sys` | Zstandard compression (C library) | `zstd` |
| `ring` | Cryptography (C/assembly) | `rustls` (TLS) |

The `cc` crate handles compiling C/C++ code from Rust build scripts, and `find-msvc-tools` (used by `ort-sys`) locates the MSVC installation on your system.

**`fastembed` → `ort` → `ort-sys`** is the primary reason MSVC Build Tools are needed, since ONNX Runtime is a substantial C++ dependency.

### Build

```bash
cargo build --release
```

The compiled binary will be at `target\release\ygrep.exe`. To make it available system-wide, either:

- **Copy to a directory already on your PATH:**
  ```bash
  copy target\release\ygrep.exe %USERPROFILE%\.cargo\bin\
  ```
- **Or add the build output directory to your PATH:**
  Go to **Settings > System > About > Advanced system settings > Environment Variables**, then add the `target\release` path to your user `Path` variable.

## License

MIT
