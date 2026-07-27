# Changelog

All notable changes to ygrep will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [3.4.0] - 2026-07-27

### Added
- **Automatic indexing on first search** — searching a workspace with no index now builds a text-only index and runs the query, instead of failing with "Workspace not indexed". Disable with `--no-auto-index` or `search.auto_index = false`. Semantic indexes are never built implicitly, since that downloads a model and takes minutes. A read-only index directory still reports the old guidance rather than attempting a build, preserving the sandboxed setup from [#12](https://github.com/yetidevworks/ygrep/issues/12)
- **Indexing-in-progress reporting** — a build writes a marker into the index directory, so a search that lands while the background session hook is still indexing reports how long the build has been running instead of claiming the workspace is unindexed. Abandoned markers older than an hour are ignored
- **Index freshness note** — `search` and `status` mention when an index is more than a day old. This is a timestamp comparison, not a tree walk, so it costs nothing
- **`ygrep index --dry-run`** — report the files, extensions, and largest entries that would be indexed, without building anything. Useful for tuning `ignore_patterns` and confirming generated assets are excluded

### Changed
- **Doc store compressed with zstd instead of LZ4** — stored file content is about half of an index, and zstd compresses code roughly 40% smaller than LZ4 at Tantivy's block granularity, with a larger 64 KB block size for a few points more. Decompression is ~19% slower (761 MB/s vs 938 MB/s measured), which a query never notices because it touches only a handful of blocks. Combined with the exclusion work, indexes are roughly half their previous size: 38.0 MB to 23.2 MB and 28.0 MB to 13.3 MB on two real projects. Existing indexes stay readable and are rebuilt into the new format on the next `ygrep index` (schema version 5 to 6)

### Fixed
- **Workspaces under a directory named like a build output indexed nothing** — ignore patterns were matched against the absolute path, so a project at `~/build/myapp` or anywhere below a `tmp`, `cache`, `dist`, `var`, or `log` directory silently indexed zero files. Patterns now match relative to the workspace root
- **Hidden workspace roots indexed nothing** — `ygrep index ~/.config/something` silently produced an empty index because the hidden-file check was applied to the workspace root itself
- **Generated assets are no longer indexed** — bundled JS, minified CSS, and compact data blobs are skipped by average line length (`max_avg_line_length`, default 400 bytes, 0 disables). Measured against real projects this drops 16% of indexed bytes in grav-api, 23% in grav-helios, and 31% in riffle, while matching nothing in a plain Rust project
- **Directory pruning follows the configured ignore patterns** — pruning used a hardcoded directory list that had drifted from `ignore_patterns`, so configured patterns never pruned subtrees and a `var/` directory was skipped more aggressively than configured
- **Binary detection no longer reads whole files** — classifying an unknown-extension file read the entire file into memory to inspect its first 8 KB
- **More build output excluded by default** — `DerivedData`, `Pods`, `Carthage`, `*.xcarchive`, `*.dSYM`, `*.framework`, `.next`, `.nuxt`, `.svelte-kit`, `.turbo`, `.parcel-cache`, `.angular`, and compiled artifacts (`*.a`, `*.rlib`, `*.rmeta`, `*.d`, `*.bc`)

## [3.3.2] - 2026-07-27

### Fixed
- **`indexes remove <path>` could delete the workspace instead of the index** ([#13](https://github.com/yetidevworks/ygrep/issues/13)) — passing an absolute path (e.g. `ygrep indexes remove ~/Developer`) made `remove` resolve to the workspace directory itself and delete it, leaving the real index in place. `Path::join` discards its base when given an absolute path, so the hash lookup `indexes_dir.join(identifier)` resolved straight to the caller's own directory. Identifiers are now only treated as a hash when they are a single path component, and every index deletion is checked against the canonicalized index directory before anything is removed. `..` traversal and symlinks pointing outside the index directory are rejected for the same reason

### Added
- **`--dry-run` and `--yes` for `indexes remove` and `indexes clean`** — `--dry-run` prints the resolved index directory and size without deleting anything. Both commands now prompt for confirmation when run interactively; `--yes` skips the prompt, and non-interactive runs are unaffected

## [3.3.1] - 2026-07-16

### Fixed
- **Read-only access to existing indexes** ([#12](https://github.com/yetidevworks/ygrep/issues/12)) — `ygrep search` and `ygrep status` no longer require write access to the index directory. They now open the index through a lock-free read-only Tantivy directory, so a sandboxed process (e.g. a coding agent) can consume a centrally-maintained index it can only read. Previously a readable but non-writable index was misreported as `Workspace not indexed`
- **Misleading errors from `search`, `status`, and `watch`** — these commands no longer collapse every open failure into "Workspace not indexed". Genuine open errors (corrupt index, permission problems for mutating commands) are now reported as-is instead of recommending a rebuild

## [3.3.0] - 2026-07-07

### Added
- **Index compaction** (`ygrep indexes compact [hash|path]`) — manually merge index segments and garbage-collect stale files for a selected index or the current workspace

### Fixed
- **Punctuation-only literal searches** — queries such as `->`, `{%`, and `::` now scan stored documents when they have no searchable index terms, preserving grep-like behavior
- **Text-only indexing segment churn** — chunk documents are now skipped unless embeddings are enabled, reducing index size and avoiding unnecessary segment growth
- **Zero-limit and empty-query searches** — text and hybrid search now return empty results immediately without issuing oversized Tantivy queries

## [3.2.4] - 2026-03-09

### Fixed
- **Segment merge warnings during git operations** — rapid per-file commits caused Tantivy's background merge threads to race with subsequent commits, producing `couldn't find segment in SegmentManager` warnings during branch switching and bulk file changes. Watch-mode indexers now use `NoMergePolicy` to prevent background merge races entirely, and batch all queued events into a single commit. Segments accumulate during watch sessions but are consolidated on the next incremental index
- **Tantivy internal warnings cluttering output** — Tantivy's internal WARN-level segment manager messages now filtered from stderr output (suppressed to error-level; visible with `YGREP_DEBUG=1`)

## [3.2.3] - 2026-03-08

### Fixed
- **Segment merge warnings during git operations** — rapid per-file commits caused Tantivy's segment merge to race with itself, producing `couldn't find segment in SegmentManager` warnings during branch switching and bulk file changes. Watch loops now batch all queued events and commit once per batch instead of per file

## [3.2.2] - 2026-03-08

### Fixed
- **Watch crashes under heavy filesystem churn** — `ygrep watch` and dashboard watchers created a new IndexWriter for every file event, causing `LockBusy` and `FileAlreadyExists` errors during rapid operations like zip extraction or git clone. Now reuses a single IndexWriter for the lifetime of each watcher, eliminating lock contention entirely

## [3.2.1] - 2026-02-28

### Fixed
- **Dashboard sleeping workspaces never waking** — `has_recent_changes()` had a 2000-entry cap that silently returned "no changes" when exceeded, so any workspace with >2000 directory entries would never wake from sleep. Now fails open (assumes changes may exist) so the workspace wakes and runs an incremental index
- **Sleep poll wasting budget on irrelevant directories** — the mtime poll only skipped `.`-prefixed, `node_modules`, and `target` directories. Aligned with the full FileWatcher skip list (`vendor`, `dist`, `build`, `cache`, `__pycache__`, `logs`, `tmp`) so the 2000-entry budget covers actual source files
- **Stale `indexed_at` on Active to Sleeping transition** — `indexed_at` was never updated when a workspace went to sleep, so the mtime comparison used a potentially stale timestamp. Now set to the actual sleep time so only genuinely new changes trigger a wake

## [3.2.0] - 2026-02-25

### Added
- **Self-update** (`ygrep update`) — checks GitHub Releases for the latest version and replaces the binary in-place. Detects Homebrew and cargo installs and suggests the appropriate upgrade command instead
- **Update check** (`ygrep update --check`) — just prints whether a newer version is available, without installing
- **Automatic update notifications** — after search, ygrep checks a local cache (refreshed every 24 hours in the background) and prints a one-liner hint to stderr when a newer version is available

## [3.1.6] - 2026-02-25

### Added
- **Verbose search diagnostics** (`-v` / `--verbose`) — shows per-stage filtering pipeline on stderr: matches before filtering, after extension filter, after path filter, and final results. Useful for debugging zero-result searches

### Fixed
- `text_hits` and `semantic_hits` in `--json` output now reflect post-filter counts — previously they reported pre-filter candidate counts, causing confusing output like `text_hits: 15` with `total: 0` when path or extension filters removed all matches (#10)

## [3.1.5] - 2026-02-23

### Fixed
- `indexes list`, `indexes clean`, `indexes remove`, and `dashboard` now detect `.ygrep/` in CWD and resolve relative `data_dir` — previously these commands only checked the global data directory, so local project indexes were invisible

## [3.1.4] - 2026-02-22

### Added
- **Auto-detect `.ygrep/` directory** — if a `.ygrep/` directory exists in the workspace root, it is automatically used as the data directory (zero config, no `.ygrep.toml` needed)
- **Relative `data_dir` in `.ygrep.toml`** — relative paths are now resolved against the workspace root, so `data_dir = ".ygrep"` stores indexes inside the project

### Fixed
- Permission error hint now suggests `mkdir .ygrep` and `YGREP_HOME` instead of the outdated `XDG_DATA_HOME` workaround

## [3.1.3] - 2026-02-19

### Added
- `YGREP_HOME` environment variable — dedicated override for data directory, used as-is (no `/ygrep` suffix appended). Takes highest priority in resolution: `YGREP_HOME` → `XDG_DATA_HOME/ygrep` → platform default
- `--data-dir` global CLI flag — overrides data directory for a single invocation without needing env var wrappers
- Version number displayed in the dashboard footer (right-aligned, stays in sync with Cargo.toml)

### Fixed
- `indexes list`, `indexes clean`, `indexes remove`, and `dashboard` now use the same data directory resolution as `index`/`search`/`watch`/`status` — previously these commands had duplicated logic that ignored config file overrides

## [3.1.2] - 2026-02-19

### Improved
- Workspace output (progress, warnings) now routes through a log channel instead of writing directly to stderr — eliminates all TUI corruption from background tasks, and messages appear in the dashboard activity log
- Sleeping workspace polling is now staggered (30-second intervals per workspace) to avoid all workspaces polling the filesystem on the same tick

### Fixed
- Corrupt index/vector warnings no longer bleed into the dashboard TUI

## [3.1.1] - 2026-02-18

### Fixed
- Dashboard auto-watch threshold increased from 1 hour to 24 hours — workspaces indexed in the last day now auto-watch on startup

## [3.1.0] - 2026-02-18

### Added
- **Dashboard** (`ygrep dashboard`) - Interactive TUI for managing all indexes at a glance: toggle watchers, re-index, delete, view real-time activity log with file change rates
  - Column sorting (`s` cycles columns, `S` toggles asc/desc) with name tiebreaker
  - Live filter (`/` to search by workspace name, `Esc` to clear)
  - Default sort: active watchers first, then alphabetical
- **Filename search** - Search results now include files matching by filename, not just content. Searching for `dashboard` now returns `src/commands/dashboard.rs` even if the file content doesn't contain that word

### Fixed
- Dashboard re-index and watch no longer bleed "Indexed N files" text into the TUI — indexing output is suppressed when running from the dashboard

### Breaking
- Index schema changed (v3 to v4) for filename search support - requires `ygrep index --rebuild`

## [3.0.5] - 2026-02-17

### Fixed
- All search flags (`-n`, `-p`, `-e`, `-r`, `-s`, `-A`, `-B`, `-K`, `--text-only`) are now global — they work in any position with or without the `search` subcommand, eliminating silent option loss when mixing `ygrep -p path search "query"`
- `-p` path filter now supports glob patterns (`*`, `**`, `?`) — use quoted globs like `-p 'src/*/tests/'` for internal pattern matching instead of relying on shell expansion

### Improved
- Updated AI skill documentation for all integrations (Claude Code, OpenCode, Codex) with glob quoting guidance and clarification that `|` is literal (use `-r` for regex OR)

## [3.0.4] - 2026-02-17

### Fixed
- `-p` and `-e` flags now accept multiple values from shell glob expansion — `ygrep "query" -p src/*/tests/ -n 20` works instead of erroring with "unexpected argument"

### Improved
- Updated AI skill documentation for all integrations (Claude Code, OpenCode, Codex) with correct argument ordering, shell glob examples, and clarification that `|` is literal (use `-r` for regex OR)

## [3.0.3] - 2026-02-17

### Improved
- `ygrep watch` now runs an incremental index update before starting the file watcher, ensuring the index is current before monitoring for changes
- `ygrep indexes list` time column now shows "updated 20h ago" instead of just "20h ago" for clarity
- `ygrep watch` now shows `[.] file (skipped: not indexable)` for files without a recognized extension, instead of silently ignoring them

## [3.0.2] - 2026-02-11

### Fixed
- Clear error message when index directory is not writable (e.g. sandboxed `~/Library/Application Support/`) with suggestion to set `XDG_DATA_HOME` (#7)

## [3.0.1] - 2026-02-11

### Fixed
- `ygrep watch` no longer blocks concurrent `ygrep search` with lockfile errors on macOS (#7) - stale `.tantivy-meta.lock` files are cleaned up before opening an index for reading, and `index.reader()` retries with exponential backoff on transient META_LOCK contention

## [3.0.0] - 2026-02-11

### Changed
- OpenCode installer now writes `SKILL.md` to `~/.config/opencode/skills/ygrep/` (replaces `.ts` tool file + `opencode.json` manipulation)
- Codex installer now writes `SKILL.md` to `~/.agents/skills/ygrep/` (replaces `~/.codex/AGENTS.md` append)
- Uninstallers include migration cleanup for old install formats

### Removed
- Factory Droid (`ygrep install droid`) integration

## [2.0.5] - 2026-02-10

### Added
- camelCase and snake_case subtoken indexing - searching `send` now finds `sendCampaign`, `send_email`, etc. via subtokens emitted at the same token position
- Multi-word AND fallback - queries like `"campaign sending"` now return results where all terms appear in the document, not just exact adjacent phrases
- `-s` / `--case-sensitive` flag for case-sensitive search (default remains case-insensitive)
- `-A` / `-B` / `-K` context flags to control lines of context before/after matches in snippets

### Fixed
- Reported line numbers now point to the actual matching line instead of the first context line above it
- `format_ai` output now displays the matching line content instead of the first snippet line (which was often a context line like `);`)
- Multi-word snippet selection prefers lines containing the most query terms

### Breaking
- Index schema changed (v2 to v3) for subtoken support - requires `ygrep index --rebuild`

## [2.0.4] - 2026-02-10

### Fixed
- Deduplicated text and regex search results that matched both the full document and its chunks, returning only the highest-scored hit per file and line range (#6)

## [2.0.3] - 2026-02-10

### Added
- macOS x86_64 (Intel) binary builds and Homebrew support (text search only)
- Honor XDG environment variables (`XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_RUNTIME_DIR`) on all platforms including macOS
- CI workflow with formatting, type checking, tests, and clippy on every push/PR to main
- 30 new tests covering tokenizer behavior, search correctness, RRF fusion, indexer operations, config defaults, watcher patterns, and result formatting (20 → 50 total)

### Fixed
- 6 broken tests: workspace open/create mismatch, score display assertion, tempdir path conflicts with walker ignore list, missing tokenizer registration

## [2.0.2] - 2026-02-10

### Added
- Windows x86_64 binary builds via dedicated GitHub Actions workflow

### Fixed
- Windows compilation error caused by platform-specific debouncer cache type mismatch
- Windows workflow dispatch now requires a release tag input, fixing "release not found" error on manual triggers

## [2.0.1] - 2026-02-06

### Changed
- `ygrep indexes list` now sorted by size (largest first) with file counts, relative timestamps, orphan markers, and `~/` shortened paths

### Fixed
- Auto-recover from corrupt index files instead of failing with "failed to fill whole buffer" error
- Corrupt HNSW vector index is now automatically recreated with a warning
- Corrupt Tantivy index is now automatically recreated when running `ygrep index`

## [2.0.0] - 2026-01-27

### Added
- **Incremental indexing** - Only re-indexes files that changed since last index, based on mtime comparison
- **Schema versioning** - Tracks index schema version in workspace metadata; automatically rebuilds when schema changes
- **Fast field schema** - `path`, `doc_id`, and `chunk_id` fields now use Tantivy columnar fast fields for efficient reads
- **Stale embedding cleanup** - Soft-deletes vector embeddings for removed files during incremental indexing

### Changed
- `ygrep index` now runs incrementally by default when an existing index is present (use `--rebuild` for full re-index)
- Non-blocking hook - Claude Code / Factory Droid startup hook runs `ygrep index` in the background (`&`) instead of blocking
- `build_indexed_files_map()` reads from fast fields instead of deserializing stored documents, significantly faster for large repos
- Incremental no-op runs complete in ~0.01-0.02s (previously required full re-index)

### Breaking
- Index schema changed (v1 to v2) - existing indexes are automatically rebuilt on first run

## [1.0.1] - 2025-12-10

### Changed
- Unified shorthand and `search` subcommand options - both now support all search flags (`-r`, `-e`, `-p`, `--text-only`)
- Shorthand query is now a single argument instead of variadic, fixing option parsing issues

### Fixed
- Fixed `-r` flag not working when placed after query in shorthand form

## [1.0.0] - 2025-12-10

### Added
- Regex search support with `-r` / `--regex` flag
- Match type indicators in output: `+` (hybrid), `~` (semantic only), none (text only)
- Sticky index mode - `--semantic` and `--text` flags are remembered for future `index` and `watch` commands
- Helpful error messages when searching unindexed workspaces (shows how to index)

### Changed
- Replaced `--format ai|json|pretty` with simpler `--json` and `--pretty` flags
- Improved CLI help with detailed descriptions and usage examples
- Updated AI tool integration skills with new output format documentation

### Fixed
- Fixed stray "unknown" indexes being created when opening unindexed workspaces
- Fixed workspace detection to properly distinguish indexed vs unindexed workspaces
- Prevented `Workspace::open()` from creating empty index directories

## [0.3.0] - 2025-12-09

### Changed
- Renamed `--embeddings` flag to `--semantic` for clarity
- Changed user-facing terminology from "embedding" to "semantic" throughout
- Progress bar now displays correctly (model loads before bar starts)

### Fixed
- Fixed line numbers in hybrid search showing top of file instead of match location
- Fixed `-n` limit not working with shorthand query form (`ygrep -n 5 query`)
- Fixed UTF-8 panic when displaying results with non-ASCII characters

## [0.2.5] - 2025-12-09

### Changed
- Optimized vector index loading (3+ seconds → ~5ms) using native HNSW dump/load
- Removed embedding daemon (no longer needed after optimization)

### Fixed
- Fixed UTF-8 character boundary panic in search results

## [0.2.4] - 2025-12-08

### Changed
- Various performance improvements and bug fixes

## [0.2.3] - 2025-12-07

### Changed
- Version bump for release

## [0.2.2] - 2025-12-06

### Added
- ONNX Runtime support for semantic embeddings
- Hybrid search combining BM25 and vector similarity
- `--embeddings` flag to build semantic index during indexing

## [0.2.1] - 2025-12-05

### Changed
- Switched to rustls for easier cross-platform builds
- Removed OpenSSL dependency

## [0.2.0] - 2025-12-04

### Changed
- Updated build configuration for multiple target platforms
- Added more build targets (macOS ARM64, Linux x86_64)

## [0.1.0] - 2025-12-03

### Added
- Initial release
- Tantivy-based full-text indexing with BM25 ranking
- Code-aware tokenizer preserving `$`, `@`, `#` as part of tokens
- Literal text matching (like grep, not regex)
- File watching for incremental index updates
- Symlink handling with cycle detection
- AI-optimized output format
- Index management commands (`indexes list`, `indexes clean`, `indexes remove`)
- Client integrations for Claude Code, OpenCode, Codex, and Factory Droid
- Cross-platform support (macOS, Linux)

### Fixed
- Fixed cross-platform debouncer type for Linux builds
- Fixed file watcher to follow symlinks correctly
- Deduplicated watch events for same file

[3.4.0]: https://github.com/yetidevworks/ygrep/compare/v3.3.2...v3.4.0
[3.3.2]: https://github.com/yetidevworks/ygrep/compare/v3.3.1...v3.3.2
[3.3.1]: https://github.com/yetidevworks/ygrep/compare/v3.3.0...v3.3.1
[3.3.0]: https://github.com/yetidevworks/ygrep/compare/v3.2.4...v3.3.0
[3.2.4]: https://github.com/yetidevworks/ygrep/compare/v3.2.3...v3.2.4
[3.2.3]: https://github.com/yetidevworks/ygrep/compare/v3.2.2...v3.2.3
[3.2.2]: https://github.com/yetidevworks/ygrep/compare/v3.2.1...v3.2.2
[3.2.1]: https://github.com/yetidevworks/ygrep/compare/v3.2.0...v3.2.1
[3.2.0]: https://github.com/yetidevworks/ygrep/compare/v3.1.6...v3.2.0
[3.1.6]: https://github.com/yetidevworks/ygrep/compare/v3.1.5...v3.1.6
[3.1.5]: https://github.com/yetidevworks/ygrep/compare/v3.1.4...v3.1.5
[3.1.4]: https://github.com/yetidevworks/ygrep/compare/v3.1.3...v3.1.4
[3.1.3]: https://github.com/yetidevworks/ygrep/compare/v3.1.2...v3.1.3
[3.1.2]: https://github.com/yetidevworks/ygrep/compare/v3.1.1...v3.1.2
[3.1.1]: https://github.com/yetidevworks/ygrep/compare/v3.1.0...v3.1.1
[3.1.0]: https://github.com/yetidevworks/ygrep/compare/v3.0.5...v3.1.0
[3.0.6]: https://github.com/yetidevworks/ygrep/compare/v3.0.5...v3.0.6
[3.0.5]: https://github.com/yetidevworks/ygrep/compare/v3.0.4...v3.0.5
[3.0.4]: https://github.com/yetidevworks/ygrep/compare/v3.0.3...v3.0.4
[3.0.3]: https://github.com/yetidevworks/ygrep/compare/v3.0.2...v3.0.3
[3.0.2]: https://github.com/yetidevworks/ygrep/compare/v3.0.1...v3.0.2
[3.0.1]: https://github.com/yetidevworks/ygrep/compare/v3.0.0...v3.0.1
[3.0.0]: https://github.com/yetidevworks/ygrep/compare/v2.0.5...v3.0.0
[2.0.5]: https://github.com/yetidevworks/ygrep/compare/v2.0.4...v2.0.5
[2.0.4]: https://github.com/yetidevworks/ygrep/compare/v2.0.3...v2.0.4
[2.0.3]: https://github.com/yetidevworks/ygrep/compare/v2.0.2...v2.0.3
[2.0.2]: https://github.com/yetidevworks/ygrep/compare/v2.0.1...v2.0.2
[2.0.1]: https://github.com/yetidevworks/ygrep/compare/v2.0.0...v2.0.1
[2.0.0]: https://github.com/yetidevworks/ygrep/compare/v1.0.1...v2.0.0
[1.0.1]: https://github.com/yetidevworks/ygrep/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/yetidevworks/ygrep/compare/v0.3.0...v1.0.0
[0.3.0]: https://github.com/yetidevworks/ygrep/compare/v0.2.5...v0.3.0
[0.2.5]: https://github.com/yetidevworks/ygrep/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/yetidevworks/ygrep/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/yetidevworks/ygrep/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/yetidevworks/ygrep/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/yetidevworks/ygrep/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/yetidevworks/ygrep/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/yetidevworks/ygrep/releases/tag/v0.1.0
