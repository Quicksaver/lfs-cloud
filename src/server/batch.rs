async fn handle_parsed_lfs_batch_request(
    repository: RepositoryMapping,
    session: AuthenticatedLfsSession,
    state: &LfsServerState,
    public_url: &str,
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
            match download_batch_response_with_storage_lookup(
                &repository,
                state,
                public_url,
                request,
            )
            .await
            {
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
            match upload_batch_response_with_storage_lookup(
                &repository,
                state,
                public_url,
                request,
            )
            .await
            {
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
    public_url: &str,
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
        public_url,
        repository.route_path(),
        objects,
    ))
}

async fn upload_batch_response_with_storage_lookup(
    repository: &RepositoryMapping,
    state: &LfsServerState,
    public_url: &str,
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
        public_url,
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

#[cfg(test)]
macro_rules! server_routing_and_batch_tests {
    () => {
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

    #[tokio::test]
    async fn wildcard_listener_exposes_the_accepted_local_address_to_requests() {
        async fn local_origin(
            axum::extract::ConnectInfo(address): axum::extract::ConnectInfo<AcceptedSocketAddress>,
        ) -> String {
            address
                .http_origin()
                .expect("accepted TCP sockets should expose a local address")
        }

        let router = Router::new().route("/", get(local_origin));
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", 0))
            .await
            .expect("wildcard test listener should bind");
        let port = listener
            .local_addr()
            .expect("wildcard listener address should be available")
            .port();
        let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(serve_with_graceful_shutdown(
            listener,
            router,
            async move {
                let _ = shutdown_receiver.await;
            },
            Duration::from_secs(1),
        ));

        let origin = reqwest::get(format!("http://127.0.0.1:{port}/"))
            .await
            .expect("wildcard listener should accept loopback traffic")
            .text()
            .await
            .expect("local origin response should be readable");
        assert_eq!(origin, format!("http://127.0.0.1:{port}"));

        shutdown_sender
            .send(())
            .expect("shutdown receiver should remain active");
        server
            .await
            .expect("server task should join")
            .expect("server should shut down cleanly");
    }

    #[tokio::test]
    async fn embedded_router_without_public_url_uses_the_connection_adapter() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let mut config = test_config();
        config.server.public_url = None;
        let router = test_router_with_config_authorizer_and_transfer_store(
            config,
            store,
            RecordingBatchAuthorizer::allow(),
            RecordingTransferStore::missing(),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("embedded router listener should bind");
        let address = listener
            .local_addr()
            .expect("embedded router listener address should be available");
        let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<AcceptedSocketAddress>(),
            )
            .with_graceful_shutdown(async move {
                let _ = shutdown_receiver.await;
            })
            .await
            .expect("embedded router should serve");
        });

        let response = reqwest::Client::new()
            .post(format!(
                "http://{address}/github.com/owner/repo.git/info/lfs/objects/batch"
            ))
            .bearer_auth(token)
            .header(CONTENT_TYPE, "application/vnd.git-lfs+json")
            .body(VALID_UPLOAD_BATCH_REQUEST)
            .send()
            .await
            .expect("embedded batch request should complete");
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = response
            .json()
            .await
            .expect("embedded batch response should be JSON");
        let upload_url = body["objects"][0]["actions"]["upload"]["href"]
            .as_str()
            .expect("missing object should receive an upload action");
        assert!(
            upload_url.starts_with(&format!("http://{address}/")),
            "upload action should use the accepted connection origin: {upload_url}"
        );

        shutdown_sender
            .send(())
            .expect("embedded router shutdown receiver should remain active");
        server.await.expect("embedded router task should join");
    }

    #[test]
    fn server_bind_rejects_invalid_host_before_listener_bind() {
        let error = ServerBind::from_config_and_overrides("bad host", 8080, None, None)
            .expect_err("host with spaces should fail config validation");

        assert!(matches!(error, ServerError::InvalidConfiguration { .. }));
    }

    #[test]
    fn inferred_plaintext_listener_allows_wildcard_bind_by_default() {
        let bind = ServerBind::from_config_and_overrides("0.0.0.0", 8080, None, None)
            .expect("unspecified bind should be structurally valid");
        let mut config = test_config();
        config.server.public_url = None;
        bind.validate_transport(&config)
            .expect("inferred direct-interface URLs should allow the wildcard default");

        let mut secure_public_config = config.clone();
        secure_public_config.server.public_url = Some("https://lfs.example.com".to_owned());
        bind.validate_transport(&secure_public_config)
            .expect("HTTPS through trusted TLS termination should allow a private bind");

        let mut insecure_explicit_config = config;
        insecure_explicit_config.server.public_url =
            Some("http://192.168.1.25:8080".to_owned());
        let error = bind
            .validate_transport(&insecure_explicit_config)
            .expect_err("explicit LAN HTTP URLs should retain the unsafe opt-in");
        assert!(error.to_string().contains("allow_insecure_http"));
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
    fn production_session_store_generates_and_reuses_managed_native_key() {
        #[derive(Default)]
        struct MemoryKeyStore(Mutex<std::collections::BTreeMap<String, Vec<u8>>>);

        impl crate::session_keys::SessionEncryptionKeyStore for MemoryKeyStore {
            fn load(&self, account: &str) -> ServerResult<Option<Vec<u8>>> {
                Ok(self
                    .0
                    .lock()
                    .expect("test key store should lock")
                    .get(account)
                    .cloned())
            }

            fn store(&self, account: &str, secret: &[u8]) -> ServerResult<()> {
                self.0
                    .lock()
                    .expect("test key store should lock")
                    .insert(account.to_owned(), secret.to_vec());
                Ok(())
            }
        }

        let config = test_config_with_github_api_url_and_auth("https://api.github.com", "");
        let database = Arc::new(
            MetadataDatabase::open_in_memory().expect("metadata database should open"),
        );
        let key_store = MemoryKeyStore::default();

        let first = production_session_store_with_key_store(
            &config,
            database.clone(),
            &key_store,
        )
        .expect("first run should generate a native key");
        let issued = first
            .issue_session(&RepositoryUser::new("github-main", "octocat", None), ["repo"])
            .expect("managed durable session should be issued");
        drop(first);

        let reopened = production_session_store_with_key_store(&config, database, &key_store)
            .expect("reopen should reuse the same native key");
        assert!(reopened.verify(&issued.token).is_some());
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

    };
}
#[cfg(test)]
macro_rules! server_authorization_tests {
    () => {
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
    fn server_router_rejects_multiple_github_authentication_providers() {
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
                    if message.contains("the GitHub login router")
            ),
            "unexpected multiple-provider diagnostic: {error}"
        );
    }

    };
}
