//! Shared error and result types for the LFS Cloud library.
//!
//! The project is still a single Rust package, so these types provide the
//! domain boundaries that later modules can share before the codebase is split
//! into CLI, server, provider, storage, and migration crates.

use std::{fmt, path::PathBuf};

/// Project-wide result type for operations that can fail with [`LfsCloudError`].
pub type Result<T> = std::result::Result<T, LfsCloudError>;

/// Result type for command-line interface operations.
pub type CliResult<T> = std::result::Result<T, CliError>;

/// Result type for server configuration, routing, and request handling.
pub type ServerResult<T> = std::result::Result<T, ServerError>;

/// Result type for repository-provider operations.
pub type RepositoryProviderResult<T> = std::result::Result<T, RepositoryProviderError>;

/// Result type for storage-provider operations.
pub type StorageResult<T> = std::result::Result<T, StorageError>;

/// Result type for migration planning and execution operations.
pub type MigrationResult<T> = std::result::Result<T, MigrationError>;

/// High-level area responsible for an LFS Cloud failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCategory {
    /// A command-line input, output, or process-boundary failure.
    Cli,
    /// A server configuration, routing, authorization, or request failure.
    Server,
    /// A repository-provider identity or permission-check failure.
    RepositoryProvider,
    /// A storage-provider object or backend failure.
    Storage,
    /// A migration discovery, transfer, or safety failure.
    Migration,
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cli => formatter.write_str("cli"),
            Self::Server => formatter.write_str("server"),
            Self::RepositoryProvider => formatter.write_str("repository_provider"),
            Self::Storage => formatter.write_str("storage"),
            Self::Migration => formatter.write_str("migration"),
        }
    }
}

/// Top-level error type for library operations that cross domain boundaries.
#[derive(Debug, thiserror::Error)]
pub enum LfsCloudError {
    /// Failure from command-line input, output, or process-boundary handling.
    #[error("cli error: {source}")]
    Cli {
        /// Underlying command-line failure.
        #[from]
        source: CliError,
    },

    /// Failure from server configuration, routing, authorization, or handling.
    #[error("server error: {source}")]
    Server {
        /// Underlying server failure.
        #[from]
        source: ServerError,
    },

    /// Failure from repository-provider identity or permission checks.
    #[error("repository provider error: {source}")]
    RepositoryProvider {
        /// Underlying repository-provider failure.
        #[from]
        source: RepositoryProviderError,
    },

    /// Failure from storage-provider object operations.
    #[error("storage error: {source}")]
    Storage {
        /// Underlying storage-provider failure.
        #[from]
        source: StorageError,
    },

    /// Failure from migration discovery, transfer, or safety checks.
    #[error("migration error: {source}")]
    Migration {
        /// Underlying migration failure.
        #[from]
        source: MigrationError,
    },
}

impl LfsCloudError {
    /// Returns the high-level domain responsible for this failure.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::{CliError, ErrorCategory, LfsCloudError};
    ///
    /// let error = LfsCloudError::from(CliError::InvalidArguments {
    ///     message: "missing --config".to_owned(),
    /// });
    ///
    /// assert_eq!(error.category(), ErrorCategory::Cli);
    /// ```
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::Cli { .. } => ErrorCategory::Cli,
            Self::Server { .. } => ErrorCategory::Server,
            Self::RepositoryProvider { .. } => ErrorCategory::RepositoryProvider,
            Self::Storage { .. } => ErrorCategory::Storage,
            Self::Migration { .. } => ErrorCategory::Migration,
        }
    }
}

/// Error type for command-line interface operations.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The user supplied invalid or incomplete command-line input.
    #[error("invalid arguments: {message}")]
    InvalidArguments {
        /// Human-readable explanation of the invalid input.
        message: String,
    },

    /// A filesystem or process I/O operation failed at the CLI boundary.
    #[error("{context}: {source}")]
    Io {
        /// Operation being attempted when I/O failed.
        context: String,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
}

impl CliError {
    /// Returns the high-level domain responsible for this failure.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        ErrorCategory::Cli
    }
}

/// Error type for server configuration, routing, and request handling.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Server configuration is syntactically valid but semantically invalid.
    #[error("invalid configuration: {message}")]
    InvalidConfiguration {
        /// Human-readable configuration validation failure.
        message: String,
    },

    /// No configured repository mapping matches an incoming LFS route.
    #[error("no configured repository route matches {path}")]
    RouteNotConfigured {
        /// Request path that did not match a configured repository route.
        path: String,
    },

    /// A request was authenticated incorrectly or lacked required access.
    #[error("unauthorized request: {reason}")]
    Unauthorized {
        /// Human-readable reason the request was denied.
        reason: String,
    },

    /// A repository-provider operation failed while handling a server request.
    #[error("repository provider failed: {source}")]
    RepositoryProvider {
        /// Underlying repository-provider failure.
        #[from]
        source: RepositoryProviderError,
    },

    /// A storage-provider operation failed while handling a server request.
    #[error("storage provider failed: {source}")]
    Storage {
        /// Underlying storage-provider failure.
        #[from]
        source: StorageError,
    },
}

