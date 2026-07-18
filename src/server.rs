//! HTTP server entrypoint and Git LFS route resolution.
//!
//! This module owns the first server-facing boundary: loading a validated
//! configuration, binding an Axum listener, reporting reachable URLs, and
//! resolving incoming Git LFS request paths to configured repository mappings
//! before requiring a local LFS Cloud session token. Batch-transfer behavior is
//! layered on top of this route and authentication context in later protocol
//! tasks.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::{self, ErrorKind},
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{FromRequest, OriginalUri, Request, State},
    http::{
        HeaderMap, HeaderValue, Method, StatusCode,
        header::{ALLOW, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, WWW_AUTHENTICATE},
    },
    response::{IntoResponse, Response},
    routing::delete,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use futures_util::{StreamExt, stream};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore};
use tokio_util::io::ReaderStream;
use url::{Url, form_urlencoded};

use crate::{
    DEFAULT_GIT_CREDENTIAL_USERNAME, GITHUB_OAUTH_CALLBACK_PATH, GitHubOAuthCallbackRouteState,
    GitHubOAuthStateRegistry, GitHubProviderConfig, GitHubRepositoryPermissionClient,
    GoogleDriveAccessToken, GoogleDriveCredential, GoogleDriveCredentialLoader,
    GoogleDriveObjectStore, GoogleDriveTokenRefresher, LFS_BASIC_TRANSFER, LfsBatchDownloadObject,
    LfsBatchObjectError, LfsBatchOperation, LfsBatchRequest, LfsBatchResponse,
    LfsBatchUploadObject, LfsObject, LfsObjectSize, LfsOid, LfsSessionToken, LocalLfsSessionStore,
    MetadataDatabase, ProviderFuture, RepositoryHandle, RepositoryIdentity, RepositoryMapping,
    RepositoryPermission, RepositoryProvider, RepositoryProviderConfig, RepositoryProviderError,
    RepositoryUser, ServerConfig, ServerError, ServerResult, StorageError, StorageProvider,
    StorageProviderConfig, StoredObject, github_oauth_callback_router, github_oauth_login_router,
    parse_lfs_batch_request_json, sessions::LfsSessionRecord,
};

const LFS_AUTH_CHALLENGE: &str = "Basic realm=\"lfs-cloud\"";
/// Authenticated endpoint for revoking the presented local LFS session.
pub const LFS_SESSION_REVOKE_PATH: &str = "/auth/session";
const GIT_LFS_JSON_CONTENT_TYPE: &str = "application/vnd.git-lfs+json";
const MAX_UPLOAD_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MIN_UPLOAD_STAGING_FREE_BYTES: u64 = 64 * 1024 * 1024;
const UPLOAD_STAGING_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const BATCH_STORAGE_LOOKUP_CONCURRENCY: usize = 16;
const AUTHORIZATION_CACHE_TTL: Duration = Duration::from_secs(15);
const GOOGLE_ACCESS_TOKEN_REFRESH_SKEW: Duration = Duration::from_secs(60);

/// Runtime options supplied by `lfs-cloud serve`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServeOptions {
    /// Optional explicit server config path.
    pub config_path: Option<PathBuf>,
    /// Optional listener host override.
    pub host: Option<String>,
    /// Optional listener port override.
    pub port: Option<u16>,
}

impl ServeOptions {
    /// Creates serve options from optional command-line overrides.
    #[must_use]
    pub fn new(config_path: Option<PathBuf>, host: Option<String>, port: Option<u16>) -> Self {
        Self {
            config_path,
            host,
            port,
        }
    }
}

/// Starts the configured LFS Cloud HTTP server and runs until shutdown.
///
/// The server currently resolves configured LFS routes, requires local LFS
/// session authentication, and can render batch responses with object-level
/// unavailable errors. Storage-backed batch actions and transfer endpoints are
/// implemented by later protocol tasks.
///
/// # Errors
///
/// Returns [`ServerError`] when configuration loading, metadata initialization,
/// listener binding, or Axum serving fails.
pub async fn serve(options: ServeOptions) -> ServerResult<()> {
    let config_path = options
        .config_path
        .unwrap_or_else(|| ServerConfig::default_path().to_path_buf());
    let mut config = ServerConfig::load_from_path(config_path)?;
    let bind = ServerBind::from_config_and_overrides(
        &config.server.host,
        config.server.port,
        options.host,
        options.port,
    )?;
    bind.validate_transport(&config)?;

    let metadata_database = Arc::new(MetadataDatabase::open(config.server.metadata_path.clone())?);
    metadata_database.sync_config(&config)?;
    config.server.host = bind.host.clone();
    config.server.port = bind.port;

    let session_store = production_session_store(&config, metadata_database.clone())?;
    let transfer_store = Arc::new(GoogleDriveTransferStore::new(
        config.clone(),
        metadata_database.clone(),
    )?);
    let router =
        server_router_with_sessions_and_transfer_store(config, session_store, transfer_store)?;
    let listener = tokio::net::TcpListener::bind((bind.host.as_str(), bind.port))
        .await
        .map_err(|source| ServerError::Bind {
            host: bind.host.clone(),
            port: bind.port,
            source,
        })?;
    let local_addr = listener
        .local_addr()
        .map_err(|source| ServerError::LocalAddress { source })?;
    let urls = advertised_server_urls(&bind.host, local_addr.port());

    println!("{}", render_server_startup_message(&urls));

    axum::serve(listener, router)
        .await
        .map_err(|source| ServerError::Serve { source })
}

fn production_session_store(
    config: &ServerConfig,
    metadata_database: Arc<MetadataDatabase>,
) -> ServerResult<LocalLfsSessionStore> {
    let github_providers = config
        .repository_providers
        .values()
        .map(|provider| match provider {
            RepositoryProviderConfig::GitHub(provider) => provider,
        })
        .collect::<Vec<_>>();

    match github_providers.as_slice() {
        [] => Ok(LocalLfsSessionStore::new()),
        [provider] => LocalLfsSessionStore::open_durable(
            metadata_database,
            provider.oauth_client_secret.as_bytes(),
        ),
        _ => Err(ServerError::InvalidConfiguration {
            message: "multiple GitHub repository providers are not yet supported by durable session storage".to_owned(),
        }),
    }
}

/// Builds the Axum router for configured Git LFS paths.
pub fn lfs_server_router(config: ServerConfig) -> Router {
    lfs_server_router_with_sessions(config, LocalLfsSessionStore::new())
}

/// Builds the full server router with authentication and Git LFS routes.
///
/// GitHub OAuth callbacks and Git LFS endpoints share `session_store` so an
/// OAuth callback can issue a local LFS Cloud token that the LFS routes accept
/// immediately.
///
/// # Errors
///
/// Returns [`ServerError`] if OAuth callback state cannot be initialized from
/// the validated server configuration.
pub fn server_router_with_sessions(
    config: ServerConfig,
    session_store: LocalLfsSessionStore,
) -> ServerResult<Router> {
    let lfs_router = lfs_server_router_with_sessions(config.clone(), session_store.clone());
    let session_router = lfs_session_revoke_router(session_store.clone());
    let Some(auth_router) = github_oauth_router(config, session_store)? else {
        return Ok(session_router.merge(lfs_router));
    };

    Ok(auth_router.merge(session_router).merge(lfs_router))
}

fn server_router_with_sessions_and_transfer_store(
    config: ServerConfig,
    session_store: LocalLfsSessionStore,
    transfer_store: Arc<dyn LfsObjectTransferStore>,
) -> ServerResult<Router> {
    let lfs_router = lfs_server_router_with_sessions_authorizer_and_transfer_store(
        config.clone(),
        session_store.clone(),
        Arc::new(GitHubBatchAuthorizer::new(&config)),
        transfer_store,
    );
    let session_router = lfs_session_revoke_router(session_store.clone());
    let Some(auth_router) = github_oauth_router(config, session_store)? else {
        return Ok(session_router.merge(lfs_router));
    };

    Ok(auth_router.merge(session_router).merge(lfs_router))
}

fn lfs_session_revoke_router(session_store: LocalLfsSessionStore) -> Router {
    Router::new()
        .route(LFS_SESSION_REVOKE_PATH, delete(revoke_lfs_session_route))
        .with_state(session_store)
}

async fn revoke_lfs_session_route(
    State(session_store): State<LocalLfsSessionStore>,
    headers: HeaderMap,
) -> Response {
    let session = match authenticate_lfs_session(&headers, &session_store) {
        Ok(session) => session,
        Err(ServerError::Unauthorized { .. }) => return authentication_required_response(),
        Err(error) => {
            tracing::error!(%error, "failed to authenticate LFS session revocation");
            return git_lfs_json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "LFS Cloud session revocation failed",
            );
        }
    };

    match session_store.revoke(session.token()) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => authentication_required_response(),
        Err(error) => {
            tracing::error!(%error, "failed to revoke LFS session");
            git_lfs_json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "LFS Cloud session revocation failed",
            )
        }
    }
}

/// Builds the Axum router with an explicit local LFS session store.
///
/// This constructor lets login/callback wiring and tests share the same
/// [`LocalLfsSessionStore`] used by request authentication. Git LFS endpoint
/// requests must present a valid local LFS session token before protocol
/// handlers receive the resolved route.
pub fn lfs_server_router_with_sessions(
    config: ServerConfig,
    session_store: LocalLfsSessionStore,
) -> Router {
    lfs_server_router_with_sessions_and_authorizer(
        config.clone(),
        session_store,
        Arc::new(GitHubBatchAuthorizer::new(&config)),
    )
}

/// Builds the Git LFS router with explicit provider-trait adapters.
///
/// This is a narrow test seam for exercising the normal route, authentication,
/// authorization, and transfer handlers without real GitHub or Google Drive
/// network calls. It does not mount OAuth routes or durable metadata storage;
/// production serving still uses the configured GitHub and Google Drive
/// clients. The configured repository provider and storage provider IDs must
/// match the injected providers. Existing-object lookups synthesize backend IDs
/// because the generic [`StorageProvider`] trait only reports object existence;
/// the adapter never persists those synthetic IDs to metadata.
///
/// # Errors
///
/// Returns [`ServerError`] when any configured repository mapping references a
/// provider ID other than the injected repository or storage provider.
#[doc(hidden)]
pub fn lfs_server_router_with_provider_adapters(
    config: ServerConfig,
    session_store: LocalLfsSessionStore,
    repository_provider: Arc<dyn RepositoryProvider + Send + Sync>,
    storage_provider: Arc<dyn StorageProvider + Send + Sync>,
) -> ServerResult<Router> {
    validate_provider_adapter_config(
        &config,
        repository_provider.provider_id(),
        storage_provider.provider_id(),
    )?;

    Ok(
        lfs_server_router_with_sessions_authorizer_and_transfer_store(
            config,
            session_store,
            Arc::new(ProviderBatchAuthorizer::new(repository_provider)),
            Arc::new(StorageProviderTransferStore::new(storage_provider)),
        ),
    )
}

fn validate_provider_adapter_config(
    config: &ServerConfig,
    repository_provider_id: &str,
    storage_provider_id: &str,
) -> ServerResult<()> {
    for repository in &config.repositories {
        if repository.repo_provider != repository_provider_id {
            return Err(ServerError::InvalidConfiguration {
                message: format!(
                    "repository {} references repository provider {}, but injected provider is {}",
                    repository.id, repository.repo_provider, repository_provider_id
                ),
            });
        }
        if repository.storage_provider != storage_provider_id {
            return Err(ServerError::InvalidConfiguration {
                message: format!(
                    "repository {} references storage provider {}, but injected provider is {}",
                    repository.id, repository.storage_provider, storage_provider_id
                ),
            });
        }
    }

    Ok(())
}

fn lfs_server_router_with_sessions_and_authorizer(
    config: ServerConfig,
    session_store: LocalLfsSessionStore,
    authorizer: Arc<dyn LfsBatchAuthorizer>,
) -> Router {
    lfs_server_router_with_sessions_authorizer_and_transfer_store(
        config,
        session_store,
        authorizer,
        Arc::new(PendingLfsObjectTransferStore),
    )
}

fn lfs_server_router_with_sessions_authorizer_and_transfer_store(
    config: ServerConfig,
    session_store: LocalLfsSessionStore,
    authorizer: Arc<dyn LfsBatchAuthorizer>,
    transfer_store: Arc<dyn LfsObjectTransferStore>,
) -> Router {
    let state = Arc::new(LfsServerState::new(
        config,
        session_store,
        authorizer,
        transfer_store,
    ));

    Router::new().fallback(handle_lfs_request).with_state(state)
}

fn github_oauth_router(
    config: ServerConfig,
    session_store: LocalLfsSessionStore,
) -> ServerResult<Option<Router>> {
    let github_providers = config
        .repository_providers
        .values()
        .map(|provider| match provider {
            RepositoryProviderConfig::GitHub(provider) => provider,
        })
        .collect::<Vec<_>>();

    let provider = match github_providers.as_slice() {
        [] => return Ok(None),
        [provider] => provider,
        _ => {
            return Err(ServerError::InvalidConfiguration {
                message: "multiple GitHub repository providers are not yet supported by the OAuth callback router".to_owned(),
            });
        }
    };
    let redirect_url = format!(
        "{}{}",
        config.server.public_url.trim_end_matches('/'),
        GITHUB_OAUTH_CALLBACK_PATH
    );
    let route_state = GitHubOAuthCallbackRouteState::with_clients_and_session_store(
        (*provider).clone(),
        GitHubOAuthStateRegistry::new(),
        redirect_url,
        crate::GitHubOAuthTokenExchanger::new()?,
        crate::GitHubUserClient::new()?,
        session_store,
    )?;

    Ok(Some(
        github_oauth_login_router(route_state.clone())
            .merge(github_oauth_callback_router(route_state)),
    ))
}

trait LfsBatchAuthorizer: Send + Sync {
    fn authorize<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        session: &'a LfsSessionRecord,
        operation: LfsBatchOperation,
    ) -> ProviderFuture<'a, ServerResult<()>>;
}

trait LfsObjectTransferStore: Send + Sync {
    fn lookup_object<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, ServerResult<Option<StoredObject>>>;

    fn upload_object<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        object: &'a LfsObject,
        source: &'a Path,
        created_by: &'a RepositoryUser,
    ) -> ProviderFuture<'a, ServerResult<StoredObject>>;

    fn download_object_response<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, ServerResult<LfsDownloadResponse>>;

    fn record_verified_object<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        object: &'a LfsObject,
        backend_id: &'a str,
        created_by: &'a RepositoryUser,
    ) -> ProviderFuture<'a, ServerResult<()>>;
}

struct LfsDownloadResponse {
    stored_object: StoredObject,
    response: Response,
}

impl LfsDownloadResponse {
    fn new(stored_object: StoredObject, response: Response) -> Self {
        Self {
            stored_object,
            response,
        }
    }

    fn stored_object(&self) -> &StoredObject {
        &self.stored_object
    }

    fn into_response(self) -> Response {
        self.response
    }
}

#[derive(Clone)]
struct ProviderBatchAuthorizer {
    provider: Arc<dyn RepositoryProvider + Send + Sync>,
}

impl ProviderBatchAuthorizer {
    fn new(provider: Arc<dyn RepositoryProvider + Send + Sync>) -> Self {
        Self { provider }
    }
}

impl LfsBatchAuthorizer for ProviderBatchAuthorizer {
    fn authorize<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        session: &'a LfsSessionRecord,
        operation: LfsBatchOperation,
    ) -> ProviderFuture<'a, ServerResult<()>> {
        Box::pin(async move {
            let required = permission_required_for_batch_operation(operation);
            if self.provider.provider_id() != repository.repo_provider {
                return Err(ServerError::InvalidConfiguration {
                    message: format!(
                        "repository {} references repository provider {}, but injected provider is {}",
                        repository.id,
                        repository.repo_provider,
                        self.provider.provider_id()
                    ),
                });
            }
            if session.metadata().provider_id != repository.repo_provider {
                return Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::PermissionDenied {
                        provider: repository.repo_provider.clone(),
                        owner: repository.owner.clone(),
                        repo: repository.name.clone(),
                        required,
                    },
                });
            }

            let handle = RepositoryHandle::new(
                repository.repo_provider.clone(),
                repository.host.clone(),
                repository.owner.clone(),
                repository.name.clone(),
            );
            let identity = self.provider.repository_identity(&handle).await?;
            let user = RepositoryUser::new(
                session.metadata().provider_id.clone(),
                session.metadata().login.clone(),
                session.metadata().stable_id.clone(),
            );

            self.provider
                .check_permission(&identity, &user, required)
                .await?;
            Ok(())
        })
    }
}

#[derive(Clone)]
struct StorageProviderTransferStore {
    provider: Arc<dyn StorageProvider + Send + Sync>,
}

impl StorageProviderTransferStore {
    fn new(provider: Arc<dyn StorageProvider + Send + Sync>) -> Self {
        Self { provider }
    }

    fn ensure_provider_matches(&self, repository: &RepositoryMapping) -> ServerResult<()> {
        if self.provider.provider_id() == repository.storage_provider {
            return Ok(());
        }

        Err(ServerError::InvalidConfiguration {
            message: format!(
                "repository {} references storage provider {}, but injected provider is {}",
                repository.id,
                repository.storage_provider,
                self.provider.provider_id()
            ),
        })
    }

    fn synthetic_existing_backend_id(&self, object: &LfsObject) -> String {
        format!(
            "lfs-cloud-provider-adapter-existing://{}/objects/{}",
            self.provider.provider_id(),
            object.oid.as_hex()
        )
    }
}

