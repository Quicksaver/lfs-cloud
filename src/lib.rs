//! Core library scaffold for LFS Cloud.
//!
//! The production CLI and server behavior is still planned. This library exists
//! so the package has a stable target for documentation, tests, and future
//! shared implementation.

pub mod credentials;
pub mod error;
pub mod github_auth;
pub mod google_drive;
pub mod lfs;
pub mod logging;
pub mod providers;
pub mod server_config;
pub mod sessions;

pub use credentials::{
    DEFAULT_GIT_CREDENTIAL_USERNAME, GitCredential, GitCredentialApproval, GitCredentialLookup,
    git_credential_helper_fallback_instructions,
};
pub use error::{
    CliError, CliResult, ErrorCategory, LfsCloudError, LfsCloudResult, MigrationError,
    MigrationResult, RepositoryPermission, RepositoryProviderError, RepositoryProviderResult,
    SanitizedMessage, ServerError, ServerResult, StorageError, StorageResult,
};
pub use github_auth::{
    DEFAULT_GITHUB_OAUTH_SCOPES, GITHUB_OAUTH_AUTHORIZE_URL, GITHUB_OAUTH_CALLBACK_PATH,
    GITHUB_OAUTH_TOKEN_URL, GitHubOAuthAccessToken, GitHubOAuthAuthorization, GitHubOAuthCallback,
    GitHubOAuthCallbackQuery, GitHubOAuthCallbackRouteResponse, GitHubOAuthCallbackRouteState,
    GitHubOAuthCode, GitHubOAuthState, GitHubOAuthStateRegistry, GitHubOAuthToken,
    GitHubOAuthTokenExchanger, GitHubRepositoryPermissionClient, GitHubUserClient,
    exchange_github_oauth_code, fetch_authenticated_github_user, github_oauth_authorization_url,
    github_oauth_callback_router,
};
pub use google_drive::{
    GOOGLE_DRIVE_API_BASE_URL, GOOGLE_DRIVE_FILE_SCOPE, GOOGLE_OAUTH_TOKEN_URL,
    GoogleDriveAccessToken, GoogleDriveCredential, GoogleDriveCredentialLoader,
    GoogleDriveRootFolder, GoogleDriveRootValidator, GoogleDriveTokenRefresher,
};
pub use lfs::{
    LFS_POINTER_VERSION, LfsBatchAction, LfsBatchObjectError, LfsBatchObjectResponse,
    LfsBatchOperation, LfsBatchRef, LfsBatchRequest, LfsBatchResponse, LfsObject, LfsObjectError,
    LfsObjectSize, LfsOid, LfsPointer,
};
pub use logging::{
    DEFAULT_LOG_ENV_VAR, DEFAULT_LOG_FILTER, TracingConfig, TracingInitError, init_tracing,
    tracing_filter,
};
pub use providers::{
    ProviderFuture, RepositoryAuthorization, RepositoryHandle, RepositoryIdentity,
    RepositoryProvider, RepositoryUser, StorageDeleteOutcome, StorageProvider, StoredObject,
};
pub use server_config::{
    DEFAULT_CONFIG_PATH, GitHubProviderConfig, GoogleDriveStorageConfig, RepositoryMapping,
    RepositoryProviderConfig, ServerConfig, ServerSettings, StorageProviderConfig,
};
pub use sessions::{
    DEFAULT_LFS_SESSION_TTL, IssuedLfsSession, LfsSessionMetadata, LfsSessionToken,
    LocalLfsSessionStore,
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
