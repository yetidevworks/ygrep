use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Skill content for ygrep (embedded at build time from claude-code/SKILL.md)
const SKILL_CONTENT: &str = include_str!(concat!(env!("OUT_DIR"), "/SKILL.md"));

/// Hook configuration for ygrep (embedded at build time from claude-code/hook.json)
const HOOK_JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/hook.json"));

/// Plugin manifest with version (embedded at build time from claude-code/plugin.json.template)
const PLUGIN_JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/plugin.json"));

/// Marketplace manifest with version (embedded at build time from claude-code/marketplace.json.template)
const MARKETPLACE_JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/marketplace.json"));

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("Could not determine home directory")
}

/// Install ygrep for Claude Code
pub fn install_claude_code() -> Result<()> {
    println!("Installing ygrep for Claude Code...");

    let home = home_dir()?;
    let plugins_dir = home.join(".claude").join("plugins");
    let marketplace_dir = plugins_dir.join("marketplaces").join("ygrep-local");

    // Create directory structure
    let plugin_dir = marketplace_dir.join("plugins").join("ygrep");
    let hooks_dir = plugin_dir.join("hooks");
    let skills_dir = plugin_dir.join("skills").join("ygrep");
    let claude_plugin_dir = plugin_dir.join(".claude-plugin");
    let marketplace_plugin_dir = marketplace_dir.join(".claude-plugin");

    fs::create_dir_all(&hooks_dir).context("Failed to create hooks directory")?;
    fs::create_dir_all(&skills_dir).context("Failed to create skills directory")?;
    fs::create_dir_all(&claude_plugin_dir).context("Failed to create .claude-plugin directory")?;
    fs::create_dir_all(&marketplace_plugin_dir)
        .context("Failed to create marketplace .claude-plugin directory")?;

    // Write plugin files
    fs::write(hooks_dir.join("hook.json"), HOOK_JSON)?;
    fs::write(skills_dir.join("SKILL.md"), SKILL_CONTENT)?;
    fs::write(claude_plugin_dir.join("plugin.json"), PLUGIN_JSON)?;
    fs::write(
        marketplace_plugin_dir.join("marketplace.json"),
        MARKETPLACE_JSON,
    )?;

    // Update known_marketplaces.json
    let known_path = plugins_dir.join("known_marketplaces.json");
    let mut known: serde_json::Value = if known_path.exists() {
        let content = fs::read_to_string(&known_path)?;
        serde_json::from_str(&content).context(format!(
            "Malformed JSON in {}. Please fix or delete the file and retry.",
            known_path.display()
        ))?
    } else {
        serde_json::json!({})
    };

    known["ygrep-local"] = serde_json::json!({
        "source": {
            "source": "directory",
            "path": marketplace_dir.to_string_lossy()
        },
        "installLocation": marketplace_dir.to_string_lossy(),
        "lastUpdated": chrono::Utc::now().to_rfc3339()
    });
    fs::write(&known_path, serde_json::to_string_pretty(&known)?)?;

    // Update installed_plugins.json
    let installed_path = plugins_dir.join("installed_plugins.json");
    let mut installed: serde_json::Value = if installed_path.exists() {
        let content = fs::read_to_string(&installed_path)?;
        serde_json::from_str(&content).context(format!(
            "Malformed JSON in {}. Please fix or delete the file and retry.",
            installed_path.display()
        ))?
    } else {
        serde_json::json!({"version": 1, "plugins": {}})
    };

    installed["plugins"]["ygrep@ygrep-local"] = serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "installedAt": chrono::Utc::now().to_rfc3339(),
        "lastUpdated": chrono::Utc::now().to_rfc3339(),
        "installPath": plugin_dir.to_string_lossy(),
        "gitCommitSha": "local",
        "isLocal": true
    });
    fs::write(&installed_path, serde_json::to_string_pretty(&installed)?)?;

    // Update settings.json to enable the plugin
    let settings_path = home.join(".claude").join("settings.json");
    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content).context(format!(
            "Malformed JSON in {}. Please fix or delete the file and retry.",
            settings_path.display()
        ))?
    } else {
        serde_json::json!({})
    };

    if settings.get("enabledPlugins").is_none() {
        settings["enabledPlugins"] = serde_json::json!({});
    }
    settings["enabledPlugins"]["ygrep@ygrep-local"] = serde_json::json!(true);
    fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;

    println!("Successfully installed ygrep for Claude Code");
    println!("Restart Claude Code to activate the plugin");
    Ok(())
}

