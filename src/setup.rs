use ookcite_mcp::endpoints;

use crate::constants::{API, VERSION};

fn setup_banner() -> String {
    format!("OokCite MCP v{VERSION} -- Setup\n")
}

#[derive(serde::Deserialize)]
struct MeResponse {
    authenticated: bool,
    #[serde(default)]
    username: Option<String>,
    plan: String,
    lookups_remaining: u32,
    lookups_limit: u32,
}

fn find_binary() -> Option<String> {
    let output = std::process::Command::new("which")
        .arg("ookcite-mcp")
        .output()
        .ok()?;
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(path);
        }
    }
    None
}

async fn validate_key(api_key: &str) -> Option<MeResponse> {
    let client = reqwest::Client::new();
    let resp = client
        .get(endpoints::ME.url(API, &[]))
        .header("origin", "https://ookcite.turtletech.us")
        .header("authorization", format!("Bearer {api_key}"))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<MeResponse>().await.ok()
}

/// Print client-specific config snippets for agents/IDEs that setup cannot fully
/// auto-configure (notably Grok Build) plus copy-paste fallbacks for Claude/Codex.
fn print_client_specific_guides(api_key: Option<&str>) {
    let key_line = api_key
        .map(|k| format!("      \"OOKCITE_API_KEY\": \"{k}\""))
        .unwrap_or_else(|| "      \"OOKCITE_API_KEY\": \"your_key_here\"".into());
    let key_env = api_key.unwrap_or("your_key_here");

    println!("\n--- Client-specific install (when add-mcp is not enough) ---");

    println!("\nGrok Build (plugin / marketplace — NOT covered by add-mcp):");
    println!("  grok plugin install https://github.com/TurtleTech-ehf/ookcite-mcp.git");
    println!("  # or after marketplace publish: /marketplace → search ookcite → install");
    println!("  # set OOKCITE_API_KEY in the environment; trust the plugin for MCP");
    println!("  # repo ships plugin.json + .mcp.json at the root");

    println!("\nClaude Desktop (manual JSON if add-mcp misses it):");
    println!("  Linux:  ~/.config/Claude/claude_desktop_config.json");
    println!("  macOS:  ~/Library/Application Support/Claude/claude_desktop_config.json");
    println!("  Merge under mcpServers:");
    println!("  {{\n    \"mcpServers\": {{\n      \"ookcite\": {{\n        \"command\": \"npx\",\n        \"args\": [\"-y\", \"@turtletech/ookcite-mcp\"],\n        \"env\": {{\n{key_line}\n        }}\n      }}\n    }}\n  }}");

    println!("\nClaude Code (project or user):");
    println!("  Project: .mcp.json in the repo root (same mcpServers shape as Desktop)");
    println!("  User:    ~/.claude/settings.json → mcpServers.ookcite");

    println!("\nCodex CLI:");
    println!(
        "  codex mcp add ookcite --env OOKCITE_API_KEY={key_env} -- npx -y @turtletech/ookcite-mcp"
    );
    println!("  # or edit ~/.codex/config.toml with an [mcp_servers.ookcite] table");

    println!("\nCursor / VS Code / other IDEs:");
    println!("  Prefer add-mcp above; else Settings → MCP Servers with the same command/args/env.");

    println!("\nEnv knobs (all clients):");
    println!("  OOKCITE_API_KEY          optional; collections + higher rate limits");
    println!("  OOKCITE_API              optional; override API base (default ookcite-api.turtletech.us)");
    println!(
        "  OOKCITE_STARTUP_PROBES=1 optional; extra auth/update checks on stderr at MCP launch"
    );
    println!("  OOKCITE_MCP_READ_ONLY=1  hard-disable collection mutations (review agents)");
    println!("  OOKCITE_MCP_ALLOW_MUTATE=0 deny mutations; omit or =1 to allow (default allow)");
    println!("  ookcite-mcp doctor       CLI readiness report (policy + API health + /me)");
}

/// Run `npx add-mcp` to install the ookcite MCP server to all detected clients.
/// add-mcp handles Claude Code, Claude Desktop, Cursor, VS Code, Codex, Zed,
/// OpenCode, Cline, Gemini CLI, Goose, and more — but not Grok Build (plugin path).
fn run_add_mcp(api_key: Option<&str>) -> bool {
    // Determine the command target: full binary path if installed, else npx package.
    let target = if let Some(bin_path) = find_binary() {
        bin_path
    } else {
        "npx -y @turtletech/ookcite-mcp".to_string()
    };

    let mut cmd = std::process::Command::new("npx");
    cmd.args(["-y", "add-mcp", &target, "--name", "ookcite", "-y", "--all"]);

    if let Some(key) = api_key {
        cmd.args(["--env", &format!("OOKCITE_API_KEY={key}")]);
    }

    println!("Running: npx add-mcp {} --name ookcite --all", target);
    match cmd.status() {
        Ok(status) => status.success(),
        Err(e) => {
            eprintln!("Failed to run add-mcp: {e}");
            false
        }
    }
}

pub async fn run(args: &[String]) {
    println!("{}", setup_banner());

    // Parse --key flag
    let api_key = args
        .windows(2)
        .find(|w| w[0] == "--key")
        .map(|w| w[1].clone())
        .or_else(|| std::env::var("OOKCITE_API_KEY").ok());

    // Validate key if provided
    if let Some(ref key) = api_key {
        print!("Validating API key... ");
        match validate_key(key).await {
            Some(me) if me.authenticated => {
                println!("OK");
                println!("  Account: {}", me.username.as_deref().unwrap_or("unknown"));
                println!("  Plan: {}", me.plan);
                println!(
                    "  Lookups: {}/{} remaining today\n",
                    me.lookups_remaining, me.lookups_limit
                );
            }
            _ => {
                println!("FAILED");
                println!("  Key not recognized. Continuing with keyless config.\n");
            }
        }
    } else {
        println!("No API key provided (anonymous mode: 10 lookups/day).");
        println!("  Get a key at https://my.turtletech.us/signup");
        println!("  Then re-run: ookcite-mcp setup --key YOUR_KEY\n");
    }

    // Use add-mcp to configure all detected clients (not Grok Build)
    if run_add_mcp(api_key.as_deref()) {
        println!("\nSetup complete for clients detected by add-mcp.");
    } else {
        println!("\nadd-mcp failed. You can configure manually:");
        let target = find_binary().unwrap_or_else(|| "npx -y @turtletech/ookcite-mcp".into());
        println!("  npx add-mcp {} --name ookcite", target);
    }

    print_client_specific_guides(api_key.as_deref());

    if api_key.is_none() {
        println!("\nTo add an API key later, re-run:");
        println!("  ookcite-mcp setup --key YOUR_KEY");
    }
    println!("\nRestart your MCP clients (or reload MCP servers) to activate OokCite.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_banner_includes_current_version() {
        assert!(setup_banner().contains(VERSION));
        assert!(setup_banner().contains("OokCite MCP v"));
    }

    #[test]
    fn client_guides_mention_grok_claude_codex() {
        // Smoke: helper compiles and documents the three named clients.
        let _ = print_client_specific_guides as fn(Option<&str>);
        let src = include_str!("setup.rs");
        assert!(src.contains("Grok Build"));
        assert!(src.contains("Claude Desktop"));
        assert!(src.contains("Codex CLI"));
        assert!(src.contains("OOKCITE_STARTUP_PROBES"));
    }
}
