//! HTTP server entrypoint and Git LFS route resolution.
//!
//! This module owns the first server-facing boundary: loading a validated
//! configuration, validating configured storage readiness, binding an Axum
//! listener, reporting reachable URLs, resolving incoming Git LFS request paths
//! to configured repository mappings, and proxying authenticated transfers.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::{Future, IntoFuture},
    io::{self, ErrorKind},
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{OriginalUri, Request, State},
    http::{
        HeaderMap, HeaderValue, Method, StatusCode,
        header::{
            ALLOW, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, RETRY_AFTER, WWW_AUTHENTICATE,
        },
    },
    middleware::{self, Next},
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
    DEFAULT_GIT_CREDENTIAL_USERNAME, ErrorCategory, GitHubPersonalAccessTokenLoginRouteState,
    LFS_BASIC_TRANSFER, LfsBatchDownloadObject, LfsBatchObjectError, LfsBatchOperation,
    LfsBatchRequest, LfsBatchResponse, LfsBatchUploadObject, LfsObject, LfsObjectSize, LfsOid,
    LfsSessionToken, LocalLfsSessionStore, MetadataDatabase, MetadataObjectVerificationStatus,
    ProviderFuture, RepositoryAuthentication, RepositoryIdentity, RepositoryMapping,
    RepositoryPermission, RepositoryProvider, RepositoryProviderError, RepositoryUser,
    SanitizedMessage, ServerConfig, ServerError, ServerResult, StorageDownloadResponse,
    StorageError, StorageProvider, StoredObject, github_personal_access_token_login_router,
    metadata::{MetadataTransferOperation, MetadataTransferResult},
    parse_lfs_batch_request_json,
    provider_factory::{
        ConfiguredStorageProviders, ServerStorageProvider, ServerStorageProviderFactory,
    },
    sessions::LfsSessionRecord,
};

const LFS_AUTH_CHALLENGE: &str = "Basic realm=\"lfscloud\"";
/// Authenticated endpoint for revoking the presented local LFS session.
pub const LFS_SESSION_REVOKE_PATH: &str = "/auth/session";
const GIT_LFS_JSON_CONTENT_TYPE: &str = "application/vnd.git-lfs+json";
const MAX_UPLOAD_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MIN_UPLOAD_STAGING_FREE_BYTES: u64 = 64 * 1024 * 1024;
const UPLOAD_STAGING_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BATCH_BODY_BYTES: usize = 2 * 1024 * 1024;
const BATCH_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const BATCH_BODY_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
const BATCH_STORAGE_LOOKUP_CONCURRENCY: usize = 16;
const AUTHORIZATION_CACHE_TTL: Duration = Duration::from_secs(15);
const SERVER_SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServerShutdownOutcome {
    Drained,
    TimedOut,
}

/// Runtime options supplied by `lfscloud serve`.
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
/// Before binding the listener, the server asks `gcloud` for an ADC access
/// token for every configured Google Drive provider and validates that each
/// root is a live, writable folder. It then serves authenticated Git LFS batch
/// and object transfer routes. SIGINT and SIGTERM stop new request admission
/// and allow active transfers up to 30 seconds to finish before process shutdown.
///
/// # Errors
///
/// Returns [`ServerError`] when configuration loading, metadata initialization,
/// storage readiness validation, listener binding, or Axum serving fails.
pub async fn serve(options: ServeOptions) -> ServerResult<()> {
    ServerBuilder::new(options).serve().await
}

type ServerShutdownSignal = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

struct ServerCompositionClients {
    storage_provider_factory: ServerStorageProviderFactory,
    github_user_client: crate::GitHubUserClient,
}

impl ServerCompositionClients {
    fn production() -> ServerResult<Self> {
        Ok(Self {
            storage_provider_factory: ServerStorageProviderFactory::production()?,
            github_user_client: crate::GitHubUserClient::new()?,
        })
    }
}

struct ServerBuilder {
    options: ServeOptions,
    clients: Option<ServerCompositionClients>,
    shutdown_signal: Option<ServerShutdownSignal>,
    #[cfg(test)]
    drive_object_api_base_url: Option<String>,
}

impl ServerBuilder {
    fn new(options: ServeOptions) -> Self {
        Self {
            options,
            clients: None,
            shutdown_signal: None,
            #[cfg(test)]
            drive_object_api_base_url: None,
        }
    }

    #[cfg(test)]
    fn with_clients(mut self, clients: ServerCompositionClients) -> Self {
        self.clients = Some(clients);
        self
    }

    #[cfg(test)]
    fn with_shutdown_signal(
        mut self,
        shutdown_signal: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        self.shutdown_signal = Some(Box::pin(shutdown_signal));
        self
    }

    #[cfg(test)]
    fn with_drive_object_api_base_url(mut self, api_base_url: impl Into<String>) -> Self {
        self.drive_object_api_base_url = Some(api_base_url.into());
        self
    }

    async fn serve(self) -> ServerResult<()> {
        let config_path = self
            .options
            .config_path
            .unwrap_or_else(|| ServerConfig::default_path().to_path_buf());
        let mut config = ServerConfig::load_from_path(config_path)?;
        let bind = ServerBind::from_config_and_overrides(
            &config.server.host,
            config.server.port,
            self.options.host,
            self.options.port,
        )?;
        bind.validate_transport(&config)?;

        let metadata_database =
            Arc::new(MetadataDatabase::open(config.server.metadata_path.clone())?);
        metadata_database.sync_config(&config)?;
        config.server.host = bind.host.clone();
        config.server.port = bind.port;

        let clients = match self.clients {
            Some(clients) => clients,
            None => ServerCompositionClients::production()?,
        };
        let session_store = production_session_store(&config, metadata_database.clone())?;
        let storage_provider_factory = clients.storage_provider_factory;
        #[cfg(test)]
        let storage_provider_factory = match self.drive_object_api_base_url {
            Some(api_base_url) => {
                storage_provider_factory.with_drive_object_api_base_url(api_base_url)
            }
            None => storage_provider_factory,
        };
        let storage_providers = storage_provider_factory
            .build(&config, metadata_database.clone())
            .await?;
        let transfer_store = Arc::new(StorageProviderTransferStore::new(
            storage_providers,
            metadata_database.clone(),
        ));
        let router = LfsRouterBuilder::new(config, session_store)
            .with_transfer_store(transfer_store)
            .with_metadata_database(metadata_database)
            .build_server(clients.github_user_client)?;
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

        let shutdown_signal = self
            .shutdown_signal
            .unwrap_or_else(|| Box::pin(shutdown_signal()));
        serve_with_graceful_shutdown(
            listener,
            router,
            shutdown_signal,
            SERVER_SHUTDOWN_DRAIN_TIMEOUT,
        )
        .await
        .map(|_| ())
        .map_err(|source| ServerError::Serve { source })
    }
}

async fn serve_with_graceful_shutdown<F>(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown_signal: F,
    drain_timeout: Duration,
) -> io::Result<ServerShutdownOutcome>
where
    F: Future<Output = ()> + Send + 'static,
{
    let (shutdown_started_sender, shutdown_started_receiver) = tokio::sync::oneshot::channel();
    let tracked_shutdown_signal = async move {
        shutdown_signal.await;
        let _ = shutdown_started_sender.send(());
    };
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(tracked_shutdown_signal)
        .into_future();
    tokio::pin!(server);

    tokio::select! {
        result = &mut server => result.map(|()| ServerShutdownOutcome::Drained),
        shutdown_started = shutdown_started_receiver => {
            if shutdown_started.is_err() {
                return server.await.map(|()| ServerShutdownOutcome::Drained);
            }

            tracing::info!(
                drain_timeout_seconds = drain_timeout.as_secs(),
                "shutdown signal received; stopped accepting requests and draining active transfers"
            );
            match tokio::time::timeout(drain_timeout, &mut server).await {
                Ok(result) => result.map(|()| ServerShutdownOutcome::Drained),
                Err(_) => {
                    tracing::warn!(
                        drain_timeout_seconds = drain_timeout.as_secs(),
                        "shutdown drain deadline expired; terminating remaining transfers"
                    );
                    Ok(ServerShutdownOutcome::TimedOut)
                }
            }
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(source) => {
                    tracing::error!(%source, "failed to install SIGTERM handler");
                    return;
                }
            };

        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(source) = result {
                    tracing::error!(%source, "failed to install SIGINT handler");
                }
            }
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    if let Err(source) = tokio::signal::ctrl_c().await {
        tracing::error!(%source, "failed to install Ctrl+C handler");
    }
}

fn production_session_store(
    config: &ServerConfig,
    metadata_database: Arc<MetadataDatabase>,
) -> ServerResult<LocalLfsSessionStore> {
    match config.single_github_pat_provider("durable session storage")? {
        None => Ok(LocalLfsSessionStore::new()),
        Some(provider) => LocalLfsSessionStore::open_durable(
            metadata_database,
            provider.authentication.session_encryption_secret(),
        ),
    }
}

/// Builds the Axum router for configured Git LFS paths.
pub fn lfs_server_router(config: ServerConfig) -> Router {
    lfs_server_router_with_sessions(config, LocalLfsSessionStore::new())
}

/// Builds the full server router with authentication and Git LFS routes.
///
/// GitHub PAT login and Git LFS endpoints share `session_store` so a successful
/// login can issue a local LFS Cloud token that the LFS routes accept immediately.
///
/// # Errors
///
/// Returns [`ServerError`] if PAT login state cannot be initialized from the
/// validated server configuration.
pub fn server_router_with_sessions(
    config: ServerConfig,
    session_store: LocalLfsSessionStore,
) -> ServerResult<Router> {
    LfsRouterBuilder::new(config, session_store).build_server(crate::GitHubUserClient::new()?)
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
            tracing::error!(
                error_category = %server_error_log_category(&error),
                "failed to authenticate LFS session revocation"
            );
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
            tracing::error!(
                error_category = %server_error_log_category(&error),
                "failed to revoke LFS session"
            );
            git_lfs_json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "LFS Cloud session revocation failed",
            )
        }
    }
}

/// Builds the Axum router with an explicit local LFS session store.
///
/// This constructor lets login wiring and tests share the same
/// [`LocalLfsSessionStore`] used by request authentication. Git LFS endpoint
/// requests must present a valid local LFS session token before protocol
/// handlers receive the resolved route.
pub fn lfs_server_router_with_sessions(
    config: ServerConfig,
    session_store: LocalLfsSessionStore,
) -> Router {
    LfsRouterBuilder::new(config, session_store).build_lfs()
}

/// Builds the Git LFS router with explicit provider-trait adapters.
///
/// This is a narrow test seam for exercising the production metadata-recording
/// provider adapter without real GitHub or Google Drive network calls. It does
/// not mount login routes and uses an in-memory metadata database. The
/// configured repository and storage provider IDs must match the injected
/// providers.
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
    let metadata_database = Arc::new(MetadataDatabase::open_in_memory()?);
    metadata_database.sync_config(&config)?;
    let storage_providers = ConfiguredStorageProviders::from_provider(&config, storage_provider)?;
    let transfer_store = Arc::new(StorageProviderTransferStore::new(
        storage_providers,
        metadata_database.clone(),
    ));
    Ok(LfsRouterBuilder::new(config, session_store)
        .with_authorizer(Arc::new(ProviderBatchAuthorizer::new(repository_provider)))
        .with_transfer_store(transfer_store)
        .with_metadata_database(metadata_database)
        .build_lfs())
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

/// Composes standalone and complete LFS server routers with shared defaults.
///
/// The standalone and complete entry points each apply the process-wide HTTP
/// request limit exactly once. Callers that need the unlayered LFS routes for
/// outer composition must opt into [`Self::build_unlimited_lfs_routes`].
struct LfsRouterBuilder {
    config: ServerConfig,
    session_store: LocalLfsSessionStore,
    authorizer: Option<Arc<dyn LfsBatchAuthorizer>>,
    transfer_store: Option<Arc<dyn LfsObjectTransferStore>>,
    batch_body_guardrails: BatchBodyGuardrails,
    metadata_database: Option<Arc<MetadataDatabase>>,
}

impl LfsRouterBuilder {
    /// Starts a router composition using lazy production provider defaults.
    fn new(config: ServerConfig, session_store: LocalLfsSessionStore) -> Self {
        Self {
            config,
            session_store,
            authorizer: None,
            transfer_store: None,
            batch_body_guardrails: BatchBodyGuardrails::default(),
            metadata_database: None,
        }
    }

    /// Overrides the config-derived repository authorizer.
    fn with_authorizer(mut self, authorizer: Arc<dyn LfsBatchAuthorizer>) -> Self {
        self.authorizer = Some(authorizer);
        self
    }

    /// Overrides the pending production transfer store.
    fn with_transfer_store(mut self, transfer_store: Arc<dyn LfsObjectTransferStore>) -> Self {
        self.transfer_store = Some(transfer_store);
        self
    }

    /// Overrides production batch-body defaults for focused guardrail tests.
    #[cfg(test)]
    fn with_batch_body_guardrails(mut self, batch_body_guardrails: BatchBodyGuardrails) -> Self {
        self.batch_body_guardrails = batch_body_guardrails;
        self
    }

    /// Attaches durable metadata recording to object transfers.
    fn with_metadata_database(mut self, metadata_database: Arc<MetadataDatabase>) -> Self {
        self.metadata_database = Some(metadata_database);
        self
    }

    /// Builds a standalone LFS router with one process-wide request-limit layer.
    fn build_lfs(self) -> Router {
        let max_concurrent_requests = self.config.server.max_concurrent_requests;
        with_http_request_limit(self.build_unlimited_lfs_routes(), max_concurrent_requests)
    }

    /// Builds the complete auth/session/LFS router with one request-limit layer.
    fn build_server(self, github_user_client: crate::GitHubUserClient) -> ServerResult<Router> {
        let max_concurrent_requests = self.config.server.max_concurrent_requests;
        let config = self.config.clone();
        let session_store = self.session_store.clone();
        let lfs_router = self.build_unlimited_lfs_routes();
        let session_router = lfs_session_revoke_router(session_store.clone());
        let Some(auth_router) =
            github_auth_router_with_client(config, session_store, github_user_client)?
        else {
            return Ok(with_http_request_limit(
                session_router.merge(lfs_router),
                max_concurrent_requests,
            ));
        };

        Ok(with_http_request_limit(
            auth_router.merge(session_router).merge(lfs_router),
            max_concurrent_requests,
        ))
    }

    /// Builds unlayered LFS routes for intentional outer router composition.
    ///
    /// This method must remain free of the process-wide request-limit layer so
    /// [`Self::build_server`] can apply that layer once around every route.
    fn build_unlimited_lfs_routes(self) -> Router {
        let authorizer = self
            .authorizer
            .unwrap_or_else(|| Arc::new(ProviderBatchAuthorizer::from_config(&self.config)));
        let transfer_store = self
            .transfer_store
            .unwrap_or_else(|| Arc::new(PendingLfsObjectTransferStore));
        let state = Arc::new(LfsServerState::new(
            self.config,
            self.session_store,
            authorizer,
            transfer_store,
            self.batch_body_guardrails,
            self.metadata_database,
        ));

        Router::new().fallback(handle_lfs_request).with_state(state)
    }
}

#[derive(Clone)]
struct HttpRequestLimiter {
    permits: Arc<Semaphore>,
}

fn with_http_request_limit(router: Router, max_concurrent_requests: usize) -> Router {
    let limiter = HttpRequestLimiter {
        permits: Arc::new(Semaphore::new(max_concurrent_requests)),
    };
    router.layer(middleware::from_fn_with_state(
        limiter,
        enforce_http_request_limit,
    ))
}

async fn enforce_http_request_limit(
    State(limiter): State<HttpRequestLimiter>,
    request: Request,
    next: Next,
) -> Response {
    let Ok(_permit) = limiter.permits.clone().try_acquire_owned() else {
        let mut response = git_lfs_json_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "LFS Cloud server has reached its concurrent request limit",
        );
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from_static("1"));
        return response;
    };

    next.run(request).await
}

fn github_auth_router_with_client(
    config: ServerConfig,
    session_store: LocalLfsSessionStore,
    user_client: crate::GitHubUserClient,
) -> ServerResult<Option<Router>> {
    let provider = match config.single_github_pat_provider("the PAT login router")? {
        None => return Ok(None),
        Some(provider) => provider,
    };
    let route_state = GitHubPersonalAccessTokenLoginRouteState::with_client_and_session_store(
        provider.clone(),
        user_client,
        session_store,
    )?;
    Ok(Some(github_personal_access_token_login_router(route_state)))
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
    ) -> ProviderFuture<'a, ServerResult<StorageDownloadResponse>>;

    fn record_verified_object<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        object: &'a LfsObject,
        backend_id: &'a str,
        created_by: &'a RepositoryUser,
    ) -> ProviderFuture<'a, ServerResult<()>>;
}

