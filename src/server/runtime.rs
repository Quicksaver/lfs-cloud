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
    if let Some(secret) = &config.server.session_encryption_secret {
        return LocalLfsSessionStore::open_durable(metadata_database, secret.as_bytes());
    }

    match config.single_github_provider("durable session storage")? {
        None => Ok(LocalLfsSessionStore::new()),
        Some(provider) => match provider.authentication.configured_personal_access_token() {
            Some(legacy_secret) => {
                tracing::warn!(
                    "repository provider personal_access_token is deprecated; configure server.session_encryption_secret"
                );
                LocalLfsSessionStore::open_durable(metadata_database, legacy_secret.as_bytes())
            }
            None => Err(ServerError::InvalidConfiguration {
                message: "server.session_encryption_secret is required when GitHub authentication is configured"
                    .to_owned(),
            }),
        },
    }
}

#[cfg(test)]
macro_rules! server_storage_and_composition_tests {
    () => {
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

    };
}
