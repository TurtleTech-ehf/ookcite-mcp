//! # OokCite MCP Server
//!
//! A [Model Context Protocol](https://modelcontextprotocol.io/) server that
//! gives LLMs the ability to validate DOIs, format citations, and catch
//! hallucinated academic references.
//!
//! Connects to the public OokCite API at <https://ookcite.turtletech.us>.
//! Basic usage requires no API key, but adding one unlocks higher rate limits.
//!
//! Implementation lives in [`server`]; argument types in [`tool_args`]; pure
//! helpers in [`collection_entries`], [`resolve_helpers`], and [`http_error`].

mod cli;
mod collection_entries;
mod constants;
mod http_error;
mod policy;
mod resolve_helpers;
mod server;
mod setup;
mod tool_args;

use rmcp::ServiceExt;

use crate::cli::{check_for_updates, validate_auth};
use crate::constants::{startup_probes_enabled, version_output};
use crate::server::Server;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{}", version_output());
        return Ok(());
    }
    if args.iter().any(|a| a == "setup") {
        setup::run(&args).await;
        return Ok(());
    }
    if args.iter().any(|a| a == "doctor") {
        // Stdout OK for human CLI; MCP never uses this path for JSON-RPC.
        println!("{}", Server::doctor_report_sync_prelude());
        let report = Server::new().doctor_report().await;
        println!("{report}");
        return Ok(());
    }

    // MCP stdio: never write banners to stdout (JSON-RPC only). Optional
    // startup probes stay on stderr and are off by default for faster connect.
    if startup_probes_enabled() {
        validate_auth().await;
        check_for_updates().await;
    } else if std::env::var("OOKCITE_API_KEY").is_err() {
        eprintln!(
            "ookcite-mcp: OOKCITE_API_KEY not set (anonymous/IP-rate-limited). \
             Set OOKCITE_STARTUP_PROBES=1 for full auth/update checks at launch. \
             Run `ookcite-mcp doctor` for a full readiness report."
        );
    }

    let server = Server::new();
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
