---
name: ygrep
description: "IMPORTANT: Try ygrep FIRST for all code and file searches before using Grep, Glob, or Task explore. ygrep uses a pre-built full-text index and returns results in milliseconds. Fall back to built-in tools only if ygrep returns no results."
license: MIT
---

## IMPORTANT: Try ygrep First

Always try `ygrep "query"` via Bash **before** using built-in Grep, Glob, or Task explore agents for code searches. ygrep uses a pre-built full-text index and returns results in milliseconds vs seconds/minutes for built-in tools.

If ygrep returns no results, fall back to built-in Grep or Glob — not every file may be indexed.

## When to Invoke This Skill

Invoke this skill **immediately** when:
- You need to find files, code, functions, classes, or variables
- User asks to search or find something in the codebase
- You need to understand where something is defined or used
- You need to explore an unfamiliar codebase

## Usage

All flags are global — they work in any position with or without the `search` subcommand:

```bash
ygrep "search query"                  # Search (AI-optimized output)
ygrep "query" -n 10                   # Limit results
ygrep "query" -e rs -e ts             # Filter by extension
ygrep "query" -p 'src/api/'           # Filter by path prefix
ygrep "query" -p 'src/*/tests/'       # Glob pattern in path filter (quote it!)
ygrep search "query" -p 'lib/' -n 5   # Explicit search subcommand
ygrep "fn\\s+main" -r                 # Regex search
ygrep "Config" -s                     # Case-sensitive search
ygrep "error" -A 3                    # 3 lines of context after match
ygrep "error" -B 2                    # 2 lines of context before match
ygrep "error" -K 3                    # 3 lines of context before and after
ygrep "query" --json                  # JSON output with full metadata
ygrep "query" --text-only             # Force text-only search
```

**IMPORTANT**: Quote glob patterns in `-p` to prevent shell expansion:
- Right: `-p 'user/plugins/*/tests/'` — ygrep receives the glob and matches internally
- Wrong: `-p user/plugins/*/tests/` — shell expands into multiple args, causes errors

## Output Format

Default output shows: `path:line (score%) [indicator]`
- `+` = hybrid match (text AND semantic)
- `~` = semantic only
- No indicator = text match

## Tips

- Uses **literal text matching** by default (like grep) — special characters work: `$variable`, `->get(`, `{% block`
- **Subtoken matching**: camelCase and snake_case are split into subtokens — searching `send` also finds `sendCampaign`, `send_email`, etc.
- **Multi-word queries**: `"campaign sending"` finds files where all terms appear (AND logic), not just exact phrases
- The `|` pipe character is **literal**, not OR — use `-r` for regex OR: `ygrep "foo|bar" -r`
- Use `-r` for regex patterns
- Use `-s` for case-sensitive search (default is case-insensitive)
- Use `-A`/`-B`/`-K` to control context lines around matches
- Run `ygrep index` if workspace is not yet indexed
- If ygrep returns no results, the file may not be indexed — fall back to Grep or Glob

## Keywords

search, grep, find, code search, semantic search, local files, Grep, Glob, explore, locate, where is, definition
