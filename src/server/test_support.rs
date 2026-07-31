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