impl LfsObjectTransferStore for StorageProviderTransferStore {
    fn lookup_object<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, ServerResult<Option<StoredObject>>> {
        Box::pin(async move {
            self.ensure_provider_matches(repository)?;
            if self.provider.object_exists(object).await? {
                Ok(Some(StoredObject::new(
                    self.provider.provider_id().to_owned(),
                    object.clone(),
                    self.synthetic_existing_backend_id(object),
                )))
            } else {
                Ok(None)
            }
        })
    }

    fn upload_object<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        object: &'a LfsObject,
        source: &'a Path,
        _created_by: &'a RepositoryUser,
    ) -> ProviderFuture<'a, ServerResult<StoredObject>> {
        Box::pin(async move {
            self.ensure_provider_matches(repository)?;
            self.provider
                .upload_object(object, source)
                .await
                .map_err(ServerError::from)
        })
    }

    fn download_object_response<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, ServerResult<LfsDownloadResponse>> {
        Box::pin(async move {
            self.ensure_provider_matches(repository)?;
            let provider_id = self.provider.provider_id().to_owned();
            let temp_file = tokio::task::spawn_blocking(tempfile::NamedTempFile::new)
                .await
                .map_err(|source| ServerError::Storage {
                    source: StorageError::Retryable {
                        provider: provider_id.clone(),
                        message: format!("download staging file task could not join: {source}"),
                    },
                })?
                .map_err(|source| ServerError::Storage {
                    source: StorageError::Retryable {
                        provider: provider_id.clone(),
                        message: format!("download staging file could not be created: {source}"),
                    },
                })?;
            let stored_object = self
                .provider
                .download_object(object, temp_file.path())
                .await?;
            let file = tokio::fs::File::open(temp_file.path())
                .await
                .map_err(|source| ServerError::Storage {
                    source: StorageError::Retryable {
                        provider: provider_id.clone(),
                        message: format!("download staging file could not be opened: {source}"),
                    },
                })?;
            let content_length = file
                .metadata()
                .await
                .map_err(|source| ServerError::Storage {
                    source: StorageError::Retryable {
                        provider: provider_id,
                        message: format!(
                            "download staging file metadata could not be read: {source}"
                        ),
                    },
                })?;
            let response = Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, "application/octet-stream")
                .header(CONTENT_LENGTH, content_length.len().to_string())
                .body(Body::from_stream(ReaderStream::new(file)))
                .map_err(|source| ServerError::Internal {
                    message: format!("download response could not be built: {source}"),
                })?;

            Ok(LfsDownloadResponse::new(stored_object, response))
        })
    }

    fn record_verified_object<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        _object: &'a LfsObject,
        _backend_id: &'a str,
        _created_by: &'a RepositoryUser,
    ) -> ProviderFuture<'a, ServerResult<()>> {
        Box::pin(async move {
            self.ensure_provider_matches(repository)?;
            // The provider-adapter seam has no durable metadata store; production
            // serving records verified objects through GoogleDriveTransferStore.
            Ok(())
        })
    }
}

struct PendingLfsObjectTransferStore;

impl LfsObjectTransferStore for PendingLfsObjectTransferStore {
    fn lookup_object<'a>(
        &'a self,
        _repository: &'a RepositoryMapping,
        _object: &'a LfsObject,
    ) -> ProviderFuture<'a, ServerResult<Option<StoredObject>>> {
        Box::pin(async {
            Err(ServerError::Storage {
                source: StorageError::Unsupported {
                    provider_type: "storage transfer handling is not configured".to_owned(),
                },
            })
        })
    }

    fn upload_object<'a>(
        &'a self,
        _repository: &'a RepositoryMapping,
        _object: &'a LfsObject,
        _source: &'a Path,
        _created_by: &'a RepositoryUser,
    ) -> ProviderFuture<'a, ServerResult<StoredObject>> {
        Box::pin(async {
            Err(ServerError::Storage {
                source: StorageError::Unsupported {
                    provider_type: "storage transfer handling is not configured".to_owned(),
                },
            })
        })
    }

    fn download_object_response<'a>(
        &'a self,
        _repository: &'a RepositoryMapping,
        _object: &'a LfsObject,
    ) -> ProviderFuture<'a, ServerResult<LfsDownloadResponse>> {
        Box::pin(async {
            Err(ServerError::Storage {
                source: StorageError::Unsupported {
                    provider_type: "storage transfer handling is not configured".to_owned(),
                },
            })
        })
    }

    fn record_verified_object<'a>(
        &'a self,
        _repository: &'a RepositoryMapping,
        _object: &'a LfsObject,
        _backend_id: &'a str,
        _created_by: &'a RepositoryUser,
    ) -> ProviderFuture<'a, ServerResult<()>> {
        Box::pin(async {
            Err(ServerError::Storage {
                source: StorageError::Unsupported {
                    provider_type: "storage transfer handling is not configured".to_owned(),
                },
            })
        })
    }
}

#[derive(Clone)]
struct GoogleDriveTransferStore {
    storage_providers: BTreeMap<String, StorageProviderConfig>,
    metadata_database: Arc<MetadataDatabase>,
    credential_loader: GoogleDriveCredentialLoader,
    token_refresher: GoogleDriveTokenRefresher,
    token_cache: GoogleDriveAccessTokenCache,
}

impl GoogleDriveTransferStore {
    fn new(config: ServerConfig, metadata_database: Arc<MetadataDatabase>) -> ServerResult<Self> {
        Ok(Self {
            storage_providers: config.storage_providers,
            metadata_database,
            credential_loader: GoogleDriveCredentialLoader::new(),
            token_refresher: GoogleDriveTokenRefresher::new()?,
            token_cache: GoogleDriveAccessTokenCache::default(),
        })
    }

    async fn object_store_for_repository(
        &self,
        repository: &RepositoryMapping,
    ) -> ServerResult<GoogleDriveObjectStore> {
        let storage = match self.storage_providers.get(&repository.storage_provider) {
            Some(StorageProviderConfig::GoogleDrive(storage)) => storage.clone(),
            None => {
                return Err(ServerError::InvalidConfiguration {
                    message: format!(
                        "repository {} references unknown storage provider {}",
                        repository.id, repository.storage_provider
                    ),
                });
            }
        };
        let credential = self.credential_loader.load_from_environment(&storage)?;
        let token = self
            .token_cache
            .get_or_refresh(&storage.id, &credential, &self.token_refresher)
            .await?;

        GoogleDriveObjectStore::new(storage, &repository.id, token).map_err(ServerError::from)
    }
}

#[derive(Clone, Default)]
struct GoogleDriveAccessTokenCache {
    tokens: Arc<AsyncMutex<HashMap<String, CachedGoogleDriveAccessToken>>>,
}

#[derive(Clone)]
struct CachedGoogleDriveAccessToken {
    token: GoogleDriveAccessToken,
    refresh_at: Instant,
}

impl GoogleDriveAccessTokenCache {
    async fn get_or_refresh(
        &self,
        provider_id: &str,
        credential: &GoogleDriveCredential,
        refresher: &GoogleDriveTokenRefresher,
    ) -> ServerResult<GoogleDriveAccessToken> {
        // Keep the lock through refresh so concurrent misses collapse into one
        // token request. Refreshes happen only near expiry, and the server-wide
        // provider semaphore bounds this path with all other upstream calls.
        let mut tokens = self.tokens.lock().await;
        let now = Instant::now();
        if let Some(cached) = tokens.get(provider_id)
            && cached.refresh_at > now
        {
            return Ok(cached.token.clone());
        }

        let token = refresher.refresh_access_token(credential).await?;
        let refresh_at = token
            .expires_in_seconds()
            .and_then(|seconds| {
                Duration::from_secs(seconds).checked_sub(GOOGLE_ACCESS_TOKEN_REFRESH_SKEW)
            })
            .and_then(|lifetime| now.checked_add(lifetime));

        if let Some(refresh_at) = refresh_at
            && refresh_at > now
        {
            tokens.insert(
                provider_id.to_owned(),
                CachedGoogleDriveAccessToken {
                    token: token.clone(),
                    refresh_at,
                },
            );
        } else {
            tokens.remove(provider_id);
        }

        Ok(token)
    }
}

impl LfsObjectTransferStore for GoogleDriveTransferStore {
    fn lookup_object<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, ServerResult<Option<StoredObject>>> {
        Box::pin(async move {
            let store = self.object_store_for_repository(repository).await?;
            store.lookup_object(object).await.map_err(ServerError::from)
        })
    }

    fn upload_object<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        object: &'a LfsObject,
        source: &'a Path,
        created_by: &'a RepositoryUser,
    ) -> ProviderFuture<'a, ServerResult<StoredObject>> {
        Box::pin(async move {
            let store = self.object_store_for_repository(repository).await?;
            let stored_object = store.upload_object(object, source).await?;
            self.record_verified_object(repository, object, &stored_object.backend_id, created_by)
                .await?;
            Ok(stored_object)
        })
    }

    fn download_object_response<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, ServerResult<LfsDownloadResponse>> {
        Box::pin(async move {
            let store = self.object_store_for_repository(repository).await?;
            let download = store.download_object_response(object).await?;
            Ok(LfsDownloadResponse::new(
                download.stored_object().clone(),
                download.into_response(),
            ))
        })
    }

    fn record_verified_object<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        object: &'a LfsObject,
        backend_id: &'a str,
        created_by: &'a RepositoryUser,
    ) -> ProviderFuture<'a, ServerResult<()>> {
        Box::pin(async move {
            self.metadata_database.record_verified_object(
                &repository.id,
                &repository.storage_provider,
                object,
                backend_id,
                created_by,
            )?;
            Ok(())
        })
    }
}

#[derive(Clone, Debug)]
struct GitHubBatchAuthorizer {
    providers: BTreeMap<String, GitHubProviderConfig>,
}

impl GitHubBatchAuthorizer {
    fn new(config: &ServerConfig) -> Self {
        let providers = config
            .repository_providers
            .iter()
            .map(|(id, provider)| match provider {
                RepositoryProviderConfig::GitHub(provider) => (id.clone(), provider.clone()),
            })
            .collect();

        Self { providers }
    }
}

impl LfsBatchAuthorizer for GitHubBatchAuthorizer {
    fn authorize<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        session: &'a LfsSessionRecord,
        operation: LfsBatchOperation,
    ) -> ProviderFuture<'a, ServerResult<()>> {
        Box::pin(async move {
            let required = permission_required_for_batch_operation(operation);
            let provider = self
                .providers
                .get(&repository.repo_provider)
                .ok_or_else(|| ServerError::InvalidConfiguration {
                    message: format!(
                        "repository {} references unknown provider {}",
                        repository.id, repository.repo_provider
                    ),
                })?;
            let token =
                session
                    .github_access_token()
                    .ok_or_else(|| ServerError::RepositoryProvider {
                        source: RepositoryProviderError::AuthenticationRequired {
                            provider: repository.repo_provider.clone(),
                        },
                    })?;

            if session.metadata().provider_id != repository.repo_provider {
                return Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::PermissionDenied {
                        provider: repository.repo_provider.clone(),
                        owner: repository.owner.clone(),
                        repo: repository.name.clone(),
                        required,
                    },
                });
            }

            let identity = RepositoryIdentity {
                provider_id: repository.repo_provider.clone(),
                stable_id: Some(repository.provider_repository_id.clone()),
                host: repository.host.clone(),
                owner: repository.owner.clone(),
                name: repository.name.clone(),
            };
            let user = RepositoryUser::new(
                session.metadata().provider_id.clone(),
                session.metadata().login.clone(),
                session.metadata().stable_id.clone(),
            );

            GitHubRepositoryPermissionClient::new()?
                .check_permission(provider, token, &identity, &user, required)
                .await?;

            Ok(())
        })
    }
}

#[derive(Clone)]
struct LfsServerState {
    routes: LfsRouteResolver,
    session_store: LocalLfsSessionStore,
    public_url: String,
    max_batch_objects: usize,
    authorizer: Arc<dyn LfsBatchAuthorizer>,
    transfer_store: Arc<dyn LfsObjectTransferStore>,
    provider_calls: Arc<Semaphore>,
    authorization_cache: Arc<std::sync::Mutex<HashMap<AuthorizationCacheKey, Instant>>>,
    authorization_locks: Arc<std::sync::Mutex<HashMap<AuthorizationCacheKey, Arc<AsyncMutex<()>>>>>,
    upload_locks: Arc<std::sync::Mutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct AuthorizationCacheKey {
    session_token: LfsSessionToken,
    repository_id: String,
    write: bool,
}

impl LfsServerState {
    fn new(
        config: ServerConfig,
        session_store: LocalLfsSessionStore,
        authorizer: Arc<dyn LfsBatchAuthorizer>,
        transfer_store: Arc<dyn LfsObjectTransferStore>,
    ) -> Self {
        let max_batch_objects = config.server.max_batch_objects;
        let max_provider_calls = config.server.max_provider_calls;
        Self {
            routes: LfsRouteResolver::new(&config),
            session_store,
            public_url: config.server.public_url,
            max_batch_objects,
            authorizer,
            transfer_store,
            provider_calls: Arc::new(Semaphore::new(max_provider_calls)),
            authorization_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            authorization_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            upload_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    fn upload_lock_for(&self, repository: &RepositoryMapping, oid: &LfsOid) -> Arc<AsyncMutex<()>> {
        let key = format!(
            "{}:{}:{}",
            repository.id,
            repository.storage_provider,
            oid.as_hex()
        );
        let mut locks = self
            .upload_locks
            .lock()
            .expect("upload lock map should not be poisoned");
        locks
            .entry(key)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    async fn authorize(
        &self,
        repository: &RepositoryMapping,
        session: &AuthenticatedLfsSession,
        operation: LfsBatchOperation,
    ) -> ServerResult<()> {
        let key = AuthorizationCacheKey {
            session_token: session.token().clone(),
            repository_id: repository.id.clone(),
            write: operation == LfsBatchOperation::Upload,
        };
        if self.authorization_is_cached(&key) {
            return Ok(());
        }

        let authorization_lock = {
            let mut locks = self
                .authorization_locks
                .lock()
                .expect("authorization lock map should not be poisoned");
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let authorization_guard = authorization_lock.lock().await;
        if self.authorization_is_cached(&key) {
            drop(authorization_guard);
            self.remove_unused_authorization_lock(&key, &authorization_lock);
            return Ok(());
        }

        let _provider_permit = self.provider_call_permit().await?;
        let result = self
            .authorizer
            .authorize(repository, session.record(), operation)
            .await;
        if result.is_ok() {
            let expires_at = Instant::now()
                .checked_add(AUTHORIZATION_CACHE_TTL)
                .expect("short authorization cache TTL should fit Instant");
            let mut cache = self
                .authorization_cache
                .lock()
                .expect("authorization cache should not be poisoned");
            cache.retain(|_, expiry| *expiry > Instant::now());
            cache.insert(key.clone(), expires_at);
        }

        drop(authorization_guard);
        self.remove_unused_authorization_lock(&key, &authorization_lock);
        if matches!(
            &result,
            Err(ServerError::RepositoryProvider {
                source: RepositoryProviderError::AuthenticationRequired { .. },
            })
        ) {
            self.session_store.revoke(session.token())?;
        }
        result
    }

    fn authorization_is_cached(&self, key: &AuthorizationCacheKey) -> bool {
        let now = Instant::now();
        let mut cache = self
            .authorization_cache
            .lock()
            .expect("authorization cache should not be poisoned");
        match cache.get(key).copied() {
            Some(expiry) if expiry > now => true,
            Some(_) => {
                cache.remove(key);
                false
            }
            None => false,
        }
    }

    fn remove_unused_authorization_lock(
        &self,
        key: &AuthorizationCacheKey,
        authorization_lock: &Arc<AsyncMutex<()>>,
    ) {
        let mut locks = self
            .authorization_locks
            .lock()
            .expect("authorization lock map should not be poisoned");
        if Arc::strong_count(authorization_lock) == 2
            && locks
                .get(key)
                .is_some_and(|stored| Arc::ptr_eq(stored, authorization_lock))
        {
            locks.remove(key);
        }
    }

    async fn provider_call_permit(&self) -> ServerResult<OwnedSemaphorePermit> {
        self.provider_calls
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ServerError::Internal {
                message: "provider call limiter closed unexpectedly".to_owned(),
            })
    }

    async fn lookup_object(
        &self,
        repository: &RepositoryMapping,
        object: &LfsObject,
    ) -> ServerResult<Option<StoredObject>> {
        let _provider_permit = self.provider_call_permit().await?;
        self.transfer_store.lookup_object(repository, object).await
    }

    async fn upload_object(
        &self,
        repository: &RepositoryMapping,
        object: &LfsObject,
        source: &Path,
        created_by: &RepositoryUser,
    ) -> ServerResult<StoredObject> {
        let _provider_permit = self.provider_call_permit().await?;
        self.transfer_store
            .upload_object(repository, object, source, created_by)
            .await
    }

    async fn download_object_response(
        &self,
        repository: &RepositoryMapping,
        object: &LfsObject,
    ) -> ServerResult<LfsDownloadResponse> {
        let _provider_permit = self.provider_call_permit().await?;
        self.transfer_store
            .download_object_response(repository, object)
            .await
    }

    async fn record_verified_object(
        &self,
        repository: &RepositoryMapping,
        object: &LfsObject,
        backend_id: &str,
        created_by: &RepositoryUser,
    ) -> ServerResult<()> {
        let _provider_permit = self.provider_call_permit().await?;
        self.transfer_store
            .record_verified_object(repository, object, backend_id, created_by)
            .await
    }
}

#[derive(Debug)]
struct AuthenticatedLfsSession {
    token: LfsSessionToken,
    record: Arc<LfsSessionRecord>,
}

impl AuthenticatedLfsSession {
    fn token(&self) -> &LfsSessionToken {
        &self.token
    }

    fn record(&self) -> &LfsSessionRecord {
        &self.record
    }

    fn metadata(&self) -> &crate::LfsSessionMetadata {
        self.record.metadata()
    }
}

async fn handle_lfs_request(
    State(state): State<Arc<LfsServerState>>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Response {
    let method = request.method().clone();
    let headers = request.headers().clone();

    match state.routes.resolve_path(uri.path()) {
        Ok(route) => match authenticate_lfs_session(&headers, &state.session_store) {
            Ok(session) => {
                handle_authenticated_lfs_request(route, session, method, request, &state).await
            }
            Err(error @ ServerError::Unauthorized { .. }) => {
                tracing::debug!(path = uri.path(), %error, "LFS route request was not authenticated");
                authentication_required_response()
            }
            Err(error) => {
                tracing::error!(path = uri.path(), %error, "failed to authenticate LFS route request");
                git_lfs_json_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "LFS Cloud authentication failed",
                )
            }
        },
        Err(ServerError::RouteNotConfigured { .. }) => git_lfs_json_error_response(
            StatusCode::NOT_FOUND,
            "No configured LFS Cloud repository route matches this path",
        ),
        Err(error @ ServerError::InvalidRequest { .. }) => {
            tracing::debug!(path = uri.path(), %error, "invalid LFS route request");
            git_lfs_json_error_response(StatusCode::BAD_REQUEST, "Invalid LFS Cloud route")
        }
        Err(error) => {
            tracing::error!(path = uri.path(), %error, "failed to resolve LFS route");
            git_lfs_json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "LFS Cloud route handling failed",
            )
        }
    }
}

async fn handle_authenticated_lfs_request(
    route: ResolvedLfsRoute,
    session: AuthenticatedLfsSession,
    method: Method,
    request: Request,
    state: &LfsServerState,
) -> Response {
    match route.endpoint {
        LfsRouteEndpoint::Batch => {
            handle_lfs_batch_request(route.repository, session, method, request, state).await
        }
        LfsRouteEndpoint::Object { oid } => {
            handle_lfs_object_request(route.repository, oid, session, method, request, state).await
        }
        LfsRouteEndpoint::Info => git_lfs_json_error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Git LFS endpoint routing is configured; transfer handling is not implemented yet",
        ),
    }
}

async fn handle_lfs_batch_request(
    repository: RepositoryMapping,
    session: AuthenticatedLfsSession,
    method: Method,
    request: Request,
    state: &LfsServerState,
) -> Response {
    if method != Method::POST {
        let mut response = git_lfs_json_error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "Git LFS batch endpoint requires POST",
        );
        response
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static("POST"));
        return response;
    }

