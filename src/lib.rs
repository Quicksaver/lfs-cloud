//! Core library scaffold for LFS Cloud.
//!
//! The production CLI and server behavior is still planned. This library exists
//! so the package has a stable target for documentation, tests, and future
//! shared implementation.

pub mod error;
pub mod logging;

pub use error::{
    CliError, CliResult, ErrorCategory, LfsCloudError, LfsCloudResult, MigrationError,
    MigrationResult, RepositoryPermission, RepositoryProviderError, RepositoryProviderResult,
    SanitizedMessage, ServerError, ServerResult, StorageError, StorageResult,
};
pub use logging::{
    DEFAULT_LOG_ENV_VAR, DEFAULT_LOG_FILTER, TracingConfig, TracingInitError, init_tracing,
    tracing_filter,
};

/// Returns the placeholder message shown by the scaffold CLI.
///
/// # Examples
///
/// ```
/// assert!(lfs_cloud::scaffold_message().contains("not implemented yet"));
/// ```
#[must_use]
pub fn scaffold_message() -> &'static str {
    "lfs-cloud scaffold: CLI and server commands are not implemented yet."
}

#[cfg(test)]
mod tests {
    use super::scaffold_message;

    #[test]
    fn scaffold_message_mentions_unimplemented_commands() {
        assert!(scaffold_message().contains("not implemented yet"));
    }
}