/// Uninstall ygrep from Claude Code
pub fn uninstall_claude_code() -> Result<()> {
    println!("Uninstalling ygrep from Claude Code...");

    let home = home_dir()?;
    let plugins_dir = home.join(".claude").join("plugins");
    let marketplace_dir = plugins_dir.join("marketplaces").join("ygrep-local");

    // Remove marketplace directory
    if marketplace_dir.exists() {
        fs::remove_dir_all(&marketplace_dir)?;
        println!("Removed ygrep plugin files");
    }

    // Update known_marketplaces.json
    let known_path = plugins_dir.join("known_marketplaces.json");
    if known_path.exists() {
        let content = fs::read_to_string(&known_path)?;
        if let Ok(mut known) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(obj) = known.as_object_mut() {
                obj.remove("ygrep-local");
                fs::write(&known_path, serde_json::to_string_pretty(&known)?)?;
            }
        }
    }

    // Update installed_plugins.json
    let installed_path = plugins_dir.join("installed_plugins.json");
    if installed_path.exists() {
        let content = fs::read_to_string(&installed_path)?;
        if let Ok(mut installed) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(plugins) = installed.get_mut("plugins").and_then(|p| p.as_object_mut()) {
                plugins.remove("ygrep@ygrep-local");
                fs::write(&installed_path, serde_json::to_string_pretty(&installed)?)?;
            }
        }
    }

    // Update settings.json
    let settings_path = home.join(".claude").join("settings.json");
    if settings_path.exists() {
        let content = fs::read_to_string(&settings_path)?;
        if let Ok(mut settings) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(enabled) = settings
                .get_mut("enabledPlugins")
                .and_then(|p| p.as_object_mut())
            {
                enabled.remove("ygrep@ygrep-local");
                fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
            }
        }
    }

    println!("Successfully uninstalled ygrep from Claude Code");
    Ok(())
}

/// Install ygrep for OpenCode
pub fn install_opencode() -> Result<()> {
    println!("Installing ygrep for OpenCode...");

    let home = home_dir()?;
    let skills_dir = home
        .join(".config")
        .join("opencode")
        .join("skills")
        .join("ygrep");

    fs::create_dir_all(&skills_dir)?;
    fs::write(skills_dir.join("SKILL.md"), SKILL_CONTENT)?;

    println!("Successfully installed ygrep for OpenCode");
    Ok(())
}

/// Uninstall ygrep from OpenCode
pub fn uninstall_opencode() -> Result<()> {
    println!("Uninstalling ygrep from OpenCode...");

    let home = home_dir()?;
    let skills_dir = home
        .join(".config")
        .join("opencode")
        .join("skills")
        .join("ygrep");

    if skills_dir.exists() {
        fs::remove_dir_all(&skills_dir)?;
        println!("Removed ygrep skill from OpenCode");
    }

    // Migration: clean up old .ts tool file from previous versions
    let old_tool = home
        .join(".config")
        .join("opencode")
        .join("tool")
        .join("ygrep.ts");
    if old_tool.exists() {
        fs::remove_file(&old_tool)?;
        println!("Removed legacy ygrep.ts tool file");
    }

    println!("Successfully uninstalled ygrep from OpenCode");
    Ok(())
}

/// Install ygrep for Codex
pub fn install_codex() -> Result<()> {
    println!("Installing ygrep for Codex...");

    let home = home_dir()?;
    let skills_dir = home.join(".agents").join("skills").join("ygrep");

    fs::create_dir_all(&skills_dir)?;
    fs::write(skills_dir.join("SKILL.md"), SKILL_CONTENT)?;

    println!("Successfully installed ygrep for Codex");
    Ok(())
}