    match Bytes::from_request(request, &()).await {
        Ok(body) => match parse_lfs_batch_request_json(&body) {
            Ok(batch_request) => {
                tracing::debug!(
                    repo_id = repository.id.as_str(),
                    provider_id = session.metadata().provider_id.as_str(),
                    operation = ?batch_request.operation,
                    object_count = batch_request.objects.len(),
                    "parsed Git LFS batch request"
                );
                handle_parsed_lfs_batch_request(repository, session, state, batch_request).await
            }
            Err(error) => {
                tracing::debug!(repo_id = repository.id.as_str(), %error, "invalid Git LFS batch request");
                git_lfs_json_error_response(
                    StatusCode::BAD_REQUEST,
                    "Invalid Git LFS batch request",
                )
            }
        },
        Err(error) => {
            tracing::debug!(
                repo_id = repository.id.as_str(),
                %error,
                "failed to read Git LFS batch request body"
            );
            git_lfs_body_error_response(error.into_response())
        }
    }
}

async fn handle_lfs_object_request(
    repository: RepositoryMapping,
    oid: LfsOid,
    session: AuthenticatedLfsSession,
    method: Method,
    request: Request,
    state: &LfsServerState,
) -> Response {
    match method {
        Method::PUT => handle_lfs_upload_request(repository, oid, session, request, state).await,
        Method::GET => handle_lfs_download_request(repository, oid, session, request, state).await,
        _ => {
            let mut response = git_lfs_json_error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "Git LFS object endpoint requires GET for downloads or PUT for uploads",
            );
            response
                .headers_mut()
                .insert(ALLOW, HeaderValue::from_static("GET, PUT"));
            response
        }
    }
}

async fn handle_lfs_download_request(
    repository: RepositoryMapping,
    oid: LfsOid,
    session: AuthenticatedLfsSession,
    request: Request,
    state: &LfsServerState,
) -> Response {
    let expected_size = match transfer_request_expected_size(&request, "download") {
        Ok(size) => size,
        Err(error) => {
            tracing::debug!(
                repo_id = repository.id.as_str(),
                oid = oid.as_hex(),
                %error,
                "Git LFS download transfer missing or invalid object size"
            );
            return git_lfs_json_error_response(
                StatusCode::BAD_REQUEST,
                "Git LFS download action did not include a valid size query parameter",
            );
        }
    };

    if let Err(error) = state
        .authorize(&repository, &session, LfsBatchOperation::Download)
        .await
    {
        tracing::debug!(
            repo_id = repository.id.as_str(),
            oid = oid.as_hex(),
            %error,
            "Git LFS download transfer authorization failed"
        );
        return git_lfs_authorization_error_response(error);
    }

    let object = LfsObject::new(oid, LfsObjectSize::new(expected_size));
    match state.download_object_response(&repository, &object).await {
        Ok(download) => {
            tracing::debug!(
                repo_id = repository.id.as_str(),
                storage_provider = download.stored_object().provider_id.as_str(),
                backend_id = download.stored_object().backend_id.as_str(),
                oid = object.oid.as_hex(),
                size = object.size.bytes(),
                "prepared verified Git LFS download response"
            );
            download.into_response()
        }
        Err(error) => {
            tracing::debug!(
                repo_id = repository.id.as_str(),
                oid = object.oid.as_hex(),
                size = object.size.bytes(),
                %error,
                "Git LFS download transfer storage read failed"
            );
            git_lfs_download_storage_error_response(error)
        }
    }
}

async fn handle_lfs_upload_request(
    repository: RepositoryMapping,
    oid: LfsOid,
    session: AuthenticatedLfsSession,
    request: Request,
    state: &LfsServerState,
) -> Response {
    let expected_size = match transfer_request_expected_size(&request, "upload") {
        Ok(size) => size,
        Err(error) => {
            tracing::debug!(
                repo_id = repository.id.as_str(),
                oid = oid.as_hex(),
                %error,
                "Git LFS upload transfer missing or invalid object size"
            );
            return git_lfs_json_error_response(
                StatusCode::BAD_REQUEST,
                "Git LFS upload action did not include a valid size query parameter",
            );
        }
    };
    if expected_size > MAX_UPLOAD_OBJECT_BYTES {
        return upload_payload_too_large_response();
    }

    let declared_size = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if let Some(size) = declared_size
        && size > MAX_UPLOAD_OBJECT_BYTES
    {
        return upload_payload_too_large_response();
    }

    if let Err(error) = state
        .authorize(&repository, &session, LfsBatchOperation::Upload)
        .await
    {
        tracing::debug!(
            repo_id = repository.id.as_str(),
            oid = oid.as_hex(),
            %error,
            "Git LFS upload transfer authorization failed"
        );
        return git_lfs_authorization_error_response(error);
    }

    let upload_lock = state.upload_lock_for(&repository, &oid);
    let _upload_lock_guard = upload_lock.lock().await;
    let object = LfsObject::new(oid.clone(), LfsObjectSize::new(expected_size));
    let created_by = RepositoryUser::new(
        session.metadata().provider_id.clone(),
        session.metadata().login.clone(),
        session.metadata().stable_id.clone(),
    );

    match state.lookup_object(&repository, &object).await {
        Ok(Some(stored_object)) => {
            tracing::debug!(
                repo_id = repository.id.as_str(),
                storage_provider = stored_object.provider_id.as_str(),
                backend_id = stored_object.backend_id.as_str(),
                oid = object.oid.as_hex(),
                size = object.size.bytes(),
                "Git LFS upload transfer found an existing object"
            );
            if let Err(error) = state
                .record_verified_object(
                    &repository,
                    &object,
                    &stored_object.backend_id,
                    &created_by,
                )
                .await
            {
                tracing::debug!(
                    repo_id = repository.id.as_str(),
                    oid = object.oid.as_hex(),
                    %error,
                    "Git LFS upload transfer metadata repair failed"
                );
                return git_lfs_storage_error_response(error);
            }
            return StatusCode::OK.into_response();
        }
        Ok(None) => {}
        Err(error) => {
            tracing::debug!(
                repo_id = repository.id.as_str(),
                oid = object.oid.as_hex(),
                %error,
                "Git LFS upload transfer existence check failed"
            );
            return git_lfs_storage_error_response(error);
        }
    }

    let staged_upload = match stage_upload_request_body(&oid, Some(expected_size), request).await {
        Ok(staged_upload) => staged_upload,
        Err(UploadStagingError::PayloadTooLarge) => {
            return upload_payload_too_large_response();
        }
        Err(UploadStagingError::InsufficientTempSpace { .. }) => {
            return upload_temp_space_exhausted_response();
        }
        Err(UploadStagingError::TimedOut) => {
            return upload_staging_timeout_response();
        }
        Err(error) => {
            let error = error.into_storage_error();
            tracing::debug!(
                repo_id = repository.id.as_str(),
                oid = oid.as_hex(),
                %error,
                "Git LFS upload transfer staging failed"
            );
            return git_lfs_storage_error_response(ServerError::from(error));
        }
    };

    match state
        .upload_object(&repository, &object, staged_upload.path(), &created_by)
        .await
    {
        Ok(stored_object) => {
            tracing::debug!(
                repo_id = repository.id.as_str(),
                storage_provider = stored_object.provider_id.as_str(),
                oid = object.oid.as_hex(),
                size = object.size.bytes(),
                "Git LFS upload transfer completed"
            );
            StatusCode::OK.into_response()
        }
        Err(error) => {
            tracing::debug!(
                repo_id = repository.id.as_str(),
                oid = object.oid.as_hex(),
                %error,
                "Git LFS upload transfer storage write failed"
            );
            git_lfs_storage_error_response(error)
        }
    }
}

fn transfer_request_expected_size(request: &Request, action: &str) -> ServerResult<u64> {
    let Some(query) = request.uri().query() else {
        return Err(ServerError::InvalidRequest {
            message: format!("{action} action missing size query parameter"),
        });
    };

    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        if key == "size" {
            let size = value
                .parse::<u64>()
                .map_err(|_| ServerError::InvalidRequest {
                    message: format!("invalid {action} size query value {value:?}"),
                })?;

            return Ok(size);
        }
    }

    Err(ServerError::InvalidRequest {
        message: format!("{action} action missing size query parameter"),
    })
}

fn upload_payload_too_large_response() -> Response {
    git_lfs_json_error_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        "Git LFS upload object exceeds the configured request size limit",
    )
}

fn upload_temp_space_exhausted_response() -> Response {
    git_lfs_json_error_response(
        StatusCode::INSUFFICIENT_STORAGE,
        "Git LFS upload staging directory does not have enough free space",
    )
}

fn upload_staging_timeout_response() -> Response {
    git_lfs_json_error_response(
        StatusCode::REQUEST_TIMEOUT,
        "Git LFS upload request timed out while reading the object body",
    )
}

struct StagedUpload {
    temp_file: tempfile::NamedTempFile,
}

impl StagedUpload {
    fn path(&self) -> &Path {
        self.temp_file.path()
    }
}

async fn stage_upload_request_body(
    expected_oid: &LfsOid,
    expected_size: Option<u64>,
    request: Request,
) -> Result<StagedUpload, UploadStagingError> {
    stage_upload_request_body_with_limit(
        expected_oid,
        expected_size,
        request,
        MAX_UPLOAD_OBJECT_BYTES,
    )
    .await
}

async fn stage_upload_request_body_with_limit(
    expected_oid: &LfsOid,
    expected_size: Option<u64>,
    request: Request,
    max_upload_bytes: u64,
) -> Result<StagedUpload, UploadStagingError> {
    stage_upload_request_body_with_guardrails(
        expected_oid,
        expected_size,
        request,
        UploadStagingGuardrails {
            max_upload_bytes,
            ..UploadStagingGuardrails::default()
        },
    )
    .await
}

#[derive(Clone, Copy, Debug)]
struct UploadStagingGuardrails {
    max_upload_bytes: u64,
    min_free_bytes: u64,
    idle_timeout: Duration,
}

impl Default for UploadStagingGuardrails {
    fn default() -> Self {
        Self {
            max_upload_bytes: MAX_UPLOAD_OBJECT_BYTES,
            min_free_bytes: MIN_UPLOAD_STAGING_FREE_BYTES,
            idle_timeout: UPLOAD_STAGING_IDLE_TIMEOUT,
        }
    }
}

async fn stage_upload_request_body_with_guardrails(
    expected_oid: &LfsOid,
    expected_size: Option<u64>,
    request: Request,
    guardrails: UploadStagingGuardrails,
) -> Result<StagedUpload, UploadStagingError> {
    let preflight_size = upload_staging_preflight_size(expected_size, guardrails.max_upload_bytes)?;
    let temp_file = tempfile::Builder::new()
        .prefix("lfs-cloud-upload-")
        .tempfile()
        .map_err(|source| StorageError::Retryable {
            provider: "lfs-cloud".to_owned(),
            message: format!("upload staging file could not be created: {source}"),
        })?;
    let staging_dir = temp_file
        .path()
        .parent()
        .ok_or_else(|| StorageError::Retryable {
            provider: "lfs-cloud".to_owned(),
            message: format!(
                "upload staging file {} did not have a parent directory",
                temp_file.path().display()
            ),
        })?;
    // Unknown-size helper callers reserve the full effective upload limit so
    // they cannot skip the temp-space guardrail before streaming begins.
    ensure_temp_space_for_upload(staging_dir, preflight_size, guardrails.min_free_bytes).await?;

    let std_file = temp_file
        .reopen()
        .map_err(|source| StorageError::StagedFileRead {
            provider: "lfs-cloud".to_owned(),
            path: temp_file.path().to_path_buf(),
            source,
        })?;
    let mut file = tokio::fs::File::from_std(std_file);
    let mut stream = request.into_body().into_data_stream();
    let mut hasher = Sha256::new();
    let mut actual_size = 0_u64;

    loop {
        let Some(chunk) = tokio::time::timeout(guardrails.idle_timeout, stream.next())
            .await
            .map_err(|_| UploadStagingError::TimedOut)?
        else {
            break;
        };
        let chunk = chunk.map_err(|source| StorageError::Retryable {
            provider: "lfs-cloud".to_owned(),
            message: format!("upload request body could not be read: {source}"),
        })?;
        let next_size = actual_size
            .checked_add(chunk.len() as u64)
            .ok_or(UploadStagingError::PayloadTooLarge)?;
        if next_size > guardrails.max_upload_bytes {
            return Err(UploadStagingError::PayloadTooLarge);
        }
        hasher.update(&chunk);
        actual_size = next_size;
        file.write_all(&chunk)
            .await
            .map_err(|source| upload_staging_file_io_error(source, "written"))?;
    }
    file.flush()
        .await
        .map_err(|source| upload_staging_file_io_error(source, "flushed"))?;
    drop(file);

    let actual_oid = format!("{:x}", hasher.finalize());
    if let Some(expected_size) = expected_size
        && expected_size != actual_size
    {
        return Err(StorageError::IntegrityMismatch {
            expected_oid: expected_oid.as_hex().to_owned(),
            expected_size,
            actual_oid,
            actual_size,
        }
        .into());
    }

    if actual_oid != expected_oid.as_hex() {
        return Err(StorageError::IntegrityMismatch {
            expected_oid: expected_oid.as_hex().to_owned(),
            expected_size: expected_size.unwrap_or(actual_size),
            actual_oid,
            actual_size,
        }
        .into());
    }

    Ok(StagedUpload { temp_file })
}

fn upload_staging_preflight_size(
    expected_size: Option<u64>,
    max_upload_bytes: u64,
) -> Result<u64, UploadStagingError> {
    let size = expected_size.unwrap_or(max_upload_bytes);
    if size > max_upload_bytes {
        return Err(UploadStagingError::PayloadTooLarge);
    }

    Ok(size)
}

async fn ensure_temp_space_for_upload(
    staging_dir: &Path,
    expected_size: u64,
    min_free_bytes: u64,
) -> Result<(), UploadStagingError> {
    let staging_dir = staging_dir.to_path_buf();
    let available = tokio::task::spawn_blocking(move || fs4::available_space(staging_dir))
        .await
        .map_err(|source| StorageError::Retryable {
            provider: "lfs-cloud".to_owned(),
            message: format!(
                "upload staging directory free-space check did not complete: {source}"
            ),
        })?
        .map_err(|source| StorageError::Retryable {
            provider: "lfs-cloud".to_owned(),
            message: format!(
                "upload staging directory free space could not be inspected: {source}"
            ),
        })?;

    ensure_temp_space_for_upload_with_available_space(expected_size, min_free_bytes, available)
}

fn ensure_temp_space_for_upload_with_available_space(
    expected_size: u64,
    min_free_bytes: u64,
    available_space: u64,
) -> Result<(), UploadStagingError> {
    let required_space = expected_size.checked_add(min_free_bytes).ok_or(
        UploadStagingError::InsufficientTempSpace {
            required_space: None,
            available_space: Some(available_space),
        },
    )?;
    if available_space < required_space {
        return Err(UploadStagingError::InsufficientTempSpace {
            required_space: Some(required_space),
            available_space: Some(available_space),
        });
    }

    Ok(())
}

