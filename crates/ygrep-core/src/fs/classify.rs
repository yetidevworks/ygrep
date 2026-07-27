//! Rules for deciding whether a file is worth indexing.
//!
//! One implementation for every caller: the indexing walk, `ygrep watch`, and the
//! dashboard's watch manager all used to carry their own copy of the extension list,
//! and the two watch copies quietly skipped the binary and generated-file checks the
//! walk applied, so a watched workspace could pick up files a rebuild would drop.

use std::path::Path;

use crate::config::IndexerConfig;

/// Bytes sampled from the head of a file to classify it as text or binary
const BINARY_SNIFF_BYTES: usize = 8192;

/// Bytes sampled when deciding whether a file is minified or bundled
pub(crate) const MINIFIED_SNIFF_BYTES: usize = 65536;

/// Extensions we always treat as text
const TEXT_EXTENSIONS: &[&str] = &[
    // Programming languages
    "rs",
    "py",
    "js",
    "ts",
    "jsx",
    "tsx",
    "mjs",
    "mts",
    "cjs",
    "cts",
    "go",
    "rb",
    "php",
    "java",
    "c",
    "cpp",
    "cc",
    "h",
    "hpp",
    "hh",
    "cs",
    "swift",
    "kt",
    "scala",
    "clj",
    "ex",
    "exs",
    "erl",
    "hs",
    "ml",
    "fs",
    "r",
    "jl",
    "lua",
    "pl",
    "pm",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "cmd",
    // Web/markup
    "html",
    "htm",
    "css",
    "scss",
    "sass",
    "less",
    "xml",
    "json",
    "yaml",
    "yml",
    "toml",
    // Templates
    "twig",
    "blade",
    "ejs",
    "hbs",
    "handlebars",
    "mustache",
    "pug",
    "jade",
    "erb",
    "haml",
    "njk",
    "nunjucks",
    "jinja",
    "jinja2",
    "liquid",
    "eta",
    // Documentation
    "md",
    "markdown",
    "rst",
    "txt",
    "csv",
    "sql",
    "graphql",
    "gql",
    // Config/build
    "dockerfile",
    "makefile",
    "cmake",
    "gradle",
    "pom",
    "ini",
    "conf",
    "cfg",
    // Frontend frameworks
    "vue",
    "svelte",
    "astro",
    // Infrastructure
    "tf",
    "hcl",
    "nix",
    // Data formats
    "proto",
    "thrift",
    "avsc",
    // Git/editor config
    "gitignore",
    "gitattributes",
    "editorconfig",
    "env",
];

/// Extensionless filenames we always treat as text
const TEXT_FILENAMES: &[&str] = &[
    "dockerfile",
    "makefile",
    "rakefile",
    "gemfile",
    "procfile",
    "readme",
    "license",
    "copying",
    "authors",
    "changelog",
    "todo",
    "contributing",
];

/// Dotfiles worth indexing, matched with the leading dot removed.
///
/// A dotfile has no extension as far as `Path` is concerned — `.gitignore` is all
/// stem — so the extension list never matched one, and the walk skipped every hidden
/// file besides. Searching for a rule you wrote in `.gitignore` or a variable you set
/// in `.env` is exactly the kind of thing a code search is for.
const TEXT_DOTFILES: &[&str] = &[
    "babelrc",
    "browserslistrc",
    "dockerignore",
    "editorconfig",
    "env",
    "eslintrc",
    "gitattributes",
    "gitignore",
    "gitmodules",
    "npmrc",
    "nvmrc",
    "prettierrc",
    "stylelintrc",
];

/// Hidden directories that hold source rather than machine state.
///
/// Everything else beginning with a dot is pruned: it is cache, credentials, or tool
/// state, and there are far too many of those to list.
pub(crate) const TEXT_DOT_DIRS: &[&str] = &[
    ".github",
    ".gitlab",
    ".circleci",
    ".devcontainer",
    ".husky",
    ".changeset",
];

/// How a filename alone classifies a file, before any content is read
enum NameClass {
    /// A name we recognise as text
    Text,
    /// A name we recognise as something we never index
    Rejected,
    /// Unrecognised — only the file's own bytes can decide
    Unknown,
}

/// Whether a path passes every check that doesn't need the file's content.
///
/// Reads at most the head of the file, and only for names it doesn't recognise.
/// Whether a file is minified is decided later, from the content the indexer reads
/// anyway — see [`content_is_minified`].
pub fn is_indexable(path: &Path, config: &IndexerConfig) -> bool {
    if !matches_extension_filter(path, &config.include_extensions) {
        return false;
    }

    is_text_file(path)
}

/// Whether a path satisfies the configured extension allowlist (empty = all)
fn matches_extension_filter(path: &Path, include_extensions: &[String]) -> bool {
    if include_extensions.is_empty() {
        return true;
    }

    match path.extension() {
        Some(ext) => {
            let ext = ext.to_string_lossy().to_lowercase();
            include_extensions.iter().any(|e| e.to_lowercase() == ext)
        }
        None => false,
    }
}

/// Whether a file is likely text
pub fn is_text_file(path: &Path) -> bool {
    match classify_name(path) {
        NameClass::Text => true,
        NameClass::Rejected => false,
        // Fall back to checking the first bytes for binary content. Read only the head:
        // reading the whole file here would pull a multi-gigabyte blob into memory just
        // to inspect its first few kilobytes.
        NameClass::Unknown => match read_head(path, BINARY_SNIFF_BYTES) {
            Ok(head) => !head.contains(&0),
            Err(_) => false,
        },
    }
}