/// Install ygrep MCP server configuration
#[cfg(feature = "mcp")]
pub fn install_mcp(workspace_path: &Path, semantic: bool) -> Result<()> {
    let exe_path = strip_unc_prefix(
        &std::env::current_exe()
            .context("Could not determine ygrep binary path")?
            .to_string_lossy(),
    );

    let workspace_str = strip_unc_prefix(
        &std::fs::canonicalize(workspace_path)
            .unwrap_or_else(|_| workspace_path.to_path_buf())
            .to_string_lossy(),
    );

    let mut args: Vec<serde_json::Value> = vec![
        "mcp".into(),
        "-C".into(),
        workspace_str.as_str().into(),
    ];
    if semantic {
        args.push("--semantic".into());
    }

    let semantic_flag = if semantic { " --semantic" } else { "" };

    let config = serde_json::json!({
        "mcpServers": {
            "ygrep": {
                "command": exe_path,
                "args": args
            }
        }
    });

    // Claude Code instructions
    println!("Claude Code (run this command):\n");
    println!(
        "  claude mcp add ygrep -- {} mcp -C {}{}\n",
        exe_path, workspace_str, semantic_flag
    );

    // OpenCode instructions
    let mut opencode_cmd: Vec<serde_json::Value> = vec![
        exe_path.as_str().into(),
        "mcp".into(),
        "-C".into(),
        workspace_str.as_str().into(),
    ];
    if semantic {
        opencode_cmd.push("--semantic".into());
    }

    let opencode_config = serde_json::json!({
        "mcp": {
            "ygrep": {
                "type": "local",
                "command": opencode_cmd
            }
        }
    });

    println!("OpenCode (add to opencode.json):\n");
    println!("{}\n", serde_json::to_string_pretty(&opencode_config).unwrap());

    // Claude Desktop / Cursor / Windsurf
    println!("Claude Desktop, Cursor, Windsurf, and other MCP clients:");
    println!("Add this to your MCP configuration file:\n");
    println!("{}\n", serde_json::to_string_pretty(&config).unwrap());

    println!("Config file locations:");

    #[cfg(target_os = "macos")]
    {
        println!("  Claude Desktop: ~/Library/Application Support/Claude/claude_desktop_config.json");
    }
    #[cfg(target_os = "windows")]
    {
        println!("  Claude Desktop: %APPDATA%\\Claude\\claude_desktop_config.json");
    }
    #[cfg(target_os = "linux")]
    {
        println!("  Claude Desktop: ~/.config/Claude/claude_desktop_config.json");
    }

    println!("  Cursor:         Settings > MCP Servers / .cursor/mcp.json");
    println!("  Windsurf:       Settings > MCP");
    println!("  OpenCode:       opencode.json (project) or ~/.config/opencode/opencode.json (global)");
    println!();
    println!(
        "Or start the server directly: ygrep mcp -C {}{}",
        workspace_str, semantic_flag
    );

    Ok(())
}

/// Uninstall ygrep MCP server
#[cfg(feature = "mcp")]
pub fn uninstall_mcp() -> Result<()> {
    println!("To remove ygrep MCP server:");
    println!();
    println!("Remove the \"ygrep\" entry from the \"mcpServers\" object in your");
    println!("MCP client configuration file.");
    println!();

    #[cfg(target_os = "macos")]
    {
        println!("  Claude Desktop: ~/Library/Application Support/Claude/claude_desktop_config.json");
    }
    #[cfg(target_os = "windows")]
    {
        println!("  Claude Desktop: %APPDATA%\\Claude\\claude_desktop_config.json");
    }
    #[cfg(target_os = "linux")]
    {
        println!("  Claude Desktop: ~/.config/Claude/claude_desktop_config.json");
    }

    Ok(())
}

/// Uninstall ygrep from Codex
pub fn uninstall_codex() -> Result<()> {
    println!("Uninstalling ygrep from Codex...");

    let home = home_dir()?;
    let skills_dir = home.join(".agents").join("skills").join("ygrep");

    if skills_dir.exists() {
        fs::remove_dir_all(&skills_dir)?;
        println!("Removed ygrep skill from Codex");
    }

    // Migration: clean up old AGENTS.md entry from previous versions
    let old_agents = home.join(".codex").join("AGENTS.md");
    if old_agents.exists() {
        let content = fs::read_to_string(&old_agents)?;
        if content.contains("name: ygrep") {
            let updated = content
                .replace(SKILL_CONTENT, "")
                .replace(&format!("\n{}", SKILL_CONTENT), "");
            if updated.trim().is_empty() {
                fs::remove_file(&old_agents)?;
            } else {
                fs::write(&old_agents, updated)?;
            }
            println!("Removed legacy ygrep entry from AGENTS.md");
        }
    }

    println!("Successfully uninstalled ygrep from Codex");
    Ok(())
}

/// Strip Windows UNC path prefix (\\?\) for cleaner display
fn strip_unc_prefix(path: &str) -> String {
    path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
}
