use ookcite_mcp::connect::{
    finalize_installation, generate_pkce, open_browser_or_device, poll_device_until_authorized,
    random_journey_id, random_token, ConnectMode, DashboardClient, LoopbackListener,
    StartBrowserRequest, StartDeviceRequest, SystemBrowser,
};
use ookcite_mcp::credentials::{
    CredentialReference, CredentialSink, PlatformCredentialSink, ProtectedFileSink,
    StoreCommandSink, SystemKeyring,
};
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum CredentialDestination {
    Platform,
    Command { store: String, retrieve: String },
    File(std::path::PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectOptions {
    destination: CredentialDestination,
    device_only: bool,
    replace_credential: bool,
    replace_config: bool,
}

fn parse_connect_options(args: &[String]) -> anyhow::Result<Option<ConnectOptions>> {
    if !args.iter().any(|argument| argument == "--connect") {
        return Ok(None);
    }
    if args.iter().any(|argument| argument == "--key") {
        anyhow::bail!("--connect and --key cannot be used together")
    }
    let flag_value = |name: &str| -> anyhow::Result<Option<String>> {
        let Some(position) = args.iter().position(|argument| argument == name) else {
            return Ok(None);
        };
        let value = args
            .get(position + 1)
            .filter(|value| !value.starts_with("--"))
            .ok_or_else(|| anyhow::anyhow!("{name} requires a value"))?;
        Ok(Some(value.clone()))
    };
    let store = flag_value("--store-command")?;
    let retrieve = flag_value("--retrieve-command")?;
    let file = flag_value("--credential-file")?;
    let destination = match (store, retrieve, file) {
        (None, None, None) => CredentialDestination::Platform,
        (Some(store), Some(retrieve), None) => CredentialDestination::Command { store, retrieve },
        (None, None, Some(path)) => CredentialDestination::File(path.into()),
        (Some(_), None, None) | (None, Some(_), None) => {
            anyhow::bail!("--store-command and --retrieve-command must be used together")
        }
        _ => anyhow::bail!("select only one credential destination"),
    };
    Ok(Some(ConnectOptions {
        destination,
        device_only: args.iter().any(|argument| argument == "--device"),
        replace_credential: args
            .iter()
            .any(|argument| argument == "--replace-credential"),
        replace_config: args.iter().any(|argument| argument == "--replace-config"),
    }))
}

fn connected_add_mcp_arguments(
    target: &str,
    reference: &CredentialReference,
    journey_id: &str,
) -> Vec<String> {
    let mut arguments = vec![
        target.into(),
        "--name".into(),
        "ookcite".into(),
        "-y".into(),
        "--all".into(),
    ];
    for (name, value) in ookcite_mcp::connect::connection_config_env(reference, journey_id) {
        arguments.push("--env".into());
        arguments.push(format!("{name}={value}"));
    }
    arguments
}

fn listing_contains_ookcite(listing: &str) -> bool {
    listing.split_whitespace().any(|word| {
        word.trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '-')
            .eq_ignore_ascii_case("ookcite")
    })
}

fn existing_ookcite_configuration() -> anyhow::Result<bool> {
    let output = std::process::Command::new("npx")
        .args(["-y", "add-mcp", "list"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|_| anyhow::anyhow!("could not inspect existing MCP configuration"))?;
    if !output.status.success() {
        anyhow::bail!("could not inspect existing MCP configuration")
    }
    Ok(listing_contains_ookcite(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn run_connected_add_mcp(
    reference: &CredentialReference,
    journey_id: &str,
    replace_config: bool,
) -> anyhow::Result<()> {
    if !replace_config && existing_ookcite_configuration()? {
        anyhow::bail!("OokCite is already configured; use --replace-config to replace it")
    }
    let target = find_binary().unwrap_or_else(|| "npx -y @turtletech/ookcite-mcp".into());
    let arguments = connected_add_mcp_arguments(&target, reference, journey_id);
    let status = std::process::Command::new("npx")
        .args(["-y", "add-mcp"])
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|_| anyhow::anyhow!("add-mcp could not be started"))?;
    if !status.success() {
        anyhow::bail!("add-mcp failed")
    }
    Ok(())
}

async fn exchange_connected_credential(
    dashboard: &DashboardClient,
    device_only: bool,
) -> anyhow::Result<(ookcite_mcp::connect::ExchangeResult, String)> {
    let pkce = generate_pkce();
    let state = random_token();
    let journey_id = random_journey_id();

    let authorization_code = if device_only {
        let started = dashboard
            .start_device(StartDeviceRequest {
                journey_id: &journey_id,
                code_challenge: &pkce.challenge,
                code_challenge_method: "S256",
                state: &state,
            })
            .await?;
        println!("Open: {}", started.verification_uri);
        println!("Enter code: {}", started.user_code);
        poll_device_until_authorized(dashboard, &started, &state).await?
    } else {
        let listener = LoopbackListener::bind(0).await?;
        let callback_port = listener.local_addr()?.port();
        let started = dashboard
            .start_browser(StartBrowserRequest {
                journey_id: &journey_id,
                code_challenge: &pkce.challenge,
                code_challenge_method: "S256",
                state: &state,
                callback_port,
            })
            .await?;
        match open_browser_or_device(&SystemBrowser, &started.authorization_url) {
            ConnectMode::Browser => {
                println!("Complete the OokCite connection in your browser.");
                match listener
                    .wait_for_callback(&state, std::time::Duration::from_secs(started.expires_in))
                    .await
                {
                    Ok(code) => code,
                    Err(error) => {
                        let _ = dashboard.cancel(&started.authorization_id, &state).await;
                        return Err(error.into());
                    }
                }
            }
            ConnectMode::Device => {
                let _ = dashboard.cancel(&started.authorization_id, &state).await;
                let device = dashboard
                    .start_device(StartDeviceRequest {
                        journey_id: &journey_id,
                        code_challenge: &pkce.challenge,
                        code_challenge_method: "S256",
                        state: &state,
                    })
                    .await?;
                println!("Open: {}", device.verification_uri);
                println!("Enter code: {}", device.user_code);
                poll_device_until_authorized(dashboard, &device, &state).await?
            }
        }
    };
    let exchange = dashboard
        .exchange(&authorization_code, &pkce.verifier, &state)
        .await?;
    Ok((exchange, journey_id))
}

#[cfg(windows)]
fn store_command(command: String) -> Vec<String> {
    vec!["cmd".into(), "/C".into(), command]
}

#[cfg(not(windows))]
fn store_command(command: String) -> Vec<String> {
    vec!["sh".into(), "-c".into(), command]
}

async fn install_connected_with_sink<S: CredentialSink + ?Sized>(
    dashboard: &DashboardClient,
    exchange: &ookcite_mcp::connect::ExchangeResult,
    journey_id: &str,
    options: &ConnectOptions,
    sink: &S,
) -> anyhow::Result<CredentialReference> {
    finalize_installation(
        dashboard,
        &crate::constants::api_base_url(),
        exchange,
        sink,
        |reference| run_connected_add_mcp(reference, journey_id, options.replace_config),
    )
    .await
}

async fn run_connect(options: ConnectOptions) -> anyhow::Result<()> {
    let dashboard_api = std::env::var("OOKCITE_DASHBOARD_API")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "https://dashboard-api.turtletech.us/api/v1".into());
    let dashboard = DashboardClient::new(dashboard_api);
    let (exchange, journey_id) =
        exchange_connected_credential(&dashboard, options.device_only).await?;

    let reference = match &options.destination {
        CredentialDestination::Platform => {
            let backend = SystemKeyring;
            let sink = PlatformCredentialSink::new(&backend, "ookcite-mcp", "default")
                .allow_replace(options.replace_credential);
            install_connected_with_sink(&dashboard, &exchange, &journey_id, &options, &sink).await?
        }
        CredentialDestination::Command { store, retrieve } => {
            let sink = StoreCommandSink {
                command: store_command(store.clone()),
                retrieve_command: retrieve.clone(),
            };
            install_connected_with_sink(&dashboard, &exchange, &journey_id, &options, &sink).await?
        }
        CredentialDestination::File(path) => {
            let sink = ProtectedFileSink::new(path);
            install_connected_with_sink(&dashboard, &exchange, &journey_id, &options, &sink).await?
        }
    };
    println!(
        "OokCite connected with a {:?} credential reference.",
        reference
    );
    println!("Restart MCP clients or reload their MCP servers to activate OokCite.");
    Ok(())
}

pub async fn run(args: &[String]) {
    println!("{}", setup_banner());

    match parse_connect_options(args) {
        Ok(Some(options)) => {
            if let Err(error) = run_connect(options).await {
                eprintln!("OokCite connection failed: {error}");
            }
            return;
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("OokCite setup options are invalid: {error}");
            return;
        }
    }

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
        let defaults =
            parse_connect_options(&["ookcite-mcp".into(), "setup".into(), "--connect".into()])
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
