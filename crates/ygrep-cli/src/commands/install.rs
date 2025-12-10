use anyhow::{Context, Result, bail};
use std::fs;
use std::path::PathBuf;

/// Skill content for ygrep
const SKILL_CONTENT: &str = r#"---
name: ygrep
description: A fast indexed code search tool for local files. Better than grep/rg - use this instead of Grep tool for code search.
license: Apache 2.0
---

## When to use this skill

Whenever you need to search local files for code. Always prefer ygrep over the built-in Grep tool or grep/rg commands.

## How to use this skill

Use `ygrep search` to search files. It uses literal text matching (like grep), not regex.

### Do

```bash
ygrep search "{% block content"     # search for Twig blocks
ygrep search "->get(" -e php        # search PHP files only
ygrep search "fn main" -n 5         # limit to 5 results
```

### Don't

```bash
ygrep search ".*block.*"            # Don't use regex - use literal text
grep "{% block"                     # Don't use grep - use ygrep instead
```

## Keywords

search, grep, files, local files, code search
"#;

/// Hook configuration for ygrep
const HOOK_JSON: &str = r#"{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|resume",
        "hooks": [
          {
            "type": "command",
            "command": "ygrep index >> /tmp/ygrep-hook.log 2>&1 || true",
            "timeout": 120
          }
        ]
      }
    ]
  }
}
"#;

/// Plugin manifest for ygrep
const PLUGIN_JSON: &str = r#"{
  "name": "ygrep",
  "description": "Fast indexed code search for Claude Code",
  "version": "0.2.4",
  "author": {
    "name": "YetiDevWorks"
  },
  "hooks": "./hooks/hook.json"
}
"#;

