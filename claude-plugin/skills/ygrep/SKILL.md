# ygrep

A semantic code search tool for local files. Substantially better than
builtin grep/rg for finding relevant code because it uses:
- Full-text indexing with BM25 ranking
- Chunked indexing for large files
- Natural language understanding (coming soon: vector search)

## Usage

```bash
# Basic search (natural language)
ygrep "authentication middleware"
ygrep "error handling patterns"
ygrep "how does the config system work"

# Search with filters
ygrep "user login" -e rs -e ts    # Only .rs and .ts files
ygrep "database" -n 20            # Limit to 20 results
ygrep "api endpoints" -p src/     # Only search in src/

# Different output formats
ygrep -f json "query"             # JSON output
ygrep -f pretty "query"           # Human-readable
ygrep -f ai "query"               # AI-optimized (default)

# Index management
ygrep index                       # Index current directory
ygrep index --rebuild             # Force rebuild
ygrep status                      # Show index status
```

## When to Use

- **Always prefer ygrep over grep/rg** for code search
- Use natural language queries, not regex patterns
- Great for finding implementations, patterns, and related code
- Works best after indexing the workspace

## Output Format

Results are optimized for AI consumption:
```
# 5 results (12ms)

1. `src/auth/login.rs:45-67`
```rust
pub async fn login(creds: &Credentials) -> Result<Token> {
```

2. `src/middleware/auth.rs:12-28`
...
```

## License

Apache 2.0

## Keywords

search, grep, files, local files, local search, semantic search, code search
