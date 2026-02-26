use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    let version = env::var("CARGO_PKG_VERSION").unwrap();

    // Path to claude-code directory (bundled within the crate)
    let claude_code_dir = Path::new("claude-code");

    // Files that don't need version substitution
    let static_files = ["SKILL.md", "hook.json"];
    for file in &static_files {
        let src = claude_code_dir.join(file);
        let dst = Path::new(&out_dir).join(file);
        fs::copy(&src, &dst).unwrap_or_else(|e| panic!("Failed to copy {}: {}", src.display(), e));
        println!("cargo:rerun-if-changed={}", src.display());
    }

    // Template files that need version substitution
    let templates = [
        ("plugin.json.template", "plugin.json"),
        ("marketplace.json.template", "marketplace.json"),
    ];

    for (template, output) in &templates {
        let src = claude_code_dir.join(template);
        let dst = Path::new(&out_dir).join(output);

        let content = fs::read_to_string(&src)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", src.display(), e));

        let processed = content.replace("{{VERSION}}", &version);

        fs::write(&dst, processed)
            .unwrap_or_else(|e| panic!("Failed to write {}: {}", dst.display(), e));

        println!("cargo:rerun-if-changed={}", src.display());
    }
}
