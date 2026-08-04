/// Builds the Axum router for configured Git LFS paths.
///
/// When `server.public_url` is omitted, serve this router with
/// `Router::into_make_service_with_connect_info::<AcceptedSocketAddress>()` so
/// batch responses can infer action URL origins from accepted connections.
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
///
/// When `server.public_url` is omitted, serve this router with
/// `Router::into_make_service_with_connect_info::<AcceptedSocketAddress>()` so
/// batch responses can infer action URL origins from accepted connections.
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
    let provider = match config.single_github_provider("the GitHub login router")? {
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
