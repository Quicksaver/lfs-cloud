//! Command-line entry point for LFS Cloud.

/// Runs the CLI.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    lfscloud::run_from_env().await
}
