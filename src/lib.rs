//! Core library for LFS Cloud.
//!
//! The root package keeps shared CLI, server, provider, storage, metadata, and
//! protocol code in one library target so the binary target can stay small.

mod cli;
pub mod credentials;
pub mod error;
pub mod git;
pub mod github_auth;
pub mod google_drive;
pub mod init;
pub mod lfs;
pub mod local_cache;
pub mod logging;
pub mod metadata;
pub mod migration;
pub mod providers;
pub mod server;
pub mod server_config;
pub mod sessions;

pub use cli::run_from_env;
pub use credentials::{
    DEFAULT_GIT_CREDENTIAL_USERNAME, GitCredential, GitCredentialApproval, GitCredentialLookup,
    git_credential_helper_fallback_instructions,
};
pub use error::{
    CliError, CliResult, ErrorCategory, LfsCloudError, LfsCloudResult, MigrationError,
    MigrationResult, RepositoryPermission, RepositoryProviderError, RepositoryProviderResult,
    SanitizedMessage, ServerError, ServerResult, StorageError, StorageResult,
};
pub use git::{GitLfsConfigChange, GitLfsConfigTarget, GitRemote, GitRepository};
pub use github_auth::{
    DEFAULT_GITHUB_OAUTH_SCOPES, GITHUB_OAUTH_AUTHORIZE_URL, GITHUB_OAUTH_CALLBACK_PATH,
    GITHUB_OAUTH_LOGIN_PATH, GITHUB_OAUTH_TOKEN_URL, GitHubOAuthAccessToken,
    GitHubOAuthAuthorization, GitHubOAuthCallback, GitHubOAuthCallbackQuery,
    GitHubOAuthCallbackRouteResponse, GitHubOAuthCallbackRouteState, GitHubOAuthCode,
    GitHubOAuthState, GitHubOAuthStateRegistry, GitHubOAuthToken, GitHubOAuthTokenExchanger,
    GitHubRepositoryPermissionClient, GitHubUserClient, exchange_github_oauth_code,
    fetch_authenticated_github_user, github_oauth_authorization_url, github_oauth_callback_router,
    github_oauth_login_router,
};
pub use google_drive::{
    GOOGLE_DRIVE_API_BASE_URL, GOOGLE_DRIVE_FILE_SCOPE, GOOGLE_OAUTH_TOKEN_URL,
    GoogleDriveAccessToken, GoogleDriveCredential, GoogleDriveCredentialLoader,
    GoogleDriveDownloadResponse, GoogleDriveObjectKey, GoogleDriveObjectStore,
    GoogleDriveRootFolder, GoogleDriveRootValidator, GoogleDriveTokenRefresher,
};
pub use init::LfsInitRoute;
pub use lfs::{
    LFS_BASIC_TRANSFER, LFS_POINTER_VERSION, LfsBatchAction, LfsBatchDownloadObject,
    LfsBatchObjectError, LfsBatchObjectResponse, LfsBatchOperation, LfsBatchRef, LfsBatchRequest,
    LfsBatchRequestParseError, LfsBatchResponse, LfsBatchUploadObject, LfsObject, LfsObjectError,
    LfsObjectSize, LfsOid, LfsPointer, parse_lfs_batch_request_json,
};
pub use local_cache::{
    DEFAULT_LOCAL_CACHE_HOME_DIR, LOCAL_CACHE_OBJECTS_DIR, LOCAL_CACHE_WORKTREES_FILE,
    LocalCacheDehydration, LocalCacheDehydrationStatus, LocalCacheError,
    LocalCacheGarbageCollection, LocalCacheGarbageCollectionObject, LocalCacheIngest,
    LocalCacheIngestStatus, LocalCacheLayout, LocalCacheMaterialization,
    LocalCacheMaterializationStatus, LocalCacheResult, LocalCacheWorktreeRegistration,
    LocalCacheWorktreeRegistrationChange, LocalCacheWorktreeRegistrationStatus,
    LocalCacheWorktreeRegistry, VerifiedLocalCacheObject,
};
pub use logging::{
    DEFAULT_LOG_ENV_VAR, DEFAULT_LOG_FILTER, TracingConfig, TracingInitError, init_tracing,
    tracing_filter,
};
pub use metadata::{
    METADATA_SCHEMA_VERSION, MetadataDatabase, MetadataObjectRecord,
    MetadataObjectVerificationStatus,
};
pub use migration::{
    CurrentCheckoutLfsPointer, CurrentCheckoutLfsPointers, GitLfsFilterConfig,
    GitLfsHistoryPointer, GitLfsHistoryPointers, GitLfsInstallation, GitLfsMigrationDiscovery,
    GitLfsScannedRef, GitLfsSourceEndpoint, GitLfsSourceEndpointSource, GitLfsTrackedPattern,
    LocalMigrationObject, LocalMigrationObjectAvailability, LocalMigrationObjectLocation,
    LocalMigrationObjectLocationKind, LocalMigrationObjectLocationStatus, MigrationFetchMode,
    MigrationSourceFetch, MigrationStorageUpload, check_local_migration_objects,
    discover_git_lfs_migration, enumerate_all_fetched_ref_lfs_pointers,
    enumerate_current_checkout_lfs_pointers, enumerate_selected_ref_lfs_pointers,
    fetch_missing_migration_objects, upload_migration_objects_to_storage,
};
pub use providers::{
    ProviderFuture, RepositoryAuthorization, RepositoryHandle, RepositoryIdentity,
    RepositoryProvider, RepositoryUser, StorageDeleteOutcome, StorageProvider, StoredObject,
};
pub use server::{
    AdvertisedServerUrls, LfsRouteEndpoint, LfsRouteResolver, ResolvedLfsRoute, ServeOptions,
    advertised_server_urls, lfs_server_router, lfs_server_router_with_sessions,
    render_server_startup_message, serve,
};
pub use server_config::{
    DEFAULT_CONFIG_PATH, DEFAULT_METADATA_DB_FILE, DEFAULT_METADATA_DIR, GitHubProviderConfig,
    GoogleDriveStorageConfig, RepositoryMapping, RepositoryProviderConfig, ServerConfig,
    ServerSettings, StorageProviderConfig,
};
pub use sessions::{
    DEFAULT_LFS_SESSION_TTL, IssuedLfsSession, LfsSessionMetadata, LfsSessionToken,
    LocalLfsSessionStore,
};
