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

/// Run `npx add-mcp` to install the ookcite MCP server to all detected clients.
/// add-mcp handles Claude Code, Claude Desktop, Cursor, VS Code, Codex, Zed,
/// OpenCode, Cline, Gemini CLI, Goose, and more.
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

    // Use add-mcp to configure all detected clients
    if run_add_mcp(api_key.as_deref()) {
        println!("\nSetup complete.");
    } else {
        println!("\nadd-mcp failed. You can configure manually:");
        let target = find_binary()
            .unwrap_or_else(|| "npx -y @turtletech/ookcite-mcp".into());
        println!("  npx add-mcp {} --name ookcite", target);
    }

    if api_key.is_none() {
        println!("\nTo add an API key later, re-run:");
        println!("  ookcite-mcp setup --key YOUR_KEY");
    }
    println!("\nRestart your MCP clients to activate OokCite.");
}