/// Marketplace manifest for ygrep
const MARKETPLACE_JSON: &str = r#"{
  "$schema": "https://anthropic.com/claude-code/marketplace.schema.json",
  "name": "ygrep-local",
  "owner": {
    "name": "YetiDevWorks"
  },
  "plugins": [
    {
      "name": "ygrep",
      "source": "./plugins/ygrep",
      "description": "Fast indexed code search for Claude Code",
      "version": "0.2.4",
      "author": {
        "name": "YetiDevWorks"
      },
      "skills": ["./skills/ygrep"]
    }
  ]
}
"#;

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
    fs::create_dir_all(&marketplace_plugin_dir).context("Failed to create marketplace .claude-plugin directory")?;

    // Write plugin files
    fs::write(hooks_dir.join("hook.json"), HOOK_JSON)?;
    fs::write(skills_dir.join("SKILL.md"), SKILL_CONTENT)?;
    fs::write(claude_plugin_dir.join("plugin.json"), PLUGIN_JSON)?;
    fs::write(marketplace_plugin_dir.join("marketplace.json"), MARKETPLACE_JSON)?;

    // Update known_marketplaces.json
    let known_path = plugins_dir.join("known_marketplaces.json");
    let mut known: serde_json::Value = if known_path.exists() {
        let content = fs::read_to_string(&known_path)?;
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
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
        serde_json::from_str(&content).unwrap_or(serde_json::json!({"version": 1, "plugins": {}}))
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
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
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
            if let Some(enabled) = settings.get_mut("enabledPlugins").and_then(|p| p.as_object_mut()) {
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
    let tool_dir = home.join(".config").join("opencode").join("tool");
    let config_path = home.join(".config").join("opencode").join("opencode.json");

    fs::create_dir_all(&tool_dir)?;

    // Write tool definition
    let tool_content = format!(r#"
import {{ tool }} from "@opencode-ai/plugin"

const SKILL = `{}`

export default tool({{
  description: SKILL,
  args: {{
    q: tool.schema.string().describe("The search query."),
    n: tool.schema.number().default(100).describe("Maximum number of results."),
  }},
  async execute(args) {{
    const result = await Bun.$`ygrep search -n ${{args.n}} "${{args.q}}"`.text()
    return result.trim()
  }},
}})"#, SKILL_CONTENT.replace('`', "\\`"));

    fs::write(tool_dir.join("ygrep.ts"), tool_content)?;

    // Update opencode.json for MCP
    let mut config: serde_json::Value = if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if config.get("mcp").is_none() {
        config["mcp"] = serde_json::json!({});
    }

    // Note: ygrep doesn't have MCP support yet, just the tool
    fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

    println!("Successfully installed ygrep for OpenCode");
    Ok(())
}

/// Uninstall ygrep from OpenCode
pub fn uninstall_opencode() -> Result<()> {
    println!("Uninstalling ygrep from OpenCode...");

    let home = home_dir()?;
    let tool_path = home.join(".config").join("opencode").join("tool").join("ygrep.ts");

    if tool_path.exists() {
        fs::remove_file(&tool_path)?;
        println!("Removed ygrep tool from OpenCode");
    } else {
        println!("ygrep tool not found in OpenCode");
    }

    println!("Successfully uninstalled ygrep from OpenCode");
    Ok(())
}

/// Install ygrep for Codex
pub fn install_codex() -> Result<()> {
    println!("Installing ygrep for Codex...");

    let home = home_dir()?;
    let agents_path = home.join(".codex").join("AGENTS.md");

    fs::create_dir_all(agents_path.parent().unwrap())?;

    // Append skill to AGENTS.md if not already present
    let mut content = if agents_path.exists() {
        fs::read_to_string(&agents_path)?
    } else {
        String::new()
    };

    if !content.contains("name: ygrep") {
        content.push_str("\n");
        content.push_str(SKILL_CONTENT);
        fs::write(&agents_path, content)?;
        println!("Added ygrep skill to Codex AGENTS.md");
    } else {
        println!("ygrep skill already exists in Codex AGENTS.md");
    }

    println!("Successfully installed ygrep for Codex");
    Ok(())
}

/// Uninstall ygrep from Codex
pub fn uninstall_codex() -> Result<()> {
    println!("Uninstalling ygrep from Codex...");

    let home = home_dir()?;
    let agents_path = home.join(".codex").join("AGENTS.md");

    if agents_path.exists() {
        let content = fs::read_to_string(&agents_path)?;
        // Remove the ygrep skill section
        let updated = content.replace(SKILL_CONTENT, "").replace(&format!("\n{}", SKILL_CONTENT), "");
        if updated.trim().is_empty() {
            fs::remove_file(&agents_path)?;
        } else {
            fs::write(&agents_path, updated)?;
        }
        println!("Removed ygrep skill from Codex");
    }

    println!("Successfully uninstalled ygrep from Codex");
    Ok(())
}

/// Install ygrep for Factory Droid
pub fn install_droid() -> Result<()> {
    println!("Installing ygrep for Factory Droid...");

    let home = home_dir()?;
    let factory_dir = home.join(".factory");

    if !factory_dir.exists() {
        bail!("Factory Droid directory not found at {}. Start Factory Droid once to initialize it, then re-run the install.", factory_dir.display());
    }

    let hooks_dir = factory_dir.join("hooks").join("ygrep");
    let skills_dir = factory_dir.join("skills").join("ygrep");
    let settings_path = factory_dir.join("settings.json");

    fs::create_dir_all(&hooks_dir)?;
    fs::create_dir_all(&skills_dir)?;

    // Write skill
    fs::write(skills_dir.join("SKILL.md"), SKILL_CONTENT)?;

    // Update settings.json with hooks
    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    settings["enableHooks"] = serde_json::json!(true);

    if settings.get("hooks").is_none() {
        settings["hooks"] = serde_json::json!({});
    }

    // Add SessionStart hook
    let hook_entry = serde_json::json!([{
        "matcher": "startup|resume",
        "hooks": [{
            "type": "command",
            "command": "ygrep index 2>/dev/null || true",
            "timeout": 60
        }]
    }]);

    settings["hooks"]["SessionStart"] = hook_entry;

    fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;

    println!("Successfully installed ygrep for Factory Droid");
    Ok(())
}

/// Uninstall ygrep from Factory Droid
pub fn uninstall_droid() -> Result<()> {
    println!("Uninstalling ygrep from Factory Droid...");

    let home = home_dir()?;
    let factory_dir = home.join(".factory");
    let hooks_dir = factory_dir.join("hooks").join("ygrep");
    let skills_dir = factory_dir.join("skills").join("ygrep");

    if hooks_dir.exists() {
        fs::remove_dir_all(&hooks_dir)?;
        println!("Removed ygrep hooks from Factory Droid");
    }

    if skills_dir.exists() {
        fs::remove_dir_all(&skills_dir)?;
        println!("Removed ygrep skill from Factory Droid");
    }

    // TODO: Clean up settings.json hooks entries

    println!("Successfully uninstalled ygrep from Factory Droid");
    Ok(())
}