fn classify_name(path: &Path) -> NameClass {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return NameClass::Unknown;
    };
    let name_lower = name.to_lowercase();

    if let Some(stripped) = name_lower.strip_prefix('.') {
        // A dotfile is only indexed when we recognise it by name: sniffing every hidden
        // file would mean opening every credential store and editor cache in the tree.
        if TEXT_DOTFILES.contains(&stripped) || stripped.starts_with("env.") {
            return NameClass::Text;
        }
        if has_text_extension(&name_lower) {
            return NameClass::Text;
        }
        return NameClass::Rejected;
    }

    if has_text_extension(&name_lower) || TEXT_FILENAMES.contains(&name_lower.as_str()) {
        return NameClass::Text;
    }

    NameClass::Unknown
}

fn has_text_extension(name_lower: &str) -> bool {
    match name_lower.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => TEXT_EXTENSIONS.contains(&ext),
        _ => false,
    }
}

/// Read at most `limit` bytes from the start of a file
fn read_head(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    let file = std::fs::File::open(path)?;
    let mut buf = Vec::with_capacity(limit.min(8192));
    file.take(limit as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Whether a file looks minified, bundled, or otherwise machine-generated.
///
/// Reads the head of the file. Callers that already have the content should use
/// [`content_is_minified`] instead.
pub fn is_minified_file(path: &Path, max_avg_line_len: usize) -> bool {
    if max_avg_line_len == 0 {
        return false;
    }

    match read_head(path, MINIFIED_SNIFF_BYTES) {
        Ok(head) => head_is_minified(&head, max_avg_line_len),
        Err(_) => false,
    }
}

/// Whether content looks minified, bundled, or otherwise machine-generated.
///
/// Generated assets (bundled JS, minified CSS, compact JSON data, TextMate grammars)
/// are enormous relative to their usefulness in a code search, and they pack many more
/// bytes per line than hand-written source. Average line length over the head of the
/// file separates the two cleanly: measured across real projects this excludes 12-30%
/// of indexed bytes in web projects while matching nothing in a plain Rust project.
///
/// A threshold of 0 disables the check.
pub fn content_is_minified(content: &[u8], max_avg_line_len: usize) -> bool {
    if max_avg_line_len == 0 {
        return false;
    }

    let head = &content[..content.len().min(MINIFIED_SNIFF_BYTES)];
    head_is_minified(head, max_avg_line_len)
}

fn head_is_minified(head: &[u8], max_avg_line_len: usize) -> bool {
    if head.is_empty() {
        return false;
    }

    // A file whose head has no newline at all is only suspicious once it is big enough
    // that a single line is implausible for hand-written source.
    let newlines = head.iter().filter(|b| **b == b'\n').count();
    let lines = newlines.max(1);

    head.len() / lines > max_avg_line_len
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn known_extensions_need_no_file_to_classify() {
        assert!(is_text_file(&PathBuf::from("/nowhere/main.rs")));
        assert!(is_text_file(&PathBuf::from("/nowhere/Makefile")));
        assert!(!is_text_file(&PathBuf::from("/nowhere/blob.wasm")));
    }

    #[test]
    fn useful_dotfiles_are_text_and_the_rest_are_not() {
        assert!(is_text_file(&PathBuf::from("/repo/.gitignore")));
        assert!(is_text_file(&PathBuf::from("/repo/.editorconfig")));
        assert!(is_text_file(&PathBuf::from("/repo/.env")));
        assert!(is_text_file(&PathBuf::from("/repo/.env.production")));
        assert!(is_text_file(&PathBuf::from("/repo/.eslintrc.json")));

        assert!(!is_text_file(&PathBuf::from("/repo/.DS_Store")));
        assert!(!is_text_file(&PathBuf::from("/repo/.credentials")));
    }

    #[test]
    fn unknown_names_fall_back_to_a_binary_sniff() {
        let temp = tempdir().unwrap();

        let text = temp.path().join("mystery");
        std::fs::write(&text, "just some words\n").unwrap();
        assert!(is_text_file(&text));

        let binary = temp.path().join("mystery.blob");
        std::fs::write(&binary, [0u8, 1, 2, 3]).unwrap();
        assert!(!is_text_file(&binary));
    }

    #[test]
    fn the_extension_allowlist_is_honoured() {
        let config = IndexerConfig {
            include_extensions: vec!["rs".into()],
            ..Default::default()
        };

        assert!(is_indexable(Path::new("/repo/main.rs"), &config));
        assert!(!is_indexable(Path::new("/repo/main.py"), &config));
    }

    #[test]
    fn minified_content_is_detected_and_source_is_not() {
        let bundle = format!("var a=1;{}\n", "x".repeat(5000));
        assert!(content_is_minified(bundle.as_bytes(), 400));

        let source: String = (0..200).map(|i| format!("    let x{i} = {i};\n")).collect();
        assert!(!content_is_minified(source.as_bytes(), 400));

        // A threshold of zero disables the check.
        assert!(!content_is_minified(bundle.as_bytes(), 0));
    }

    #[test]
    fn the_minified_check_reads_only_the_head_of_a_file() {
        let temp = tempdir().unwrap();
        let big = temp.path().join("big.js");
        std::fs::write(&big, vec![b'a'; 200_000]).unwrap();

        assert!(is_minified_file(&big, 400));
        assert_eq!(read_head(&big, 8192).unwrap().len(), 8192);
    }
}
