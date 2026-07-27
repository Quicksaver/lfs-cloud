//! Core library for LFS Cloud.
//!
//! The root package keeps shared CLI, server, provider, storage, metadata, and
//! protocol code in one library target so the binary target can stay small.

mod child_process;
mod cli;
pub mod credentials;
pub mod error;
pub mod git;
mod git_output;
pub mod github_auth;
pub mod google_drive;
mod http_transport;
pub mod init;
pub mod lfs;
pub mod local_cache;
pub mod logging;
pub mod metadata;
pub mod migration;
mod process_output;
pub mod providers;
pub mod server;
pub mod server_config;
pub mod sessions;

pub use cli::run_from_env;
pub use credentials::{
    DEFAULT_GIT_CREDENTIAL_USERNAME, GitCredential, GitCredentialApproval, GitCredentialLookup,
    GitCredentialRejection, git_credential_helper_fallback_instructions,
};
pub use error::{
    CliError, CliResult, ErrorCategory, LfsCloudError, LfsCloudResult, MigrationError,
    MigrationResult, RepositoryPermission, RepositoryProviderError, RepositoryProviderResult,
    SanitizedMessage, ServerError, ServerResult, StorageError, StorageResult,
};
pub use git::{GitLfsConfigChange, GitLfsConfigTarget, GitRemote, GitRepository};
pub use github_auth::{
    GITHUB_PERSONAL_ACCESS_TOKEN_LOGIN_PATH, GitHubLoginRouteResponse, GitHubPersonalAccessToken,
    GitHubPersonalAccessTokenLoginRouteState, GitHubRepositoryPermissionClient,
    GitHubRepositoryProvider, GitHubUserClient, github_personal_access_token_login_router,
};
pub use google_drive::{
    GOOGLE_DRIVE_API_BASE_URL, GOOGLE_DRIVE_FILE_SCOPE, GoogleDriveAccessToken,
    GoogleDriveDownloadResponse, GoogleDriveGcloudTokenProvider, GoogleDriveObjectKey,
    GoogleDriveObjectStore, GoogleDriveRootFolder, GoogleDriveRootValidator,
};
pub use init::LfsInitRoute;
pub use lfs::{
    LFS_BASIC_TRANSFER, LFS_POINTER_SIZE_CUTOFF, LFS_POINTER_VERSION, LfsBatchAction,
    LfsBatchDownloadObject, LfsBatchHashAlgorithm, LfsBatchObjectError, LfsBatchObjectResponse,
    LfsBatchOperation, LfsBatchRef, LfsBatchRequest, LfsBatchRequestParseError, LfsBatchResponse,
    LfsBatchUploadObject, LfsObject, LfsObjectError, LfsObjectSize, LfsOid, LfsPointer,
    parse_lfs_batch_request_json,
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
    CurrentCheckoutLfsPointer, CurrentCheckoutLfsPointers, DEFAULT_MIGRATION_UPLOAD_CONCURRENCY,
    GitLfsFilterConfig, GitLfsHistoryPointer, GitLfsHistoryPointers, GitLfsInstallation,
    GitLfsMigrationDiscovery, GitLfsScannedRef, GitLfsSourceEndpoint, GitLfsSourceEndpointSource,
    GitLfsTrackedPattern, LocalMigrationObject, LocalMigrationObjectAvailability,
    LocalMigrationObjectLocation, LocalMigrationObjectLocationKind,
    LocalMigrationObjectLocationStatus, MigrationFetchMode, MigrationObjectUploadFailure,
    MigrationObjectUploadOutcome, MigrationObjectUploadStatus, MigrationSourceFetch,
    MigrationStorageUpload, MigrationStorageUploadOptions, check_local_migration_objects,
    discover_git_lfs_migration, discover_git_lfs_migration_from_remote,
    enumerate_all_fetched_ref_lfs_pointers, enumerate_current_checkout_lfs_pointers,
    enumerate_fetched_ref_lfs_pointers_for_remote, enumerate_selected_ref_lfs_pointers,
    fetch_migration_git_refs, fetch_missing_migration_objects,
    fetch_missing_migration_objects_from_remote, upload_migration_objects_to_storage,
    upload_migration_objects_to_storage_with_options,
};
pub use providers::{
    ProviderFuture, RepositoryAuthentication, RepositoryAuthorization, RepositoryIdentity,
    RepositoryProvider, RepositoryUser, StorageDeleteOutcome, StorageProvider, StoredObject,
};
#[doc(hidden)]
pub use server::lfs_server_router_with_provider_adapters;
pub use server::{
    AdvertisedServerUrls, LFS_SESSION_REVOKE_PATH, LfsRouteEndpoint, LfsRouteResolver,
    ResolvedLfsRoute, ServeOptions, advertised_server_urls, lfs_server_router,
    lfs_server_router_with_sessions, render_server_startup_message, serve,
};
pub use server_config::{
    DEFAULT_CONFIG_PATH, DEFAULT_METADATA_DB_FILE, DEFAULT_METADATA_DIR,
    GitHubAuthenticationConfig, GitHubProviderConfig, GoogleDriveGcloudCredentialsConfig,
    GoogleDriveStorageConfig, RepositoryMapping, RepositoryProviderConfig, ServerConfig,
    ServerSettings, StorageProviderConfig,
};
pub use sessions::{
    DEFAULT_LFS_SESSION_TTL, IssuedLfsSession, LfsSessionMetadata, LfsSessionToken,
    LocalLfsSessionStore,
};
