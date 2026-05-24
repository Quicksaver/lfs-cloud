//! Command-line entry point for LFS Cloud.

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "lfs-cloud", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the local Git LFS-compatible HTTP server.
    Serve(ServeCommand),
}

#[derive(Debug, Parser)]
struct ServeCommand {
    /// Server config path to load.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Host or interface address to bind.
    #[arg(long)]
    host: Option<String>,

    /// TCP port to bind.
    #[arg(long)]
    port: Option<u16>,
}

/// Runs the CLI.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    lfs_cloud::init_tracing(&lfs_cloud::TracingConfig::default())?;
    tracing::debug!("starting lfs-cloud scaffold CLI");

    match Cli::parse().command {
        Some(Command::Serve(command)) => {
            lfs_cloud::serve(lfs_cloud::ServeOptions::new(
                command.config,
                command.host,
                command.port,
            ))
            .await
            .context("failed to run lfs-cloud server")?;
        }
        None => {
            println!("{}", lfs_cloud::scaffold_message());
        }
    }

    Ok(())
}