fn upload_staging_file_io_error(source: io::Error, action: &str) -> UploadStagingError {
    if is_temp_space_exhausted(&source) {
        return UploadStagingError::InsufficientTempSpace {
            required_space: None,
            available_space: None,
        };
    }

    StorageError::Retryable {
        provider: "lfs-cloud".to_owned(),
        message: format!("upload staging file could not be {action}: {source}"),
    }
    .into()
}

fn is_temp_space_exhausted(source: &io::Error) -> bool {
    matches!(
        source.kind(),
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded
    ) || matches!(
        source.raw_os_error(),
        // ENOSPC on Unix, EDQUOT on Linux, and EDQUOT on Darwin/BSD.
        Some(28) | Some(122) | Some(69)
    )
}

#[derive(Debug)]
enum UploadStagingError {
    PayloadTooLarge,
    InsufficientTempSpace {
        required_space: Option<u64>,
        available_space: Option<u64>,
    },
    TimedOut,
    Storage(StorageError),
}

impl UploadStagingError {
    fn into_storage_error(self) -> StorageError {
        match self {
            Self::PayloadTooLarge => StorageError::QuotaExceeded {
                provider: "lfs-cloud".to_owned(),
                message: "upload object exceeded request size limit".to_owned(),
            },
            Self::InsufficientTempSpace {
                required_space,
                available_space,
            } => {
                let message = match (required_space, available_space) {
                    (Some(required_space), Some(available_space)) => format!(
                        "upload staging directory has {available_space} bytes available but requires {required_space} bytes"
                    ),
                    (None, Some(available_space)) => format!(
                        "upload staging directory has {available_space} bytes available but required space exceeds supported size"
                    ),
                    _ => "upload staging directory does not have enough free space".to_owned(),
                };

                StorageError::QuotaExceeded {
                    provider: "lfs-cloud".to_owned(),
                    message,
                }
            }
            Self::TimedOut => StorageError::Retryable {
                provider: "lfs-cloud".to_owned(),
                message: "upload request body timed out while reading".to_owned(),
            },
            Self::Storage(error) => error,
        }
    }
}

impl From<StorageError> for UploadStagingError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

async fn handle_parsed_lfs_batch_request(
    repository: RepositoryMapping,
    session: AuthenticatedLfsSession,
    state: &LfsServerState,
    request: LfsBatchRequest,
) -> Response {
    if request.objects.len() > state.max_batch_objects {
        return git_lfs_json_error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "Git LFS batch contains more than {} object entries",
                state.max_batch_objects
            ),
        );
    }

    if !request.transfers.is_empty()
        && !request
            .transfers
            .iter()
            .any(|transfer| transfer == LFS_BASIC_TRANSFER)
    {
        return git_lfs_json_error_response(
            StatusCode::CONFLICT,
            "unsupported Git LFS transfer requested; only basic is available",
        );
    }

    if request.operation == LfsBatchOperation::Upload
        && request
            .objects
            .iter()
            .any(|object| object.size.bytes() > MAX_UPLOAD_OBJECT_BYTES)
    {
        return upload_payload_too_large_response();
    }

    if let Err(error) = state
        .authorize(&repository, &session, request.operation)
        .await
    {
        tracing::debug!(
            repo_id = repository.id.as_str(),
            operation = ?request.operation,
            %error,
            "Git LFS batch authorization failed"
        );
        return git_lfs_authorization_error_response(error);
    }

    match request.operation {
        LfsBatchOperation::Download => {
            match download_batch_response_with_storage_lookup(&repository, state, request).await {
                Ok(response) => git_lfs_json_response(response),
                Err(error) => {
                    tracing::debug!(
                        repo_id = repository.id.as_str(),
                        %error,
                        "Git LFS download batch storage lookup failed"
                    );
                    git_lfs_storage_error_response(error)
                }
            }
        }
        LfsBatchOperation::Upload => {
            match upload_batch_response_with_storage_lookup(&repository, state, request).await {
                Ok(response) => git_lfs_json_response(response),
                Err(error) => {
                    tracing::debug!(
                        repo_id = repository.id.as_str(),
                        %error,
                        "Git LFS upload batch storage lookup failed"
                    );
                    git_lfs_storage_error_response(error)
                }
            }
        }
    }
}

fn permission_required_for_batch_operation(operation: LfsBatchOperation) -> RepositoryPermission {
    match operation {
        LfsBatchOperation::Download => RepositoryPermission::Read,
        LfsBatchOperation::Upload => RepositoryPermission::Write,
    }
}

async fn download_batch_response_with_storage_lookup(
    repository: &RepositoryMapping,
    state: &LfsServerState,
    request: LfsBatchRequest,
) -> ServerResult<LfsBatchResponse> {
    let requested_objects = request.objects;
    let unique_objects = requested_objects.iter().cloned().collect::<BTreeSet<_>>();
    let outcomes = stream::iter(unique_objects)
        .map(|object| async move {
            let outcome = match state.lookup_object(repository, &object).await {
                Ok(Some(_)) => LfsBatchDownloadObject::available(object),
                Ok(None) => LfsBatchDownloadObject::missing(object),
                Err(error) => LfsBatchDownloadObject::error(
                    object,
                    lfs_batch_object_error_from_server_error(&error),
                ),
            };
            (outcome_object(&outcome).clone(), outcome)
        })
        .buffered(BATCH_STORAGE_LOOKUP_CONCURRENCY)
        .collect::<BTreeMap<_, _>>()
        .await;
    let objects = requested_objects
        .into_iter()
        .map(|object| {
            outcomes
                .get(&object)
                .expect("every requested object should have one lookup outcome")
                .clone()
        })
        .collect::<Vec<_>>();

    Ok(LfsBatchResponse::download(
        &state.public_url,
        repository.route_path(),
        objects,
    ))
}

async fn upload_batch_response_with_storage_lookup(
    repository: &RepositoryMapping,
    state: &LfsServerState,
    request: LfsBatchRequest,
) -> ServerResult<LfsBatchResponse> {
    let requested_objects = request.objects;
    let unique_objects = requested_objects.iter().cloned().collect::<BTreeSet<_>>();
    let outcomes = stream::iter(unique_objects)
        .map(|object| async move {
            let outcome = match state.lookup_object(repository, &object).await {
                Ok(Some(_)) => LfsBatchUploadObject::present(object),
                Ok(None) => LfsBatchUploadObject::needed(object),
                Err(error) => LfsBatchUploadObject::error(
                    object,
                    lfs_batch_object_error_from_server_error(&error),
                ),
            };
            (upload_outcome_object(&outcome).clone(), outcome)
        })
        .buffered(BATCH_STORAGE_LOOKUP_CONCURRENCY)
        .collect::<BTreeMap<_, _>>()
        .await;
    let objects = requested_objects
        .into_iter()
        .map(|object| {
            outcomes
                .get(&object)
                .expect("every requested object should have one lookup outcome")
                .clone()
        })
        .collect::<Vec<_>>();

    Ok(LfsBatchResponse::upload(
        &state.public_url,
        repository.route_path(),
        objects,
    ))
}

fn outcome_object(outcome: &LfsBatchDownloadObject) -> &LfsObject {
    match outcome {
        LfsBatchDownloadObject::Available { object }
        | LfsBatchDownloadObject::Missing { object }
        | LfsBatchDownloadObject::Error { object, .. } => object,
    }
}

fn upload_outcome_object(outcome: &LfsBatchUploadObject) -> &LfsObject {
    match outcome {
        LfsBatchUploadObject::Needed { object }
        | LfsBatchUploadObject::Present { object }
        | LfsBatchUploadObject::Error { object, .. } => object,
    }
}

fn git_lfs_json_response(response: LfsBatchResponse) -> Response {
    (
        StatusCode::OK,
        [(CONTENT_TYPE, GIT_LFS_JSON_CONTENT_TYPE)],
        Json(response),
    )
        .into_response()
}

fn git_lfs_json_error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        [(CONTENT_TYPE, GIT_LFS_JSON_CONTENT_TYPE)],
        Json(LfsErrorResponse {
            message: message.into(),
        }),
    )
        .into_response()
}

fn git_lfs_body_error_response(rejection_response: Response) -> Response {
    let status = rejection_response.status();
    let message = if status == StatusCode::PAYLOAD_TOO_LARGE {
        "Git LFS request body exceeds the configured limit"
    } else {
        "Git LFS request body could not be read"
    };

    git_lfs_json_error_response(status, message)
}

fn git_lfs_authorization_error_response(error: ServerError) -> Response {
    let (status, message) = match error {
        ServerError::RepositoryProvider {
            source: RepositoryProviderError::AuthenticationRequired { .. },
        } => (
            StatusCode::UNAUTHORIZED,
            "repository provider authentication is required for this Git LFS operation",
        ),
        ServerError::RepositoryProvider {
            source:
                RepositoryProviderError::PermissionDenied { .. }
                | RepositoryProviderError::SsoRequired { .. },
        } => (
            StatusCode::FORBIDDEN,
            "repository provider denied this Git LFS operation",
        ),
        ServerError::RepositoryProvider {
            source: RepositoryProviderError::RepositoryNotFound { .. },
        } => (
            StatusCode::NOT_FOUND,
            "repository provider could not find this repository",
        ),
        ServerError::InvalidRequest { .. } => (
            StatusCode::BAD_REQUEST,
            "repository authorization request was invalid",
        ),
        ServerError::RepositoryProvider { .. } => (
            StatusCode::BAD_GATEWAY,
            "repository provider authorization failed",
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Git LFS authorization failed",
        ),
    };

    let mut response = git_lfs_json_error_response(status, message);
    if status == StatusCode::UNAUTHORIZED {
        let headers = response.headers_mut();
        headers.append(
            WWW_AUTHENTICATE,
            HeaderValue::from_static(LFS_AUTH_CHALLENGE),
        );
        headers.append(
            WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"lfs-cloud\""),
        );
    }

    response
}

fn git_lfs_storage_error_response(error: ServerError) -> Response {
    let (status, message) = match error {
        ServerError::Storage {
            source: StorageError::IntegrityMismatch { .. },
        } => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "uploaded Git LFS object did not match the requested SHA-256",
        ),
        ServerError::Storage {
            source: StorageError::ObjectNotFound { .. },
        } => (StatusCode::NOT_FOUND, "Git LFS object was not found"),
        ServerError::Storage {
            source: StorageError::Conflict { .. },
        } => (
            StatusCode::CONFLICT,
            "Git LFS storage reported an object conflict",
        ),
        ServerError::Storage {
            source: StorageError::QuotaExceeded { .. },
        } => (
            StatusCode::INSUFFICIENT_STORAGE,
            "Git LFS storage quota was exceeded",
        ),
        ServerError::Storage {
            source: StorageError::Retryable { .. },
        } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Git LFS storage operation can be retried later",
        ),
        ServerError::Storage {
            source:
                StorageError::AuthenticationRequired { .. } | StorageError::CredentialLoad { .. },
        } => (
            StatusCode::BAD_GATEWAY,
            "Git LFS storage authentication failed",
        ),
        ServerError::Storage {
            source: StorageError::Unsupported { .. },
        } => (
            StatusCode::NOT_IMPLEMENTED,
            "Git LFS storage transfer handling is not configured",
        ),
        ServerError::Storage { .. } => {
            (StatusCode::BAD_GATEWAY, "Git LFS storage operation failed")
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Git LFS transfer handling failed",
        ),
    };

    git_lfs_json_error_response(status, message)
}

fn git_lfs_download_storage_error_response(error: ServerError) -> Response {
    if matches!(
        &error,
        ServerError::Storage {
            source: StorageError::IntegrityMismatch { .. },
        }
    ) {
        return git_lfs_json_error_response(
            StatusCode::BAD_GATEWAY,
            "Git LFS storage returned an object that failed integrity validation",
        );
    }

    git_lfs_storage_error_response(error)
}

fn lfs_batch_object_error_from_server_error(error: &ServerError) -> LfsBatchObjectError {
    match error {
        ServerError::Storage {
            source: StorageError::ObjectNotFound { .. },
        } => LfsBatchObjectError::new(404, "object not found"),
        ServerError::Storage {
            source: StorageError::Conflict { .. },
        } => LfsBatchObjectError::new(409, "object storage conflict"),
        ServerError::Storage {
            source: StorageError::QuotaExceeded { .. },
        } => LfsBatchObjectError::new(507, "object storage quota exceeded"),
        ServerError::Storage {
            source: StorageError::Retryable { .. },
        } => LfsBatchObjectError::new(503, "object storage lookup can be retried later"),
        ServerError::Storage {
            source:
                StorageError::AuthenticationRequired { .. } | StorageError::CredentialLoad { .. },
        } => LfsBatchObjectError::new(502, "object storage authentication failed"),
        ServerError::Storage {
            source: StorageError::Unsupported { .. },
        } => LfsBatchObjectError::new(501, "object storage lookup is not configured"),
        ServerError::Storage { .. } => {
            LfsBatchObjectError::new(502, "object storage lookup failed")
        }
        _ => LfsBatchObjectError::new(500, "object availability lookup failed"),
    }
}

#[derive(Clone, Debug, Serialize)]
struct LfsErrorResponse {
    message: String,
}

fn authenticate_lfs_session(
    headers: &HeaderMap,
    session_store: &LocalLfsSessionStore,
) -> ServerResult<AuthenticatedLfsSession> {
    let token = lfs_session_token_from_authorization_header(headers)?;
    let record = session_store
        .verify_record(&token)
        .ok_or_else(|| unauthorized("invalid or expired lfs session token"))?;

    Ok(AuthenticatedLfsSession { token, record })
}

fn lfs_session_token_from_authorization_header(
    headers: &HeaderMap,
) -> ServerResult<LfsSessionToken> {
    let Some(value) = headers.get(AUTHORIZATION) else {
        return Err(unauthorized("missing authorization header"));
    };
    let value = value
        .to_str()
        .map_err(|_| unauthorized("authorization header is not valid UTF-8"))?;

    if let Some(token) = authorization_credentials(value, "Bearer") {
        return LfsSessionToken::from_secret(token.to_owned()).map_err(|_| {
            unauthorized("bearer authorization did not contain a valid lfs session token")
        });
    }

    if let Some(credentials) = authorization_credentials(value, "Basic") {
        return lfs_session_token_from_basic_credentials(credentials);
    }

    Err(unauthorized("unsupported authorization scheme"))
}

fn lfs_session_token_from_basic_credentials(credentials: &str) -> ServerResult<LfsSessionToken> {
    let decoded = BASE64_STANDARD
        .decode(credentials)
        .map_err(|_| unauthorized("basic authorization credentials were not valid base64"))?;
    let decoded = String::from_utf8(decoded)
        .map_err(|_| unauthorized("basic authorization credentials were not valid UTF-8"))?;
    let Some((username, password)) = decoded.split_once(':') else {
        return Err(unauthorized(
            "basic authorization credentials were malformed",
        ));
    };

    if username != DEFAULT_GIT_CREDENTIAL_USERNAME {
        return Err(unauthorized(
            "basic authorization username was not accepted",
        ));
    }

    LfsSessionToken::from_secret(password.to_owned())
        .map_err(|_| unauthorized("basic authorization did not contain a valid lfs session token"))
}

fn authorization_credentials<'a>(value: &'a str, scheme: &str) -> Option<&'a str> {
    let value = value.trim();
    let scheme_end = value.find(char::is_whitespace).unwrap_or(value.len());
    let (actual_scheme, rest) = value.split_at(scheme_end);

    if !actual_scheme.eq_ignore_ascii_case(scheme) {
        return None;
    }

    let credentials = rest.trim_start();
    (!credentials.is_empty()).then_some(credentials)
}

fn unauthorized(reason: impl Into<String>) -> ServerError {
    ServerError::Unauthorized {
        reason: reason.into(),
    }
}

fn authentication_required_response() -> Response {
    let mut headers = HeaderMap::new();
    headers.append(
        WWW_AUTHENTICATE,
        HeaderValue::from_static(LFS_AUTH_CHALLENGE),
    );
    headers.append(
        WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"lfs-cloud\""),
    );

    (
        StatusCode::UNAUTHORIZED,
        headers,
        [(CONTENT_TYPE, GIT_LFS_JSON_CONTENT_TYPE)],
        Json(LfsErrorResponse {
            message: "LFS Cloud authentication required".to_owned(),
        }),
    )
        .into_response()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ServerBind {
    host: String,
    port: u16,
}

impl ServerBind {
    fn from_config_and_overrides(
        config_host: &str,
        config_port: u16,
        host_override: Option<String>,
        port_override: Option<u16>,
    ) -> ServerResult<Self> {
        let host = host_override.unwrap_or_else(|| config_host.to_owned());
        let port = port_override.unwrap_or(config_port);

        if host.trim().is_empty() {
            return Err(ServerError::InvalidConfiguration {
                message: "server.host must not be empty".to_owned(),
            });
        }
        if host.trim() != host {
            return Err(ServerError::InvalidConfiguration {
                message: "server.host must not include leading or trailing whitespace".to_owned(),
            });
        }
        if !is_valid_bind_host(&host) {
            return Err(ServerError::InvalidConfiguration {
                message: "server.host must be an IP address or DNS hostname".to_owned(),
            });
        }
        if port == 0 {
            // User-facing server config should advertise a stable URL instead
            // of silently choosing an OS-assigned ephemeral listener.
            return Err(ServerError::InvalidConfiguration {
                message: "server.port must be greater than zero".to_owned(),
            });
        }

        Ok(Self { host, port })
    }

