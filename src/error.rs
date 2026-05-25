//! Shared error and result types for the LFS Cloud library.
//!
//! The project is still a single Rust package, so these types provide the
//! domain boundaries that later modules can share before the codebase is split
//! into CLI, server, provider, storage, and migration crates.

use std::{fmt, path::PathBuf};

/// Project-wide result type for operations that can fail with [`LfsCloudError`].
pub type LfsCloudResult<T> = std::result::Result<T, LfsCloudError>;

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
#[non_exhaustive]
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
#[non_exhaustive]
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
        source: RepositoryProviderError,
    },

    /// Failure from storage-provider object operations.
    #[error("storage error: {source}")]
    Storage {
        /// Underlying storage-provider failure.
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
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The user supplied invalid or incomplete command-line input.
    #[error("invalid arguments: {message}")]
    InvalidArguments {
        /// Human-readable explanation of the invalid input.
        message: String,
    },

    /// One or more CLI readiness checks completed and reported a failed state.
    #[error("status checks failed: {message}")]
    StatusFailed {
        /// Human-readable explanation of the failed checks.
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

    /// A local shared-cache operation failed.
    #[error("local cache error: {source}")]
    LocalCache {
        /// Underlying local cache failure.
        #[source]
        source: crate::local_cache::LocalCacheError,
    },

    /// An external command completed unsuccessfully.
    #[error("{command} failed with status {status}: {stderr}")]
    ExternalCommand {
        /// Command line being executed, without secret arguments.
        command: String,
        /// Process exit status or signal summary.
        status: String,
        /// Sanitized stderr emitted by the command.
        stderr: SanitizedMessage,
    },

    /// An external command completed but returned malformed or unsafe output.
    #[error("{command} returned invalid output: {message}")]
    ExternalCommandOutput {
        /// Command line being executed, without secret arguments.
        command: String,
        /// Sanitized explanation of the invalid output.
        message: SanitizedMessage,
    },

    /// Git has no credential helper configured for storing local LFS tokens.
    #[error("no Git credential helper is configured for {lfs_url}\n{instructions}")]
    GitCredentialHelperNotConfigured {
        /// LFS URL whose local credential could not be stored persistently.
        lfs_url: String,
        /// User-facing recovery instructions that do not contain secrets.
        instructions: SanitizedMessage,
    },
}

impl CliError {
    /// Returns this error type's own domain.
    ///
    /// When this error is nested inside [`LfsCloudError`], use
    /// [`LfsCloudError::category`] to report the handling boundary.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        ErrorCategory::Cli
    }
}

