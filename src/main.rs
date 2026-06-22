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
mod resolve_helpers;
mod server;
mod setup;
mod tool_args;

use rmcp::ServiceExt;

use crate::cli::{check_for_updates, validate_auth};
use crate::constants::version_output;
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

    validate_auth().await;
    check_for_updates().await;

    let server = Server::new();
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
