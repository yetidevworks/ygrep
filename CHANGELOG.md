# Changelog

All notable changes to ygrep will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