    fn validate_transport(&self, config: &ServerConfig) -> ServerResult<()> {
        if config.server.allow_insecure_http {
            return Ok(());
        }

        let public_url = Url::parse(&config.server.public_url).map_err(|source| {
            ServerError::InvalidConfiguration {
                message: format!("server.public_url must be a valid absolute URL: {source}"),
            }
        })?;
        if public_url.scheme() == "https"
            || self
                .host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
        {
            return Ok(());
        }

        Err(ServerError::InvalidConfiguration {
            message: "server.host must be an exact loopback IP when server.public_url uses HTTP; set server.allow_insecure_http to true only for a trusted development network or use HTTPS through trusted TLS termination".to_owned(),
        })
    }
}

/// Repository route and endpoint resolved from an incoming request path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedLfsRoute {
    /// Configured repository mapping matched by the request path.
    pub repository: RepositoryMapping,
    /// Git LFS endpoint beneath the repository's `/info/lfs` base path.
    pub endpoint: LfsRouteEndpoint,
}

/// Git LFS endpoint beneath a configured repository route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LfsRouteEndpoint {
    /// The repository's base `/info/lfs` path.
    Info,
    /// The Git LFS batch API at `/objects/batch`.
    Batch,
    /// An object transfer endpoint at `/objects/{oid}`.
    Object {
        /// SHA-256 object identifier from the transfer path.
        oid: LfsOid,
    },
}

/// Resolves request paths to configured repository LFS routes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LfsRouteResolver {
    routes: Vec<ConfiguredLfsRoute>,
}

impl LfsRouteResolver {
    /// Builds a resolver from validated server configuration.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::{LfsRouteEndpoint, LfsRouteResolver, ServerConfig};
    ///
    /// let config = ServerConfig::load_from_str(
    ///     r#"
    /// server:
    ///   public_url: http://127.0.0.1:8080
    /// repository_providers:
    ///   github-main:
    ///     type: github
    ///     api_url: https://api.github.com
    ///     oauth_client_id: test-client
    ///     oauth_client_secret: test-secret
    /// storage_providers:
    ///   drive-user-a:
    ///     type: google_drive
    ///     credentials_ref: google-drive-user-a
    ///     root_folder_id: root
    /// repositories:
    ///   - id: github-main:owner/repo
    ///     repo_provider: github-main
    ///     host: github.com
    ///     owner: owner
    ///     name: repo
    ///     provider_repository_id: "8675309"
    ///     storage_provider: drive-user-a
    /// "#,
    /// )?;
    /// let resolver = LfsRouteResolver::new(&config);
    /// let route = resolver.resolve_path("/github.com/owner/repo.git/info/lfs/objects/batch")?;
    ///
    /// assert_eq!(route.endpoint, LfsRouteEndpoint::Batch);
    /// # Ok::<(), lfs_cloud::ServerError>(())
    /// ```
    #[must_use]
    pub fn new(config: &ServerConfig) -> Self {
        let mut routes = config
            .repositories
            .iter()
            .cloned()
            .map(|repository| {
                let route_path = repository.route_path();
                ConfiguredLfsRoute {
                    route_path_with_slash: format!("{route_path}/"),
                    route_path,
                    repository,
                }
            })
            .collect::<Vec<_>>();

        routes.sort_by(|left, right| right.route_path.len().cmp(&left.route_path.len()));

        Self { routes }
    }

    /// Resolves an HTTP request path to a configured Git LFS route.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::RouteNotConfigured`] for unknown repositories and
    /// [`ServerError::InvalidRequest`] for malformed endpoints under a known
    /// repository route.
    pub fn resolve_path(&self, path: &str) -> ServerResult<ResolvedLfsRoute> {
        if !path.starts_with('/') {
            return Err(ServerError::InvalidRequest {
                message: "route path must start with '/'".to_owned(),
            });
        }

        for route in &self.routes {
            if path == route.route_path || path == route.route_path_with_slash {
                return Ok(ResolvedLfsRoute {
                    repository: route.repository.clone(),
                    endpoint: LfsRouteEndpoint::Info,
                });
            }

            let Some(suffix) = path.strip_prefix(&route.route_path_with_slash) else {
                continue;
            };

            return Ok(ResolvedLfsRoute {
                repository: route.repository.clone(),
                endpoint: parse_lfs_route_endpoint(suffix)?,
            });
        }

        Err(ServerError::RouteNotConfigured {
            path: path.to_owned(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConfiguredLfsRoute {
    route_path: String,
    route_path_with_slash: String,
    repository: RepositoryMapping,
}

fn parse_lfs_route_endpoint(suffix: &str) -> ServerResult<LfsRouteEndpoint> {
    if suffix == "objects/batch" {
        return Ok(LfsRouteEndpoint::Batch);
    }

    if let Some(oid) = suffix.strip_prefix("objects/") {
        if oid.contains('/') || oid.is_empty() {
            return Err(ServerError::InvalidRequest {
                message: format!("unsupported LFS object endpoint {suffix:?}"),
            });
        }

        return Ok(LfsRouteEndpoint::Object {
            oid: LfsOid::new(oid).map_err(|source| ServerError::InvalidRequest {
                message: format!("invalid LFS object id in route: {source}"),
            })?,
        });
    }

    Err(ServerError::InvalidRequest {
        message: format!("unsupported LFS endpoint {suffix:?}"),
    })
}

/// URLs printed when the server starts listening.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvertisedServerUrls {
    /// URL suitable for same-machine Git LFS clients.
    pub local: String,
    /// URL suitable for trusted LAN clients, when it can be detected.
    pub network: Option<String>,
}

/// Computes local and LAN URLs for a bound listener.
#[must_use]
pub fn advertised_server_urls(bind_host: &str, port: u16) -> AdvertisedServerUrls {
    let local_host = if is_unspecified_host(bind_host) {
        "127.0.0.1".to_owned()
    } else {
        advertised_url_host(bind_host)
    };
    let network = if is_unspecified_host(bind_host) {
        detect_lan_ipv4().map(|ip| format!("http://{ip}:{port}"))
    } else {
        None
    };

    AdvertisedServerUrls {
        local: format!("http://{local_host}:{port}"),
        network,
    }
}

fn advertised_url_host(host: &str) -> String {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(ip)) => format!("[{ip}]"),
        Ok(IpAddr::V4(ip)) => ip.to_string(),
        Err(_) => host.to_owned(),
    }
}

fn is_valid_bind_host(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok() || is_valid_dns_hostname(host)
}

fn is_valid_dns_hostname(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.bytes().enumerate().all(|(index, byte)| {
                    let is_alphanumeric = byte.is_ascii_alphanumeric();
                    let is_inner_hyphen = byte == b'-' && index > 0 && index + 1 < label.len();
                    is_alphanumeric || is_inner_hyphen
                })
        })
}

/// Renders the startup message shown by `lfs-cloud serve`.
#[must_use]
pub fn render_server_startup_message(urls: &AdvertisedServerUrls) -> String {
    let network = urls.network.as_deref().unwrap_or("(not detected)");

    format!(
        "lfs-cloud server running\n  local:   {}\n  network: {}",
        urls.local, network
    )
}

fn is_unspecified_host(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::" | "[::]")
}

