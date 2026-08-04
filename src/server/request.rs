#[derive(Clone)]
struct LfsServerState {
    routes: LfsRouteResolver,
    session_store: LocalLfsSessionStore,
    public_url: Option<String>,
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

    fn public_url_for_request(&self, request: &Request) -> ServerResult<String> {
        if let Some(public_url) = &self.public_url {
            return Ok(public_url.clone());
        }

        request
            .extensions()
            .get::<ConnectInfo<AcceptedSocketAddress>>()
            .and_then(|ConnectInfo(address)| address.http_origin())
            .ok_or_else(|| ServerError::Internal {
                message: "accepted socket local address is unavailable for inferred public URL"
                    .to_owned(),
            })
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
            let public_url = match state.public_url_for_request(&request) {
                Ok(public_url) => public_url,
                Err(error) => {
                    tracing::error!(
                        error_category = %server_error_log_category(&error),
                        "failed to infer Git LFS action URL origin"
                    );
                    return git_lfs_json_error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "LFS Cloud could not determine its action URL",
                    );
                }
            };
            handle_lfs_batch_request(
                route.repository,
                session,
                method,
                request,
                state,
                &public_url,
            )
            .await
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
    public_url: &str,
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
                handle_parsed_lfs_batch_request(
                    repository,
                    session,
                    state,
                    public_url,
                    batch_request,
                )
                .await
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

#[cfg(test)]
macro_rules! server_transfer_tests {
    () => {
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

    };
}