impl ServerError {
    /// Returns the high-level domain responsible for this failure.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        ErrorCategory::Server
    }
}

/// Repository access level required by an LFS Cloud operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryPermission {
    /// Read access, sufficient for LFS object downloads.
    Read,
    /// Write access, required for LFS object uploads.
    Write,
    /// Administrative access, reserved for repository-level management actions.
    Admin,
}

impl fmt::Display for RepositoryPermission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read => formatter.write_str("read"),
            Self::Write => formatter.write_str("write"),
            Self::Admin => formatter.write_str("admin"),
        }
    }
}

/// Error type for repository-provider identity and permission operations.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryProviderError {
    /// The provider operation requires an authenticated repository identity.
    #[error("{provider} authentication required")]
    AuthenticationRequired {
        /// Configured repository provider ID.
        provider: String,
    },

    /// The authenticated user lacks the required repository permission.
    #[error("{provider} denied {required} access to {owner}/{repo}")]
    PermissionDenied {
        /// Configured repository provider ID.
        provider: String,
        /// Repository owner or namespace.
        owner: String,
        /// Repository name.
        repo: String,
        /// Permission required by the requested operation.
        required: RepositoryPermission,
    },

    /// The repository provider could not find the requested repository.
    #[error("{provider} repository not found: {owner}/{repo}")]
    RepositoryNotFound {
        /// Configured repository provider ID.
        provider: String,
        /// Repository owner or namespace.
        owner: String,
        /// Repository name.
        repo: String,
    },

    /// Provider access is blocked until organization SSO is authorized.
    #[error("{provider} requires SSO authorization for {organization}")]
    SsoRequired {
        /// Configured repository provider ID.
        provider: String,
        /// Organization or namespace requiring SSO authorization.
        organization: String,
    },

    /// The configured repository provider type is not implemented.
    #[error("unsupported repository provider type: {provider_type}")]
    Unsupported {
        /// Provider type from configuration.
        provider_type: String,
    },

    /// The repository provider returned an unexpected upstream failure.
    #[error("{provider} upstream failure{status_text}: {message}", status_text = status_text(*status))]
    Upstream {
        /// Configured repository provider ID.
        provider: String,
        /// Optional upstream HTTP status code.
        status: Option<u16>,
        /// Sanitized upstream error message.
        message: String,
    },
}

impl RepositoryProviderError {
    /// Returns the high-level domain responsible for this failure.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        ErrorCategory::RepositoryProvider
    }
}

/// Error type for storage-provider object and backend operations.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The storage provider operation requires valid backend credentials.
    #[error("{provider} storage authentication required")]
    AuthenticationRequired {
        /// Configured storage provider ID.
        provider: String,
    },

    /// The requested object is not present in the configured storage backend.
    #[error("{provider} object not found: sha256:{oid} ({size} bytes)")]
    ObjectNotFound {
        /// Configured storage provider ID.
        provider: String,
        /// Hex SHA-256 object identifier without the `sha256:` prefix.
        oid: String,
        /// Expected object size in bytes.
        size: u64,
    },

    /// The storage backend reported a conflicting existing object or metadata record.
    #[error("{provider} object conflict: sha256:{oid}")]
    Conflict {
        /// Configured storage provider ID.
        provider: String,
        /// Hex SHA-256 object identifier without the `sha256:` prefix.
        oid: String,
    },

    /// The storage backend quota is exhausted or the operation would exceed it.
    #[error("{provider} quota exceeded: {message}")]
    QuotaExceeded {
        /// Configured storage provider ID.
        provider: String,
        /// Sanitized quota failure details.
        message: String,
    },

    /// Stored or staged bytes did not match the expected LFS pointer metadata.
    #[error(
        "integrity mismatch: expected sha256:{expected_oid} ({expected_size} bytes), got sha256:{actual_oid} ({actual_size} bytes)"
    )]
    IntegrityMismatch {
        /// Expected hex SHA-256 object identifier without the `sha256:` prefix.
        expected_oid: String,
        /// Expected object size in bytes.
        expected_size: u64,
        /// Actual hex SHA-256 object identifier without the `sha256:` prefix.
        actual_oid: String,
        /// Actual object size in bytes.
        actual_size: u64,
    },

    /// The operation failed in a way that may succeed if retried later.
    #[error("{provider} retryable storage failure: {message}")]
    Retryable {
        /// Configured storage provider ID.
        provider: String,
        /// Sanitized retryable failure details.
        message: String,
    },

    /// The configured storage provider type is not implemented.
    #[error("unsupported storage provider type: {provider_type}")]
    Unsupported {
        /// Provider type from configuration.
        provider_type: String,
    },

    /// The storage provider returned an unexpected upstream failure.
    #[error("{provider} upstream failure{status_text}: {message}", status_text = status_text(*status))]
    Upstream {
        /// Configured storage provider ID.
        provider: String,
        /// Optional upstream HTTP status code.
        status: Option<u16>,
        /// Sanitized upstream error message.
        message: String,
    },
}

