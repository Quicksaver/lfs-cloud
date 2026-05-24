//! Command-line entry point for LFS Cloud.

/// Runs the CLI.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    lfs_cloud::run_from_env().await
}
