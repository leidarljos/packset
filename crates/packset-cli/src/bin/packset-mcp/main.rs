//! `packset-mcp`: the pack over the Model Context Protocol.
//!
//! A reader in front of the writer the seat already runs, on the port
//! `PACKSET_PORT` names. Stdio, because a seat runs this beside the agent.

mod args;
mod prompts;
mod server;

use rmcp::{transport::stdio, ServiceExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("packset-mcp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let running = server::PacksetServer::from_env().serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}