impl StorageError {
    /// Returns the high-level domain responsible for this failure.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        ErrorCategory::Storage
    }
}

/// Error type for migration discovery, transfer, and safety operations.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// The selected working directory is not inside a Git repository.
    #[error("not a Git repository: {}", path.display())]
    NotGitRepository {
        /// Path that was expected to be inside a Git repository.
        path: PathBuf,
    },

    /// Existing Git LFS configuration does not expose a source endpoint.
    #[error("source Git LFS endpoint is not configured")]
    SourceEndpointMissing,

    /// A source LFS object required by the migration could not be found.
    #[error("source object missing: sha256:{oid} ({size} bytes)")]
    SourceObjectMissing {
        /// Hex SHA-256 object identifier without the `sha256:` prefix.
        oid: String,
        /// Expected object size in bytes.
        size: u64,
    },

    /// A dry-run migration path attempted to perform a write.
    #[error("dry-run attempted to write {}", path.display())]
    DryRunWriteAttempt {
        /// Path that would have been written during a non-dry-run migration.
        path: PathBuf,
    },

    /// A repository-provider operation failed during migration.
    #[error("repository provider failed during migration: {source}")]
    RepositoryProvider {
        /// Underlying repository-provider failure.
        #[from]
        source: RepositoryProviderError,
    },

    /// A storage-provider operation failed during migration.
    #[error("storage provider failed during migration: {source}")]
    Storage {
        /// Underlying storage-provider failure.
        #[from]
        source: StorageError,
    },
}

impl MigrationError {
    /// Returns the high-level domain responsible for this failure.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        ErrorCategory::Migration
    }
}

fn status_text(status: Option<u16>) -> String {
    status.map_or_else(String::new, |status| format!(" ({status})"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        CliError, ErrorCategory, LfsCloudError, MigrationError, RepositoryPermission,
        RepositoryProviderError, Result, ServerError, StorageError, StorageResult,
    };

    #[test]
    fn top_level_errors_preserve_their_domain_category() {
        let errors = [
            LfsCloudError::from(CliError::InvalidArguments {
                message: "missing --config".to_owned(),
            }),
            LfsCloudError::from(ServerError::RouteNotConfigured {
                path: "/github.com/owner/repo.git/info/lfs".to_owned(),
            }),
            LfsCloudError::from(RepositoryProviderError::PermissionDenied {
                provider: "github-main".to_owned(),
                owner: "owner".to_owned(),
                repo: "repo".to_owned(),
                required: RepositoryPermission::Write,
            }),
            LfsCloudError::from(StorageError::ObjectNotFound {
                provider: "drive-user-a".to_owned(),
                oid: "abc123".to_owned(),
                size: 42,
            }),
            LfsCloudError::from(MigrationError::SourceEndpointMissing),
        ];

        assert_eq!(errors[0].category(), ErrorCategory::Cli);
        assert_eq!(errors[1].category(), ErrorCategory::Server);
        assert_eq!(errors[2].category(), ErrorCategory::RepositoryProvider);
        assert_eq!(errors[3].category(), ErrorCategory::Storage);
        assert_eq!(errors[4].category(), ErrorCategory::Migration);
    }

    #[test]
    fn domain_errors_convert_through_request_handling_boundaries() {
        let provider_error = RepositoryProviderError::AuthenticationRequired {
            provider: "github-main".to_owned(),
        };
        let server_error = ServerError::from(provider_error);
        let top_level_error = LfsCloudError::from(server_error);

        assert_eq!(top_level_error.category(), ErrorCategory::Server);
        assert_eq!(
            top_level_error.to_string(),
            "server error: repository provider failed: github-main authentication required"
        );
    }

    #[test]
    fn storage_errors_have_lfs_object_context() {
        let error = StorageError::IntegrityMismatch {
            expected_oid: "expected".to_owned(),
            expected_size: 10,
            actual_oid: "actual".to_owned(),
            actual_size: 9,
        };

        assert_eq!(error.category(), ErrorCategory::Storage);
        assert_eq!(
            error.to_string(),
            "integrity mismatch: expected sha256:expected (10 bytes), got sha256:actual (9 bytes)"
        );
    }

    #[test]
    fn migration_dry_run_errors_include_the_blocked_write_path() {
        let error = MigrationError::DryRunWriteAttempt {
            path: PathBuf::from(".lfsconfig"),
        };

        assert_eq!(error.category(), ErrorCategory::Migration);
        assert_eq!(error.to_string(), "dry-run attempted to write .lfsconfig");
    }

    #[test]
    fn result_aliases_use_domain_specific_error_types() {
        let storage_result: StorageResult<()> = Err(StorageError::QuotaExceeded {
            provider: "drive-user-a".to_owned(),
            message: "storage limit reached".to_owned(),
        });
        let top_level_result: Result<()> = storage_result.map_err(LfsCloudError::from);

        assert_eq!(
            top_level_result.unwrap_err().to_string(),
            "storage error: drive-user-a quota exceeded: storage limit reached"
        );
    }
}
