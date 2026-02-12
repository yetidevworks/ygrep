# ygrep MCP Server

ygrep includes a built-in [Model Context Protocol](https://modelcontextprotocol.io/) (MCP) server that exposes code search as tools to any MCP-compatible AI client — Claude Code, Claude Desktop, Cursor, Windsurf, Cline, Continue, and others.

No per-tool integration code required. One config, any client.

**Everything is automatic.** The MCP server handles indexing, file watching, and schema upgrades internally. No `ygrep watch`, no `ygrep index`, no background processes. Add `--semantic` to enable embedding-based search that finds code by meaning, not just keywords.

## Install ygrep

If you don't have ygrep yet, install it first:

**macOS / Linux (Homebrew):**

```bash
brew install yetidevworks/ygrep/ygrep
```

**Windows:**

Download `ygrep-*-windows-x86_64.zip` from the [latest release](https://github.com/yetidevworks/ygrep/releases), extract it, and add `ygrep.exe` to your PATH.

**From source (any platform, requires Rust):**

```bash
cargo install --path crates/ygrep-cli
```

Verify it's working:

```bash
ygrep --version
```

## Quick Start

Run `ygrep install mcp` from your project directory to get setup instructions for all supported clients:

```bash
cd /path/to/your/project
ygrep install mcp
```

To include semantic search (embedding-based) in the generated config:

```bash
ygrep install mcp --semantic
```

## Client Setup

### Claude Code

```bash
claude mcp add ygrep -- ygrep mcp -C /path/to/your/project
```

To enable semantic search (finds code by meaning, not just keywords):

```bash
claude mcp add ygrep -- ygrep mcp -C /path/to/your/project --semantic
```

That's it. Claude Code will start the MCP server automatically when you open a session. The `ygrep_search`, `ygrep_index`, and `ygrep_status` tools become available immediately.

To scope it to a single project instead of globally, use the `--scope project` flag:

```bash
claude mcp add --scope project ygrep -- ygrep mcp -C /path/to/your/project
```

To remove it:

```bash
claude mcp remove ygrep
```

### Claude Desktop

Add to your Claude Desktop config file:

| OS | Config path |
| --- | --- |
| macOS | `~/Library/Application Support/Claude/claude_desktop_config.json` |
| Windows | `%APPDATA%\Claude\claude_desktop_config.json` |
| Linux | `~/.config/Claude/claude_desktop_config.json` |

```json
{
  "mcpServers": {
    "ygrep": {
      "command": "ygrep",
      "args": ["mcp", "-C", "/path/to/your/project"]
    }
  }
}
```

Restart Claude Desktop after saving.

### Cursor

Add via **Settings > MCP Servers**, or add to `.cursor/mcp.json` in your project:

```json
{
  "mcpServers": {
    "ygrep": {
      "command": "ygrep",
      "args": ["mcp", "-C", "/path/to/your/project"]
    }
  }
}
```

### OpenCode

Add to `opencode.json` in your project root (or `~/.config/opencode/opencode.json` for global):

```json
{
  "mcp": {
    "ygrep": {
      "type": "local",
      "command": ["ygrep", "mcp", "-C", "/path/to/your/project"]
    }
  }
}
```

Note: OpenCode uses `command` as an array (not separate `command` + `args` fields).

### Windsurf

Add via **Settings > MCP**.

### Other MCP Clients

Any client that supports the MCP stdio transport can use ygrep. The server command is:

```bash
ygrep mcp -C /path/to/your/project
```

## What happens on startup

When the MCP server starts (`ygrep mcp`), it:

1. **Detects stale indexes** — if ygrep was upgraded and the index schema changed, the old index is automatically cleared and rebuilt
2. **Auto-indexes** the workspace if no index exists yet (with semantic search if `--semantic` was passed)
3. **Starts a file watcher** that keeps the index up-to-date as files change
4. **Serves tools** over stdio until the client disconnects

The first search on an un-indexed workspace takes a few seconds (indexing), then every subsequent search is instant. The file watcher runs inside the server process — no separate `ygrep watch` needed.

## Tools

The server exposes three tools:

### `ygrep_search`

Search the indexed codebase. This is the primary tool the AI will use.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `query` | string | *(required)* | Search query (literal text or regex) |
| `limit` | number | `50` | Maximum results to return |
| `extensions` | string[] | — | Filter by file extension (e.g. `["rs", "ts"]`) |
| `paths` | string[] | — | Filter by path pattern (e.g. `["src/", "tests/"]`) |
| `regex` | boolean | `false` | Treat query as a regex pattern |
| `case_sensitive` | boolean | `false` | Case-sensitive search |
| `text_only` | boolean | `false` | Disable semantic search |

Returns file paths, line numbers, and code snippets in AI-optimized format.

If the workspace hasn't been indexed yet, the tool auto-indexes before searching.

### `ygrep_index`

Rebuild or update the search index. The AI rarely needs this — the file watcher handles incremental updates automatically.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `rebuild` | boolean | `false` | Force a full rebuild (default: incremental) |
| `semantic` | boolean | `false` | Enable semantic/embedding index |

### `ygrep_status`

Check workspace index status. No parameters. Returns:

- Workspace root path
- Whether the workspace is indexed
- Index type (text or semantic)
- Whether semantic search is available

## Semantic search

By default, the MCP server uses text-only search (fast full-text index). To enable semantic search — which finds code by meaning, not just exact keywords — add the `--semantic` flag:

```bash
ygrep mcp -C /path/to/project --semantic
```

Or in your MCP client config:

```json
{
  "args": ["mcp", "-C", "/path/to/project", "--semantic"]
}
```

Semantic search downloads a ~100MB embedding model on first use. Once built, the semantic index is maintained automatically by the file watcher. If `--semantic` is not passed but the workspace was previously indexed with semantic search, the existing semantic index is preserved.

## Running the server directly

```bash
# Serve current directory
ygrep mcp

# Serve a specific workspace
ygrep mcp -C /path/to/project

# With semantic search
ygrep mcp -C /path/to/project --semantic
```

The server communicates over **stdio** (JSON-RPC). stdout is the MCP channel; all logging goes to stderr.

## Multiple workspaces

Each MCP server instance serves one workspace. To use ygrep across multiple projects, add a separate entry per workspace:

```bash
# Claude Code
claude mcp add ygrep-frontend -- ygrep mcp -C /path/to/frontend
claude mcp add ygrep-backend -- ygrep mcp -C /path/to/backend
```

```jsonc
// Claude Desktop / Cursor
{
  "mcpServers": {
    "ygrep-frontend": {
      "command": "ygrep",
      "args": ["mcp", "-C", "/path/to/frontend"]
    },
    "ygrep-backend": {
      "command": "ygrep",
      "args": ["mcp", "-C", "/path/to/backend"]
    }
  }
}
```

```jsonc
// OpenCode (opencode.json)
{
  "mcp": {
    "ygrep-frontend": {
      "type": "local",
      "command": ["ygrep", "mcp", "-C", "/path/to/frontend"]
    },
    "ygrep-backend": {
      "type": "local",
      "command": ["ygrep", "mcp", "-C", "/path/to/backend"]
    }
  }
}
```

## Uninstalling

```bash
ygrep uninstall mcp
```

This prints instructions on where to remove the `"ygrep"` entry from your client config.

For Claude Code specifically:

```bash
claude mcp remove ygrep
```

## MCP vs Skills

ygrep also offers skill-based integrations for Claude Code, OpenCode, and Codex (`ygrep install claude-code`, etc.). These teach the AI to run `ygrep` CLI commands via the shell. Both approaches give the AI access to ygrep — here's how they differ:

### Why use MCP

- **Works with any MCP client:** Claude Desktop, Cursor, Windsurf, Cline, Continue, and any future client. Skills only work with the 3 tools that have hand-written integrations.
- **Always available:** MCP tools appear automatically alongside the AI's built-in tools. With skills, the AI must be reminded to use ygrep (e.g., running `/ygrep` each session in Claude Code).
- **Zero setup after install:** Auto-indexes on first search, built-in file watcher keeps the index fresh. No separate `ygrep watch` process, no background hook needed.
- **No shell overhead:** The AI calls `ygrep_search` directly as a tool instead of spawning a subprocess for each search.

### Why use skills

- **Behavioral guidance:** Skills teach the AI *when* to use ygrep (e.g., "prefer ygrep over built-in Grep/Glob"). MCP tools are available but the AI decides on its own when to use them.
- **Richer instructions:** A skill can include usage tips, output format explanations, and workflow guidance that MCP tool descriptions can't convey as effectively.

### Can I use both?

Yes. For Claude Code, you could install the skill for behavioral guidance *and* add the MCP server for always-available tools. They don't conflict.

### Which should I pick?

| Situation | Recommendation |
| --- | --- |
| Claude Desktop, Cursor, Windsurf, Cline, or any non-skill client | MCP (only option) |
| Claude Code, OpenCode, or Codex and you want zero-friction setup | MCP |
| Claude Code and you want the AI to *always* prefer ygrep over built-in search | Skill (or both) |

## How it works

The MCP server is a thin wrapper around the same `Workspace` API used by the CLI. It uses the [rmcp](https://github.com/modelcontextprotocol/rust-sdk) Rust SDK with stdio transport.

- **Transport:** stdio (JSON-RPC 2.0)
- **Protocol version:** 2024-11-05
- **Feature flag:** `mcp` (included in default build)
- **Binary:** same `ygrep` binary, `mcp` subcommand

To build without MCP support:

```bash
cargo build --release --no-default-features --features embeddings
```

## Making ygrep the default search tool

By default, MCP tools are available but the AI still decides when to use them. To make the AI *always* prefer ygrep over built-in search, add a custom rule to your client.

**Cursor:** Create `.cursor/rules/ygrep.mdc` in your project:

```markdown
---
description: Use ygrep for code search
alwaysApply: true
---

When searching for code, definitions, or references in the codebase, ALWAYS use
the ygrep_search MCP tool instead of built-in search tools. ygrep uses a
pre-built full-text index and returns results in milliseconds.

- Use ygrep_search for all code and file searches
- Fall back to built-in search only if ygrep returns no results
- Use the extensions parameter to filter by file type when relevant
- Use the regex parameter for pattern matching
```

Or add the same text globally in **Cursor > Settings > General > Rules for AI**.

**Windsurf:** Add the rule text above to **Windsurf > Settings > AI Rules**.

**Claude Code:** The skill-based approach handles this automatically:

```bash
ygrep install claude-code
```

This installs a skill that teaches Claude to prefer ygrep. You can combine it with MCP for best results — the MCP server provides the tool, the skill provides the behavioral guidance.

**Claude Desktop:** Does not currently support custom rules or system prompts. The AI will use ygrep when it determines it's relevant based on the tool descriptions.