#[derive(Clone)]
struct ProviderBatchAuthorizer {
    providers: BTreeMap<String, Arc<dyn RepositoryProvider + Send + Sync>>,
}

impl ProviderBatchAuthorizer {
    fn new(provider: Arc<dyn RepositoryProvider + Send + Sync>) -> Self {
        Self {
            providers: BTreeMap::from([(provider.provider_id().to_owned(), provider)]),
        }
    }

    fn from_config(config: &ServerConfig) -> Self {
        let providers = config
            .repository_providers
            .iter()
            .map(|(id, provider)| (id.clone(), provider.build_provider()))
            .collect();

        Self { providers }
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
            let provider = self
                .providers
                .get(&repository.repo_provider)
                .ok_or_else(|| ServerError::InvalidConfiguration {
                    message: format!(
                        "repository {} references unknown provider {}",
                        repository.id, repository.repo_provider
                    ),
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

            let token = session.github_personal_access_token().ok_or_else(|| {
                ServerError::RepositoryProvider {
                    source: RepositoryProviderError::AuthenticationRequired {
                        provider: repository.repo_provider.clone(),
                    },
                }
            })?;
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
            let authentication = RepositoryAuthentication::new(user, token.as_str());

            provider
                .check_permission(&identity, &authentication, required)
                .await?;
            Ok(())
        })
    }
}

#[derive(Clone)]
struct StorageProviderTransferStore {
    providers: ConfiguredStorageProviders,
    metadata_database: Arc<MetadataDatabase>,
}

impl StorageProviderTransferStore {
    fn new(
        providers: ConfiguredStorageProviders,
        metadata_database: Arc<MetadataDatabase>,
    ) -> Self {
        Self {
            providers,
            metadata_database,
        }
    }

    fn validate_stored_object_namespace(
        repository: &RepositoryMapping,
        stored_object: &StoredObject,
    ) -> ServerResult<()> {
        if stored_object.provider_id == repository.storage_provider
            && stored_object.repository_namespace == repository.id
        {
            Ok(())
        } else {
            Err(ServerError::Storage {
                source: StorageError::RepositoryNamespaceMismatch {
                    provider: repository.storage_provider.clone(),
                },
            })
        }
    }

    fn ensure_stored_object_namespace(
        repository: &RepositoryMapping,
        stored_object: StoredObject,
    ) -> ServerResult<StoredObject> {
        Self::validate_stored_object_namespace(repository, &stored_object)?;
        Ok(stored_object)
    }

    async fn record_verified_object_metadata(
        &self,
        repository: &RepositoryMapping,
        object: &LfsObject,
        backend_id: String,
        created_by: RepositoryUser,
    ) -> ServerResult<()> {
        self.metadata_database
            .record_verified_object_async(
                repository.id.clone(),
                repository.storage_provider.clone(),
                object.clone(),
                backend_id,
                created_by,
            )
            .await?;
        Ok(())
    }

    async fn lookup_and_repair_object(
        &self,
        repository: &RepositoryMapping,
        object: &LfsObject,
    ) -> ServerResult<Option<StoredObject>> {
        let runtime = self.providers.provider_for(repository)?;
        self.lookup_and_repair_object_with_runtime(repository, object, runtime)
            .await
    }

    async fn lookup_and_repair_object_with_runtime(
        &self,
        repository: &RepositoryMapping,
        object: &LfsObject,
        runtime: &ServerStorageProvider,
    ) -> ServerResult<Option<StoredObject>> {
        let provider = runtime.provider();
        let metadata = self
            .metadata_database
            .lookup_object_async(
                repository.id.clone(),
                repository.storage_provider.clone(),
                object.clone(),
            )
            .await?;
        let Some(metadata) = metadata else {
            return provider
                .lookup_object(&repository.id, object)
                .await?
                .map(|stored_object| {
                    Self::ensure_stored_object_namespace(repository, stored_object)
                })
                .transpose();
        };

        if let Some(backend_id_lookup) = runtime.backend_id_lookup()
            && let Some(stored_object) = backend_id_lookup
                .lookup_object_by_backend_id(&repository.id, object, &metadata.backend_id)
                .await?
        {
            let stored_object = Self::ensure_stored_object_namespace(repository, stored_object)?;
            if metadata.verification_status != MetadataObjectVerificationStatus::Verified {
                self.record_verified_object_metadata(
                    repository,
                    object,
                    stored_object.backend_id.clone(),
                    metadata.created_by,
                )
                .await?;
            }
            return Ok(Some(stored_object));
        }

        let replacement = provider
            .lookup_object(&repository.id, object)
            .await?
            .map(|stored_object| Self::ensure_stored_object_namespace(repository, stored_object))
            .transpose()?;
        if let Some(stored_object) = &replacement {
            if stored_object.backend_id != metadata.backend_id
                || metadata.verification_status != MetadataObjectVerificationStatus::Verified
            {
                self.record_verified_object_metadata(
                    repository,
                    object,
                    stored_object.backend_id.clone(),
                    metadata.created_by,
                )
                .await?;
            }
        } else {
            self.metadata_database
                .mark_object_stale_async(
                    repository.id.clone(),
                    repository.storage_provider.clone(),
                    object.clone(),
                    metadata.backend_id,
                )
                .await?;
        }

        Ok(replacement)
    }

    async fn staged_download_response(
        &self,
        repository: &RepositoryMapping,
        object: &LfsObject,
        runtime: &ServerStorageProvider,
    ) -> ServerResult<StorageDownloadResponse> {
        let provider = runtime.provider();
        let provider_id = provider.provider_id().to_owned();
        let metadata = self
            .metadata_database
            .lookup_object_async(
                repository.id.clone(),
                repository.storage_provider.clone(),
                object.clone(),
            )
            .await?;
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
        let stored_object = match provider
            .download_object(&repository.id, object, temp_file.path())
            .await
        {
            Ok(stored_object) => stored_object,
            Err(source) => {
                if matches!(source, StorageError::ObjectNotFound { .. })
                    && let Some(metadata) = &metadata
                {
                    self.metadata_database
                        .mark_object_stale_async(
                            repository.id.clone(),
                            repository.storage_provider.clone(),
                            object.clone(),
                            metadata.backend_id.clone(),
                        )
                        .await?;
                }
                return Err(ServerError::Storage { source });
            }
        };
        let stored_object = Self::ensure_stored_object_namespace(repository, stored_object)?;
        // The fallback download already discovered the object while staging it,
        // so reconcile from that result instead of repeating a provider lookup.
        if let Some(metadata) = metadata
            && (stored_object.backend_id != metadata.backend_id
                || metadata.verification_status != MetadataObjectVerificationStatus::Verified)
        {
            self.record_verified_object_metadata(
                repository,
                object,
                stored_object.backend_id.clone(),
                metadata.created_by,
            )
            .await?;
        }
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
                    message: format!("download staging file metadata could not be read: {source}"),
                },
            })?;
        let temp_path = temp_file.into_temp_path();
        let body_stream = stream::unfold(
            (ReaderStream::new(file), temp_path),
            |(mut reader, temp_path)| async move {
                reader
                    .next()
                    .await
                    .map(|chunk| (chunk, (reader, temp_path)))
            },
        );
        let response = Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "application/octet-stream")
            .header(CONTENT_LENGTH, content_length.len().to_string())
            .body(Body::from_stream(body_stream))
            .map_err(|source| ServerError::Internal {
                message: format!("download response could not be built: {source}"),
            })?;

        Ok(StorageDownloadResponse::new(stored_object, response))
    }
}

impl LfsObjectTransferStore for StorageProviderTransferStore {
    fn lookup_object<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, ServerResult<Option<StoredObject>>> {
        Box::pin(async move { self.lookup_and_repair_object(repository, object).await })
    }