/// Error type for server configuration, routing, and request handling.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// The server configuration file could not be read.
    #[error("failed to read server config {}: {source}", path.display())]
    ConfigRead {
        /// Configuration path that was being read.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },

    /// The server configuration could not be parsed as YAML.
    #[error("failed to parse server config {path}: {source}")]
    ConfigParse {
        /// Configuration path, or `<memory>` for test/string sources.
        path: String,
        /// Underlying config parser or deserializer failure.
        #[source]
        source: config::ConfigError,
    },

    /// Server configuration is syntactically valid but semantically invalid.
    #[error("invalid configuration: {message}")]
    InvalidConfiguration {
        /// Human-readable configuration validation failure.
        message: String,
    },

    /// The server could not create the metadata database directory.
    #[error("failed to create metadata database directory {}: {source}", path.display())]
    MetadataDirectoryCreate {
        /// Directory path that should contain the SQLite metadata database.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },

    /// The server could not open the SQLite metadata database.
    #[error("failed to open metadata database {}: {source}", path.display())]
    MetadataOpen {
        /// SQLite database path that was being opened.
        path: PathBuf,
        /// Underlying SQLite failure.
        #[source]
        source: rusqlite::Error,
    },

    /// The server could not configure the SQLite metadata connection.
    #[error("failed to configure metadata database {}: {source}", path.display())]
    MetadataConfigure {
        /// SQLite database path that was being configured.
        path: PathBuf,
        /// Underlying SQLite failure.
        #[source]
        source: rusqlite::Error,
    },

    /// The server could not apply metadata database migrations.
    #[error("failed to migrate metadata database {}: {source}", path.display())]
    MetadataMigration {
        /// SQLite database path that was being migrated.
        path: PathBuf,
        /// Underlying SQLite failure.
        #[source]
        source: rusqlite::Error,
    },

    /// The server could not query or update metadata database records.
    #[error("failed to operate on metadata database {}: {source}", path.display())]
    MetadataOperation {
        /// SQLite database path whose records were being queried or updated.
        path: PathBuf,
        /// Underlying SQLite failure.
        #[source]
        source: rusqlite::Error,
    },

    /// The metadata database connection mutex was poisoned.
    #[error("metadata database connection lock was poisoned for {}", path.display())]
    MetadataConnectionPoisoned {
        /// SQLite database path whose connection lock was poisoned.
        path: PathBuf,
    },

    /// The server could not bind its configured listener.
    #[error("failed to bind server listener on {host}:{port}: {source}")]
    Bind {
        /// Listener host or interface.
        host: String,
        /// Listener TCP port.
        port: u16,
        /// Underlying socket failure.
        #[source]
        source: std::io::Error,
    },

    /// The server could not inspect the listener's actual local address.
    #[error("failed to inspect server listener address: {source}")]
    LocalAddress {
        /// Underlying socket failure.
        #[source]
        source: std::io::Error,
    },

    /// The HTTP server stopped because the Axum runtime returned an error.
    #[error("server runtime failed: {source}")]
    Serve {
        /// Underlying server runtime failure.
        #[source]
        source: std::io::Error,
    },

    /// No configured repository mapping matches an incoming LFS route.
    #[error("no configured repository route matches {path}")]
    RouteNotConfigured {
        /// Request path that did not match a configured repository route.
        path: String,
    },

    /// An incoming server request was malformed or missing required fields.
    #[error("invalid request: {message}")]
    InvalidRequest {
        /// Human-readable request validation failure.
        message: String,
    },

    /// The server could not complete a request due to an internal invariant failure.
    #[error("internal server error: {message}")]
    Internal {
        /// Human-readable internal failure summary.
        message: String,
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
    /// Returns this error type's own domain.
    ///
    /// When this error is nested inside [`LfsCloudError`], use
    /// [`LfsCloudError::category`] to report the handling boundary.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        ErrorCategory::Server
    }
}

/// Repository access level required by an LFS Cloud operation.
#[non_exhaustive]
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
#[non_exhaustive]
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
        message: SanitizedMessage,
    },
}

impl RepositoryProviderError {
    /// Returns this error type's own domain.
    ///
    /// When this error is nested inside [`LfsCloudError`], use
    /// [`LfsCloudError::category`] to report the handling boundary.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        ErrorCategory::RepositoryProvider
    }
}

/// Error type for storage-provider object and backend operations.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The storage provider could not load its server-side credentials.
    #[error("{provider} storage credential {reference:?} could not be loaded: {message}")]
    CredentialLoad {
        /// Configured storage provider ID.
        provider: String,
        /// Non-secret credential reference from server configuration.
        reference: String,
        /// Sanitized load or parse failure details.
        message: SanitizedMessage,
    },

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

    /// A staged upload file could not be opened or read before provider upload.
    #[error("{provider} staged upload file {} could not be read: {source}", path.display())]
    StagedFileRead {
        /// Configured storage provider ID.
        provider: String,
        /// Local staged file path.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
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
        message: SanitizedMessage,
    },
}

impl StorageError {
    /// Returns this error type's own domain.
    ///
    /// When this error is nested inside [`LfsCloudError`], use
    /// [`LfsCloudError::category`] to report the handling boundary.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        ErrorCategory::Storage
    }
}

/// Error type for migration discovery, transfer, and safety operations.
#[non_exhaustive]
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

    /// A filesystem or process I/O operation failed at the migration boundary.
    #[error("{context}: {source}")]
    Io {
        /// Operation being attempted when I/O failed.
        context: String,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// An external command needed for migration discovery failed.
    #[error("{command} failed with status {status}: {stderr}")]
    ExternalCommand {
        /// Command line being executed, without secret arguments.
        command: String,
        /// Process exit status or signal summary.
        status: String,
        /// Sanitized stderr emitted by the command.
        stderr: SanitizedMessage,
    },

    /// An external command completed but returned malformed or unsafe output.
    #[error("{command} returned invalid output: {message}")]
    ExternalCommandOutput {
        /// Command line being executed, without secret arguments.
        command: String,
        /// Sanitized explanation of the invalid output.
        message: SanitizedMessage,
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
    /// Returns this error type's own domain.
    ///
    /// When this error is nested inside [`LfsCloudError`], use
    /// [`LfsCloudError::category`] to report the handling boundary.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        ErrorCategory::Migration
    }
}