fn detect_lan_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))).ok()?;
    // UDP connect only asks the OS which local interface would be used; no
    // LFS Cloud payload is sent to this public address.
    socket.connect(SocketAddr::from(([8, 8, 8, 8], 80))).ok()?;

    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, ErrorKind},
        path::Path as FsPath,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use axum::{
        Json, Router,
        body::{Body, Bytes, to_bytes},
        extract::Path,
        http::{
            HeaderMap, HeaderValue, Method, Request, StatusCode,
            header::{ALLOW, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, WWW_AUTHENTICATE},
        },
        response::Response,
        routing::get,
    };
    use tokio::sync::{Barrier, Notify};
    use tower::ServiceExt;

    use super::{
        BASE64_STANDARD, GoogleDriveAccessTokenCache, LFS_AUTH_CHALLENGE, LFS_SESSION_REVOKE_PATH,
        LfsBatchAuthorizer, LfsDownloadResponse, LfsObjectTransferStore, LfsRouteEndpoint,
        LfsRouteResolver, LfsSessionRecord, MAX_UPLOAD_OBJECT_BYTES, ServerBind,
        UploadStagingGuardrails, advertised_server_urls, authenticate_lfs_session,
        ensure_temp_space_for_upload_with_available_space, lfs_server_router_with_sessions,
        lfs_server_router_with_sessions_authorizer_and_transfer_store, production_session_store,
        render_server_startup_message, server_router_with_sessions, stage_upload_request_body,
        stage_upload_request_body_with_guardrails, stage_upload_request_body_with_limit,
        upload_staging_file_io_error, upload_staging_preflight_size,
    };
    use base64::Engine as _;
    use futures_util::stream;
    use sha2::{Digest, Sha256};

    use crate::{
        DEFAULT_GIT_CREDENTIAL_USERNAME, GitHubOAuthAccessToken, GoogleDriveCredential,
        GoogleDriveTokenRefresher, LfsBatchOperation, LfsBatchResponse, LfsObject, LfsObjectSize,
        LfsOid, LfsSessionToken, LocalLfsSessionStore, MetadataDatabase, ProviderFuture,
        RepositoryMapping, RepositoryPermission, RepositoryProviderError, RepositoryUser,
        ServerConfig, ServerError, ServerResult, StorageError, StoredObject,
    };

    const VALID_BATCH_REQUEST: &str = r#"{
        "operation": "download",
        "transfers": ["basic"],
        "ref": { "name": "refs/heads/main" },
        "objects": [
            {
                "oid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size": 42
            }
        ]
    }"#;
    const VALID_UPLOAD_BATCH_REQUEST: &str = r#"{
        "operation": "upload",
        "transfers": ["basic"],
        "objects": [
            {
                "oid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size": 42
            }
        ]
    }"#;
    const UNSUPPORTED_TRANSFER_BATCH_REQUEST: &str = r#"{
        "operation": "download",
        "transfers": ["ssh"],
        "objects": [
            {
                "oid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size": 42
            }
        ]
    }"#;

    fn test_config() -> ServerConfig {
        test_config_with_github_api_url("https://api.github.com")
    }

    fn test_config_with_github_api_url(api_url: &str) -> ServerConfig {
        ServerConfig::load_from_str(&format!(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: {api_url}
    oauth_client_id: test-client
    oauth_client_secret: test-secret
storage_providers:
  drive-user-a:
    type: google_drive
    credentials_ref: google-drive-user-a
    root_folder_id: root
repositories:
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    provider_repository_id: "8675309"
    storage_provider: drive-user-a
"#,
        ))
        .expect("test config should load")
    }

    fn test_config_with_work_limits(
        max_batch_objects: usize,
        max_provider_calls: usize,
    ) -> ServerConfig {
        let mut config = test_config();
        config.server.max_batch_objects = max_batch_objects;
        config.server.max_provider_calls = max_provider_calls;
        config
    }

    #[tokio::test]
    async fn google_drive_access_token_cache_single_flights_refreshes() {
        let refreshes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handler_refreshes = refreshes.clone();
        let app = Router::new().route(
            "/token",
            axum::routing::post(move || {
                let handler_refreshes = handler_refreshes.clone();
                async move {
                    handler_refreshes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Json(serde_json::json!({
                        "access_token": "cached-access-token",
                        "token_type": "Bearer",
                        "expires_in": 3600,
                        "scope": "https://www.googleapis.com/auth/drive.file"
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("token server should bind");
        let address = listener
            .local_addr()
            .expect("token server address should be available");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("token server should run");
        });
        let credential = GoogleDriveCredential::from_json(
            "drive-user-a",
            "test-ref",
            &serde_json::json!({
                "client_id": "client-id",
                "client_secret": "client-secret",
                "refresh_token": "refresh-token",
                "token_uri": format!("http://{address}/token")
            })
            .to_string(),
        )
        .expect("test credential should parse");
        let refresher = GoogleDriveTokenRefresher::new().expect("token refresher should build");
        let cache = GoogleDriveAccessTokenCache::default();

        let (first, second, third) = tokio::join!(
            cache.get_or_refresh("drive-user-a", &credential, &refresher),
            cache.get_or_refresh("drive-user-a", &credential, &refresher),
            cache.get_or_refresh("drive-user-a", &credential, &refresher),
        );

        let first = first.expect("first refresh should succeed");
        assert_eq!(second.expect("second refresh should succeed"), first);
        assert_eq!(third.expect("third refresh should succeed"), first);
        assert_eq!(refreshes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[derive(Clone, Default)]
    struct RecordingBatchAuthorizer {
        required: Arc<Mutex<Vec<RepositoryPermission>>>,
        deny: bool,
    }

    impl RecordingBatchAuthorizer {
        fn allow() -> Self {
            Self::default()
        }

        fn deny() -> Self {
            Self {
                required: Arc::new(Mutex::new(Vec::new())),
                deny: true,
            }
        }

        fn required_permissions(&self) -> Vec<RepositoryPermission> {
            self.required
                .lock()
                .expect("authorization records should not be poisoned")
                .clone()
        }
    }

    impl LfsBatchAuthorizer for RecordingBatchAuthorizer {
        fn authorize<'a>(
            &'a self,
            repository: &'a RepositoryMapping,
            _session: &'a LfsSessionRecord,
            operation: LfsBatchOperation,
        ) -> ProviderFuture<'a, ServerResult<()>> {
            Box::pin(async move {
                let required = match operation {
                    LfsBatchOperation::Download => RepositoryPermission::Read,
                    LfsBatchOperation::Upload => RepositoryPermission::Write,
                };
                self.required
                    .lock()
                    .expect("authorization records should not be poisoned")
                    .push(required);

                if self.deny {
                    return Err(ServerError::RepositoryProvider {
                        source: RepositoryProviderError::PermissionDenied {
                            provider: repository.repo_provider.clone(),
                            owner: repository.owner.clone(),
                            repo: repository.name.clone(),
                            required,
                        },
                    });
                }

                Ok(())
            })
        }
    }

    struct AuthenticationRequiredBatchAuthorizer;

    impl LfsBatchAuthorizer for AuthenticationRequiredBatchAuthorizer {
        fn authorize<'a>(
            &'a self,
            repository: &'a RepositoryMapping,
            _session: &'a LfsSessionRecord,
            _operation: LfsBatchOperation,
        ) -> ProviderFuture<'a, ServerResult<()>> {
            Box::pin(async move {
                Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::AuthenticationRequired {
                        provider: repository.repo_provider.clone(),
                    },
                })
            })
        }
    }

    #[derive(Clone, Default)]
    struct RecordingTransferStore {
        lookup_object: Arc<Mutex<Option<StoredObject>>>,
        lookup_unsupported: bool,
        lookups: Arc<Mutex<Vec<LfsObject>>>,
        lookup_delay: Option<Duration>,
        active_lookups: Arc<std::sync::atomic::AtomicUsize>,
        peak_lookups: Arc<std::sync::atomic::AtomicUsize>,
        download_body: Arc<Mutex<Option<Vec<u8>>>>,
        download_integrity_mismatch: bool,
        downloads: Arc<Mutex<Vec<RecordedDownload>>>,
        uploads: Arc<Mutex<Vec<RecordedUpload>>>,
        verified: Arc<Mutex<Vec<RecordedVerification>>>,
        upload_started: Option<Arc<Notify>>,
        upload_release: Option<Arc<Barrier>>,
    }

    impl RecordingTransferStore {
        fn missing() -> Self {
            Self::default()
        }

        fn existing() -> Self {
            let stored_object = StoredObject::new(
                "drive-user-a",
                LfsObject::new(
                    LfsOid::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                        .expect("test oid should parse"),
                    LfsObjectSize::new(42),
                ),
                "drive-file-existing",
            );
            let body_len = usize::try_from(stored_object.object.size.bytes())
                .expect("test object size should fit usize");
            Self {
                lookup_object: Arc::new(Mutex::new(Some(stored_object))),
                download_body: Arc::new(Mutex::new(Some(vec![0; body_len]))),
                download_integrity_mismatch: false,
                downloads: Arc::new(Mutex::new(Vec::new())),
                uploads: Arc::new(Mutex::new(Vec::new())),
                verified: Arc::new(Mutex::new(Vec::new())),
                upload_started: None,
                upload_release: None,
                ..Self::default()
            }
        }

        fn lookup_unsupported() -> Self {
            Self {
                lookup_unsupported: true,
                ..Self::default()
            }
        }

        fn missing_with_lookup_delay(delay: Duration) -> Self {
            Self {
                lookup_delay: Some(delay),
                ..Self::default()
            }
        }

        fn existing_object(stored_object: StoredObject) -> Self {
            Self {
                lookup_object: Arc::new(Mutex::new(Some(stored_object))),
                download_body: Arc::new(Mutex::new(None)),
                download_integrity_mismatch: false,
                downloads: Arc::new(Mutex::new(Vec::new())),
                uploads: Arc::new(Mutex::new(Vec::new())),
                verified: Arc::new(Mutex::new(Vec::new())),
                upload_started: None,
                upload_release: None,
                ..Self::default()
            }
        }

        fn blocking_missing(upload_started: Arc<Notify>, upload_release: Arc<Barrier>) -> Self {
            Self {
                lookup_object: Arc::new(Mutex::new(None)),
                download_body: Arc::new(Mutex::new(None)),
                download_integrity_mismatch: false,
                downloads: Arc::new(Mutex::new(Vec::new())),
                uploads: Arc::new(Mutex::new(Vec::new())),
                verified: Arc::new(Mutex::new(Vec::new())),
                upload_started: Some(upload_started),
                upload_release: Some(upload_release),
                ..Self::default()
            }
        }

        fn existing_object_with_download_body(stored_object: StoredObject, body: Vec<u8>) -> Self {
            Self {
                lookup_object: Arc::new(Mutex::new(Some(stored_object))),
                download_body: Arc::new(Mutex::new(Some(body))),
                download_integrity_mismatch: false,
                downloads: Arc::new(Mutex::new(Vec::new())),
                uploads: Arc::new(Mutex::new(Vec::new())),
                verified: Arc::new(Mutex::new(Vec::new())),
                upload_started: None,
                upload_release: None,
                ..Self::default()
            }
        }

        fn existing_object_with_download_integrity_mismatch(stored_object: StoredObject) -> Self {
            Self {
                lookup_object: Arc::new(Mutex::new(Some(stored_object))),
                download_body: Arc::new(Mutex::new(Some(Vec::new()))),
                download_integrity_mismatch: true,
                downloads: Arc::new(Mutex::new(Vec::new())),
                uploads: Arc::new(Mutex::new(Vec::new())),
                verified: Arc::new(Mutex::new(Vec::new())),
                upload_started: None,
                upload_release: None,
                ..Self::default()
            }
        }

        fn downloads(&self) -> Vec<RecordedDownload> {
            self.downloads
                .lock()
                .expect("download records should not be poisoned")
                .clone()
        }

        fn lookups(&self) -> Vec<LfsObject> {
            self.lookups
                .lock()
                .expect("lookup records should not be poisoned")
                .clone()
        }

        fn peak_lookups(&self) -> usize {
            self.peak_lookups.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn uploads(&self) -> Vec<RecordedUpload> {
            self.uploads
                .lock()
                .expect("upload records should not be poisoned")
                .clone()
        }

        fn verified_records(&self) -> Vec<RecordedVerification> {
            self.verified
                .lock()
                .expect("verification records should not be poisoned")
                .clone()
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RecordedDownload {
        repo_id: String,
        object: LfsObject,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RecordedUpload {
        repo_id: String,
        object: LfsObject,
        bytes: Vec<u8>,
        created_by: RepositoryUser,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RecordedVerification {
        repo_id: String,
        object: LfsObject,
        backend_id: String,
        created_by: RepositoryUser,
    }

    impl LfsObjectTransferStore for RecordingTransferStore {
        fn lookup_object<'a>(
            &'a self,
            _repository: &'a RepositoryMapping,
            object: &'a LfsObject,
        ) -> ProviderFuture<'a, ServerResult<Option<StoredObject>>> {
            Box::pin(async move {
                self.lookups
                    .lock()
                    .expect("lookup records should not be poisoned")
                    .push(object.clone());
                let active = self
                    .active_lookups
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                self.peak_lookups
                    .fetch_max(active, std::sync::atomic::Ordering::SeqCst);
                if let Some(delay) = self.lookup_delay {
                    tokio::time::sleep(delay).await;
                }
                self.active_lookups
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);

                if self.lookup_unsupported {
                    return Err(ServerError::Storage {
                        source: StorageError::Unsupported {
                            provider_type: "test-storage".to_owned(),
                        },
                    });
                }

                let lookup_object = self
                    .lookup_object
                    .lock()
                    .expect("lookup records should not be poisoned")
                    .clone();

                Ok(lookup_object.filter(|stored_object| stored_object.object == *object))
            })
        }

        fn upload_object<'a>(
            &'a self,
            repository: &'a RepositoryMapping,
            object: &'a LfsObject,
            source: &'a FsPath,
            created_by: &'a RepositoryUser,
        ) -> ProviderFuture<'a, ServerResult<StoredObject>> {
            Box::pin(async move {
                if let Some(upload_started) = &self.upload_started {
                    upload_started.notify_waiters();
                }
                if let Some(upload_release) = &self.upload_release {
                    upload_release.wait().await;
                }
                let bytes = std::fs::read(source).map_err(|source| ServerError::Internal {
                    message: format!("test upload file could not be read: {source}"),
                })?;
                self.uploads
                    .lock()
                    .expect("upload records should not be poisoned")
                    .push(RecordedUpload {
                        repo_id: repository.id.clone(),
                        object: object.clone(),
                        bytes,
                        created_by: created_by.clone(),
                    });

                let stored_object = StoredObject::new(
                    repository.storage_provider.clone(),
                    object.clone(),
                    "drive-file-uploaded",
                );
                self.lookup_object
                    .lock()
                    .expect("lookup records should not be poisoned")
                    .replace(stored_object.clone());

                Ok(stored_object)
            })
        }

        fn download_object_response<'a>(
            &'a self,
            repository: &'a RepositoryMapping,
            object: &'a LfsObject,
        ) -> ProviderFuture<'a, ServerResult<LfsDownloadResponse>> {
            Box::pin(async move {
                let Some(stored_object) = self.lookup_object(repository, object).await? else {
                    return Err(ServerError::Storage {
                        source: crate::StorageError::ObjectNotFound {
                            provider: repository.storage_provider.clone(),
                            oid: object.oid.as_hex().to_owned(),
                            size: object.size.bytes(),
                        },
                    });
                };
                if self.download_integrity_mismatch {
                    return Err(ServerError::Storage {
                        source: crate::StorageError::IntegrityMismatch {
                            expected_oid: object.oid.as_hex().to_owned(),
                            expected_size: object.size.bytes(),
                            actual_oid:
                                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                                    .to_owned(),
                            actual_size: object.size.bytes(),
                        },
                    });
                }
                let body = self
                    .download_body
                    .lock()
                    .expect("download body should not be poisoned")
                    .clone()
                    .ok_or_else(|| ServerError::Storage {
                        source: crate::StorageError::ObjectNotFound {
                            provider: stored_object.provider_id.clone(),
                            oid: object.oid.as_hex().to_owned(),
                            size: object.size.bytes(),
                        },
                    })?;

                self.downloads
                    .lock()
                    .expect("download records should not be poisoned")
                    .push(RecordedDownload {
                        repo_id: repository.id.clone(),
                        object: object.clone(),
                    });

                let response = Response::builder()
                    .status(StatusCode::OK)
                    .header(CONTENT_TYPE, "application/octet-stream")
                    .header(CONTENT_LENGTH, body.len().to_string())
                    .body(Body::from(body))
                    .map_err(|source| ServerError::Internal {
                        message: format!("test download response could not be built: {source}"),
                    })?;

                Ok(LfsDownloadResponse::new(stored_object, response))
            })
        }

        fn record_verified_object<'a>(
            &'a self,
            repository: &'a RepositoryMapping,
            object: &'a LfsObject,
            backend_id: &'a str,
            created_by: &'a RepositoryUser,
        ) -> ProviderFuture<'a, ServerResult<()>> {
            Box::pin(async move {
                self.verified
                    .lock()
                    .expect("verification records should not be poisoned")
                    .push(RecordedVerification {
                        repo_id: repository.id.clone(),
                        object: object.clone(),
                        backend_id: backend_id.to_owned(),
                        created_by: created_by.clone(),
                    });

                Ok(())
            })
        }
    }

    fn test_router_with_authorizer(
        store: LocalLfsSessionStore,
        authorizer: RecordingBatchAuthorizer,
    ) -> Router {
        test_router_with_authorizer_and_transfer_store(
            store,
            authorizer,
            RecordingTransferStore::missing(),
        )
    }

    fn test_router_with_authorizer_and_transfer_store(
        store: LocalLfsSessionStore,
        authorizer: RecordingBatchAuthorizer,
        transfer_store: RecordingTransferStore,
    ) -> Router {
        test_router_with_config_authorizer_and_transfer_store(
            test_config(),
            store,
            authorizer,
            transfer_store,
        )
    }

    fn test_router_with_config_authorizer_and_transfer_store(
        config: ServerConfig,
        store: LocalLfsSessionStore,
        authorizer: RecordingBatchAuthorizer,
        transfer_store: RecordingTransferStore,
    ) -> Router {
        lfs_server_router_with_sessions_authorizer_and_transfer_store(
            config,
            store,
            Arc::new(authorizer),
            Arc::new(transfer_store),
        )
    }

    #[test]
    fn route_resolver_matches_configured_lfs_paths() {
        let resolver = LfsRouteResolver::new(&test_config());

        let info = resolver
            .resolve_path("/github.com/owner/repo.git/info/lfs")
            .expect("base info route should resolve");
        let info_with_trailing_slash = resolver
            .resolve_path("/github.com/owner/repo.git/info/lfs/")
            .expect("base info route with a trailing slash should resolve");
        let batch = resolver
            .resolve_path("/github.com/owner/repo.git/info/lfs/objects/batch")
            .expect("batch route should resolve");
        let object = resolver
            .resolve_path(
                "/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .expect("object route should resolve");

        assert_eq!(info.repository.id, "github-main:owner/repo");
        assert_eq!(info.endpoint, LfsRouteEndpoint::Info);
        assert_eq!(info_with_trailing_slash.endpoint, LfsRouteEndpoint::Info);
        assert_eq!(batch.endpoint, LfsRouteEndpoint::Batch);
        assert!(
            matches!(object.endpoint, LfsRouteEndpoint::Object { oid } if oid.as_hex() == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn route_resolver_rejects_unknown_repositories_and_endpoints() {
        let resolver = LfsRouteResolver::new(&test_config());

        let unknown_repo = resolver
            .resolve_path("/github.com/owner/other.git/info/lfs/objects/batch")
            .expect_err("unknown route should be denied");
        let unknown_endpoint = resolver
            .resolve_path("/github.com/owner/repo.git/info/lfs/locks")
            .expect_err("unknown endpoint should be invalid");
        let bad_oid = resolver
            .resolve_path("/github.com/owner/repo.git/info/lfs/objects/not-a-sha")
            .expect_err("bad object oid should be invalid");

        assert!(matches!(
            unknown_repo,
            ServerError::RouteNotConfigured { .. }
        ));
        assert!(matches!(
            unknown_endpoint,
            ServerError::InvalidRequest { .. }
        ));
        assert!(matches!(bad_oid, ServerError::InvalidRequest { .. }));
    }

    #[test]
    fn advertised_urls_report_localhost_and_best_effort_network_url() {
        let localhost = advertised_server_urls("127.0.0.1", 8080);
        let all_interfaces = advertised_server_urls("0.0.0.0", 8080);
        let all_ipv6_interfaces = advertised_server_urls("::", 8080);

        assert_eq!(localhost.local, "http://127.0.0.1:8080");
        assert_eq!(localhost.network, None);
        assert_eq!(all_interfaces.local, "http://127.0.0.1:8080");
        assert_eq!(all_ipv6_interfaces.local, "http://127.0.0.1:8080");

        let message = render_server_startup_message(&all_interfaces);
        assert!(message.contains("lfs-cloud server running"));
        assert!(message.contains("local:   http://127.0.0.1:8080"));
        assert!(message.contains("network: "));
    }

    #[test]
    fn advertised_urls_bracket_ipv6_literals() {
        let loopback = advertised_server_urls("::1", 8080);

        assert_eq!(loopback.local, "http://[::1]:8080");
        assert_eq!(loopback.network, None);
    }

    #[test]
    fn server_bind_rejects_invalid_host_before_listener_bind() {
        let error = ServerBind::from_config_and_overrides("bad host", 8080, None, None)
            .expect_err("host with spaces should fail config validation");

        assert!(matches!(error, ServerError::InvalidConfiguration { .. }));
    }

    #[test]
    fn plaintext_listener_requires_loopback_or_explicit_unsafe_opt_in() {
        let bind = ServerBind::from_config_and_overrides("0.0.0.0", 8080, None, None)
            .expect("unspecified bind should be structurally valid");
        let config = test_config();
        let error = bind
            .validate_transport(&config)
            .expect_err("plaintext public listener should require explicit opt-in");
        assert!(error.to_string().contains("exact loopback IP"));

        let mut secure_public_config = config.clone();
        secure_public_config.server.public_url = "https://lfs.example.com".to_owned();
        bind.validate_transport(&secure_public_config)
            .expect("HTTPS through trusted TLS termination should allow a private bind");

        let mut development_config = config;
        development_config.server.allow_insecure_http = true;
        bind.validate_transport(&development_config)
            .expect("explicit unsafe opt-in should allow trusted LAN development");
    }

    #[test]
    fn auth_accepts_bearer_and_basic_lfs_session_tokens() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let mut bearer_headers = HeaderMap::new();
        bearer_headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).expect("bearer header should parse"),
        );
        let mut basic_headers = HeaderMap::new();
        basic_headers.insert(
            AUTHORIZATION,
            basic_authorization(DEFAULT_GIT_CREDENTIAL_USERNAME, &token),
        );

        let bearer_session = authenticate_lfs_session(&bearer_headers, &store)
            .expect("bearer token should authenticate");
        let basic_session = authenticate_lfs_session(&basic_headers, &store)
            .expect("basic credential token should authenticate");

        assert_eq!(bearer_session.metadata().login, "octocat");
        assert_eq!(basic_session.metadata().login, "octocat");
        assert_eq!(basic_session.metadata().provider_id, "github-main");
    }

    #[test]
    fn auth_rejects_missing_malformed_wrong_and_expired_tokens() {
        let (store, token) = issued_session_token(Duration::from_secs(1));
        let cases = [
            HeaderMap::new(),
            authorization_headers("Digest abc123"),
            authorization_headers("Bearer local token"),
            authorization_headers("Basic not-base64"),
            {
                let mut headers = HeaderMap::new();
                headers.insert(AUTHORIZATION, basic_authorization("github", &token));
                headers
            },
        ];

        for headers in cases {
            let error = authenticate_lfs_session(&headers, &store)
                .expect_err("invalid credentials should be denied");
            assert!(matches!(error, ServerError::Unauthorized { .. }));
        }

        std::thread::sleep(Duration::from_millis(1200));

        for headers in [authorization_headers(&format!("Bearer {token}")), {
            let mut headers = HeaderMap::new();
            headers.insert(
                AUTHORIZATION,
                basic_authorization(DEFAULT_GIT_CREDENTIAL_USERNAME, &token),
            );
            headers
        }] {
            let error = authenticate_lfs_session(&headers, &store)
                .expect_err("expired token should be denied");
            assert!(matches!(error, ServerError::Unauthorized { .. }));
        }
    }

    #[test]
    fn production_session_store_restores_credentials_after_database_reopen() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let database_path = directory.path().join("metadata.sqlite3");
        let config = test_config();
        let issued = {
            let database =
                Arc::new(MetadataDatabase::open(&database_path).expect("metadata DB should open"));
            let store = production_session_store(&config, database)
                .expect("production session store should open");
            let github_token = GitHubOAuthAccessToken::from_secret("gho_production_restart")
                .expect("GitHub token should parse");

            store
                .issue_session_with_github_token(
                    &RepositoryUser::new("github-main", "octocat", Some("42".to_owned())),
                    ["repo"],
                    github_token,
                )
                .expect("session should be issued")
        };

        let reopened_database =
            Arc::new(MetadataDatabase::open(&database_path).expect("metadata DB should reopen"));
        let reopened = production_session_store(&config, reopened_database)
            .expect("production session store should reopen");
        let restored = reopened
            .verify_record(&issued.token)
            .expect("production credential should survive restart");

        assert_eq!(restored.metadata().stable_id.as_deref(), Some("42"));
        assert_eq!(
            restored
                .github_access_token()
                .expect("GitHub token should be restored")
                .as_str(),
            "gho_production_restart"
        );
    }

    #[tokio::test]
    async fn configured_lfs_routes_require_valid_session_tokens() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let router = lfs_server_router_with_sessions(test_config(), store);

        let unauthenticated = router
            .clone()
            .oneshot(lfs_request(
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                None,
            ))
            .await
            .expect("router should respond");
        let unknown_route = router
            .clone()
            .oneshot(lfs_request(
                "/github.com/owner/other.git/info/lfs/objects/batch",
                None,
            ))
            .await
            .expect("router should respond");
        let authenticated = router
            .oneshot(lfs_request(
                "/github.com/owner/repo.git/info/lfs",
                Some(&format!("Bearer {token}")),
            ))
            .await
            .expect("router should respond");

        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
        let challenge_values = unauthenticated
            .headers()
            .get_all(WWW_AUTHENTICATE)
            .iter()
            .map(|value| value.to_str().expect("challenge should be valid ASCII"))
            .collect::<Vec<_>>();
        assert!(challenge_values.contains(&LFS_AUTH_CHALLENGE));
        assert!(challenge_values.contains(&"Bearer realm=\"lfs-cloud\""));
        assert_lfs_json_error(
            unauthenticated,
            StatusCode::UNAUTHORIZED,
            "LFS Cloud authentication required",
        )
        .await;
        assert_lfs_json_error(
            unknown_route,
            StatusCode::NOT_FOUND,
            "No configured LFS Cloud repository route matches this path",
        )
        .await;
        assert_lfs_json_error(
            authenticated,
            StatusCode::NOT_IMPLEMENTED,
            "Git LFS endpoint routing is configured; transfer handling is not implemented yet",
        )
        .await;
    }

    #[tokio::test]
    async fn authenticated_session_route_revokes_the_presented_token() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let router = server_router_with_sessions(test_config(), store.clone())
            .expect("server router should build");

        let response = router
            .clone()
            .oneshot(lfs_request_with_method_and_body(
                Method::DELETE,
                LFS_SESSION_REVOKE_PATH,
                Some(&format!("Bearer {token}")),
                "",
            ))
            .await
            .expect("router should respond");
        let replay = router
            .oneshot(lfs_request_with_method_and_body(
                Method::DELETE,
                LFS_SESSION_REVOKE_PATH,
                Some(&format!("Bearer {token}")),
                "",
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            store
                .verify(
                    &LfsSessionToken::from_secret(token)
                        .expect("issued token should remain valid syntax")
                )
                .is_none()
        );
        assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn definitive_upstream_authentication_failure_revokes_local_session() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let router = lfs_server_router_with_sessions_authorizer_and_transfer_store(
            test_config(),
            store.clone(),
            Arc::new(AuthenticationRequiredBatchAuthorizer),
            Arc::new(RecordingTransferStore::missing()),
        );

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                VALID_BATCH_REQUEST,
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            store
                .verify(
                    &LfsSessionToken::from_secret(token)
                        .expect("issued token should remain valid syntax")
                )
                .is_none()
        );
    }

    #[tokio::test]
    async fn authenticated_batch_route_parses_valid_batch_requests() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let router = test_router_with_authorizer(store, RecordingBatchAuthorizer::allow());

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                VALID_BATCH_REQUEST,
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/vnd.git-lfs+json")
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should collect");
        let body: LfsBatchResponse =
            serde_json::from_slice(&body).expect("response should be Git LFS batch JSON");

        assert_eq!(body.transfer, "basic");
        assert_eq!(body.objects.len(), 1);
        assert_eq!(
            body.objects[0].oid.as_hex(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(body.objects[0].size.bytes(), 42);
        assert_eq!(
            body.objects[0].error.as_ref().map(|error| error.code),
            Some(404)
        );
        assert!(body.objects[0].actions.is_empty());
    }

    #[tokio::test]
    async fn batch_route_rejects_object_count_before_provider_calls() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let authorizer = RecordingBatchAuthorizer::allow();
        let transfer_store = RecordingTransferStore::missing();
        let router = test_router_with_config_authorizer_and_transfer_store(
            test_config_with_work_limits(1, 16),
            store,
            authorizer.clone(),
            transfer_store.clone(),
        );
        let body = serde_json::json!({
            "operation": "download",
            "objects": [
                { "oid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "size": 42 },
                { "oid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "size": 42 }
            ]
        })
        .to_string();

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                body,
            ))
            .await
            .expect("router should respond");

        assert_lfs_json_error(
            response,
            StatusCode::PAYLOAD_TOO_LARGE,
            "Git LFS batch contains more than 1 object entries",
        )
        .await;
        assert!(authorizer.required_permissions().is_empty());
        assert!(transfer_store.lookups().is_empty());
    }

    #[tokio::test]
    async fn batch_route_deduplicates_storage_lookups_and_preserves_results() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let authorizer = RecordingBatchAuthorizer::allow();
        let transfer_store = RecordingTransferStore::missing();
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            authorizer.clone(),
            transfer_store.clone(),
        );
        let body = serde_json::json!({
            "operation": "download",
            "objects": [
                { "oid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "size": 42 },
                { "oid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "size": 42 },
                { "oid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "size": 42 }
            ]
        })
        .to_string();

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                body,
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should collect");
        let body: LfsBatchResponse =
            serde_json::from_slice(&body).expect("response should be Git LFS batch JSON");
        assert_eq!(body.objects.len(), 3);
        assert_eq!(transfer_store.lookups().len(), 1);
        assert_eq!(
            authorizer.required_permissions(),
            vec![RepositoryPermission::Read]
        );
    }

    #[tokio::test]
    async fn batch_provider_calls_obey_the_server_wide_concurrency_limit() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let transfer_store =
            RecordingTransferStore::missing_with_lookup_delay(Duration::from_millis(40));
        let router = test_router_with_config_authorizer_and_transfer_store(
            test_config_with_work_limits(10, 2),
            store,
            RecordingBatchAuthorizer::allow(),
            transfer_store.clone(),
        );
        let objects = (1_u8..=6)
            .map(|value| {
                serde_json::json!({
                    "oid": format!("{value:064x}"),
                    "size": 42
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::json!({
            "operation": "download",
            "objects": objects
        })
        .to_string();

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                body,
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(transfer_store.lookups().len(), 6);
        assert_eq!(transfer_store.peak_lookups(), 2);
    }

    #[tokio::test]
    async fn authenticated_download_batch_route_returns_download_actions_for_existing_objects() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::allow(),
            RecordingTransferStore::existing(),
        );

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                VALID_BATCH_REQUEST,
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should collect");
        let body: LfsBatchResponse =
            serde_json::from_slice(&body).expect("response should be Git LFS batch JSON");

        assert_eq!(body.objects.len(), 1);
        assert_eq!(body.objects[0].error, None);
        assert!(body.objects[0].actions.contains_key("download"));
        assert_eq!(
            body.objects[0]
                .actions
                .get("download")
                .map(|action| action.href.as_str()),
            Some(
                "http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?size=42"
            )
        );
    }

    #[tokio::test]
    async fn authenticated_download_batch_route_maps_storage_lookup_errors_per_object() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::allow(),
            RecordingTransferStore::lookup_unsupported(),
        );

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                VALID_BATCH_REQUEST,
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should collect");
        let body: LfsBatchResponse =
            serde_json::from_slice(&body).expect("response should be Git LFS batch JSON");

        assert_eq!(body.objects.len(), 1);
        assert!(body.objects[0].actions.is_empty());
        let error = body.objects[0]
            .error
            .as_ref()
            .expect("storage lookup failure should be object-level");
        assert_eq!(error.code, 501);
        assert_eq!(error.message, "object storage lookup is not configured");
    }

    #[tokio::test]
    async fn authenticated_batch_route_rejects_unsupported_transfers() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let authorizer = RecordingBatchAuthorizer::allow();
        let router = test_router_with_authorizer(store, authorizer.clone());

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                UNSUPPORTED_TRANSFER_BATCH_REQUEST,
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/vnd.git-lfs+json")
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should collect");
        let body: serde_json::Value =
            serde_json::from_slice(&body).expect("response should be JSON");

        assert_eq!(
            body.get("message").and_then(|value| value.as_str()),
            Some("unsupported Git LFS transfer requested; only basic is available")
        );
        assert!(authorizer.required_permissions().is_empty());
    }

    #[tokio::test]
    async fn authenticated_upload_batch_route_returns_upload_actions_for_missing_objects() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let router = test_router_with_authorizer(store, RecordingBatchAuthorizer::allow());

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                VALID_UPLOAD_BATCH_REQUEST,
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/vnd.git-lfs+json")
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should collect");
        let body: LfsBatchResponse =
            serde_json::from_slice(&body).expect("response should be Git LFS batch JSON");

        assert_eq!(body.transfer, "basic");
        assert_eq!(body.objects.len(), 1);
        assert_eq!(
            body.objects[0].oid.as_hex(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(body.objects[0].error, None);
        assert!(body.objects[0].actions.contains_key("upload"));
        assert_eq!(
            body.objects[0]
                .actions
                .get("upload")
                .map(|action| action.href.as_str()),
            Some(
                "http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?size=42"
            )
        );
    }

    #[tokio::test]
    async fn authenticated_upload_batch_route_returns_no_action_for_existing_objects() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::allow(),
            RecordingTransferStore::existing(),
        );

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                VALID_UPLOAD_BATCH_REQUEST,
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should collect");
        let body: LfsBatchResponse =
            serde_json::from_slice(&body).expect("response should be Git LFS batch JSON");

        assert_eq!(body.objects.len(), 1);
        assert_eq!(body.objects[0].error, None);
        assert_eq!(body.objects[0].authenticated, None);
        assert!(body.objects[0].actions.is_empty());
    }

    #[tokio::test]
    async fn upload_endpoint_stages_verifies_and_stores_object_bytes() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let transfer_store = RecordingTransferStore::missing();
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::allow(),
            transfer_store.clone(),
        );
        let body = b"hello from lfs cloud";
        let oid = format!("{:x}", Sha256::digest(body));
        let path = format!(
            "/github.com/owner/repo.git/info/lfs/objects/{oid}?size={}",
            body.len()
        );

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::PUT,
                &path,
                Some(&format!("Bearer {token}")),
                body.to_vec(),
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        let uploads = transfer_store.uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].repo_id, "github-main:owner/repo");
        assert_eq!(uploads[0].object.oid.as_hex(), oid);
        assert_eq!(uploads[0].object.size.bytes(), body.len() as u64);
        assert_eq!(uploads[0].bytes, body);
        assert_eq!(uploads[0].created_by.login, "octocat");
    }

    #[tokio::test]
    async fn download_endpoint_streams_existing_object_bytes() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let body = b"download me through lfs cloud".to_vec();
        let oid = format!("{:x}", Sha256::digest(&body));
        let object = LfsObject::new(
            LfsOid::new(&oid).expect("test oid should parse"),
            LfsObjectSize::new(body.len() as u64),
        );
        let transfer_store = RecordingTransferStore::existing_object_with_download_body(
            StoredObject::new("drive-user-a", object.clone(), "drive-file-existing"),
            body.clone(),
        );
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::allow(),
            transfer_store.clone(),
        );
        let path = format!(
            "/github.com/owner/repo.git/info/lfs/objects/{oid}?size={}",
            body.len()
        );

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::GET,
                &path,
                Some(&format!("Bearer {token}")),
                Body::empty(),
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/octet-stream")
        );
        assert_eq!(
            response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok()),
            Some(body.len().to_string().as_str())
        );
        let downloaded = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("download body should collect");
        assert_eq!(&downloaded[..], body.as_slice());

        let downloads = transfer_store.downloads();
        assert_eq!(downloads.len(), 1);
        assert_eq!(downloads[0].repo_id, "github-main:owner/repo");
        assert_eq!(downloads[0].object, object);
    }

    #[tokio::test]
    async fn download_endpoint_reports_storage_integrity_failures_as_backend_errors() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let object = LfsObject::new(
            LfsOid::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .expect("test oid should parse"),
            LfsObjectSize::new(42),
        );
        let transfer_store =
            RecordingTransferStore::existing_object_with_download_integrity_mismatch(
                StoredObject::new("drive-user-a", object, "drive-file-existing"),
            );
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::allow(),
            transfer_store,
        );

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::GET,
                "/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?size=42",
                Some(&format!("Bearer {token}")),
                Body::empty(),
            ))
            .await
            .expect("router should respond");

        assert_lfs_json_error(
            response,
            StatusCode::BAD_GATEWAY,
            "Git LFS storage returned an object that failed integrity validation",
        )
        .await;
    }

    #[tokio::test]
    async fn download_endpoint_requires_size_query_parameter() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let transfer_store = RecordingTransferStore::existing();
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::allow(),
            transfer_store.clone(),
        );

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::GET,
                "/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                Some(&format!("Bearer {token}")),
                Body::empty(),
            ))
            .await
            .expect("router should respond");

        assert_lfs_json_error(
            response,
            StatusCode::BAD_REQUEST,
            "Git LFS download action did not include a valid size query parameter",
        )
        .await;
        assert!(transfer_store.downloads().is_empty());
    }

    #[tokio::test]
    async fn download_endpoint_accepts_objects_larger_than_upload_limit() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let object = LfsObject::new(
            LfsOid::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .expect("test oid should parse"),
            LfsObjectSize::new(MAX_UPLOAD_OBJECT_BYTES + 1),
        );
        let transfer_store = RecordingTransferStore::existing_object_with_download_body(
            StoredObject::new("drive-user-a", object.clone(), "drive-file-existing"),
            b"download body".to_vec(),
        );
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::allow(),
            transfer_store.clone(),
        );
        let path = format!(
            "/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?size={}",
            MAX_UPLOAD_OBJECT_BYTES + 1
        );

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::GET,
                &path,
                Some(&format!("Bearer {token}")),
                Body::empty(),
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            transfer_store.downloads(),
            vec![RecordedDownload {
                repo_id: "github-main:owner/repo".to_owned(),
                object,
            }]
        );
    }

    #[tokio::test]
    async fn download_endpoint_authorizes_read_before_storage_lookup() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let transfer_store = RecordingTransferStore::existing();
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::deny(),
            transfer_store.clone(),
        );

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::GET,
                "/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?size=42",
                Some(&format!("Bearer {token}")),
                Body::empty(),
            ))
            .await
            .expect("router should respond");

        assert_lfs_json_error(
            response,
            StatusCode::FORBIDDEN,
            "repository provider denied this Git LFS operation",
        )
        .await;
        assert!(transfer_store.downloads().is_empty());
    }

    #[tokio::test]
    async fn upload_endpoint_rejects_bytes_that_do_not_match_route_oid() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let transfer_store = RecordingTransferStore::missing();
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::allow(),
            transfer_store.clone(),
        );

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::PUT,
                "/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?size=25",
                Some(&format!("Bearer {token}")),
                "not the requested object",
            ))
            .await
            .expect("router should respond");

        assert_lfs_json_error(
            response,
            StatusCode::UNPROCESSABLE_ENTITY,
            "uploaded Git LFS object did not match the requested SHA-256",
        )
        .await;
        assert!(transfer_store.uploads().is_empty());
    }

    #[tokio::test]
    async fn upload_endpoint_rejects_bytes_that_do_not_match_batch_size() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let transfer_store = RecordingTransferStore::missing();
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::allow(),
            transfer_store.clone(),
        );
        let body = b"hello from lfs cloud";
        let oid = format!("{:x}", Sha256::digest(body));
        let path = format!(
            "/github.com/owner/repo.git/info/lfs/objects/{oid}?size={}",
            body.len() + 1
        );

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::PUT,
                &path,
                Some(&format!("Bearer {token}")),
                body.to_vec(),
            ))
            .await
            .expect("router should respond");

        assert_lfs_json_error(
            response,
            StatusCode::UNPROCESSABLE_ENTITY,
            "uploaded Git LFS object did not match the requested SHA-256",
        )
        .await;
        assert!(transfer_store.uploads().is_empty());
    }

    #[tokio::test]
    async fn upload_endpoint_rejects_declared_oversized_uploads_before_staging() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let transfer_store = RecordingTransferStore::missing();
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::allow(),
            transfer_store.clone(),
        );

        let response = router
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?size=42")
                    .header(AUTHORIZATION, format!("Bearer {token}"))
                    .header(CONTENT_LENGTH, (MAX_UPLOAD_OBJECT_BYTES + 1).to_string())
                    .body(Body::from("small body"))
                    .expect("test request should build"),
            )
            .await
            .expect("router should respond");

        assert_lfs_json_error(
            response,
            StatusCode::PAYLOAD_TOO_LARGE,
            "Git LFS upload object exceeds the configured request size limit",
        )
        .await;
        assert!(transfer_store.uploads().is_empty());
    }

    #[tokio::test]
    async fn upload_endpoint_rejects_oversized_action_size_before_staging() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let transfer_store = RecordingTransferStore::missing();
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::allow(),
            transfer_store.clone(),
        );

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::PUT,
                &format!(
                    "/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?size={}",
                    MAX_UPLOAD_OBJECT_BYTES + 1
                ),
                Some(&format!("Bearer {token}")),
                Body::empty(),
            ))
            .await
            .expect("router should respond");

        assert_lfs_json_error(
            response,
            StatusCode::PAYLOAD_TOO_LARGE,
            "Git LFS upload object exceeds the configured request size limit",
        )
        .await;
        assert!(transfer_store.uploads().is_empty());
    }

    #[tokio::test]
    async fn staged_upload_uses_declared_content_length_in_integrity_errors() {
        let body = "declared size should be preserved";
        let request = Request::builder()
            .method(Method::PUT)
            .uri("/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?size=33")
            .header(CONTENT_LENGTH, "1234")
            .body(Body::from(body))
            .expect("test request should build");

        let error = match stage_upload_request_body(
            &LfsOid::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .expect("test oid should parse"),
            Some(1234),
            request,
        )
        .await
        {
            Ok(_) => panic!("mismatched object should fail staging"),
            Err(error) => error,
        };

        match error {
            super::UploadStagingError::Storage(crate::StorageError::IntegrityMismatch {
                expected_size,
                actual_size,
                ..
            }) => {
                assert_eq!(expected_size, 1234);
                assert_eq!(actual_size, body.len() as u64);
            }
            other => panic!("unexpected staging error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn staged_upload_aborts_when_stream_exceeds_size_limit() {
        let body = "0123456789";
        let request = Request::builder()
            .method(Method::PUT)
            .uri("/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .body(Body::from(body))
            .expect("test request should build");

        let error = match stage_upload_request_body_with_limit(
            &LfsOid::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .expect("test oid should parse"),
            None,
            request,
            4,
        )
        .await
        {
            Ok(_) => panic!("oversized body should fail staging"),
            Err(error) => error,
        };

        assert!(matches!(error, super::UploadStagingError::PayloadTooLarge));
    }

    #[test]
    fn upload_staging_preflight_uses_effective_limit_for_unknown_sizes() {
        assert_eq!(
            upload_staging_preflight_size(None, 42)
                .expect("unknown size should reserve the effective limit"),
            42
        );
        assert_eq!(
            upload_staging_preflight_size(Some(7), 42)
                .expect("declared size below the limit should be accepted"),
            7
        );
        assert!(matches!(
            upload_staging_preflight_size(Some(43), 42),
            Err(super::UploadStagingError::PayloadTooLarge)
        ));
    }

    #[tokio::test]
    async fn staged_upload_rejects_declared_size_above_effective_limit_before_body() {
        let request = Request::builder()
            .method(Method::PUT)
            .uri("/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?size=10")
            .body(Body::from_stream(stream::pending::<
                Result<Bytes, std::io::Error>,
            >()))
            .expect("test request should build");

        let error = match stage_upload_request_body_with_limit(
            &LfsOid::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .expect("test oid should parse"),
            Some(10),
            request,
            4,
        )
        .await
        {
            Ok(_) => panic!("declared size above limit should fail staging"),
            Err(error) => error,
        };

        assert!(matches!(error, super::UploadStagingError::PayloadTooLarge));
    }

    #[test]
    fn temp_space_guardrail_requires_expected_size_plus_headroom() {
        ensure_temp_space_for_upload_with_available_space(10, 5, 15)
            .expect("exact expected size plus headroom should be accepted");

        let error = ensure_temp_space_for_upload_with_available_space(10, 5, 14)
            .expect_err("insufficient temp space should be rejected");
        assert!(matches!(
            error,
            super::UploadStagingError::InsufficientTempSpace {
                required_space: Some(15),
                available_space: Some(14)
            }
        ));

        let overflow = ensure_temp_space_for_upload_with_available_space(u64::MAX, 1, u64::MAX)
            .expect_err("overflowing required space should be rejected");
        assert!(matches!(
            overflow,
            super::UploadStagingError::InsufficientTempSpace {
                required_space: None,
                available_space: Some(u64::MAX)
            }
        ));
    }

    #[test]
    fn temp_space_write_errors_map_to_insufficient_temp_space() {
        let error =
            upload_staging_file_io_error(io::Error::from(ErrorKind::StorageFull), "written");

        assert!(matches!(
            error,
            super::UploadStagingError::InsufficientTempSpace {
                required_space: None,
                available_space: None
            }
        ));
    }

    #[tokio::test]
    async fn staged_upload_aborts_when_body_stream_stalls() {
        let request = Request::builder()
            .method(Method::PUT)
            .uri("/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .body(Body::from_stream(stream::pending::<
                Result<Bytes, std::io::Error>,
            >()))
            .expect("test request should build");

        let error = match stage_upload_request_body_with_guardrails(
            &LfsOid::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .expect("test oid should parse"),
            None,
            request,
            UploadStagingGuardrails {
                max_upload_bytes: MAX_UPLOAD_OBJECT_BYTES,
                min_free_bytes: 0,
                idle_timeout: Duration::from_millis(1),
            },
        )
        .await
        {
            Ok(_) => panic!("stalled upload body should fail staging"),
            Err(error) => error,
        };

        assert!(matches!(error, super::UploadStagingError::TimedOut));
    }

    #[tokio::test]
    async fn upload_staging_guardrail_responses_use_lfs_json_errors() {
        assert_lfs_json_error(
            super::upload_temp_space_exhausted_response(),
            StatusCode::INSUFFICIENT_STORAGE,
            "Git LFS upload staging directory does not have enough free space",
        )
        .await;

        assert_lfs_json_error(
            super::upload_staging_timeout_response(),
            StatusCode::REQUEST_TIMEOUT,
            "Git LFS upload request timed out while reading the object body",
        )
        .await;
    }

    #[tokio::test]
    async fn upload_endpoint_authorizes_write_before_staging_body() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let transfer_store = RecordingTransferStore::missing();
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::deny(),
            transfer_store.clone(),
        );
        let body = b"blocked upload body";
        let oid = format!("{:x}", Sha256::digest(body));
        let path = format!(
            "/github.com/owner/repo.git/info/lfs/objects/{oid}?size={}",
            body.len()
        );

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::PUT,
                &path,
                Some(&format!("Bearer {token}")),
                body.to_vec(),
            ))
            .await
            .expect("router should respond");

        assert_lfs_json_error(
            response,
            StatusCode::FORBIDDEN,
            "repository provider denied this Git LFS operation",
        )
        .await;
        assert!(transfer_store.uploads().is_empty());
    }

    #[tokio::test]
    async fn upload_endpoint_serializes_retrying_uploads_for_the_same_object() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let upload_started = Arc::new(Notify::new());
        let upload_release = Arc::new(Barrier::new(2));
        let transfer_store = RecordingTransferStore::blocking_missing(
            upload_started.clone(),
            upload_release.clone(),
        );
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::allow(),
            transfer_store.clone(),
        );
        let body = b"hello from lfs cloud";
        let oid = format!("{:x}", Sha256::digest(body));
        let path = format!(
            "/github.com/owner/repo.git/info/lfs/objects/{oid}?size={}",
            body.len()
        );
        let upload_started_wait = upload_started.notified();

        let first_router = router.clone();
        let first_token = token.clone();
        let first_path = path.clone();
        let first = tokio::spawn(async move {
            first_router
                .oneshot(lfs_request_with_method_and_body(
                    Method::PUT,
                    &first_path,
                    Some(&format!("Bearer {first_token}")),
                    body.to_vec(),
                ))
                .await
                .expect("first router response should exist")
        });

        upload_started_wait.await;

        let second_router = router.clone();
        let second_token = token.clone();
        let second_path = path.clone();
        let second = tokio::spawn(async move {
            second_router
                .oneshot(lfs_request_with_method_and_body(
                    Method::PUT,
                    &second_path,
                    Some(&format!("Bearer {second_token}")),
                    body.to_vec(),
                ))
                .await
                .expect("second router response should exist")
        });

        upload_release.wait().await;

        let first_response = first.await.expect("first upload task should complete");
        let second_response = second.await.expect("second upload task should complete");

        assert_eq!(first_response.status(), StatusCode::OK);
        assert_eq!(second_response.status(), StatusCode::OK);
        assert_eq!(transfer_store.uploads().len(), 1);
    }

    #[tokio::test]
    async fn upload_endpoint_repairs_metadata_for_existing_backend_objects() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let body = b"hello from lfs cloud";
        let oid = format!("{:x}", Sha256::digest(body));
        let object = LfsObject::new(
            LfsOid::new(&oid).expect("test oid should parse"),
            LfsObjectSize::new(body.len() as u64),
        );
        let transfer_store = RecordingTransferStore::existing_object(StoredObject::new(
            "drive-user-a",
            object.clone(),
            "drive-file-existing",
        ));
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            RecordingBatchAuthorizer::allow(),
            transfer_store.clone(),
        );
        let path = format!(
            "/github.com/owner/repo.git/info/lfs/objects/{oid}?size={}",
            body.len()
        );

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::PUT,
                &path,
                Some(&format!("Bearer {token}")),
                body.to_vec(),
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        assert!(transfer_store.uploads().is_empty());
        let verified_records = transfer_store.verified_records();
        assert_eq!(verified_records.len(), 1);
        assert_eq!(verified_records[0].backend_id, "drive-file-existing");
    }

    #[tokio::test]
    async fn batch_route_authorizes_download_as_read_and_upload_as_write() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let authorizer = RecordingBatchAuthorizer::allow();
        let router = test_router_with_authorizer(store, authorizer.clone());

        for body in [VALID_BATCH_REQUEST, VALID_UPLOAD_BATCH_REQUEST] {
            let response = router
                .clone()
                .oneshot(lfs_request_with_method_and_body(
                    Method::POST,
                    "/github.com/owner/repo.git/info/lfs/objects/batch",
                    Some(&format!("Bearer {token}")),
                    body,
                ))
                .await
                .expect("router should respond");

            assert_eq!(response.status(), StatusCode::OK);
        }

        assert_eq!(
            authorizer.required_permissions(),
            vec![RepositoryPermission::Read, RepositoryPermission::Write]
        );
    }

    #[tokio::test]
    async fn batch_authorization_is_reused_by_its_download_action() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let authorizer = RecordingBatchAuthorizer::allow();
        let router = test_router_with_authorizer_and_transfer_store(
            store,
            authorizer.clone(),
            RecordingTransferStore::existing(),
        );

        let batch_response = router
            .clone()
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                VALID_BATCH_REQUEST,
            ))
            .await
            .expect("batch request should receive a response");
        assert_eq!(batch_response.status(), StatusCode::OK);

        let download_response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::GET,
                "/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?size=42",
                Some(&format!("Bearer {token}")),
                Body::empty(),
            ))
            .await
            .expect("download action should receive a response");

        assert_eq!(download_response.status(), StatusCode::OK);
        assert_eq!(
            authorizer.required_permissions(),
            vec![RepositoryPermission::Read]
        );
    }

    #[tokio::test]
    async fn malformed_transfer_action_is_rejected_before_authorization() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let authorizer = RecordingBatchAuthorizer::allow();
        let router = test_router_with_authorizer(store, authorizer.clone());

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::GET,
                "/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                Some(&format!("Bearer {token}")),
                Body::empty(),
            ))
            .await
            .expect("router should respond");

        assert_lfs_json_error(
            response,
            StatusCode::BAD_REQUEST,
            "Git LFS download action did not include a valid size query parameter",
        )
        .await;
        assert!(authorizer.required_permissions().is_empty());
    }

    #[tokio::test]
    async fn batch_route_rejects_repository_permission_denials() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let authorizer = RecordingBatchAuthorizer::deny();
        let router = test_router_with_authorizer(store, authorizer.clone());

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                VALID_UPLOAD_BATCH_REQUEST,
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/vnd.git-lfs+json")
        );
        assert_eq!(
            authorizer.required_permissions(),
            vec![RepositoryPermission::Write]
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should collect");
        let body: serde_json::Value =
            serde_json::from_slice(&body).expect("response should be JSON");

        assert_eq!(
            body.get("message").and_then(|value| value.as_str()),
            Some("repository provider denied this Git LFS operation")
        );
    }

    #[tokio::test]
    async fn batch_route_returns_auth_challenge_when_github_token_is_missing() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let router = lfs_server_router_with_sessions(test_config(), store);

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                VALID_BATCH_REQUEST,
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let challenge_values = response.headers().get_all(WWW_AUTHENTICATE);
        assert!(
            challenge_values
                .iter()
                .any(|value| value.to_str().ok() == Some(LFS_AUTH_CHALLENGE))
        );
        assert!(
            challenge_values
                .iter()
                .any(|value| value.to_str().ok() == Some("Bearer realm=\"lfs-cloud\""))
        );
    }

    #[tokio::test]
    async fn default_batch_authorizer_checks_github_permissions() {
        let github_api_url = start_permission_server("read").await;
        let config = test_config_with_github_api_url(&github_api_url);
        let store = LocalLfsSessionStore::new();
        let user = RepositoryUser::new("github-main", "octocat", Some("42".to_owned()));
        let github_token =
            GitHubOAuthAccessToken::from_secret("gho_authorization").expect("token should parse");
        let issued = store
            .issue_session_with_github_token(&user, ["read:user", "repo"], github_token)
            .expect("session should be issued");
        let router = lfs_server_router_with_sessions(config, store);

        let download = router
            .clone()
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {}", issued.token.as_str())),
                VALID_BATCH_REQUEST,
            ))
            .await
            .expect("router should respond");
        let upload = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {}", issued.token.as_str())),
                VALID_UPLOAD_BATCH_REQUEST,
            ))
            .await
            .expect("router should respond");

        assert_eq!(download.status(), StatusCode::OK);
        assert_eq!(upload.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn default_batch_authorizer_denies_session_user_identity_mismatch() {
        let github_api_url = start_permission_server_for_user("admin", 99).await;
        let config = test_config_with_github_api_url(&github_api_url);
        let store = LocalLfsSessionStore::new();
        let user = RepositoryUser::new("github-main", "octocat", Some("42".to_owned()));
        let github_token =
            GitHubOAuthAccessToken::from_secret("gho_authorization").expect("token should parse");
        let issued = store
            .issue_session_with_github_token(&user, ["read:user", "repo"], github_token)
            .expect("session should be issued");
        let router = lfs_server_router_with_sessions(config, store);

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {}", issued.token.as_str())),
                VALID_BATCH_REQUEST,
            ))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn batch_route_rejects_invalid_json_after_authentication() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let router = lfs_server_router_with_sessions(test_config(), store);

        let unauthenticated = router
            .clone()
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                None,
                "{not-json",
            ))
            .await
            .expect("router should respond");
        let invalid = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                "{not-json",
            ))
            .await
            .expect("router should respond");

        assert_lfs_json_error(
            unauthenticated,
            StatusCode::UNAUTHORIZED,
            "LFS Cloud authentication required",
        )
        .await;
        assert_lfs_json_error(
            invalid,
            StatusCode::BAD_REQUEST,
            "Invalid Git LFS batch request",
        )
        .await;
    }

    #[tokio::test]
    async fn batch_route_preserves_payload_too_large_after_authentication() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let router = lfs_server_router_with_sessions(test_config(), store);
        let large_body = "x".repeat(2 * 1024 * 1024 + 1);

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                large_body,
            ))
            .await
            .expect("router should respond");

        assert_lfs_json_error(
            response,
            StatusCode::PAYLOAD_TOO_LARGE,
            "Git LFS request body exceeds the configured limit",
        )
        .await;
    }

    #[tokio::test]
    async fn batch_route_requires_auth_before_buffering_body() {
        let (store, _token) = issued_session_token(Duration::from_secs(60));
        let router = lfs_server_router_with_sessions(test_config(), store);
        let large_body = "x".repeat(2 * 1024 * 1024 + 1);

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::POST,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                None,
                large_body,
            ))
            .await
            .expect("router should respond");

        assert_lfs_json_error(
            response,
            StatusCode::UNAUTHORIZED,
            "LFS Cloud authentication required",
        )
        .await;
    }

    #[tokio::test]
    async fn batch_route_requires_post_requests() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let router = lfs_server_router_with_sessions(test_config(), store);

        let response = router
            .oneshot(lfs_request_with_method_and_body(
                Method::GET,
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
                VALID_BATCH_REQUEST,
            ))
            .await
            .expect("router should respond");

        assert_eq!(
            response
                .headers()
                .get(ALLOW)
                .and_then(|value| value.to_str().ok()),
            Some("POST")
        );
        assert_lfs_json_error(
            response,
            StatusCode::METHOD_NOT_ALLOWED,
            "Git LFS batch endpoint requires POST",
        )
        .await;
    }

    #[tokio::test]
    async fn server_router_mounts_github_oauth_callback_before_lfs_fallback() {
        let router = server_router_with_sessions(test_config(), LocalLfsSessionStore::new())
            .expect("server router should build");

        let response = router
            .oneshot(lfs_request("/auth/github/callback", None))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn server_router_rejects_multiple_github_oauth_providers() {
        let config = ServerConfig::load_from_str(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: test-client-a
    oauth_client_secret: test-secret-a
  github-secondary:
    type: github
    api_url: https://api.github.com
    oauth_client_id: test-client-b
    oauth_client_secret: test-secret-b
storage_providers:
  drive-user-a:
    type: google_drive
    credentials_ref: google-drive-user-a
    root_folder_id: root
repositories:
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    provider_repository_id: "8675309"
    storage_provider: drive-user-a
"#,
        )
        .expect("test config should load");

        let error = server_router_with_sessions(config, LocalLfsSessionStore::new())
            .expect_err("router should reject ambiguous GitHub providers");
        assert!(matches!(error, ServerError::InvalidConfiguration { .. }));
    }

    async fn assert_lfs_json_error(
        response: axum::response::Response,
        status: StatusCode,
        message: &str,
    ) {
        assert_eq!(response.status(), status);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/vnd.git-lfs+json")
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should collect");
        let body: serde_json::Value =
            serde_json::from_slice(&body).expect("response should be JSON");

        assert_eq!(
            body.get("message").and_then(|value| value.as_str()),
            Some(message)
        );
    }

    fn issued_session_token(ttl: Duration) -> (LocalLfsSessionStore, String) {
        let store = LocalLfsSessionStore::new();
        let user = RepositoryUser::new("github-main", "octocat", Some("42".to_owned()));
        let issued = store
            .issue_session_with_ttl(&user, ["read:user"], ttl)
            .expect("session token should be issued");

        (store, issued.token.as_str().to_owned())
    }

    async fn start_permission_server(permission: &'static str) -> String {
        start_permission_server_for_user(permission, 42).await
    }

    async fn start_permission_server_for_user(permission: &'static str, user_id: u64) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("permission server should bind");
        let address = listener
            .local_addr()
            .expect("permission server address should be available");
        let router = Router::new()
            .route(
                "/repos/{owner}/{repo}",
                get(|| async { Json(serde_json::json!({ "id": 8675309_u64 })) }),
            )
            .route(
                "/repos/{owner}/{repo}/collaborators/{username}/permission",
                get(
                    move |Path((_owner, _repo, _username)): Path<(
                        String,
                        String,
                        String,
                    )>| async move {
                        Json(serde_json::json!({
                            "permission": permission,
                            "user": { "login": "octocat", "id": user_id }
                        }))
                    },
                ),
            );

        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("permission server should run");
        });

        format!("http://{address}")
    }

    fn authorization_headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(value).expect("test authorization header should parse"),
        );
        headers
    }

    fn basic_authorization(username: &str, password: &str) -> HeaderValue {
        let encoded = BASE64_STANDARD.encode(format!("{username}:{password}"));
        HeaderValue::from_str(&format!("Basic {encoded}"))
            .expect("test basic authorization header should parse")
    }

    fn lfs_request(path: &str, authorization: Option<&str>) -> Request<Body> {
        lfs_request_with_method_and_body(Method::GET, path, authorization, "")
    }

    fn lfs_request_with_method_and_body(
        method: Method,
        path: &str,
        authorization: Option<&str>,
        body: impl Into<Body>,
    ) -> Request<Body> {
        let mut builder = Request::builder().uri(path);
        if let Some(authorization) = authorization {
            builder = builder.header(AUTHORIZATION, authorization);
        }

        builder = builder
            .method(method)
            .header(CONTENT_TYPE, "application/vnd.git-lfs+json");

        builder
            .body(body.into())
            .expect("test request should build")
    }
}
