//! Startup probes: API key validation and update check.

use serde::Deserialize;

use ookcite_mcp::endpoints;

use crate::constants::API;

pub async fn validate_auth() {
    let api_key = match std::env::var("OOKCITE_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!(
                "ookcite-mcp: anonymous mode (10 lookups/day). \
                 Set OOKCITE_API_KEY for more."
            );
            return;
        }
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let resp = client
        .get(endpoints::ME.url(API, &[]))
        .header("origin", "https://ookcite.turtletech.us")
        .header("authorization", format!("Bearer {api_key}"))
        .send()
        .await;

    #[derive(Deserialize)]
    struct MeResponse {
        authenticated: bool,
        plan: String,
        lookups_remaining: u32,
        lookups_limit: u32,
    }

    match resp {
        Ok(r) if r.status().is_success() => match r.json::<MeResponse>().await {
            Ok(me) if me.authenticated => {
                eprintln!(
                    "ookcite-mcp: {} plan, {}/{} lookups remaining",
                    me.plan, me.lookups_remaining, me.lookups_limit
                );
            }
            _ => {
                eprintln!("ookcite-mcp: WARNING: API key not recognized");
            }
        },
        _ => {
            eprintln!("ookcite-mcp: WARNING: could not reach API for key validation");
        }
    }
}

pub async fn check_for_updates() {
    let current = env!("CARGO_PKG_VERSION");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap();

    #[derive(Deserialize)]
    struct NpmPackage {
        #[serde(rename = "dist-tags")]
        dist_tags: std::collections::HashMap<String, String>,
    }

    let resp = client
        .get("https://registry.npmjs.org/@turtletech/ookcite-mcp")
        .header("accept", "application/vnd.npm.install-v1+json")
        .send()
        .await;

    let latest = match resp {
        Ok(r) if r.status().is_success() => r
            .json::<NpmPackage>()
            .await
            .ok()
            .and_then(|p| p.dist_tags.get("latest").cloned()),
        _ => None,
    };

    if let Some(ref latest) = latest {
        if latest != current {
            eprintln!(
                "ookcite-mcp: UPDATE AVAILABLE: v{current} -> v{latest}. \
                 Run: npx @turtletech/ookcite-mcp@latest setup"
            );
        }
    }
}