    fn upload_object<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        object: &'a LfsObject,
        source: &'a Path,
        created_by: &'a RepositoryUser,
    ) -> ProviderFuture<'a, ServerResult<StoredObject>> {
        Box::pin(async move {
            let provider = self.providers.provider_for(repository)?.provider();
            let stored_object = provider
                .upload_object(&repository.id, object, source)
                .await?;
            let stored_object = Self::ensure_stored_object_namespace(repository, stored_object)?;
            self.record_verified_object_metadata(
                repository,
                object,
                stored_object.backend_id.clone(),
                created_by.clone(),
            )
            .await?;
            Ok(stored_object)
        })
    }

    fn download_object_response<'a>(
        &'a self,
        repository: &'a RepositoryMapping,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, ServerResult<StorageDownloadResponse>> {
        Box::pin(async move {
            let runtime = self.providers.provider_for(repository)?;
            if let Some(streaming_download) = runtime.streaming_download() {
                let stored_object = self
                    .lookup_and_repair_object_with_runtime(repository, object, runtime)
                    .await?
                    .ok_or_else(|| ServerError::Storage {
                        source: StorageError::ObjectNotFound {
                            provider: repository.storage_provider.clone(),
                            oid: object.oid.as_hex().to_owned(),
                            size: object.size.bytes(),
                        },
                    })?;
                Self::validate_stored_object_namespace(repository, &stored_object)?;
                let download = streaming_download
                    .download_object_response(&repository.id, object, stored_object)
                    .await
                    .map_err(ServerError::from)?;
                Self::validate_stored_object_namespace(repository, download.stored_object())?;
                return Ok(download);
            }
            self.staged_download_response(repository, object, runtime)
                .await
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
            self.providers.provider_for(repository)?;
            self.record_verified_object_metadata(
                repository,
                object,
                backend_id.to_owned(),
                created_by.clone(),
            )
            .await
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
    ) -> ProviderFuture<'a, ServerResult<StorageDownloadResponse>> {
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
struct LfsServerState {
    routes: LfsRouteResolver,
    session_store: LocalLfsSessionStore,
    public_url: String,
    max_batch_objects: usize,
    batch_body_guardrails: BatchBodyGuardrails,
    authorizer: Arc<dyn LfsBatchAuthorizer>,
    transfer_store: Arc<dyn LfsObjectTransferStore>,
    metadata_database: Option<Arc<MetadataDatabase>>,
    provider_calls: Arc<Semaphore>,
    authorization_cache: Arc<std::sync::Mutex<HashMap<AuthorizationCacheKey, Instant>>>,
    authorization_locks: Arc<std::sync::Mutex<HashMap<AuthorizationCacheKey, Arc<AsyncMutex<()>>>>>,
    upload_locks: Arc<std::sync::Mutex<HashMap<String, Weak<AsyncMutex<()>>>>>,
    upload_staging: UploadStagingCoordinator,
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
        batch_body_guardrails: BatchBodyGuardrails,
        metadata_database: Option<Arc<MetadataDatabase>>,
    ) -> Self {
        let max_batch_objects = config.server.max_batch_objects;
        let max_provider_calls = config.server.max_provider_calls;
        let upload_staging = UploadStagingCoordinator::new(
            config.server.max_concurrent_uploads,
            config.server.max_concurrent_uploads_per_user,
        );
        Self {
            routes: LfsRouteResolver::new(&config),
            session_store,
            public_url: config.server.public_url,
            max_batch_objects,
            batch_body_guardrails,
            authorizer,
            transfer_store,
            metadata_database,
            provider_calls: Arc::new(Semaphore::new(max_provider_calls)),
            authorization_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            authorization_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            upload_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            upload_staging,
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
        // Weak entries preserve single-flight coordination only while an
        // upload holder or waiter owns the lock. Purging dead entries on every
        // admission prevents completed object keys from accumulating.
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }

        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
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
    ) -> ServerResult<StorageDownloadResponse> {
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

    async fn start_transfer_attempt(
        &self,
        repository: &RepositoryMapping,
        object: &LfsObject,
        operation: MetadataTransferOperation,
        user: &RepositoryUser,
    ) -> ServerResult<Option<i64>> {
        let Some(database) = &self.metadata_database else {
            return Ok(None);
        };

        database
            .start_transfer_attempt_async(
                repository.id.clone(),
                repository.storage_provider.clone(),
                object.clone(),
                operation,
                user.clone(),
            )
            .await
            .map(Some)
    }

    async fn finish_transfer_attempt(
        &self,
        attempt_id: Option<i64>,
        result: MetadataTransferResult,
    ) -> ServerResult<()> {
        let (Some(database), Some(attempt_id)) = (&self.metadata_database, attempt_id) else {
            return Ok(());
        };

        database
            .finish_transfer_attempt_async(attempt_id, result)
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

    fn upload_staging_principal(&self) -> String {
        let metadata = self.metadata();
        match metadata.stable_id.as_deref() {
            Some(stable_id) => format!("{}:id:{stable_id}", metadata.provider_id),
            None => format!("{}:login:{}", metadata.provider_id, metadata.login),
        }
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
            Err(ServerError::Unauthorized { .. }) => {
                tracing::debug!("LFS route request was not authenticated");
                authentication_required_response()
            }
            Err(error) => {
                tracing::error!(
                    error_category = %server_error_log_category(&error),
                    "failed to authenticate LFS route request"
                );
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
        Err(ServerError::InvalidRequest { .. }) => {
            tracing::debug!("invalid LFS route request");
            git_lfs_json_error_response(StatusCode::BAD_REQUEST, "Invalid LFS Cloud route")
        }
        Err(error) => {
            tracing::error!(
                error_category = %server_error_log_category(&error),
                "failed to resolve LFS route"
            );
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
            StatusCode::NOT_FOUND,
            "Git LFS base path is not an operation endpoint; use /objects/batch",
        ),
    }
}

#[derive(Clone, Copy, Debug)]
struct BatchBodyGuardrails {
    max_bytes: usize,
    idle_timeout: Duration,
    total_timeout: Duration,
}

impl Default for BatchBodyGuardrails {
    fn default() -> Self {
        Self {
            max_bytes: MAX_BATCH_BODY_BYTES,
            idle_timeout: BATCH_BODY_IDLE_TIMEOUT,
            total_timeout: BATCH_BODY_TOTAL_TIMEOUT,
        }
    }
}

#[derive(Debug)]
enum BatchBodyReadError {
    PayloadTooLarge,
    TimedOut,
    Unreadable(axum::Error),
}

async fn read_batch_request_body(
    request: Request,
    guardrails: BatchBodyGuardrails,
) -> Result<Bytes, BatchBodyReadError> {
    if request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > guardrails.max_bytes as u64)
    {
        return Err(BatchBodyReadError::PayloadTooLarge);
    }

    let total_deadline = tokio::time::Instant::now() + guardrails.total_timeout;
    let mut stream = request.into_body().into_data_stream();
    let mut body = Vec::new();

    loop {
        let next = tokio::select! {
            _ = tokio::time::sleep_until(total_deadline) => {
                return Err(BatchBodyReadError::TimedOut);
            }
            next = tokio::time::timeout(guardrails.idle_timeout, stream.next()) => {
                next.map_err(|_| BatchBodyReadError::TimedOut)?
            }
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(BatchBodyReadError::Unreadable)?;
        let next_length = body
            .len()
            .checked_add(chunk.len())
            .ok_or(BatchBodyReadError::PayloadTooLarge)?;
        if next_length > guardrails.max_bytes {
            return Err(BatchBodyReadError::PayloadTooLarge);
        }
        body.extend_from_slice(&chunk);
    }

    Ok(Bytes::from(body))
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

    match read_batch_request_body(request, state.batch_body_guardrails).await {
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
            Err(_) => {
                tracing::debug!(
                    repo_id = repository.id.as_str(),
                    "invalid Git LFS batch request"
                );
                git_lfs_json_error_response(
                    StatusCode::BAD_REQUEST,
                    "Invalid Git LFS batch request",
                )
            }
        },
        Err(BatchBodyReadError::PayloadTooLarge) => git_lfs_json_error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Git LFS request body exceeds the configured limit",
        ),
        Err(BatchBodyReadError::TimedOut) => git_lfs_json_error_response(
            StatusCode::REQUEST_TIMEOUT,
            "Git LFS batch request timed out while reading the request body",
        ),
        Err(BatchBodyReadError::Unreadable(error)) => {
            tracing::debug!(
                repo_id = repository.id.as_str(),
                error_type = std::any::type_name_of_val(&error),
                "failed to read Git LFS batch request body"
            );
            git_lfs_json_error_response(
                StatusCode::BAD_REQUEST,
                "Git LFS request body could not be read",
            )
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
        Err(_) => {
            tracing::debug!(
                repo_id = repository.id.as_str(),
                oid = oid.as_hex(),
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
            error_category = %server_error_log_category(&error),
            "Git LFS download transfer authorization failed"
        );
        return git_lfs_authorization_error_response(error);
    }

    let object = LfsObject::new(oid, LfsObjectSize::new(expected_size));
    let transfer_user = repository_user_from_session(&session);
    let attempt_id = match state
        .start_transfer_attempt(
            &repository,
            &object,
            MetadataTransferOperation::Download,
            &transfer_user,
        )
        .await
    {
        Ok(attempt_id) => attempt_id,
        Err(error) => {
            tracing::error!(
                repo_id = repository.id.as_str(),
                oid = object.oid.as_hex(),
                error_category = %server_error_log_category(&error),
                "failed to record Git LFS download transfer start"
            );
            return git_lfs_download_storage_error_response(error);
        }
    };
    match state.download_object_response(&repository, &object).await {
        Ok(download) => {
            let backend_id = download.stored_object().backend_id.clone();
            if let Err(error) = state
                .finish_transfer_attempt(
                    attempt_id,
                    MetadataTransferResult::succeeded(Some(backend_id)),
                )
                .await
            {
                tracing::error!(
                    repo_id = repository.id.as_str(),
                    oid = object.oid.as_hex(),
                    error_category = %server_error_log_category(&error),
                    "failed to record Git LFS download transfer success"
                );
                return git_lfs_download_storage_error_response(error);
            }
            tracing::debug!(
                repo_id = repository.id.as_str(),
                storage_provider = download.stored_object().provider_id.as_str(),
                oid = object.oid.as_hex(),
                size = object.size.bytes(),
                "prepared verified Git LFS download response"
            );
            download.into_response()
        }
        Err(error) => {
            finish_failed_transfer_attempt(state, attempt_id, &error, true).await;
            tracing::debug!(
                repo_id = repository.id.as_str(),
                oid = object.oid.as_hex(),
                size = object.size.bytes(),
                error_category = %server_error_log_category(&error),
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
        Err(_) => {
            tracing::debug!(
                repo_id = repository.id.as_str(),
                oid = oid.as_hex(),
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
            error_category = %server_error_log_category(&error),
            "Git LFS upload transfer authorization failed"
        );
        return git_lfs_authorization_error_response(error);
    }

    let object = LfsObject::new(oid.clone(), LfsObjectSize::new(expected_size));
    let created_by = repository_user_from_session(&session);
    let attempt_id = match state
        .start_transfer_attempt(
            &repository,
            &object,
            MetadataTransferOperation::Upload,
            &created_by,
        )
        .await
    {
        Ok(attempt_id) => attempt_id,
        Err(error) => {
            tracing::error!(
                repo_id = repository.id.as_str(),
                oid = object.oid.as_hex(),
                error_category = %server_error_log_category(&error),
                "failed to record Git LFS upload transfer start"
            );
            return git_lfs_storage_error_response(error);
        }
    };
    let upload_lock = state.upload_lock_for(&repository, &oid);
    let _upload_lock_guard = upload_lock.lock().await;
    let _durable_upload_lock = match state.metadata_database.as_ref().map(|database| {
        database.acquire_object_upload_lock(
            repository.id.clone(),
            repository.storage_provider.clone(),
            object.clone(),
        )
    }) {
        Some(lock) => match lock.await {
            Ok(lock) => lock,
            Err(error) => {
                finish_failed_transfer_attempt(state, attempt_id, &error, false).await;
                tracing::debug!(
                    repo_id = repository.id.as_str(),
                    oid = object.oid.as_hex(),
                    error_category = %server_error_log_category(&error),
                    "Git LFS upload durable lock acquisition failed"
                );
                return git_lfs_storage_error_response(error);
            }
        },
        None => None,
    };

    match state.lookup_object(&repository, &object).await {
        Ok(Some(stored_object)) => {
            tracing::debug!(
                repo_id = repository.id.as_str(),
                storage_provider = stored_object.provider_id.as_str(),
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
                finish_failed_transfer_attempt(state, attempt_id, &error, false).await;
                tracing::debug!(
                    repo_id = repository.id.as_str(),
                    oid = object.oid.as_hex(),
                    error_category = %server_error_log_category(&error),
                    "Git LFS upload transfer metadata repair failed"
                );
                return git_lfs_storage_error_response(error);
            }
            if let Err(error) = state
                .finish_transfer_attempt(
                    attempt_id,
                    MetadataTransferResult::succeeded(Some(stored_object.backend_id.clone())),
                )
                .await
            {
                tracing::error!(
                    repo_id = repository.id.as_str(),
                    oid = object.oid.as_hex(),
                    error_category = %server_error_log_category(&error),
                    "failed to record Git LFS upload transfer success"
                );
                return git_lfs_storage_error_response(error);
            }
            return StatusCode::OK.into_response();
        }
        Ok(None) => {}
        Err(error) => {
            finish_failed_transfer_attempt(state, attempt_id, &error, false).await;
            tracing::debug!(
                repo_id = repository.id.as_str(),
                oid = object.oid.as_hex(),
                error_category = %server_error_log_category(&error),
                "Git LFS upload transfer existence check failed"
            );
            return git_lfs_storage_error_response(error);
        }
    }

    let staging_lease = match state
        .upload_staging
        .try_acquire(&session.upload_staging_principal())
    {
        Ok(lease) => lease,
        Err(UploadStagingError::ConcurrencyLimit) => {
            finish_failed_transfer_attempt_with_message(
                state,
                attempt_id,
                ErrorCategory::Storage,
                "Git LFS upload staging has reached its concurrency limit",
            )
            .await;
            return upload_staging_overloaded_response();
        }
        Err(error) => {
            let error = error.into_storage_error();
            let error = ServerError::from(error);
            finish_failed_transfer_attempt(state, attempt_id, &error, false).await;
            tracing::debug!(
                repo_id = repository.id.as_str(),
                oid = oid.as_hex(),
                error_category = %server_error_log_category(&error),
                "Git LFS upload staging admission failed"
            );
            return git_lfs_storage_error_response(error);
        }
    };

    let staged_upload = match stage_upload_request_body_with_lease(
        &oid,
        Some(expected_size),
        request,
        UploadStagingGuardrails::default(),
        staging_lease,
    )
    .await
    {
        Ok(staged_upload) => staged_upload,
        Err(UploadStagingError::PayloadTooLarge) => {
            finish_failed_transfer_attempt_with_message(
                state,
                attempt_id,
                ErrorCategory::Storage,
                "Git LFS upload object exceeds the configured request size limit",
            )
            .await;
            return upload_payload_too_large_response();
        }
        Err(UploadStagingError::InsufficientTempSpace { .. }) => {
            finish_failed_transfer_attempt_with_message(
                state,
                attempt_id,
                ErrorCategory::Storage,
                "Git LFS upload staging directory does not have enough free space",
            )
            .await;
            return upload_temp_space_exhausted_response();
        }
        Err(UploadStagingError::TimedOut) => {
            finish_failed_transfer_attempt_with_message(
                state,
                attempt_id,
                ErrorCategory::Storage,
                "Git LFS upload request timed out while reading the object body",
            )
            .await;
            return upload_staging_timeout_response();
        }
        Err(UploadStagingError::ConcurrencyLimit) => {
            finish_failed_transfer_attempt_with_message(
                state,
                attempt_id,
                ErrorCategory::Storage,
                "Git LFS upload staging has reached its concurrency limit",
            )
            .await;
            return upload_staging_overloaded_response();
        }
        Err(error) => {
            let error = error.into_storage_error();
            let error = ServerError::from(error);
            finish_failed_transfer_attempt(state, attempt_id, &error, false).await;
            tracing::debug!(
                repo_id = repository.id.as_str(),
                oid = oid.as_hex(),
                error_category = %server_error_log_category(&error),
                "Git LFS upload transfer staging failed"
            );
            return git_lfs_storage_error_response(error);
        }
    };

    match state
        .upload_object(&repository, &object, staged_upload.path(), &created_by)
        .await
    {
        Ok(stored_object) => {
            if let Err(error) = state
                .finish_transfer_attempt(
                    attempt_id,
                    MetadataTransferResult::succeeded(Some(stored_object.backend_id.clone())),
                )
                .await
            {
                tracing::error!(
                    repo_id = repository.id.as_str(),
                    oid = object.oid.as_hex(),
                    error_category = %server_error_log_category(&error),
                    "failed to record Git LFS upload transfer success"
                );
                return git_lfs_storage_error_response(error);
            }
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
            finish_failed_transfer_attempt(state, attempt_id, &error, false).await;
            tracing::debug!(
                repo_id = repository.id.as_str(),
                oid = object.oid.as_hex(),
                error_category = %server_error_log_category(&error),
                "Git LFS upload transfer storage write failed"
            );
            git_lfs_storage_error_response(error)
        }
    }
}

fn repository_user_from_session(session: &AuthenticatedLfsSession) -> RepositoryUser {
    RepositoryUser::new(
        session.metadata().provider_id.clone(),
        session.metadata().login.clone(),
        session.metadata().stable_id.clone(),
    )
}

async fn finish_failed_transfer_attempt(
    state: &LfsServerState,
    attempt_id: Option<i64>,
    error: &ServerError,
    download: bool,
) {
    let category = server_error_log_category(error);
    let (_, message) = git_lfs_storage_error_response_parts(error, download);
    finish_failed_transfer_attempt_with_message(state, attempt_id, category, message).await;
}

fn server_error_log_category(error: &ServerError) -> ErrorCategory {
    match error {
        ServerError::RepositoryProvider { source } => source.category(),
        ServerError::Storage { source } => source.category(),
        _ => error.category(),
    }
}

async fn finish_failed_transfer_attempt_with_message(
    state: &LfsServerState,
    attempt_id: Option<i64>,
    category: ErrorCategory,
    message: &'static str,
) {
    if let Err(error) = state
        .finish_transfer_attempt(
            attempt_id,
            MetadataTransferResult::failed(category, SanitizedMessage::new(message)),
        )
        .await
    {
        tracing::error!(
            error_category = %server_error_log_category(&error),
            "failed to record Git LFS transfer failure"
        );
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

fn upload_staging_overloaded_response() -> Response {
    let mut response = git_lfs_json_error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "Git LFS upload staging has reached its concurrency limit",
    );
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

struct StagedUpload {
    _lease: UploadStagingLease,
    temp_file: tempfile::NamedTempFile,
}

impl StagedUpload {
    fn path(&self) -> &Path {
        self.temp_file.path()
    }
}

#[cfg(test)]
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

#[cfg(test)]
async fn stage_upload_request_body_with_limit(
    expected_oid: &LfsOid,
    expected_size: Option<u64>,
    request: Request,
    max_upload_bytes: u64,
) -> Result<StagedUpload, UploadStagingError> {
    let coordinator = UploadStagingCoordinator::new(1, 1);
    let lease = coordinator.try_acquire("standalone")?;
    stage_upload_request_body_with_lease(
        expected_oid,
        expected_size,
        request,
        UploadStagingGuardrails {
            max_upload_bytes,
            ..UploadStagingGuardrails::default()
        },
        lease,
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

#[cfg(test)]
async fn stage_upload_request_body_with_guardrails(
    expected_oid: &LfsOid,
    expected_size: Option<u64>,
    request: Request,
    guardrails: UploadStagingGuardrails,
) -> Result<StagedUpload, UploadStagingError> {
    let coordinator = UploadStagingCoordinator::new(1, 1);
    let lease = coordinator.try_acquire("standalone")?;
    stage_upload_request_body_with_lease(expected_oid, expected_size, request, guardrails, lease)
        .await
}

async fn stage_upload_request_body_with_lease(
    expected_oid: &LfsOid,
    expected_size: Option<u64>,
    request: Request,
    guardrails: UploadStagingGuardrails,
    lease: UploadStagingLease,
) -> Result<StagedUpload, UploadStagingError> {
    let preflight_size = upload_staging_preflight_size(expected_size, guardrails.max_upload_bytes)?;
    let temp_file = tempfile::Builder::new()
        .prefix("lfscloud-upload-")
        .tempfile()
        .map_err(|source| StorageError::Retryable {
            provider: "lfscloud".to_owned(),
            message: format!("upload staging file could not be created: {source}"),
        })?;
    let staging_dir = temp_file
        .path()
        .parent()
        .ok_or_else(|| StorageError::Retryable {
            provider: "lfscloud".to_owned(),
            message: format!(
                "upload staging file {} did not have a parent directory",
                temp_file.path().display()
            ),
        })?;
    // Unknown-size helper callers reserve the full effective upload limit so
    // they cannot skip the temp-space guardrail before streaming begins.
    let lease = lease
        .reserve(staging_dir, preflight_size, guardrails.min_free_bytes)
        .await?;

    let std_file = temp_file
        .reopen()
        .map_err(|source| StorageError::StagedFileRead {
            provider: "lfscloud".to_owned(),
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
            provider: "lfscloud".to_owned(),
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

    Ok(StagedUpload {
        _lease: lease,
        temp_file,
    })
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

#[derive(Clone)]
struct UploadStagingCoordinator {
    global_slots: Arc<Semaphore>,
    per_user_limit: usize,
    per_user_slots: Arc<std::sync::Mutex<HashMap<String, Weak<Semaphore>>>>,
    reservations: Arc<std::sync::Mutex<UploadStagingReservationState>>,
}

#[derive(Default)]
struct UploadStagingReservationState {
    available_space_snapshot: Option<u64>,
    reserved_bytes: u64,
}

impl UploadStagingCoordinator {
    fn new(global_limit: usize, per_user_limit: usize) -> Self {
        Self {
            global_slots: Arc::new(Semaphore::new(global_limit)),
            per_user_limit,
            per_user_slots: Arc::new(std::sync::Mutex::new(HashMap::new())),
            reservations: Arc::new(std::sync::Mutex::new(
                UploadStagingReservationState::default(),
            )),
        }
    }

    fn try_acquire(&self, principal: &str) -> Result<UploadStagingLease, UploadStagingError> {
        let global_permit = self
            .global_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| UploadStagingError::ConcurrencyLimit)?;
        let user_slots = {
            let mut slots = self
                .per_user_slots
                .lock()
                .expect("upload staging user-slot map should not be poisoned");
            // Weak entries avoid turning one-off authenticated users into a
            // process-lifetime map while preserving one semaphore per active
            // principal across concurrent admission attempts.
            slots.retain(|_, semaphore| semaphore.strong_count() > 0);
            match slots.get(principal).and_then(Weak::upgrade) {
                Some(semaphore) => semaphore,
                None => {
                    let semaphore = Arc::new(Semaphore::new(self.per_user_limit));
                    slots.insert(principal.to_owned(), Arc::downgrade(&semaphore));
                    semaphore
                }
            }
        };
        let user_permit = user_slots
            .try_acquire_owned()
            .map_err(|_| UploadStagingError::ConcurrencyLimit)?;

        Ok(UploadStagingLease {
            coordinator: self.clone(),
            _global_permit: global_permit,
            _user_permit: user_permit,
            reservation: None,
        })
    }

    fn reserve_with_available_space(
        &self,
        expected_size: u64,
        min_free_bytes: u64,
        available_space: u64,
    ) -> Result<UploadStagingDiskReservation, UploadStagingError> {
        let mut state = self
            .reservations
            .lock()
            .expect("upload staging reservation state should not be poisoned");
        let request_required = expected_size.checked_add(min_free_bytes).ok_or(
            UploadStagingError::InsufficientTempSpace {
                required_space: None,
                available_space: Some(available_space),
            },
        )?;
        if available_space < request_required {
            return Err(UploadStagingError::InsufficientTempSpace {
                required_space: Some(request_required),
                available_space: Some(available_space),
            });
        }

        // Freeze one capacity snapshot while any managed staging file is
        // alive. Every declared size spends that shared budget atomically;
        // the per-request live check above remains a secondary signal for
        // unrelated filesystem pressure.
        let snapshot = *state
            .available_space_snapshot
            .get_or_insert(available_space);
        let aggregate_required = state
            .reserved_bytes
            .checked_add(expected_size)
            .and_then(|reserved| reserved.checked_add(min_free_bytes))
            .ok_or(UploadStagingError::InsufficientTempSpace {
                required_space: None,
                available_space: Some(snapshot),
            })?;
        if snapshot < aggregate_required {
            return Err(UploadStagingError::InsufficientTempSpace {
                required_space: Some(aggregate_required),
                available_space: Some(snapshot),
            });
        }

        state.reserved_bytes = state
            .reserved_bytes
            .checked_add(expected_size)
            .expect("validated upload staging reservation should not overflow");
        Ok(UploadStagingDiskReservation {
            bytes: expected_size,
            reservations: self.reservations.clone(),
        })
    }
}

struct UploadStagingLease {
    coordinator: UploadStagingCoordinator,
    _global_permit: OwnedSemaphorePermit,
    _user_permit: OwnedSemaphorePermit,
    reservation: Option<UploadStagingDiskReservation>,
}

impl UploadStagingLease {
    async fn reserve(
        self,
        staging_dir: &Path,
        expected_size: u64,
        min_free_bytes: u64,
    ) -> Result<Self, UploadStagingError> {
        let staging_dir = staging_dir.to_path_buf();
        let available_space =
            tokio::task::spawn_blocking(move || fs4::available_space(staging_dir))
                .await
                .map_err(|source| StorageError::Retryable {
                    provider: "lfscloud".to_owned(),
                    message: format!(
                        "upload staging directory free-space check did not complete: {source}"
                    ),
                })?
                .map_err(|source| StorageError::Retryable {
                    provider: "lfscloud".to_owned(),
                    message: format!(
                        "upload staging directory free space could not be inspected: {source}"
                    ),
                })?;

        self.reserve_with_available_space(expected_size, min_free_bytes, available_space)
    }

    fn reserve_with_available_space(
        mut self,
        expected_size: u64,
        min_free_bytes: u64,
        available_space: u64,
    ) -> Result<Self, UploadStagingError> {
        let reservation = self.coordinator.reserve_with_available_space(
            expected_size,
            min_free_bytes,
            available_space,
        )?;
        self.reservation = Some(reservation);
        Ok(self)
    }
}

struct UploadStagingDiskReservation {
    bytes: u64,
    reservations: Arc<std::sync::Mutex<UploadStagingReservationState>>,
}

impl Drop for UploadStagingDiskReservation {
    fn drop(&mut self) {
        let mut state = self
            .reservations
            .lock()
            .expect("upload staging reservation state should not be poisoned");
        state.reserved_bytes = state
            .reserved_bytes
            .checked_sub(self.bytes)
            .expect("upload staging reservations should release exactly once");
        if state.reserved_bytes == 0 {
            state.available_space_snapshot = None;
        }
    }
}

fn upload_staging_file_io_error(source: io::Error, action: &str) -> UploadStagingError {
    if is_temp_space_exhausted(&source) {
        return UploadStagingError::InsufficientTempSpace {
            required_space: None,
            available_space: None,
        };
    }

    StorageError::Retryable {
        provider: "lfscloud".to_owned(),
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
    ConcurrencyLimit,
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
                provider: "lfscloud".to_owned(),
                message: "upload object exceeded request size limit".to_owned(),
            },
            Self::ConcurrencyLimit => StorageError::Retryable {
                provider: "lfscloud".to_owned(),
                message: "upload staging concurrency limit reached".to_owned(),
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
                    provider: "lfscloud".to_owned(),
                    message,
                }
            }
            Self::TimedOut => StorageError::Retryable {
                provider: "lfscloud".to_owned(),
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
            error_category = %server_error_log_category(&error),
            "Git LFS batch authorization failed"
        );
        return git_lfs_authorization_error_response(error);
    }

    match request.operation {
        LfsBatchOperation::Download => {
            match download_batch_response_with_storage_lookup(&repository, state, request).await {
                Ok(response) => git_lfs_json_response(with_session_action_authorization(
                    response,
                    session.token(),
                )),
                Err(error) => {
                    tracing::debug!(
                        repo_id = repository.id.as_str(),
                        error_category = %server_error_log_category(&error),
                        "Git LFS download batch storage lookup failed"
                    );
                    git_lfs_storage_error_response(error)
                }
            }
        }
        LfsBatchOperation::Upload => {
            match upload_batch_response_with_storage_lookup(&repository, state, request).await {
                Ok(response) => git_lfs_json_response(with_session_action_authorization(
                    response,
                    session.token(),
                )),
                Err(error) => {
                    tracing::debug!(
                        repo_id = repository.id.as_str(),
                        error_category = %server_error_log_category(&error),
                        "Git LFS upload batch storage lookup failed"
                    );
                    git_lfs_storage_error_response(error)
                }
            }
        }
    }
}

fn with_session_action_authorization(
    mut response: LfsBatchResponse,
    token: &LfsSessionToken,
) -> LfsBatchResponse {
    // The reference Git LFS client does not carry batch credentials to action
    // URLs automatically. Supplying the repository-scoped local credential in
    // each action keeps backend provider tokens private while letting the
    // client authenticate the advertised upload or download request.
    let credentials = BASE64_STANDARD.encode(format!(
        "{DEFAULT_GIT_CREDENTIAL_USERNAME}:{}",
        token.as_str()
    ));
    let authorization = format!("Basic {credentials}");

    for object in &mut response.objects {
        for action in object.actions.values_mut() {
            action
                .header
                .insert("Authorization".to_owned(), authorization.clone());
        }
    }

    response
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
    let objects = batch_objects_with_storage_lookup(
        repository,
        state,
        request.objects,
        download_batch_lookup_outcome,
    )
    .await;

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
    let objects = batch_objects_with_storage_lookup(
        repository,
        state,
        request.objects,
        upload_batch_lookup_outcome,
    )
    .await;

    Ok(LfsBatchResponse::upload(
        &state.public_url,
        repository.route_path(),
        objects,
    ))
}

async fn batch_objects_with_storage_lookup<T>(
    repository: &RepositoryMapping,
    state: &LfsServerState,
    requested_objects: Vec<LfsObject>,
    outcome_from_lookup: fn(LfsObject, ServerResult<Option<StoredObject>>) -> T,
) -> Vec<T>
where
    T: Clone,
{
    let unique_objects = requested_objects.iter().cloned().collect::<BTreeSet<_>>();
    let outcomes = stream::iter(unique_objects)
        .map(|object| async move {
            let lookup = state.lookup_object(repository, &object).await;
            (object.clone(), outcome_from_lookup(object, lookup))
        })
        .buffered(BATCH_STORAGE_LOOKUP_CONCURRENCY)
        .collect::<BTreeMap<_, _>>()
        .await;
    requested_objects
        .into_iter()
        .map(|object| {
            outcomes
                .get(&object)
                .expect("every requested object should have one lookup outcome")
                .clone()
        })
        .collect()
}

fn download_batch_lookup_outcome(
    object: LfsObject,
    lookup: ServerResult<Option<StoredObject>>,
) -> LfsBatchDownloadObject {
    match lookup {
        Ok(Some(_)) => LfsBatchDownloadObject::available(object),
        Ok(None) => LfsBatchDownloadObject::missing(object),
        Err(error) => {
            LfsBatchDownloadObject::error(object, lfs_batch_object_error_from_server_error(&error))
        }
    }
}

fn upload_batch_lookup_outcome(
    object: LfsObject,
    lookup: ServerResult<Option<StoredObject>>,
) -> LfsBatchUploadObject {
    match lookup {
        Ok(Some(_)) => LfsBatchUploadObject::present(object),
        Ok(None) => LfsBatchUploadObject::needed(object),
        Err(error) => {
            LfsBatchUploadObject::error(object, lfs_batch_object_error_from_server_error(&error))
        }
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
            HeaderValue::from_static("Bearer realm=\"lfscloud\""),
        );
    }

    response
}

fn git_lfs_storage_error_response(error: ServerError) -> Response {
    let (status, message) = git_lfs_storage_error_response_parts(&error, false);
    git_lfs_json_error_response(status, message)
}

fn git_lfs_storage_error_response_parts(
    error: &ServerError,
    download: bool,
) -> (StatusCode, &'static str) {
    let classification = classify_lfs_storage_error(error);
    if download {
        (
            classification.download_status,
            classification.download_message,
        )
    } else {
        (classification.upload_status, classification.upload_message)
    }
}

/// Stable client-facing classification shared by transfer and batch errors.
///
/// Upload and download transfers may require different HTTP statuses and
/// messages, while the batch object response has its own numeric code and
/// message.
#[derive(Clone, Copy)]
struct LfsStorageErrorClassification {
    upload_status: StatusCode,
    download_status: StatusCode,
    upload_message: &'static str,
    download_message: &'static str,
    batch_code: u16,
    batch_message: &'static str,
}

impl LfsStorageErrorClassification {
    /// Builds a classification whose upload and download transfers share one
    /// status and message; the batch response remains independently specified.
    const fn uniform_transfer_response(
        status: StatusCode,
        message: &'static str,
        batch_code: u16,
        batch_message: &'static str,
    ) -> Self {
        Self {
            upload_status: status,
            download_status: status,
            upload_message: message,
            download_message: message,
            batch_code,
            batch_message,
        }
    }
}

fn classify_lfs_storage_error(error: &ServerError) -> LfsStorageErrorClassification {
    match error {
        // Upload mismatches describe invalid client bytes (422), while
        // download mismatches expose an invalid storage response (502).
        ServerError::Storage {
            source: StorageError::IntegrityMismatch { .. },
        } => LfsStorageErrorClassification {
            upload_status: StatusCode::UNPROCESSABLE_ENTITY,
            download_status: StatusCode::BAD_GATEWAY,
            upload_message: "uploaded Git LFS object did not match the requested OID or size",
            download_message: "Git LFS storage returned an object that failed integrity validation",
            batch_code: 502,
            batch_message: "object storage lookup failed",
        },
        ServerError::Storage {
            source: StorageError::ObjectNotFound { .. },
        } => LfsStorageErrorClassification::uniform_transfer_response(
            StatusCode::NOT_FOUND,
            "Git LFS object was not found",
            404,
            "object not found",
        ),
        ServerError::Storage {
            source: StorageError::Conflict { .. },
        } => LfsStorageErrorClassification::uniform_transfer_response(
            StatusCode::CONFLICT,
            "Git LFS storage reported an object conflict",
            409,
            "object storage conflict",
        ),
        ServerError::Storage {
            source: StorageError::QuotaExceeded { .. },
        } => LfsStorageErrorClassification::uniform_transfer_response(
            StatusCode::INSUFFICIENT_STORAGE,
            "Git LFS storage quota was exceeded",
            507,
            "object storage quota exceeded",
        ),
        ServerError::Storage {
            source: StorageError::Retryable { .. },
        } => LfsStorageErrorClassification::uniform_transfer_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Git LFS storage operation can be retried later",
            503,
            "object storage lookup can be retried later",
        ),
        ServerError::Storage {
            source: StorageError::PermissionDenied { .. },
        } => LfsStorageErrorClassification::uniform_transfer_response(
            StatusCode::BAD_GATEWAY,
            "Git LFS storage access was denied",
            502,
            "object storage access was denied",
        ),
        ServerError::Storage {
            source:
                StorageError::AuthenticationRequired { .. } | StorageError::CredentialLoad { .. },
        } => LfsStorageErrorClassification::uniform_transfer_response(
            StatusCode::BAD_GATEWAY,
            "Git LFS storage authentication failed",
            502,
            "object storage authentication failed",
        ),
        ServerError::Storage {
            source: StorageError::Unsupported { .. },
        } => LfsStorageErrorClassification::uniform_transfer_response(
            StatusCode::NOT_IMPLEMENTED,
            "Git LFS storage transfer handling is not configured",
            501,
            "object storage lookup is not configured",
        ),
        ServerError::Storage { .. } => LfsStorageErrorClassification::uniform_transfer_response(
            StatusCode::BAD_GATEWAY,
            "Git LFS storage operation failed",
            502,
            "object storage lookup failed",
        ),
        _ => LfsStorageErrorClassification::uniform_transfer_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Git LFS transfer handling failed",
            500,
            "object availability lookup failed",
        ),
    }
}

fn git_lfs_download_storage_error_response(error: ServerError) -> Response {
    let (status, message) = git_lfs_storage_error_response_parts(&error, true);
    git_lfs_json_error_response(status, message)
}

fn lfs_batch_object_error_from_server_error(error: &ServerError) -> LfsBatchObjectError {
    let classification = classify_lfs_storage_error(error);
    LfsBatchObjectError::new(classification.batch_code, classification.batch_message)
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
        HeaderValue::from_static("Bearer realm=\"lfscloud\""),
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
    /// The repository's base `/info/lfs` path, which identifies the repository
    /// but is not itself a Git LFS operation endpoint.
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
    /// use lfscloud::{LfsRouteEndpoint, LfsRouteResolver, ServerConfig};
    ///
    /// let config = ServerConfig::load_from_str(
    ///     r#"
    /// server:
    ///   public_url: http://127.0.0.1:8080
    /// repository_providers:
    ///   github-main:
    ///     type: github
    ///     api_url: https://api.github.com
    ///     personal_access_token: github-pat
    /// storage_providers:
    ///   drive-user-a:
    ///     type: google_drive
    ///     credentials:
    ///       type: gcloud
    ///       config_dir: .gcloud-drive
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
    /// # Ok::<(), lfscloud::ServerError>(())
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
                    repository_identity_path: format!(
                        "/{}/{}/{}",
                        repository.host, repository.owner, repository.name
                    ),
                    route_path_with_slash: format!("{route_path}/"),
                    route_path,
                    case_insensitive_identity: config
                        .repository_mapping_is_case_insensitive(&repository),
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
            if route.path_matches(path, &route.route_path)
                || route.path_matches(path, &route.route_path_with_slash)
            {
                return Ok(ResolvedLfsRoute {
                    repository: route.repository.clone(),
                    endpoint: LfsRouteEndpoint::Info,
                });
            }

            let Some(suffix) = route.strip_path_prefix(path) else {
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
    repository_identity_path: String,
    route_path: String,
    route_path_with_slash: String,
    case_insensitive_identity: bool,
    repository: RepositoryMapping,
}

impl ConfiguredLfsRoute {
    fn path_matches(&self, candidate: &str, configured: &str) -> bool {
        if !self.case_insensitive_identity {
            return candidate == configured;
        }

        let identity_length = self.repository_identity_path.len();
        let Some(candidate_identity) = candidate.get(..identity_length) else {
            return false;
        };
        let Some(candidate_suffix) = candidate.get(identity_length..) else {
            return false;
        };
        let Some(configured_suffix) = configured.get(identity_length..) else {
            return false;
        };

        candidate_identity.eq_ignore_ascii_case(&self.repository_identity_path)
            && candidate_suffix == configured_suffix
    }

    fn strip_path_prefix<'a>(&self, path: &'a str) -> Option<&'a str> {
        let prefix = &self.route_path_with_slash;
        let candidate = path.get(..prefix.len())?;
        self.path_matches(candidate, prefix)
            .then(|| &path[prefix.len()..])
    }
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

/// Renders the startup message shown by `lfscloud serve`.
#[must_use]
pub fn render_server_startup_message(urls: &AdvertisedServerUrls) -> String {
    let network = urls.network.as_deref().unwrap_or("(not detected)");

    format!(
        "LFS Cloud server running\n  local:   {}\n  network: {}",
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
        fs,
        io::{self, ErrorKind, Write},
        net::TcpListener as StdTcpListener,
        path::Path as FsPath,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{
        Json, Router,
        body::{Body, Bytes, to_bytes},
        extract::{OriginalUri, Path},
        http::{
            HeaderMap, HeaderValue, Method, Request, StatusCode,
            header::{
                ALLOW, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, RETRY_AFTER, WWW_AUTHENTICATE,
            },
        },
        response::{IntoResponse, Response},
        routing::get,
    };
    use tokio::sync::{Barrier, Notify};
    use tower::ServiceExt;

    use super::{
        BASE64_STANDARD, BatchBodyGuardrails, ConfiguredStorageProviders, LFS_AUTH_CHALLENGE,
        LFS_SESSION_REVOKE_PATH, LfsBatchAuthorizer, LfsObjectTransferStore, LfsRouteEndpoint,
        LfsRouteResolver, LfsRouterBuilder, LfsSessionRecord, MAX_UPLOAD_OBJECT_BYTES,
        ProviderBatchAuthorizer, ServeOptions, ServerBind, ServerBuilder, ServerCompositionClients,
        ServerShutdownOutcome, StorageProviderTransferStore, UploadStagingCoordinator,
        UploadStagingGuardrails, advertised_server_urls, authenticate_lfs_session,
        lfs_server_router, lfs_server_router_with_sessions, production_session_store,
        render_server_startup_message, serve_with_graceful_shutdown, server_router_with_sessions,
        stage_upload_request_body, stage_upload_request_body_with_guardrails,
        stage_upload_request_body_with_limit, upload_staging_file_io_error,
        upload_staging_preflight_size,
    };
    use base64::Engine as _;
    use futures_util::stream;
    use sha2::{Digest, Sha256};
    use tracing::instrument::WithSubscriber as _;

    use crate::{
        DEFAULT_GIT_CREDENTIAL_USERNAME, ErrorCategory, GitHubPersonalAccessToken,
        GitHubUserClient, GoogleDriveAccessToken, GoogleDriveRootValidator,
        GoogleDriveStorageConfig, LfsBatchOperation, LfsBatchResponse, LfsObject, LfsObjectSize,
        LfsOid, LfsSessionToken, LocalLfsSessionStore, MetadataDatabase,
        MetadataObjectVerificationStatus, ProviderFuture, RepositoryMapping, RepositoryPermission,
        RepositoryProviderConfig, RepositoryProviderError, RepositoryUser, SanitizedMessage,
        ServerConfig, ServerError, ServerResult, StorageDeleteOutcome, StorageDownloadResponse,
        StorageError, StorageProvider, StorageProviderConfig, StorageResult, StoredObject,
        google_drive::{GoogleDriveAccessTokenCache, GoogleDriveAccessTokenSource},
        provider_factory::ServerStorageProviderFactory,
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
    personal_access_token: github-pat
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
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

    #[test]
    fn stored_object_validation_rejects_foreign_storage_provider() {
        let config = test_config();
        let repository = &config.repositories[0];
        let object = LfsObject::new(
            LfsOid::new("a".repeat(64)).expect("test OID should parse"),
            LfsObjectSize::new(42),
        );
        let stored_object = StoredObject::new(
            "drive-user-b",
            repository.id.clone(),
            object,
            "foreign-provider-object",
        );

        let error = StorageProviderTransferStore::validate_stored_object_namespace(
            repository,
            &stored_object,
        )
        .expect_err("foreign provider metadata should be rejected");

        assert!(matches!(
            error,
            ServerError::Storage {
                source: StorageError::RepositoryNamespaceMismatch { ref provider }
            } if provider == "drive-user-a"
        ));
    }

    struct CountingFallbackStorageProvider {
        object: LfsObject,
        bytes: Vec<u8>,
        lookup_calls: AtomicUsize,
    }

    impl StorageProvider for CountingFallbackStorageProvider {
        fn provider_id(&self) -> &str {
            "drive-user-a"
        }

        fn lookup_object<'a>(
            &'a self,
            repository_namespace: &'a str,
            object: &'a LfsObject,
        ) -> ProviderFuture<'a, StorageResult<Option<StoredObject>>> {
            Box::pin(async move {
                self.lookup_calls.fetch_add(1, Ordering::SeqCst);
                Ok((object == &self.object).then(|| {
                    StoredObject::new(
                        self.provider_id(),
                        repository_namespace,
                        object.clone(),
                        "fallback-object",
                    )
                }))
            })
        }

        fn upload_object<'a>(
            &'a self,
            _repository_namespace: &'a str,
            _object: &'a LfsObject,
            _source: &'a FsPath,
        ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
            Box::pin(async {
                Err(StorageError::Unsupported {
                    provider_type: "test fallback storage".to_owned(),
                })
            })
        }

        fn download_object<'a>(
            &'a self,
            repository_namespace: &'a str,
            object: &'a LfsObject,
            destination: &'a FsPath,
        ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
            Box::pin(async move {
                let stored_object = self
                    .lookup_object(repository_namespace, object)
                    .await?
                    .ok_or_else(|| StorageError::ObjectNotFound {
                        provider: self.provider_id().to_owned(),
                        oid: object.oid.as_hex().to_owned(),
                        size: object.size.bytes(),
                    })?;
                fs::write(destination, &self.bytes).map_err(|source| StorageError::Retryable {
                    provider: self.provider_id().to_owned(),
                    message: format!("test fallback download could not be staged: {source}"),
                })?;
                Ok(stored_object)
            })
        }

        fn delete_or_mark_object<'a>(
            &'a self,
            _repository_namespace: &'a str,
            _object: &'a LfsObject,
        ) -> ProviderFuture<'a, StorageResult<StorageDeleteOutcome>> {
            Box::pin(async {
                Ok(StorageDeleteOutcome::Retained {
                    reason: "test fallback storage retains objects".to_owned(),
                })
            })
        }
    }

    #[tokio::test]
    async fn staged_download_resolves_fallback_provider_object_once() {
        let bytes = b"single fallback lookup".to_vec();
        let object = LfsObject::new(
            LfsOid::new(format!("{:x}", Sha256::digest(&bytes)))
                .expect("test object OID should parse"),
            LfsObjectSize::new(u64::try_from(bytes.len()).expect("test bytes should fit u64")),
        );
        let provider = Arc::new(CountingFallbackStorageProvider {
            object: object.clone(),
            bytes,
            lookup_calls: AtomicUsize::new(0),
        });
        let config = test_config();
        let repository = &config.repositories[0];
        let providers = ConfiguredStorageProviders::from_provider(&config, provider.clone())
            .expect("fallback provider should compose");
        let metadata = Arc::new(MetadataDatabase::open_in_memory().expect("metadata should open"));
        metadata
            .sync_config(&config)
            .expect("metadata config should synchronize");
        let creator = RepositoryUser::new("github-main", "octocat", Some("user-1".to_owned()));
        metadata
            .record_verified_object(
                &repository.id,
                &repository.storage_provider,
                &object,
                "stale-fallback-object",
                &creator,
            )
            .expect("stale backend metadata should record");
        let store = StorageProviderTransferStore::new(providers, metadata.clone());

        let response = store
            .download_object_response(repository, &object)
            .await
            .expect("fallback download should succeed");

        assert_eq!(response.stored_object().backend_id, "fallback-object");
        let repaired = metadata
            .lookup_object(&repository.id, &repository.storage_provider, &object)
            .expect("repaired metadata should load")
            .expect("repaired metadata should exist");
        assert_eq!(repaired.backend_id, "fallback-object");
        assert_eq!(
            repaired.verification_status,
            MetadataObjectVerificationStatus::Verified
        );
        assert_eq!(
            provider.lookup_calls.load(Ordering::SeqCst),
            1,
            "fallback download should not repeat object discovery"
        );
    }

    #[tokio::test]
    async fn staged_download_marks_missing_fallback_provider_object_stale() {
        let available_bytes = b"available fallback object".to_vec();
        let available_object = LfsObject::new(
            LfsOid::new(format!("{:x}", Sha256::digest(&available_bytes)))
                .expect("test object OID should parse"),
            LfsObjectSize::new(
                u64::try_from(available_bytes.len()).expect("test bytes should fit u64"),
            ),
        );
        let missing_bytes = b"missing fallback object";
        let missing_object = LfsObject::new(
            LfsOid::new(format!("{:x}", Sha256::digest(missing_bytes)))
                .expect("test object OID should parse"),
            LfsObjectSize::new(
                u64::try_from(missing_bytes.len()).expect("test bytes should fit u64"),
            ),
        );
        let provider = Arc::new(CountingFallbackStorageProvider {
            object: available_object,
            bytes: available_bytes,
            lookup_calls: AtomicUsize::new(0),
        });
        let config = test_config();
        let repository = &config.repositories[0];
        let providers = ConfiguredStorageProviders::from_provider(&config, provider.clone())
            .expect("fallback provider should compose");
        let metadata = Arc::new(MetadataDatabase::open_in_memory().expect("metadata should open"));
        metadata
            .sync_config(&config)
            .expect("metadata config should synchronize");
        metadata
            .record_verified_object(
                &repository.id,
                &repository.storage_provider,
                &missing_object,
                "missing-fallback-object",
                &RepositoryUser::new("github-main", "octocat", Some("user-1".to_owned())),
            )
            .expect("missing backend metadata should record");
        let store = StorageProviderTransferStore::new(providers, metadata.clone());

        let error = store
            .download_object_response(repository, &missing_object)
            .await
            .expect_err("missing fallback download should fail");

        assert!(matches!(
            error,
            ServerError::Storage {
                source: StorageError::ObjectNotFound { .. }
            }
        ));
        let stale = metadata
            .lookup_object(
                &repository.id,
                &repository.storage_provider,
                &missing_object,
            )
            .expect("stale metadata should load")
            .expect("stale metadata should exist");
        assert_eq!(
            stale.verification_status,
            MetadataObjectVerificationStatus::Stale
        );
        assert_eq!(
            provider.lookup_calls.load(Ordering::SeqCst),
            1,
            "missing fallback download should not repeat object discovery"
        );
    }

    #[derive(Clone)]
    struct StaticGoogleDriveAccessTokenSource {
        token: GoogleDriveAccessToken,
    }

    impl GoogleDriveAccessTokenSource for StaticGoogleDriveAccessTokenSource {
        fn access_token<'a>(
            &'a self,
            _storage: &'a GoogleDriveStorageConfig,
        ) -> ProviderFuture<'a, StorageResult<GoogleDriveAccessToken>> {
            Box::pin(async { Ok(self.token.clone()) })
        }
    }

    fn static_drive_token_source() -> Arc<dyn GoogleDriveAccessTokenSource> {
        Arc::new(StaticGoogleDriveAccessTokenSource {
            token: GoogleDriveAccessToken::for_test("drive-test-token"),
        })
    }

    #[derive(Clone)]
    struct CountingGoogleDriveAccessTokenSource {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl GoogleDriveAccessTokenSource for CountingGoogleDriveAccessTokenSource {
        fn access_token<'a>(
            &'a self,
            _storage: &'a GoogleDriveStorageConfig,
        ) -> ProviderFuture<'a, StorageResult<GoogleDriveAccessToken>> {
            Box::pin(async move {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(GoogleDriveAccessToken::for_test("cached-access-token"))
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn production_server_builder_exercises_complete_composition() {
        let upstream = Router::new()
            .route(
                "/user",
                get(|| async { Json(serde_json::json!({ "login": "octocat", "id": 42 })) }),
            )
            .route(
                "/repos/{owner}/{repo}",
                get(|| async { Json(serde_json::json!({ "id": 8675309_u64 })) }),
            )
            .route(
                "/repos/{owner}/{repo}/collaborators/{username}/permission",
                get(|| async {
                    Json(serde_json::json!({
                        "permission": "write",
                        "user": { "login": "octocat", "id": 42 }
                    }))
                }),
            )
            .route(
                "/drive/v3/files/root",
                get(|| async {
                    Json(serde_json::json!({
                        "id": "root",
                        "name": "Composition Test Root",
                        "mimeType": "application/vnd.google-apps.folder",
                        "trashed": false,
                        "capabilities": { "canAddChildren": true }
                    }))
                }),
            )
            .route(
                "/drive/v3/files",
                get(|| async { Json(serde_json::json!({ "files": [] })) }),
            );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("composition upstream listener should bind");
        let upstream_address = upstream_listener
            .local_addr()
            .expect("composition upstream address should resolve");
        let upstream_task = tokio::spawn(async move {
            axum::serve(upstream_listener, upstream)
                .await
                .expect("composition upstream should run");
        });
        let upstream_url = format!("http://{upstream_address}");

        let server_port = unused_tcp_port();
        let server_url = format!("http://127.0.0.1:{server_port}");
        let directory = tempfile::tempdir().expect("composition tempdir should be created");
        let config_path = directory.path().join("lfscloud.yml");
        let metadata_path = directory.path().join("state/metadata.sqlite3");
        fs::write(
            &config_path,
            format!(
                r#"
server:
  host: 127.0.0.1
  port: {server_port}
  public_url: {server_url}
  metadata_path: state/metadata.sqlite3
repository_providers:
  github-main:
    type: github
    api_url: {upstream_url}
    personal_access_token: github-pat-composition
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
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
            ),
        )
        .expect("composition config should be written");
        let clients = ServerCompositionClients {
            storage_provider_factory: ServerStorageProviderFactory::with_drive_dependencies(
                static_drive_token_source(),
                GoogleDriveRootValidator::with_client_and_api_base_url(
                    reqwest::Client::new(),
                    &upstream_url,
                )
                .expect("composition Drive root validator should build"),
            ),
            github_user_client: GitHubUserClient::new()
                .expect("composition GitHub user client should build"),
        };
        let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(
            ServerBuilder::new(ServeOptions::new(Some(config_path), None, None))
                .with_clients(clients)
                .with_drive_object_api_base_url(&upstream_url)
                .with_shutdown_signal(async move {
                    let _ = shutdown_receiver.await;
                })
                .serve(),
        );

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("composition HTTP client should build");
        wait_for_server_response(&client, format!("{server_url}/status")).await;
        let login_response = client
            .post(format!("{server_url}/auth/github/pat"))
            .bearer_auth("github-pat-composition")
            .send()
            .await
            .expect("composition PAT login should respond");
        assert_eq!(login_response.status(), reqwest::StatusCode::OK);
        let login_body: serde_json::Value = login_response
            .json()
            .await
            .expect("composition PAT login should return JSON");
        let lfs_token = login_body["lfs_token"]
            .as_str()
            .expect("composition PAT login should issue an LFS token");
        assert_ne!(lfs_token, "github-pat-composition");

        let object_oid = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let basic_auth =
            BASE64_STANDARD.encode(format!("{DEFAULT_GIT_CREDENTIAL_USERNAME}:{lfs_token}"));
        let batch_response = client
            .post(format!(
                "{server_url}/github.com/owner/repo.git/info/lfs/objects/batch"
            ))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Basic {basic_auth}"),
            )
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/vnd.git-lfs+json",
            )
            .json(&serde_json::json!({
                "operation": "upload",
                "transfers": ["basic"],
                "objects": [{ "oid": object_oid, "size": 42 }]
            }))
            .send()
            .await
            .expect("composition LFS batch should respond");
        assert_eq!(batch_response.status(), reqwest::StatusCode::OK);
        let batch_body: serde_json::Value = batch_response
            .json()
            .await
            .expect("composition LFS batch should return JSON");
        assert!(
            batch_body["objects"][0]["actions"]["upload"]["href"]
                .as_str()
                .is_some_and(|href| href.contains(object_oid))
        );

        shutdown_sender
            .send(())
            .expect("composition shutdown receiver should remain active");
        server
            .await
            .expect("composition server task should join")
            .expect("composition server should shut down cleanly");

        let metadata = rusqlite::Connection::open(&metadata_path)
            .expect("composition metadata database should reopen");
        let active_mappings: i64 = metadata
            .query_row(
                "SELECT COUNT(*) FROM repository_mappings WHERE is_active = 1",
                [],
                |row| row.get(0),
            )
            .expect("composition metadata mapping should be queryable");
        assert_eq!(active_mappings, 1);
        let durable_sessions: i64 = metadata
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .expect("composition durable session should be queryable");
        assert_eq!(durable_sessions, 1);
        drop(metadata);
        let metadata_bytes = fs::read(&metadata_path)
            .expect("composition metadata database bytes should be readable");
        assert!(
            !metadata_bytes
                .windows(lfs_token.len())
                .any(|window| window == lfs_token.as_bytes())
        );
        assert!(
            !metadata_bytes
                .windows(b"github-pat-composition".len())
                .any(|window| window == b"github-pat-composition")
        );

        upstream_task.abort();
        let _ = upstream_task.await;
    }

    fn unused_tcp_port() -> u16 {
        StdTcpListener::bind("127.0.0.1:0")
            .expect("ephemeral port probe should bind")
            .local_addr()
            .expect("ephemeral port should resolve")
            .port()
    }

    async fn wait_for_server_response(client: &reqwest::Client, url: String) -> reqwest::Response {
        for _ in 0..100 {
            match client.get(&url).send().await {
                Ok(response) => return response,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }

        panic!("composition server did not bind {url}");
    }

    #[tokio::test]
    async fn google_drive_startup_validation_mints_one_token_and_checks_root() {
        let token_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let root_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/drive/v3/files/{file_id}",
                get({
                    let root_requests = root_requests.clone();
                    move |Path(file_id): Path<String>| {
                        let root_requests = root_requests.clone();
                        async move {
                            root_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            Json(serde_json::json!({
                                "id": file_id,
                                "name": "LFS Cloud Root",
                                "mimeType": "application/vnd.google-apps.folder",
                                "trashed": false,
                                "capabilities": { "canAddChildren": true }
                            }))
                        }
                    }
                }),
            )
            .route(
                "/drive/v3/files",
                get(|| async { Json(serde_json::json!({ "files": [] })) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Drive startup test server should bind");
        let address = listener
            .local_addr()
            .expect("Drive startup test server address should be available");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("Drive startup test server should run");
        });

        let directory = tempfile::tempdir().expect("tempdir should be created");
        let database = Arc::new(
            MetadataDatabase::open(directory.path().join("metadata.sqlite3"))
                .expect("metadata database should open"),
        );
        let config = test_config();
        database
            .sync_config(&config)
            .expect("metadata config should synchronize");
        let repository = config.repositories[0].clone();
        let factory = ServerStorageProviderFactory::with_drive_dependencies(
            Arc::new(CountingGoogleDriveAccessTokenSource {
                calls: token_requests.clone(),
            }),
            GoogleDriveRootValidator::with_client_and_api_base_url(
                reqwest::Client::new(),
                format!("http://{address}"),
            )
            .expect("root validator should build"),
        )
        .with_drive_object_api_base_url(format!("http://{address}"));
        let providers = factory
            .build(&config, database.clone())
            .await
            .expect("configured Drive root should validate before startup");
        let store = StorageProviderTransferStore::new(providers, database);
        let object = LfsObject::new(
            LfsOid::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .expect("test OID should parse"),
            LfsObjectSize::new(42),
        );
        store
            .lookup_object(&repository, &object)
            .await
            .expect("validated token should remain cached for transfers");

        assert_eq!(token_requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(root_requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn google_drive_startup_validation_rejects_unusable_root() {
        let app = Router::new().route(
            "/drive/v3/files/{file_id}",
            get(|| async {
                Json(serde_json::json!({
                    "id": "root",
                    "name": "Read Only Root",
                    "mimeType": "application/vnd.google-apps.folder",
                    "trashed": false,
                    "capabilities": { "canAddChildren": false }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Drive startup test server should bind");
        let address = listener
            .local_addr()
            .expect("Drive startup test server address should be available");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("Drive startup test server should run");
        });

        let directory = tempfile::tempdir().expect("tempdir should be created");
        let database = Arc::new(
            MetadataDatabase::open(directory.path().join("metadata.sqlite3"))
                .expect("metadata database should open"),
        );
        let config = test_config();
        database
            .sync_config(&config)
            .expect("metadata config should synchronize");
        let factory = ServerStorageProviderFactory::with_drive_dependencies(
            static_drive_token_source(),
            GoogleDriveRootValidator::with_client_and_api_base_url(
                reqwest::Client::new(),
                format!("http://{address}"),
            )
            .expect("root validator should build"),
        );

        let error = match factory.build(&config, database).await {
            Ok(_) => panic!("read-only Drive root must prevent server readiness"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            ServerError::Storage {
                source: StorageError::Upstream {
                    status: Some(200),
                    ..
                }
            }
        ));
        assert!(error.to_string().contains("cannot accept child objects"));
        assert!(!error.to_string().contains("startup-access-token"));
    }

    #[tokio::test]
    async fn google_drive_transfer_lookup_uses_and_repairs_stored_backend_ids() {
        let object = LfsObject::new(
            LfsOid::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .expect("test OID should parse"),
            LfsObjectSize::new(42),
        );
        let drive_requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Drive metadata test server should bind");
        let address = listener
            .local_addr()
            .expect("Drive metadata test server address should be available");
        let handler_object = object.clone();
        let app = Router::new()
            .route(
                "/drive/v3/files/{file_id}",
                get({
                    let drive_requests = drive_requests.clone();
                    let object = handler_object;
                    move |Path(file_id): Path<String>| {
                        let drive_requests = drive_requests.clone();
                        let object = object.clone();
                        async move {
                            drive_requests
                                .lock()
                                .expect("Drive metadata requests lock should not poison")
                                .push(format!("get:{file_id}"));
                            if file_id == "root" {
                                return Json(serde_json::json!({
                                    "id": "root",
                                    "name": "LFS Cloud Root",
                                    "mimeType": "application/vnd.google-apps.folder",
                                    "trashed": false,
                                    "capabilities": { "canAddChildren": true }
                                }))
                                .into_response();
                            }
                            if file_id == "drive-file-current" {
                                return Json(serde_json::json!({
                                    "id": "drive-file-current",
                                    "name": format!("sha256-{}-42.lfs", object.oid.as_hex()),
                                    "size": "42",
                                    "parents": ["root"],
                                    "trashed": false,
                                    "appProperties": {
                                        "lfsCloudVersion": "1",
                                        "lfsCloudRepoNamespace": "github-main:owner/repo",
                                        "lfsCloudOid": object.oid.as_hex(),
                                        "lfsCloudSize": "42"
                                    }
                                }))
                                .into_response();
                            }
                            StatusCode::NOT_FOUND.into_response()
                        }
                    }
                }),
            )
            .route(
                "/drive/v3/files",
                get({
                    let drive_requests = drive_requests.clone();
                    let object = object.clone();
                    move |OriginalUri(uri): OriginalUri| {
                        let drive_requests = drive_requests.clone();
                        let object = object.clone();
                        async move {
                            let query = uri.query().unwrap_or_default();
                            drive_requests
                                .lock()
                                .expect("Drive list requests lock should not poison")
                                .push(format!("list:{query}"));
                            let decoded_query = url::form_urlencoded::parse(query.as_bytes())
                                .find_map(|(key, value)| (key == "q").then(|| value.into_owned()))
                                .unwrap_or_default();
                            if decoded_query.contains("lfsCloudFolderKind") {
                                Json(serde_json::json!({ "files": [] }))
                            } else {
                                Json(serde_json::json!({
                                    "files": [{
                                        "id": "drive-file-repaired",
                                        "name": format!("sha256-{}-42.lfs", object.oid.as_hex()),
                                        "size": "42",
                                        "appProperties": {
                                            "lfsCloudVersion": "1",
                                            "lfsCloudRepoNamespace": "github-main:owner/repo",
                                            "lfsCloudOid": object.oid.as_hex(),
                                            "lfsCloudSize": "42"
                                        }
                                    }]
                                }))
                            }
                        }
                    }
                }),
            );
        let server_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("Drive metadata test server should run");
        });
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let database = Arc::new(
            MetadataDatabase::open(directory.path().join("metadata.sqlite3"))
                .expect("metadata database should open"),
        );
        let config = test_config();
        database
            .sync_config(&config)
            .expect("metadata config should synchronize");
        database
            .record_verified_object(
                "github-main:owner/repo",
                "drive-user-a",
                &object,
                "drive-file-current",
                &RepositoryUser::new("github-main", "octocat", Some("user-1".to_owned())),
            )
            .expect("verified object metadata should record");
        let repository = config.repositories[0].clone();
        let factory = ServerStorageProviderFactory::with_drive_dependencies(
            static_drive_token_source(),
            GoogleDriveRootValidator::with_client_and_api_base_url(
                reqwest::Client::new(),
                format!("http://{address}"),
            )
            .expect("root validator should build"),
        )
        .with_drive_object_api_base_url(format!("http://{address}"));
        let providers = factory
            .build(&config, database.clone())
            .await
            .expect("configured Drive provider should build");
        let runtime = providers
            .provider_for(&repository)
            .expect("repository provider should be registered");
        assert!(runtime.backend_id_lookup().is_some());
        assert!(runtime.streaming_download().is_some());
        let store = StorageProviderTransferStore::new(providers, database.clone());
        drive_requests
            .lock()
            .expect("Drive metadata requests lock should not poison")
            .clear();

        let found = store
            .lookup_object(&repository, &object)
            .await
            .expect("metadata-backed lookup should succeed")
            .expect("metadata-backed object should exist");

        assert_eq!(found.backend_id, "drive-file-current");
        assert_eq!(
            drive_requests
                .lock()
                .expect("Drive metadata requests lock should not poison")
                .as_slice(),
            ["get:drive-file-current"]
        );

        database
            .record_verified_object(
                "github-main:owner/repo",
                "drive-user-a",
                &object,
                "drive-file-missing",
                &RepositoryUser::new("github-main", "other", Some("user-2".to_owned())),
            )
            .expect("stale backend fixture should record");
        let repaired = store
            .lookup_object(&repository, &object)
            .await
            .expect("stale backend lookup should repair")
            .expect("replacement Drive object should exist");
        server_task.abort();

        assert_eq!(repaired.backend_id, "drive-file-repaired");
        let repaired_metadata = database
            .lookup_object("github-main:owner/repo", "drive-user-a", &object)
            .expect("repaired metadata lookup should succeed")
            .expect("repaired metadata should exist");
        assert_eq!(repaired_metadata.backend_id, "drive-file-repaired");
        assert_eq!(repaired_metadata.created_by.login, "octocat");
    }

    #[tokio::test]
    async fn google_drive_access_token_cache_single_flights_refreshes() {
        let refreshes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cache = GoogleDriveAccessTokenCache::default();
        let storage = match &test_config().storage_providers["drive-user-a"] {
            StorageProviderConfig::GoogleDrive(storage) => storage.clone(),
        };
        let token_source = CountingGoogleDriveAccessTokenSource {
            calls: refreshes.clone(),
        };

        let (first, second, third) = tokio::join!(
            cache.get_or_refresh(&storage, &token_source),
            cache.get_or_refresh(&storage, &token_source),
            cache.get_or_refresh(&storage, &token_source),
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

    struct SecretBearingBatchAuthorizer {
        message: String,
    }

    impl LfsBatchAuthorizer for SecretBearingBatchAuthorizer {
        fn authorize<'a>(
            &'a self,
            repository: &'a RepositoryMapping,
            _session: &'a LfsSessionRecord,
            _operation: LfsBatchOperation,
        ) -> ProviderFuture<'a, ServerResult<()>> {
            Box::pin(async move {
                Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::Upstream {
                        provider: repository.repo_provider.clone(),
                        status: Some(502),
                        message: SanitizedMessage::new(self.message.clone()),
                    },
                })
            })
        }
    }

    struct SecretBearingTransferStore {
        message: String,
    }

    impl LfsObjectTransferStore for SecretBearingTransferStore {
        fn lookup_object<'a>(
            &'a self,
            repository: &'a RepositoryMapping,
            _object: &'a LfsObject,
        ) -> ProviderFuture<'a, ServerResult<Option<StoredObject>>> {
            Box::pin(async move {
                Err(ServerError::Storage {
                    source: StorageError::Upstream {
                        provider: repository.storage_provider.clone(),
                        status: Some(502),
                        message: SanitizedMessage::new(self.message.clone()),
                    },
                })
            })
        }

        fn upload_object<'a>(
            &'a self,
            _repository: &'a RepositoryMapping,
            _object: &'a LfsObject,
            _source: &'a FsPath,
            _created_by: &'a RepositoryUser,
        ) -> ProviderFuture<'a, ServerResult<StoredObject>> {
            Box::pin(async { unreachable!("secret-bearing store is lookup-only") })
        }

        fn download_object_response<'a>(
            &'a self,
            repository: &'a RepositoryMapping,
            _object: &'a LfsObject,
        ) -> ProviderFuture<'a, ServerResult<StorageDownloadResponse>> {
            Box::pin(async move {
                Err(ServerError::Storage {
                    source: StorageError::Upstream {
                        provider: repository.storage_provider.clone(),
                        status: Some(502),
                        message: SanitizedMessage::new(self.message.clone()),
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
            Box::pin(async { unreachable!("secret-bearing store is lookup-only") })
        }
    }

    #[derive(Clone, Default)]
    struct CapturedTracingWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for CapturedTracingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.bytes
                .lock()
                .expect("captured tracing bytes should not be poisoned")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl CapturedTracingWriter {
        fn rendered(&self) -> String {
            String::from_utf8(
                self.bytes
                    .lock()
                    .expect("captured tracing bytes should not be poisoned")
                    .clone(),
            )
            .expect("tracing output should be UTF-8")
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
                "github-main:owner/repo",
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
            repository: &'a RepositoryMapping,
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

                Ok(lookup_object.filter(|stored_object| {
                    stored_object.repository_namespace == repository.id
                        && stored_object.object == *object
                }))
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
                    repository.id.clone(),
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
        ) -> ProviderFuture<'a, ServerResult<StorageDownloadResponse>> {
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

                Ok(StorageDownloadResponse::new(stored_object, response))
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
        LfsRouterBuilder::new(config, store)
            .with_authorizer(Arc::new(authorizer))
            .with_transfer_store(Arc::new(transfer_store))
            .build_lfs()
    }

    fn test_router_with_transfer_metadata(
        config: ServerConfig,
        store: LocalLfsSessionStore,
        authorizer: RecordingBatchAuthorizer,
        transfer_store: RecordingTransferStore,
        metadata_database: Arc<MetadataDatabase>,
    ) -> Router {
        LfsRouterBuilder::new(config, store)
            .with_authorizer(Arc::new(authorizer))
            .with_transfer_store(Arc::new(transfer_store))
            .with_metadata_database(metadata_database)
            .build_unlimited_lfs_routes()
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
    fn route_resolver_matches_github_identity_without_case_sensitivity() {
        let resolver = LfsRouteResolver::new(&test_config());

        let batch = resolver
            .resolve_path("/GITHUB.COM/Owner/Repo.git/info/lfs/objects/batch")
            .expect("mixed-case GitHub identity should resolve");
        let uppercase_protocol_path = resolver
            .resolve_path("/GITHUB.COM/Owner/Repo.git/INFO/LFS/objects/batch")
            .expect_err("only the GitHub identity should ignore case");

        assert_eq!(batch.repository.id, "github-main:owner/repo");
        assert_eq!(batch.endpoint, LfsRouteEndpoint::Batch);
        assert!(matches!(
            uppercase_protocol_path,
            ServerError::RouteNotConfigured { .. }
        ));
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
        assert!(message.contains("LFS Cloud server running"));
        assert!(message.contains("local:   http://127.0.0.1:8080"));
        assert!(message.contains("network: "));
    }

    #[test]
    fn advertised_urls_bracket_ipv6_literals() {
        let loopback = advertised_server_urls("::1", 8080);

        assert_eq!(loopback.local, "http://[::1]:8080");
        assert_eq!(loopback.network, None);
    }

    #[tokio::test]
    async fn graceful_shutdown_drains_an_active_request() {
        let request_started = Arc::new(Notify::new());
        let release_request = Arc::new(Notify::new());
        let router = Router::new().route(
            "/",
            get({
                let request_started = request_started.clone();
                let release_request = release_request.clone();
                move || {
                    let request_started = request_started.clone();
                    let release_request = release_request.clone();
                    async move {
                        request_started.notify_one();
                        release_request.notified().await;
                        "transfer completed"
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("test listener should expose its address");
        let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(serve_with_graceful_shutdown(
            listener,
            router,
            async move {
                let _ = shutdown_receiver.await;
            },
            Duration::from_secs(1),
        ));
        let request = tokio::spawn(async move {
            reqwest::get(format!("http://{address}/"))
                .await
                .expect("active request should receive a response")
                .text()
                .await
                .expect("active response body should be readable")
        });

        request_started.notified().await;
        shutdown_sender
            .send(())
            .expect("shutdown receiver should remain active");
        tokio::time::timeout(Duration::from_secs(1), async {
            while let Ok(Ok(_)) = tokio::time::timeout(
                Duration::from_millis(100),
                tokio::net::TcpStream::connect(address),
            )
            .await
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown should stop listener admission");
        release_request.notify_one();

        assert_eq!(
            request.await.expect("request task should finish"),
            "transfer completed"
        );
        assert_eq!(
            server
                .await
                .expect("server task should finish")
                .expect("server should shut down cleanly"),
            ServerShutdownOutcome::Drained
        );
    }

    #[tokio::test]
    async fn graceful_shutdown_stops_waiting_at_the_drain_deadline() {
        let request_started = Arc::new(Notify::new());
        let router = Router::new().route(
            "/",
            get({
                let request_started = request_started.clone();
                move || {
                    let request_started = request_started.clone();
                    async move {
                        request_started.notify_one();
                        std::future::pending::<&'static str>().await
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("test listener should expose its address");
        let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(serve_with_graceful_shutdown(
            listener,
            router,
            async move {
                let _ = shutdown_receiver.await;
            },
            Duration::from_millis(50),
        ));
        let request = tokio::spawn(async move {
            let _ = reqwest::get(format!("http://{address}/")).await;
        });

        request_started.notified().await;
        shutdown_sender
            .send(())
            .expect("shutdown receiver should remain active");

        assert_eq!(
            server
                .await
                .expect("server task should finish")
                .expect("deadline expiry should be a controlled shutdown"),
            ServerShutdownOutcome::TimedOut
        );
        request.abort();
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
            let github_pat =
                GitHubPersonalAccessToken::from_secret("github_pat_production_restart")
                    .expect("GitHub PAT should parse");

            store
                .issue_session_with_github_pat(
                    &RepositoryUser::new("github-main", "octocat", Some("42".to_owned())),
                    ["repo"],
                    github_pat,
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
                .github_personal_access_token()
                .expect("GitHub PAT should be restored")
                .as_str(),
            "github_pat_production_restart"
        );
    }

    #[test]
    fn provider_batch_authorizer_keys_adapters_by_config_map_identity() {
        let mut config = test_config();
        let RepositoryProviderConfig::GitHub(provider) = config
            .repository_providers
            .get_mut("github-main")
            .expect("test GitHub provider should exist");
        provider.id = "drifted-embedded-id".to_owned();

        let authorizer = ProviderBatchAuthorizer::from_config(&config);

        assert!(authorizer.providers.contains_key("github-main"));
        assert!(!authorizer.providers.contains_key("drifted-embedded-id"));
    }

    #[test]
    fn production_session_store_names_multiple_provider_consumer() {
        let mut config = test_config();
        let second = match &config.repository_providers["github-main"] {
            RepositoryProviderConfig::GitHub(provider) => {
                let mut provider = provider.clone();
                provider.id = "github-secondary".to_owned();
                RepositoryProviderConfig::GitHub(provider)
            }
        };
        config
            .repository_providers
            .insert("github-secondary".to_owned(), second);
        let database = Arc::new(
            MetadataDatabase::open_in_memory().expect("test metadata database should open"),
        );

        let error = production_session_store(&config, database)
            .expect_err("durable sessions should reject ambiguous GitHub providers");

        assert!(
            matches!(
                error,
                ServerError::InvalidConfiguration { ref message }
                    if message.contains("durable session storage")
            ),
            "unexpected multiple-provider diagnostic: {error}"
        );
    }

    #[tokio::test]
    async fn public_lfs_router_entry_point_mounts_configured_routes() {
        let router = lfs_server_router(test_config());

        let configured_route = router
            .clone()
            .oneshot(lfs_request(
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                None,
            ))
            .await
            .expect("router should respond");
        let unknown_route = router
            .oneshot(lfs_request(
                "/github.com/owner/other.git/info/lfs/objects/batch",
                None,
            ))
            .await
            .expect("router should respond");

        assert_eq!(configured_route.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(unknown_route.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn complete_server_router_mounts_session_and_configured_lfs_routes() {
        let router = server_router_with_sessions(test_config(), LocalLfsSessionStore::new())
            .expect("complete server router should build");

        let session_route = router
            .clone()
            .oneshot(lfs_request_with_method_and_body(
                Method::DELETE,
                LFS_SESSION_REVOKE_PATH,
                None,
                "",
            ))
            .await
            .expect("session route should respond");
        let configured_lfs_route = router
            .clone()
            .oneshot(lfs_request(
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                None,
            ))
            .await
            .expect("configured LFS route should respond");
        let unknown_lfs_route = router
            .oneshot(lfs_request(
                "/github.com/owner/other.git/info/lfs/objects/batch",
                None,
            ))
            .await
            .expect("unknown LFS route should respond");

        assert_eq!(session_route.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(configured_lfs_route.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(unknown_lfs_route.status(), StatusCode::NOT_FOUND);
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
        assert!(challenge_values.contains(&"Bearer realm=\"lfscloud\""));
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
            StatusCode::NOT_FOUND,
            "Git LFS base path is not an operation endpoint; use /objects/batch",
        )
        .await;
    }

    #[tokio::test]
    async fn server_tracing_events_never_render_request_or_provider_secrets() {
        const CREDENTIAL_SECRET: &str = "credential-secret-sentinel";
        const PROVIDER_SECRET: &str = "provider-secret-sentinel";
        const DRIVE_SECRET: &str = "drive-secret-sentinel";
        const URL_SECRET: &str = "url-query-secret-sentinel";
        const HELPER_SECRET: &str = "helper-secret-sentinel";

        let captured = CapturedTracingWriter::default();
        let tracing_writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(move || tracing_writer.clone())
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);

        async {
            let (store, _) = issued_session_token(Duration::from_secs(60));
            let router = test_router_with_authorizer(store, RecordingBatchAuthorizer::allow());
            let response = router
                .oneshot(lfs_request(
                    "/github.com/owner/repo.git/info/lfs/objects/batch",
                    Some(&format!("Bearer {CREDENTIAL_SECRET}")),
                ))
                .await
                .expect("router should respond");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

            let (store, token) = issued_session_token(Duration::from_secs(60));
            let router = test_router_with_authorizer(store, RecordingBatchAuthorizer::allow());
            let response = router
                .oneshot(lfs_request_with_method_and_body(
                    Method::GET,
                    &format!(
                        "/github.com/owner/repo.git/info/lfs/objects/{}?size={URL_SECRET}",
                        "a".repeat(64)
                    ),
                    Some(&format!("Bearer {token}")),
                    "",
                ))
                .await
                .expect("router should respond");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);

            let (store, token) = issued_session_token(Duration::from_secs(60));
            let router = LfsRouterBuilder::new(test_config(), store)
                .with_authorizer(Arc::new(SecretBearingBatchAuthorizer {
                    message: format!("provider diagnostic {PROVIDER_SECRET} {HELPER_SECRET}"),
                }))
                .with_transfer_store(Arc::new(RecordingTransferStore::missing()))
                .build_lfs();
            let response = router
                .oneshot(lfs_request_with_method_and_body(
                    Method::POST,
                    "/github.com/owner/repo.git/info/lfs/objects/batch",
                    Some(&format!("Bearer {token}")),
                    VALID_BATCH_REQUEST,
                ))
                .await
                .expect("router should respond");
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

            let (store, token) = issued_session_token(Duration::from_secs(60));
            let router = LfsRouterBuilder::new(test_config(), store)
                .with_authorizer(Arc::new(RecordingBatchAuthorizer::allow()))
                .with_transfer_store(Arc::new(SecretBearingTransferStore {
                    message: format!("Drive diagnostic {DRIVE_SECRET}"),
                }))
                .build_lfs();
            let response = router
                .oneshot(lfs_request_with_method_and_body(
                    Method::GET,
                    &format!(
                        "/github.com/owner/repo.git/info/lfs/objects/{}?size=42",
                        "a".repeat(64)
                    ),
                    Some(&format!("Bearer {token}")),
                    "",
                ))
                .await
                .expect("router should respond");
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        }
        .with_subscriber(dispatch)
        .await;

        let rendered = captured.rendered();
        assert!(rendered.contains("LFS route request was not authenticated"));
        assert!(rendered.contains("Git LFS download transfer missing or invalid object size"));
        assert!(rendered.contains("Git LFS batch authorization failed"));
        assert!(rendered.contains("Git LFS download transfer storage read failed"));
        for secret in [
            CREDENTIAL_SECRET,
            PROVIDER_SECRET,
            DRIVE_SECRET,
            URL_SECRET,
            HELPER_SECRET,
        ] {
            assert!(
                !rendered.contains(secret),
                "captured tracing output leaked sentinel {secret:?}: {rendered}"
            );
        }
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
        let router = LfsRouterBuilder::new(test_config(), store.clone())
            .with_authorizer(Arc::new(AuthenticationRequiredBatchAuthorizer))
            .with_transfer_store(Arc::new(RecordingTransferStore::missing()))
            .build_lfs();

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
        let expected_authorization = format!(
            "Basic {}",
            BASE64_STANDARD.encode(format!("{DEFAULT_GIT_CREDENTIAL_USERNAME}:{token}"))
        );
        assert_eq!(
            body.objects[0]
                .actions
                .get("download")
                .map(|action| action.href.as_str()),
            Some(
                "http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?size=42"
            )
        );
        assert_eq!(
            body.objects[0]
                .actions
                .get("download")
                .and_then(|action| action.header.get("Authorization")),
            Some(&expected_authorization)
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

    #[test]
    fn storage_permission_denial_maps_to_non_retryable_gateway_errors() {
        let error = ServerError::Storage {
            source: StorageError::PermissionDenied {
                provider: "drive-user-a".to_owned(),
                message: "Drive domain policy denied access".to_owned(),
            },
        };

        assert_eq!(
            super::git_lfs_storage_error_response_parts(&error, false),
            (StatusCode::BAD_GATEWAY, "Git LFS storage access was denied")
        );
        assert_eq!(
            super::lfs_batch_object_error_from_server_error(&error),
            crate::LfsBatchObjectError::new(502, "object storage access was denied")
        );
    }

    #[test]
    fn server_error_log_category_preserves_nested_error_domains() {
        let storage_error = ServerError::Storage {
            source: StorageError::Retryable {
                provider: "drive-user-a".to_owned(),
                message: "temporary Drive failure".to_owned(),
            },
        };
        let repository_error = ServerError::RepositoryProvider {
            source: RepositoryProviderError::AuthenticationRequired {
                provider: "github".to_owned(),
            },
        };

        assert_eq!(
            super::server_error_log_category(&storage_error),
            ErrorCategory::Storage
        );
        assert_eq!(
            super::server_error_log_category(&repository_error),
            ErrorCategory::RepositoryProvider
        );
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
        let expected_authorization = format!(
            "Basic {}",
            BASE64_STANDARD.encode(format!("{DEFAULT_GIT_CREDENTIAL_USERNAME}:{token}"))
        );
        assert_eq!(
            body.objects[0]
                .actions
                .get("upload")
                .map(|action| action.href.as_str()),
            Some(
                "http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?size=42"
            )
        );
        assert_eq!(
            body.objects[0]
                .actions
                .get("upload")
                .and_then(|action| action.header.get("Authorization")),
            Some(&expected_authorization)
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
    async fn object_endpoints_record_successful_and_failed_transfer_lifecycles() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let database_path = directory.path().join("metadata.sqlite3");
        let metadata_database =
            Arc::new(MetadataDatabase::open(&database_path).expect("metadata DB should open"));
        let config = test_config();
        metadata_database
            .sync_config(&config)
            .expect("metadata config should sync");

        let upload_body = b"record this upload";
        let upload_oid = format!("{:x}", Sha256::digest(upload_body));
        let (upload_sessions, upload_token) = issued_session_token(Duration::from_secs(60));
        let upload_router = test_router_with_transfer_metadata(
            config.clone(),
            upload_sessions,
            RecordingBatchAuthorizer::allow(),
            RecordingTransferStore::missing(),
            metadata_database.clone(),
        );
        let upload_response = upload_router
            .oneshot(lfs_request_with_method_and_body(
                Method::PUT,
                &format!(
                    "/github.com/owner/repo.git/info/lfs/objects/{upload_oid}?size={}",
                    upload_body.len()
                ),
                Some(&format!("Bearer {upload_token}")),
                upload_body.to_vec(),
            ))
            .await
            .expect("upload router should respond");
        assert_eq!(upload_response.status(), StatusCode::OK);

        let download_object = LfsObject::new(
            LfsOid::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .expect("test OID should parse"),
            LfsObjectSize::new(42),
        );
        let (download_sessions, download_token) = issued_session_token(Duration::from_secs(60));
        let download_router = test_router_with_transfer_metadata(
            config,
            download_sessions,
            RecordingBatchAuthorizer::allow(),
            RecordingTransferStore::existing_object_with_download_integrity_mismatch(
                StoredObject::new(
                    "drive-user-a",
                    "github-main:owner/repo",
                    download_object,
                    "secret-backend-id-must-not-leak",
                ),
            ),
            metadata_database,
        );
        let download_response = download_router
            .oneshot(lfs_request_with_method_and_body(
                Method::GET,
                "/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?size=42",
                Some(&format!("Bearer {download_token}")),
                Body::empty(),
            ))
            .await
            .expect("download router should respond");
        assert_eq!(download_response.status(), StatusCode::BAD_GATEWAY);

        let connection = rusqlite::Connection::open(&database_path)
            .expect("metadata inspection connection should open");
        let rows = {
            let mut statement = connection
                .prepare(
                    "SELECT operation, status, backend_id, error_category, error_message
                     FROM transfer_attempts
                     ORDER BY id",
                )
                .expect("transfer attempt query should prepare");
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                })
                .expect("transfer attempt query should execute")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("transfer attempt rows should decode")
        };
        assert_eq!(
            rows,
            vec![
                (
                    "upload".to_owned(),
                    "succeeded".to_owned(),
                    Some("drive-file-uploaded".to_owned()),
                    None,
                    None,
                ),
                (
                    "download".to_owned(),
                    "failed".to_owned(),
                    None,
                    Some("storage".to_owned()),
                    Some(
                        "Git LFS storage returned an object that failed integrity validation"
                            .to_owned(),
                    ),
                ),
            ]
        );
        let persisted = format!("{rows:?}");
        assert!(!persisted.contains(download_token.as_str()));
        assert!(!persisted.contains("secret-backend-id-must-not-leak"));
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
            StoredObject::new(
                "drive-user-a",
                "github-main:owner/repo",
                object.clone(),
                "drive-file-existing",
            ),
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
                StoredObject::new(
                    "drive-user-a",
                    "github-main:owner/repo",
                    object,
                    "drive-file-existing",
                ),
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
            StoredObject::new(
                "drive-user-a",
                "github-main:owner/repo",
                object.clone(),
                "drive-file-existing",
            ),
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
            "uploaded Git LFS object did not match the requested OID or size",
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
            "uploaded Git LFS object did not match the requested OID or size",
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
        let coordinator = UploadStagingCoordinator::new(1, 1);
        let reservation = coordinator
            .reserve_with_available_space(10, 5, 15)
            .expect("exact expected size plus headroom should be accepted");
        drop(reservation);

        let error = match coordinator.reserve_with_available_space(10, 5, 14) {
            Ok(_) => panic!("insufficient temp space should be rejected"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            super::UploadStagingError::InsufficientTempSpace {
                required_space: Some(15),
                available_space: Some(14)
            }
        ));

        let overflow = match coordinator.reserve_with_available_space(u64::MAX, 1, u64::MAX) {
            Ok(_) => panic!("overflowing required space should be rejected"),
            Err(error) => error,
        };
        assert!(matches!(
            overflow,
            super::UploadStagingError::InsufficientTempSpace {
                required_space: None,
                available_space: Some(u64::MAX)
            }
        ));
    }

    #[test]
    fn upload_staging_concurrency_is_bounded_globally_and_per_user() {
        let coordinator = UploadStagingCoordinator::new(2, 1);
        let first_user = coordinator
            .try_acquire("github-main:42")
            .expect("first user should acquire a staging slot");

        assert!(matches!(
            coordinator.try_acquire("github-main:42"),
            Err(super::UploadStagingError::ConcurrencyLimit)
        ));

        let second_user = coordinator
            .try_acquire("github-main:84")
            .expect("another user should acquire the second global slot");
        assert!(matches!(
            coordinator.try_acquire("github-main:126"),
            Err(super::UploadStagingError::ConcurrencyLimit)
        ));

        drop(first_user);
        coordinator
            .try_acquire("github-main:126")
            .expect("dropping a lease should release its global slot");
        drop(second_user);
    }

    #[test]
    fn upload_staging_reservations_admit_only_aggregate_capacity() {
        let coordinator = UploadStagingCoordinator::new(3, 3);
        let first = coordinator
            .try_acquire("github-main:42")
            .expect("first upload should acquire concurrency")
            .reserve_with_available_space(60, 10, 100)
            .expect("first upload should reserve capacity");

        let rejected = coordinator
            .try_acquire("github-main:84")
            .expect("second upload should acquire concurrency")
            .reserve_with_available_space(31, 10, 100);
        assert!(matches!(
            rejected,
            Err(super::UploadStagingError::InsufficientTempSpace {
                required_space: Some(101),
                available_space: Some(100)
            })
        ));

        let second = coordinator
            .try_acquire("github-main:84")
            .expect("second upload should reacquire concurrency")
            .reserve_with_available_space(30, 10, 100)
            .expect("exact aggregate capacity should be accepted");
        drop(first);

        coordinator
            .try_acquire("github-main:126")
            .expect("third upload should acquire concurrency")
            .reserve_with_available_space(60, 10, 100)
            .expect("released capacity should be reusable");
        drop(second);
    }

    #[tokio::test]
    async fn concurrent_upload_staging_reservations_are_atomic() {
        let coordinator = UploadStagingCoordinator::new(2, 2);
        let barrier = Arc::new(Barrier::new(3));
        let release = Arc::new(Barrier::new(3));
        let mut tasks = Vec::new();

        for principal in ["github-main:42", "github-main:84"] {
            let coordinator = coordinator.clone();
            let barrier = barrier.clone();
            let release = release.clone();
            tasks.push(tokio::spawn(async move {
                let lease = coordinator
                    .try_acquire(principal)
                    .expect("concurrency should allow both contenders");
                barrier.wait().await;
                let reservation = lease.reserve_with_available_space(60, 0, 100);
                release.wait().await;
                reservation.is_ok()
            }));
        }

        barrier.wait().await;
        release.wait().await;
        let mut admitted = 0;
        for task in tasks {
            admitted += usize::from(task.await.expect("reservation task should join"));
        }

        assert_eq!(admitted, 1, "only one weighted reservation should fit");
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
            Some(1),
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

        let overloaded = super::upload_staging_overloaded_response();
        assert_eq!(
            overloaded.headers().get(RETRY_AFTER),
            Some(&HeaderValue::from_static("1"))
        );
        assert_lfs_json_error(
            overloaded,
            StatusCode::SERVICE_UNAVAILABLE,
            "Git LFS upload staging has reached its concurrency limit",
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
    async fn upload_endpoint_enforces_per_user_staging_limit() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let upload_started = Arc::new(Notify::new());
        let upload_release = Arc::new(Barrier::new(2));
        let transfer_store = RecordingTransferStore::blocking_missing(
            upload_started.clone(),
            upload_release.clone(),
        );
        let mut config = test_config();
        config.server.max_concurrent_uploads = 2;
        config.server.max_concurrent_uploads_per_user = 1;
        let router = test_router_with_config_authorizer_and_transfer_store(
            config,
            store,
            RecordingBatchAuthorizer::allow(),
            transfer_store,
        );
        let first_body = b"first per-user upload";
        let first_oid = format!("{:x}", Sha256::digest(first_body));
        let first_path = format!(
            "/github.com/owner/repo.git/info/lfs/objects/{first_oid}?size={}",
            first_body.len()
        );
        let first_upload_started = upload_started.notified();
        let first = tokio::spawn({
            let router = router.clone();
            let token = token.clone();
            async move {
                router
                    .oneshot(lfs_request_with_method_and_body(
                        Method::PUT,
                        &first_path,
                        Some(&format!("Bearer {token}")),
                        first_body.to_vec(),
                    ))
                    .await
                    .expect("first router response should exist")
            }
        });
        first_upload_started.await;

        let second_body = b"second per-user upload";
        let second_oid = format!("{:x}", Sha256::digest(second_body));
        let second_path = format!(
            "/github.com/owner/repo.git/info/lfs/objects/{second_oid}?size={}",
            second_body.len()
        );
        let overloaded = tokio::time::timeout(
            Duration::from_secs(1),
            router.oneshot(lfs_request_with_method_and_body(
                Method::PUT,
                &second_path,
                Some(&format!("Bearer {token}")),
                second_body.to_vec(),
            )),
        )
        .await;
        first.abort();
        let _ = first.await;
        let overloaded = overloaded
            .expect("competing upload should be rejected without waiting for the active upload")
            .expect("competing router response should exist");

        assert_eq!(
            overloaded.headers().get(RETRY_AFTER),
            Some(&HeaderValue::from_static("1"))
        );
        assert_lfs_json_error(
            overloaded,
            StatusCode::SERVICE_UNAVAILABLE,
            "Git LFS upload staging has reached its concurrency limit",
        )
        .await;
    }

    #[tokio::test]
    async fn upload_endpoint_enforces_global_staging_limit_across_users() {
        let store = LocalLfsSessionStore::new();
        let first_token = issue_session_token(&store, "octocat", "42", Duration::from_secs(60));
        let second_token = issue_session_token(&store, "hubot", "84", Duration::from_secs(60));
        let upload_started = Arc::new(Notify::new());
        let upload_release = Arc::new(Barrier::new(2));
        let transfer_store = RecordingTransferStore::blocking_missing(
            upload_started.clone(),
            upload_release.clone(),
        );
        let mut config = test_config();
        config.server.max_concurrent_uploads = 1;
        config.server.max_concurrent_uploads_per_user = 1;
        let router = test_router_with_config_authorizer_and_transfer_store(
            config,
            store,
            RecordingBatchAuthorizer::allow(),
            transfer_store,
        );
        let first_body = b"first global upload";
        let first_oid = format!("{:x}", Sha256::digest(first_body));
        let first_path = format!(
            "/github.com/owner/repo.git/info/lfs/objects/{first_oid}?size={}",
            first_body.len()
        );
        let first_upload_started = upload_started.notified();
        let first = tokio::spawn({
            let router = router.clone();
            async move {
                router
                    .oneshot(lfs_request_with_method_and_body(
                        Method::PUT,
                        &first_path,
                        Some(&format!("Bearer {first_token}")),
                        first_body.to_vec(),
                    ))
                    .await
                    .expect("first router response should exist")
            }
        });
        first_upload_started.await;

        let second_body = b"second global upload";
        let second_oid = format!("{:x}", Sha256::digest(second_body));
        let second_path = format!(
            "/github.com/owner/repo.git/info/lfs/objects/{second_oid}?size={}",
            second_body.len()
        );
        let overloaded = tokio::time::timeout(
            Duration::from_secs(1),
            router.oneshot(lfs_request_with_method_and_body(
                Method::PUT,
                &second_path,
                Some(&format!("Bearer {second_token}")),
                second_body.to_vec(),
            )),
        )
        .await;
        first.abort();
        let _ = first.await;
        let overloaded = overloaded
            .expect("competing upload should be rejected without waiting for the active upload")
            .expect("competing router response should exist");

        assert_eq!(
            overloaded.headers().get(RETRY_AFTER),
            Some(&HeaderValue::from_static("1"))
        );
        assert_lfs_json_error(
            overloaded,
            StatusCode::SERVICE_UNAVAILABLE,
            "Git LFS upload staging has reached its concurrency limit",
        )
        .await;
    }

    #[tokio::test]
    async fn independent_server_states_serialize_retrying_uploads_durably() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let database_path = directory.path().join("metadata.sqlite3");
        let config = test_config();
        let first_database = Arc::new(
            MetadataDatabase::open(&database_path).expect("first metadata DB should open"),
        );
        first_database
            .sync_config(&config)
            .expect("metadata config should sync");
        let second_database = Arc::new(
            MetadataDatabase::open(&database_path).expect("second metadata DB should open"),
        );
        let upload_started = Arc::new(Notify::new());
        let upload_release = Arc::new(Barrier::new(2));
        let transfer_store = RecordingTransferStore::blocking_missing(
            upload_started.clone(),
            upload_release.clone(),
        );
        let (first_sessions, first_token) = issued_session_token(Duration::from_secs(60));
        let first_router = test_router_with_transfer_metadata(
            config.clone(),
            first_sessions,
            RecordingBatchAuthorizer::allow(),
            transfer_store.clone(),
            first_database,
        );
        let (second_sessions, second_token) = issued_session_token(Duration::from_secs(60));
        let second_router = test_router_with_transfer_metadata(
            config,
            second_sessions,
            RecordingBatchAuthorizer::allow(),
            transfer_store.clone(),
            second_database,
        );
        let body = b"hello from independent lfs cloud states";
        let oid = format!("{:x}", Sha256::digest(body));
        let path = format!(
            "/github.com/owner/repo.git/info/lfs/objects/{oid}?size={}",
            body.len()
        );
        let first_upload_started = upload_started.notified();

        let first = tokio::spawn({
            let path = path.clone();
            async move {
                first_router
                    .oneshot(lfs_request_with_method_and_body(
                        Method::PUT,
                        &path,
                        Some(&format!("Bearer {first_token}")),
                        body.to_vec(),
                    ))
                    .await
                    .expect("first router response should exist")
            }
        });
        first_upload_started.await;

        let second = tokio::spawn({
            let path = path.clone();
            async move {
                second_router
                    .oneshot(lfs_request_with_method_and_body(
                        Method::PUT,
                        &path,
                        Some(&format!("Bearer {second_token}")),
                        body.to_vec(),
                    ))
                    .await
                    .expect("second router response should exist")
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            transfer_store.uploads().is_empty(),
            "the second server state must wait outside the backend upload"
        );
        upload_release.wait().await;

        let first_response = first.await.expect("first upload task should complete");
        let second_response = second.await.expect("second upload task should complete");
        assert_eq!(first_response.status(), StatusCode::OK);
        assert_eq!(second_response.status(), StatusCode::OK);
        assert_eq!(transfer_store.uploads().len(), 1);
    }

    #[tokio::test]
    async fn completed_upload_lock_is_not_retained() {
        let config = test_config();
        let repository = config.repositories[0].clone();
        let state = super::LfsServerState::new(
            config,
            LocalLfsSessionStore::new(),
            Arc::new(RecordingBatchAuthorizer::allow()),
            Arc::new(RecordingTransferStore::missing()),
            BatchBodyGuardrails::default(),
            None,
        );
        let oid = LfsOid::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .expect("test oid should parse");
        let upload_lock = state.upload_lock_for(&repository, &oid);
        let retained_lock = Arc::downgrade(&upload_lock);

        let upload_guard = upload_lock.lock().await;
        drop(upload_guard);
        drop(upload_lock);

        assert!(
            retained_lock.upgrade().is_none(),
            "completed uploads must release their per-object lock allocation"
        );

        let next_oid =
            LfsOid::new("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
                .expect("second test oid should parse");
        let _next_upload_lock = state.upload_lock_for(&repository, &next_oid);
        assert_eq!(
            state
                .upload_locks
                .lock()
                .expect("upload lock map should not be poisoned")
                .len(),
            1,
            "a later upload should purge completed object keys"
        );
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
            "github-main:owner/repo",
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
    async fn batch_route_returns_auth_challenge_when_github_pat_is_missing() {
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
                .any(|value| value.to_str().ok() == Some("Bearer realm=\"lfscloud\""))
        );
    }

    #[tokio::test]
    async fn default_batch_authorizer_checks_github_permissions() {
        let github_api_url = start_permission_server("read").await;
        let config = test_config_with_github_api_url(&github_api_url);
        let store = LocalLfsSessionStore::new();
        let user = RepositoryUser::new("github-main", "octocat", Some("42".to_owned()));
        let github_pat = GitHubPersonalAccessToken::from_secret("github_pat_authorization")
            .expect("token should parse");
        let issued = store
            .issue_session_with_github_pat(&user, ["read:user", "repo"], github_pat)
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
        let github_pat = GitHubPersonalAccessToken::from_secret("github_pat_authorization")
            .expect("token should parse");
        let issued = store
            .issue_session_with_github_pat(&user, ["read:user", "repo"], github_pat)
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
    async fn authenticated_batch_route_times_out_an_idle_body() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let router = LfsRouterBuilder::new(test_config(), store)
            .with_authorizer(Arc::new(RecordingBatchAuthorizer::allow()))
            .with_transfer_store(Arc::new(RecordingTransferStore::default()))
            .with_batch_body_guardrails(BatchBodyGuardrails {
                idle_timeout: Duration::from_millis(10),
                total_timeout: Duration::from_secs(1),
                ..BatchBodyGuardrails::default()
            })
            .build_lfs();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/github.com/owner/repo.git/info/lfs/objects/batch")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from_stream(stream::pending::<
                Result<Bytes, std::io::Error>,
            >()))
            .expect("test request should build");

        let response = router
            .oneshot(request)
            .await
            .expect("router should respond after the idle deadline");

        assert_lfs_json_error(
            response,
            StatusCode::REQUEST_TIMEOUT,
            "Git LFS batch request timed out while reading the request body",
        )
        .await;
    }

    #[tokio::test]
    async fn authenticated_batch_route_enforces_a_total_body_deadline() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let router = LfsRouterBuilder::new(test_config(), store)
            .with_authorizer(Arc::new(RecordingBatchAuthorizer::allow()))
            .with_transfer_store(Arc::new(RecordingTransferStore::default()))
            .with_batch_body_guardrails(BatchBodyGuardrails {
                idle_timeout: Duration::from_millis(20),
                total_timeout: Duration::from_millis(45),
                ..BatchBodyGuardrails::default()
            })
            .build_lfs();
        let slow_drip = stream::unfold((), |_| async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Some((Ok::<_, std::io::Error>(Bytes::from_static(b" ")), ()))
        });
        let request = Request::builder()
            .method(Method::POST)
            .uri("/github.com/owner/repo.git/info/lfs/objects/batch")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from_stream(slow_drip))
            .expect("test request should build");

        let response = router
            .oneshot(request)
            .await
            .expect("router should respond after the total deadline");

        assert_lfs_json_error(
            response,
            StatusCode::REQUEST_TIMEOUT,
            "Git LFS batch request timed out while reading the request body",
        )
        .await;
    }

    #[tokio::test]
    async fn standalone_lfs_request_limit_rejects_overload_without_queueing() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let mut config = test_config();
        config.server.max_concurrent_requests = 1;
        let router = lfs_server_router_with_sessions(config, store);
        assert_request_limit_rejects_overload_without_queueing(router, &token).await;
    }

    #[tokio::test]
    async fn complete_server_request_limit_rejects_overload_without_queueing() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let mut config = test_config();
        config.server.max_concurrent_requests = 1;
        let router = server_router_with_sessions(config, store)
            .expect("complete server router should build");
        assert_request_limit_rejects_overload_without_queueing(router, &token).await;
    }

    async fn assert_request_limit_rejects_overload_without_queueing(router: Router, token: &str) {
        let body_polled = Arc::new(Notify::new());
        let body_polled_in_stream = body_polled.clone();
        let blocked_body = stream::once(async move {
            body_polled_in_stream.notify_one();
            std::future::pending::<Result<Bytes, std::io::Error>>().await
        });
        let blocked_request = Request::builder()
            .method(Method::POST)
            .uri("/github.com/owner/repo.git/info/lfs/objects/batch")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from_stream(blocked_body))
            .expect("test request should build");
        let blocked = tokio::spawn(router.clone().oneshot(blocked_request));
        body_polled.notified().await;

        let overloaded = router
            .oneshot(lfs_request(
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                None,
            ))
            .await
            .expect("overloaded router should respond immediately");
        blocked.abort();

        assert_eq!(
            overloaded
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            Some("1")
        );
        assert_lfs_json_error(
            overloaded,
            StatusCode::SERVICE_UNAVAILABLE,
            "LFS Cloud server has reached its concurrent request limit",
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
    async fn server_router_mounts_github_pat_login_in_pat_mode() {
        let config = ServerConfig::load_from_str(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    personal_access_token: github_pat_configured
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
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
        .expect("PAT server config should load");
        let router = server_router_with_sessions(config, LocalLfsSessionStore::new())
            .expect("PAT server router should build");

        let response = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(crate::GITHUB_PERSONAL_ACCESS_TOKEN_LOGIN_PATH)
                    .body(Body::empty())
                    .expect("PAT login request should build"),
            )
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn server_router_rejects_multiple_github_pat_providers() {
        let config = ServerConfig::load_from_str(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    personal_access_token: github-pat-a
  github-secondary:
    type: github
    api_url: https://api.github.com
    personal_access_token: github-pat-b
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
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
        assert!(
            matches!(
                error,
                ServerError::InvalidConfiguration { ref message }
                    if message.contains("the PAT login router")
            ),
            "unexpected multiple-provider diagnostic: {error}"
        );
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
        let token = issue_session_token(&store, "octocat", "42", ttl);

        (store, token)
    }

    fn issue_session_token(
        store: &LocalLfsSessionStore,
        login: &str,
        stable_id: &str,
        ttl: Duration,
    ) -> String {
        let user = RepositoryUser::new("github-main", login, Some(stable_id.to_owned()));
        let issued = store
            .issue_session_with_ttl(&user, ["read:user"], ttl)
            .expect("session token should be issued");

        issued.token.as_str().to_owned()
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
