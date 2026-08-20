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

/// Print a portable configuration snippet when automatic setup is unavailable.
fn print_manual_config(api_key: Option<&str>) {
    let key_line = api_key
        .map(|k| format!("      \"OOKCITE_API_KEY\": \"{k}\""))
        .unwrap_or_else(|| "      \"OOKCITE_API_KEY\": \"your_key_here\"".into());

    println!("\n--- Manual MCP configuration ---");
    println!("  Locate the MCP configuration in your client's documentation.");
    println!("  Merge this entry under mcpServers:");
    println!("  {{\n    \"mcpServers\": {{\n      \"ookcite\": {{\n        \"command\": \"npx\",\n        \"args\": [\"-y\", \"@turtletech/ookcite-mcp\"],\n        \"env\": {{\n{key_line}\n        }}\n      }}\n    }}\n  }}");

    println!("\nEnv knobs (all clients):");
    println!("  OOKCITE_API_KEY          optional; collections + higher rate limits");
    println!("  OOKCITE_API              optional; override API base (default ookcite-api.turtletech.us)");
    println!(
        "  OOKCITE_STARTUP_PROBES=1 optional; extra auth/update checks on stderr at MCP launch"
    );
    println!("  OOKCITE_MCP_READ_ONLY=1  hard-disable collection mutations (review automation)");
    println!("  OOKCITE_MCP_ALLOW_MUTATE=0 deny mutations; omit or =1 to allow (default allow)");
    println!("  ookcite-mcp doctor       CLI readiness report (policy + API health + /me)");
}

/// Run `npx add-mcp` to install the OokCite MCP server in detected clients.
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
        println!("No API key provided (anonymous mode: 20 lookups/day).");
        println!("  Get a key at https://my.turtletech.us/signup");
        println!("  Then re-run: ookcite-mcp setup --key YOUR_KEY\n");
    }

    // Use add-mcp to configure all detected clients.
    if run_add_mcp(api_key.as_deref()) {
        println!("\nSetup complete for clients detected by add-mcp.");
    } else {
        println!("\nadd-mcp failed. You can configure manually:");
        let target = find_binary().unwrap_or_else(|| "npx -y @turtletech/ookcite-mcp".into());
        println!("  npx add-mcp {} --name ookcite", target);
    }

    print_manual_config(api_key.as_deref());

    if api_key.is_none() {
        println!("\nTo add an API key later, re-run:");
        println!("  ookcite-mcp setup --key YOUR_KEY");
    }
    println!("\nRestart your MCP clients (or reload MCP servers) to activate OokCite.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use ookcite_mcp::credentials::CredentialReference;

    #[test]
    fn setup_banner_includes_current_version() {
        assert!(setup_banner().contains(VERSION));
        assert!(setup_banner().contains("OokCite MCP v"));
    }

    #[test]
    fn manual_config_documents_portable_setup() {
        let _ = print_manual_config as fn(Option<&str>);
        let src = include_str!("setup.rs");
        assert!(src.contains("Manual MCP configuration"));
        assert!(src.contains("mcpServers"));
        assert!(src.contains("@turtletech/ookcite-mcp"));
        assert!(src.contains("OOKCITE_STARTUP_PROBES"));
    }

    #[test]
    fn connect_options_default_to_platform_storage_and_support_explicit_sinks() {
        let defaults = parse_connect_options(&["ookcite-mcp".into(), "setup".into(), "--connect".into()])
            .unwrap()
            .unwrap();
        assert_eq!(defaults.destination, CredentialDestination::Platform);
        assert!(!defaults.device_only);
        assert!(!defaults.replace_credential);

        let helper = parse_connect_options(&[
            "ookcite-mcp".into(),
            "setup".into(),
            "--connect".into(),
            "--store-command".into(),
            "credential-cli store ookcite".into(),
            "--retrieve-command".into(),
            "credential-cli read ookcite".into(),
            "--device".into(),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            helper.destination,
            CredentialDestination::Command {
                store: "credential-cli store ookcite".into(),
                retrieve: "credential-cli read ookcite".into(),
            }
        );
        assert!(helper.device_only);

        let file = parse_connect_options(&[
            "ookcite-mcp".into(),
            "setup".into(),
            "--connect".into(),
            "--credential-file".into(),
            "/tmp/ookcite-key".into(),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            file.destination,
            CredentialDestination::File("/tmp/ookcite-key".into())
        );
    }

    #[test]
    fn connect_options_reject_ambiguous_or_incomplete_storage_flags() {
        let both = vec![
            "ookcite-mcp".into(),
            "setup".into(),
            "--connect".into(),
            "--credential-file".into(),
            "/tmp/key".into(),
            "--store-command".into(),
            "store".into(),
            "--retrieve-command".into(),
            "read".into(),
        ];
        assert!(parse_connect_options(&both).is_err());
        let missing_retrieval = vec![
            "ookcite-mcp".into(),
            "setup".into(),
            "--connect".into(),
            "--store-command".into(),
            "store".into(),
        ];
        assert!(parse_connect_options(&missing_retrieval).is_err());
    }

    #[test]
    fn connected_add_mcp_arguments_contain_references_and_journey_but_no_key() {
        let journey = "018f47e2-19c3-7b8a-8f62-62fe39151ec4";
        for reference in [
            CredentialReference::Platform {
                service: "ookcite-mcp".into(),
                account: "default".into(),
            },
            CredentialReference::Command {
                retrieve_command: "credential-cli read ookcite".into(),
            },
            CredentialReference::File {
                path: "/tmp/ookcite-key".into(),
            },
        ] {
            let arguments = connected_add_mcp_arguments("ookcite-mcp", &reference, journey);
            let rendered = format!("{arguments:?}");
            assert!(rendered.contains("OOKCITE_JOURNEY_ID"));
            assert!(rendered.contains(journey));
            assert!(!rendered.contains("ookc_example-secret-value"));
            assert!(!arguments.iter().any(|argument| argument == "--key"));
        }
    }

    #[test]
    fn automatic_configuration_refuses_an_existing_named_server() {
        assert!(listing_contains_ookcite(
            "Cursor\n  ookcite  stdio  npx -y @turtletech/ookcite-mcp\n"
        ));
        assert!(listing_contains_ookcite("ookcite: configured\n"));
        assert!(!listing_contains_ookcite("context7\nother-citation-tool\n"));
    }
}