/// A message that callers have scrubbed for safe diagnostic display.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SanitizedMessage(String);

impl SanitizedMessage {
    /// Wraps an upstream message after the caller has removed secrets and PII.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::SanitizedMessage;
    ///
    /// let message = SanitizedMessage::new("rate limit exceeded");
    /// assert_eq!(message.as_str(), "rate limit exceeded");
    /// ```
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// Returns the scrubbed message text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SanitizedMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

struct StatusText(Option<u16>);

impl fmt::Display for StatusText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(status) = self.0 {
            write!(formatter, " ({status})")?;
        }

        Ok(())
    }
}

fn status_text(status: Option<u16>) -> StatusText {
    StatusText(status)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        CliError, ErrorCategory, LfsCloudError, LfsCloudResult, MigrationError,
        RepositoryPermission, RepositoryProviderError, SanitizedMessage, ServerError, StorageError,
        StorageResult,
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
            LfsCloudError::RepositoryProvider {
                source: RepositoryProviderError::PermissionDenied {
                    provider: "github-main".to_owned(),
                    owner: "owner".to_owned(),
                    repo: "repo".to_owned(),
                    required: RepositoryPermission::Write,
                },
            },
            LfsCloudError::Storage {
                source: StorageError::ObjectNotFound {
                    provider: "drive-user-a".to_owned(),
                    oid: "abc123".to_owned(),
                    size: 42,
                },
            },
            LfsCloudError::from(MigrationError::SourceEndpointMissing),
        ];

        assert_eq!(errors[0].category(), ErrorCategory::Cli);
        assert_eq!(errors[1].category(), ErrorCategory::Server);
        assert_eq!(errors[2].category(), ErrorCategory::RepositoryProvider);
        assert_eq!(errors[3].category(), ErrorCategory::Storage);
        assert_eq!(errors[4].category(), ErrorCategory::Migration);
    }

    #[test]
    fn provider_and_storage_errors_require_explicit_top_level_boundaries() {
        let provider_error = RepositoryProviderError::PermissionDenied {
            provider: "github-main".to_owned(),
            owner: "owner".to_owned(),
            repo: "repo".to_owned(),
            required: RepositoryPermission::Write,
        };
        let storage_error = StorageError::ObjectNotFound {
            provider: "drive-user-a".to_owned(),
            oid: "abc123".to_owned(),
            size: 42,
        };

        let server_provider_error = LfsCloudError::from(ServerError::from(provider_error));
        let server_storage_error = LfsCloudError::from(ServerError::from(storage_error));

        assert_eq!(server_provider_error.category(), ErrorCategory::Server);
        assert_eq!(server_storage_error.category(), ErrorCategory::Server);
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
    fn upstream_errors_format_optional_status_without_extra_spacing() {
        let provider_without_status = RepositoryProviderError::Upstream {
            provider: "github-main".to_owned(),
            status: None,
            message: SanitizedMessage::new("request timed out"),
        };
        let provider_with_status = RepositoryProviderError::Upstream {
            provider: "github-main".to_owned(),
            status: Some(502),
            message: SanitizedMessage::new("bad gateway"),
        };
        let storage_without_status = StorageError::Upstream {
            provider: "drive-user-a".to_owned(),
            status: None,
            message: SanitizedMessage::new("backend unavailable"),
        };
        let storage_with_status = StorageError::Upstream {
            provider: "drive-user-a".to_owned(),
            status: Some(429),
            message: SanitizedMessage::new("rate limit exceeded"),
        };

        assert_eq!(
            provider_without_status.to_string(),
            "github-main upstream failure: request timed out"
        );
        assert_eq!(
            provider_with_status.to_string(),
            "github-main upstream failure (502): bad gateway"
        );
        assert_eq!(
            storage_without_status.to_string(),
            "drive-user-a upstream failure: backend unavailable"
        );
        assert_eq!(
            storage_with_status.to_string(),
            "drive-user-a upstream failure (429): rate limit exceeded"
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
        let top_level_result: LfsCloudResult<()> =
            storage_result.map_err(|source| LfsCloudError::Storage { source });

        assert_eq!(
            top_level_result.unwrap_err().to_string(),
            "storage error: drive-user-a quota exceeded: storage limit reached"
        );
    }
}
