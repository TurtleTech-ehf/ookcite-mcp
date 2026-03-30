use std::path::PathBuf;

const API: &str = "https://ookcite-api.turtletech.us";

#[derive(serde::Deserialize)]
struct MeResponse {
    authenticated: bool,
    #[serde(default)]
    username: Option<String>,
    plan: String,
    lookups_remaining: u32,
    lookups_limit: u32,
}

struct McpClient {
    name: &'static str,
    config_path: PathBuf,
    /// JSON path to mcpServers object (some clients nest it differently).
    servers_key: &'static str,
}

fn detect_clients() -> Vec<McpClient> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return vec![],
    };

    let candidates = [
        #[cfg(target_os = "linux")]
        McpClient {
            name: "Claude Desktop",
            config_path: home.join(".config/Claude/claude_desktop_config.json"),
            servers_key: "mcpServers",
        },
        #[cfg(target_os = "macos")]
        McpClient {
            name: "Claude Desktop",
            config_path: home.join("Library/Application Support/Claude/claude_desktop_config.json"),
            servers_key: "mcpServers",
        },
        McpClient {
            name: "Claude Code",
            config_path: home.join(".claude/settings.json"),
            servers_key: "mcpServers",
        },
        McpClient {
            name: "Cursor",
            config_path: home.join(".cursor/mcp.json"),
            servers_key: "mcpServers",
        },
        McpClient {
            name: "Codex",
            config_path: home.join(".codex/config.json"),
            servers_key: "mcpServers",
        },
        #[cfg(target_os = "windows")]
        McpClient {
            name: "Claude Desktop",
            config_path: home.join("AppData/Roaming/Claude/claude_desktop_config.json"),
            servers_key: "mcpServers",
        },
    ];

    candidates
        .into_iter()
        .filter(|c| c.config_path.parent().is_some_and(|p| p.exists()))
        .collect()
}

fn binary_in_path() -> bool {
    std::process::Command::new("ookcite-mcp")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn build_server_entry(api_key: Option<&str>) -> serde_json::Value {
    // If the binary is in PATH (cargo install / global npm install), use it directly.
    // Otherwise use npx which handles download + caching automatically.
    let mut entry = if binary_in_path() {
        serde_json::json!({
            "command": "ookcite-mcp"
        })
    } else {
        serde_json::json!({
            "command": "npx",
            "args": ["-y", "@turtletech/ookcite-mcp"]
        })
    };
    if let Some(key) = api_key {
        entry["env"] = serde_json::json!({
            "OOKCITE_API_KEY": key
        });
    }
    entry
}

fn write_config(client: &McpClient, api_key: Option<&str>) -> Result<(), String> {
    let mut config: serde_json::Value = if client.config_path.exists() {
        let content = std::fs::read_to_string(&client.config_path)
            .map_err(|e| format!("Failed to read {}: {e}", client.config_path.display()))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse {}: {e}", client.config_path.display()))?
    } else {
        serde_json::json!({})
    };

    let servers = config
        .as_object_mut()
        .ok_or("Config is not a JSON object")?
        .entry(client.servers_key)
        .or_insert_with(|| serde_json::json!({}));

    servers
        .as_object_mut()
        .ok_or("mcpServers is not a JSON object")?
        .insert("ookcite".into(), build_server_entry(api_key));

    if let Some(parent) = client.config_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create dir: {e}"))?;
    }

    let formatted = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("Failed to serialize: {e}"))?;
    std::fs::write(&client.config_path, formatted)
        .map_err(|e| format!("Failed to write {}: {e}", client.config_path.display()))?;

    Ok(())
}

async fn validate_key(api_key: &str) -> Option<MeResponse> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{API}/api/v1/me"))
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

pub async fn run(args: &[String]) {
    println!("OokCite MCP -- Setup\n");

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
                println!(
                    "  Account: {}",
                    me.username.as_deref().unwrap_or("unknown")
                );
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

    // Detect and configure clients
    let clients = detect_clients();
    if clients.is_empty() {
        println!("No MCP clients detected.");
        println!("Manually add to your client config:\n");
        let entry = build_server_entry(api_key.as_deref());
        let snippet = serde_json::json!({ "mcpServers": { "ookcite": entry } });
        println!(
            "{}",
            serde_json::to_string_pretty(&snippet).unwrap_or_default()
        );
        return;
    }

    println!("Detected MCP clients:");
    let mut configured = 0;
    for client in &clients {
        print!("  {} ({})... ", client.name, client.config_path.display());
        match write_config(client, api_key.as_deref()) {
            Ok(()) => {
                println!("configured");
                configured += 1;
            }
            Err(e) => println!("FAILED: {e}"),
        }
    }

    println!("\n{configured}/{} clients configured.", clients.len());
    if api_key.is_none() {
        println!("\nTo add an API key later, re-run:");
        println!("  ookcite-mcp setup --key YOUR_KEY");
    }
    println!("\nRestart your MCP client to activate OokCite.");
}
