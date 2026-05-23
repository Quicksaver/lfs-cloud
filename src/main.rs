//! Command-line entry point for the LFS Cloud scaffold.

/// Runs the placeholder CLI.
fn main() -> anyhow::Result<()> {
    lfs_cloud::init_tracing(&lfs_cloud::TracingConfig::default())?;
    tracing::debug!("starting lfs-cloud scaffold CLI");

    println!("{}", lfs_cloud::scaffold_message());

    Ok(())
}
