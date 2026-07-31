async fn read_drive_response_body_with_idle_timeout(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    mut response: reqwest::Response,
    idle_timeout: Duration,
) -> StorageResult<String> {
    let mut body = Vec::new();
    while body.len() < MAX_GOOGLE_ERROR_BODY_LEN {
        let chunk = tokio::time::timeout(idle_timeout, response.chunk())
            .await
            .map_err(|_| StorageError::Retryable {
                provider: storage.id.clone(),
                message: "Google Drive upload response stalled before completion".to_owned(),
            })?
            .map_err(|source| drive_transport_error(storage, token, source))?;
        let Some(chunk) = chunk else {
            break;
        };
        let remaining = MAX_GOOGLE_ERROR_BODY_LEN - body.len();
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            break;
        }
        body.extend_from_slice(&chunk);
    }

    Ok(String::from_utf8_lossy(&body).into_owned())
}

async fn verify_drive_download_response_to_tempfile(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    object: &LfsObject,
    download_response: reqwest::Response,
) -> StorageResult<File> {
    let provider = storage.id.clone();
    let temp_file = tempfile::tempfile().map_err(|source| StorageError::Retryable {
        provider: provider.clone(),
        message: format!("Drive download staging file could not be created: {source}"),
    })?;
    let mut temp_file = tokio::fs::File::from_std(temp_file);
    let mut stream = download_response.bytes_stream();
    let mut hasher = Sha256::new();
    let mut actual_size = 0_u64;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|source| drive_transport_error(storage, token, source))?;
        hasher.update(&chunk);
        actual_size += chunk.len() as u64;
        temp_file
            .write_all(&chunk)
            .await
            .map_err(|source| drive_download_staging_file_error(storage, source))?;
    }

    let actual_oid = format!("{:x}", hasher.finalize());
    if actual_oid != object.oid.as_hex() || actual_size != object.size.bytes() {
        return Err(StorageError::IntegrityMismatch {
            expected_oid: object.oid.as_hex().to_owned(),
            expected_size: object.size.bytes(),
            actual_oid,
            actual_size,
        });
    }

    temp_file
        .flush()
        .await
        .map_err(|source| drive_download_staging_file_error(storage, source))?;
    temp_file
        .seek(SeekFrom::Start(0))
        .await
        .map_err(|source| drive_download_staging_file_error(storage, source))?;

    Ok(temp_file.into_std().await)
}

async fn persist_verified_drive_download_file(
    storage: &GoogleDriveStorageConfig,
    mut source: File,
    destination: &Path,
) -> StorageResult<()> {
    let storage = storage.clone();
    let provider = storage.id.clone();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || {
        source.seek(SeekFrom::Start(0)).map_err(|error| {
            drive_download_destination_file_error(&storage, &destination, error)
        })?;
        let destination_parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(destination_parent).map_err(|error| {
            drive_download_destination_file_error(&storage, &destination, error)
        })?;
        let mut destination_file = tempfile::Builder::new()
            .prefix(".lfscloud-download-")
            .tempfile_in(destination_parent)
            .map_err(|error| {
                drive_download_destination_file_error(&storage, &destination, error)
            })?;
        io::copy(&mut source, destination_file.as_file_mut()).map_err(|error| {
            drive_download_destination_file_error(&storage, &destination, error)
        })?;
        destination_file.as_file_mut().sync_all().map_err(|error| {
            drive_download_destination_file_error(&storage, &destination, error)
        })?;
        destination_file.persist(&destination).map_err(|error| {
            drive_download_destination_file_error(&storage, &destination, error.error)
        })?;
        Ok(())
    })
    .await
    .map_err(|error| StorageError::Retryable {
        provider,
        message: format!("Drive download destination write task failed: {error}"),
    })?
}

fn drive_download_staging_file_error(
    storage: &GoogleDriveStorageConfig,
    source: std::io::Error,
) -> StorageError {
    StorageError::Retryable {
        provider: storage.id.clone(),
        message: format!("Drive download staging file could not be written: {source}"),
    }
}

fn drive_download_destination_file_error(
    storage: &GoogleDriveStorageConfig,
    path: &Path,
    source: std::io::Error,
) -> StorageError {
    StorageError::Retryable {
        provider: storage.id.clone(),
        message: format!(
            "Drive download destination file {} could not be written: {source}",
            path.display()
        ),
    }
}

fn parse_drive_download_error(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    object: &LfsObject,
    status: StatusCode,
    body: &str,
) -> StorageError {
    let diagnostic = drive_error_message(token, body);
    if let Some(error) = classify_common_drive_error(storage, status, &diagnostic) {
        return error;
    }
    if status == StatusCode::NOT_FOUND
        || diagnostic.reasons.iter().any(|reason| reason == "notFound")
    {
        return StorageError::ObjectNotFound {
            provider: storage.id.clone(),
            oid: object.oid.as_hex().to_owned(),
            size: object.size.bytes(),
        };
    }
    if status == StatusCode::CONFLICT {
        return StorageError::Conflict {
            provider: storage.id.clone(),
            oid: object.oid.as_hex().to_owned(),
        };
    }

    StorageError::Upstream {
        provider: storage.id.clone(),
        status: Some(status.as_u16()),
        message: SanitizedMessage::new(diagnostic.message),
    }
}


#[cfg(test)]
pub(super) mod download_tests {
    use super::*;
    use super::object_store_tests::CapturedDriveFilesListRequest;
    use super::upload_tests::is_shard_folder_query;

    #[tokio::test]
    async fn object_store_streams_download_response_from_verified_drive_file() {
        let object_bytes = b"0123456789abcdef0123456789abcdef0123456789";
        let object = lfs_object_for_bytes(object_bytes);
        let server = DriveDownloadServer::start(
            drive_object_list_json(
                "drive-file-download",
                object.oid.as_hex(),
                object.size.bytes(),
            ),
            StatusCode::OK,
            object_bytes.to_vec(),
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let download = store
            .download_object_response(&object)
            .await
            .expect("Drive media download should stream");

        assert_eq!(download.stored_object().provider_id, "drive-user-a");
        assert_eq!(download.stored_object().object, object);
        assert_eq!(download.stored_object().backend_id, "drive-file-download");

        let response = download.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            response.headers().get(CONTENT_LENGTH).unwrap(),
            &object.size.bytes().to_string()
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("download body should collect");
        assert_eq!(&body[..], object_bytes);

        let list_requests = server.list_requests();
        assert_eq!(list_requests.len(), 2);
        assert_eq!(
            list_requests[0].headers.get(AUTHORIZATION).unwrap(),
            "Bearer access-token"
        );
        let download_requests = server.download_requests();
        assert_eq!(download_requests.len(), 1);
        assert_eq!(download_requests[0].file_id, "drive-file-download");
        assert_eq!(
            download_requests[0].headers.get(AUTHORIZATION).unwrap(),
            "Bearer access-token"
        );
        assert_eq!(
            download_requests[0].headers.get(ACCEPT_ENCODING).unwrap(),
            "identity"
        );
        let query = form_pairs(&download_requests[0].query);
        assert_eq!(query["alt"], "media");
        assert_eq!(query["supportsAllDrives"], "true");
    }

    #[tokio::test]
    async fn object_store_times_out_a_stalled_download_stream() {
        let object_bytes = b"download stream idle timeout bytes";
        let object = lfs_object_for_bytes(object_bytes);
        let server = DriveDownloadServer::start_with_download_delay(
            drive_object_list_json(
                "drive-file-download",
                object.oid.as_hex(),
                object.size.bytes(),
            ),
            object_bytes.to_vec(),
            Duration::from_secs(5),
        )
        .await;
        let client = reqwest::Client::builder()
            .read_timeout(Duration::from_millis(50))
            .build()
            .expect("test Drive client should build");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            client,
            &server.base_url,
        )
        .expect("object store should build");

        let download = store
            .download_object_response(&object)
            .await
            .expect("valid response metadata should begin proxying");
        let error = to_bytes(download.into_response().into_body(), usize::MAX)
            .await
            .expect_err("a stalled Drive body should terminate the proxy stream");

        assert!(error.to_string().contains("download stream failed"));
    }

    #[tokio::test]
    async fn object_store_storage_provider_trait_downloads_to_path() {
        let object_bytes = b"storage provider migration download bytes";
        let object = lfs_object_for_bytes(object_bytes);
        let server = DriveDownloadServer::start(
            drive_object_list_json(
                "drive-file-download",
                object.oid.as_hex(),
                object.size.bytes(),
            ),
            StatusCode::OK,
            object_bytes.to_vec(),
        )
        .await;
        let destination_root =
            tempfile::tempdir().expect("temporary download root should be created");
        let destination = destination_root.path().join("nested/object.bin");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");
        let storage: &dyn StorageProvider = &store;

        let downloaded = storage
            .download_object("github.com/owner/repo", &object, &destination)
            .await
            .expect("trait-backed Drive download should succeed");

        assert_eq!(downloaded.provider_id, "drive-user-a");
        assert_eq!(downloaded.object, object);
        assert_eq!(downloaded.backend_id, "drive-file-download");
        assert_eq!(
            std::fs::read(&destination).expect("downloaded file should be readable"),
            object_bytes
        );
    }

    #[tokio::test]
    async fn object_store_storage_provider_delete_retains_drive_objects() {
        let object = lfs_object_for_bytes(b"retained migration object bytes");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            "http://127.0.0.1:1",
        )
        .expect("object store should build");
        let storage: &dyn StorageProvider = &store;

        let outcome = storage
            .delete_or_mark_object("github.com/owner/repo", &object)
            .await
            .expect("Drive object cleanup should retain objects for now");

        assert!(matches!(
            outcome,
            StorageDeleteOutcome::Retained { ref reason }
                if reason.contains("deletion is not implemented")
        ));
    }

    #[tokio::test]
    async fn object_store_maps_missing_lookup_to_download_object_not_found() {
        let server =
            DriveDownloadServer::start(r#"{"files":[]}"#, StatusCode::OK, Vec::new()).await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let error = store
            .download_object_response(&lfs_object())
            .await
            .expect_err("missing Drive object should not stream");

        assert!(matches!(
            error,
            StorageError::ObjectNotFound {
                ref provider,
                ref oid,
                size: 42,
            } if provider == "drive-user-a" && oid == OBJECT_OID
        ));
        assert!(server.download_requests().is_empty());
    }

    #[tokio::test]
    async fn object_store_classifies_download_provider_failures() {
        let auth_error = download_error_from(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"missing scope access-token","errors":[{"reason":"insufficientPermissions"}]}}"#,
        )
        .await;
        assert!(matches!(
            auth_error,
            StorageError::AuthenticationRequired { ref provider } if provider == "drive-user-a"
        ));
        assert!(!auth_error.to_string().contains("access-token"));

        let not_found_error = download_error_from(
            StatusCode::NOT_FOUND,
            r#"{"error":{"message":"file missing","errors":[{"reason":"notFound"}]}}"#,
        )
        .await;
        assert!(matches!(
            not_found_error,
            StorageError::ObjectNotFound {
                ref provider,
                ref oid,
                size: 42,
            } if provider == "drive-user-a" && oid == OBJECT_OID
        ));

        let conflict_error =
            download_error_from(StatusCode::CONFLICT, r#"{"error":{"message":"conflict"}}"#).await;
        assert!(matches!(
            conflict_error,
            StorageError::Conflict {
                ref provider,
                ref oid,
            } if provider == "drive-user-a" && oid == OBJECT_OID
        ));

        let quota_error = download_error_from(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"storage full access-token","errors":[{"reason":"storageQuotaExceeded"}]}}"#,
        )
        .await;
        assert!(matches!(
            quota_error,
            StorageError::QuotaExceeded {
                ref provider,
                ref message,
            } if provider == "drive-user-a"
                && message.contains("storage full")
                && !message.contains("access-token")
        ));

        let retryable_error = download_error_from(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"message":"try later access-token","errors":[{"reason":"userRateLimitExceeded"}]}}"#,
        )
        .await;
        assert!(matches!(
            retryable_error,
            StorageError::Retryable {
                ref provider,
                ref message,
            } if provider == "drive-user-a"
                && message.contains("try later")
                && !message.contains("access-token")
        ));
    }

    #[tokio::test]
    async fn object_store_rejects_download_content_length_mismatch() {
        let server = DriveDownloadServer::start(
            drive_object_list_json("drive-file-download", OBJECT_OID, 42),
            StatusCode::OK,
            vec![b'x'; 41],
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let error = store
            .download_object_response(&lfs_object())
            .await
            .expect_err("content-length mismatch should fail before streaming");

        assert!(matches!(
            error,
            StorageError::Upstream {
                ref provider,
                ref message,
                ..
            } if provider == "drive-user-a"
                && message.as_str().contains("Content-Length 41")
                && message.as_str().contains("requested size 42")
        ));
    }

    #[tokio::test]
    async fn object_store_rejects_corrupt_download_stream() {
        let server = DriveDownloadServer::start(
            drive_object_list_json("drive-file-download", OBJECT_OID, 42),
            StatusCode::OK,
            vec![b'x'; 42],
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let download = store
            .download_object_response(&lfs_object())
            .await
            .expect("valid response metadata should begin proxying");
        let error = to_bytes(download.into_response().into_body(), usize::MAX)
            .await
            .expect_err("hash mismatch should terminate the response stream");
        assert!(error.to_string().contains("integrity verification"));
    }

    #[tokio::test]
    async fn object_store_rejects_truncated_download_stream() {
        let server = DriveDownloadServer::start_with_declared_download_content_length(
            drive_object_list_json("drive-file-download", OBJECT_OID, 42),
            StatusCode::OK,
            vec![b'x'; 41],
            Some(42),
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let error = match store.download_object_response(&lfs_object()).await {
            Ok(download) => to_bytes(download.into_response().into_body(), usize::MAX)
                .await
                .expect_err("body stream should reject truncated response")
                .to_string(),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("integrity mismatch")
                || error.contains("error decoding response body")
                || error.contains("Google Drive request failed")
        );
    }

    #[tokio::test]
    async fn object_store_rejects_download_without_content_length() {
        let server = DriveDownloadServer::start_without_download_content_length(
            drive_object_list_json("drive-file-download", OBJECT_OID, 42),
            StatusCode::OK,
            vec![b'x'; 42],
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let error = store
            .download_object_response(&lfs_object())
            .await
            .expect_err("missing content-length should fail before streaming");

        assert!(matches!(
            error,
            StorageError::Upstream {
                ref provider,
                ref message,
                ..
            } if provider == "drive-user-a" && message.as_str().contains("omitted Content-Length")
        ));
    }

    async fn download_error_from(status: StatusCode, body: &'static str) -> StorageError {
        let server = DriveDownloadServer::start(
            drive_object_list_json("drive-file-download", OBJECT_OID, 42),
            status,
            body.as_bytes().to_vec(),
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        store
            .download_object_response(&lfs_object())
            .await
            .expect_err("download should fail for this provider response")
    }

    struct DriveDownloadServer {
        base_url: String,
        state: Arc<DriveDownloadServerState>,
        server_task: tokio::task::JoinHandle<()>,
    }

    impl DriveDownloadServer {
        async fn start(
            list_body: impl Into<String>,
            download_status: StatusCode,
            download_body: Vec<u8>,
        ) -> Self {
            let declared_download_content_length = download_body.len() as u64;
            Self::start_with_declared_download_content_length(
                list_body,
                download_status,
                download_body,
                Some(declared_download_content_length),
            )
            .await
        }

        async fn start_with_download_delay(
            list_body: impl Into<String>,
            download_body: Vec<u8>,
            download_delay: Duration,
        ) -> Self {
            let declared_download_content_length = download_body.len() as u64;
            Self::start_with_declared_download_content_length_and_delay(
                list_body,
                StatusCode::OK,
                download_body,
                Some(declared_download_content_length),
                download_delay,
            )
            .await
        }

        async fn start_without_download_content_length(
            list_body: impl Into<String>,
            download_status: StatusCode,
            download_body: Vec<u8>,
        ) -> Self {
            Self::start_with_declared_download_content_length(
                list_body,
                download_status,
                download_body,
                None,
            )
            .await
        }

        async fn start_with_declared_download_content_length(
            list_body: impl Into<String>,
            download_status: StatusCode,
            download_body: Vec<u8>,
            declared_download_content_length: Option<u64>,
        ) -> Self {
            Self::start_with_declared_download_content_length_and_delay(
                list_body,
                download_status,
                download_body,
                declared_download_content_length,
                Duration::ZERO,
            )
            .await
        }

        async fn start_with_declared_download_content_length_and_delay(
            list_body: impl Into<String>,
            download_status: StatusCode,
            download_body: Vec<u8>,
            declared_download_content_length: Option<u64>,
            download_delay: Duration,
        ) -> Self {
            let state = Arc::new(DriveDownloadServerState {
                list_body: list_body.into(),
                download_status,
                download_body,
                declared_download_content_length,
                download_delay,
                list_requests: Mutex::new(Vec::new()),
                download_requests: Mutex::new(Vec::new()),
            });
            let app = Router::new()
                .route("/drive/v3/files", get(drive_download_list_handler))
                .route(
                    "/drive/v3/files/{file_id}",
                    get(drive_download_media_handler),
                )
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test Drive download server should bind");
            let address = listener
                .local_addr()
                .expect("test Drive download server address should be available");
            let server_task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("test Drive download server should run");
            });

            Self {
                base_url: format!("http://{address}"),
                state,
                server_task,
            }
        }

        fn list_requests(&self) -> Vec<CapturedDriveFilesListRequest> {
            self.state
                .list_requests
                .lock()
                .expect("test Drive download list requests lock should not poison")
                .clone()
        }

        fn download_requests(&self) -> Vec<CapturedDriveDownloadRequest> {
            self.state
                .download_requests
                .lock()
                .expect("test Drive download media requests lock should not poison")
                .clone()
        }
    }

    impl Drop for DriveDownloadServer {
        fn drop(&mut self) {
            self.server_task.abort();
        }
    }

    struct DriveDownloadServerState {
        list_body: String,
        download_status: StatusCode,
        download_body: Vec<u8>,
        declared_download_content_length: Option<u64>,
        download_delay: Duration,
        list_requests: Mutex<Vec<CapturedDriveFilesListRequest>>,
        download_requests: Mutex<Vec<CapturedDriveDownloadRequest>>,
    }

    #[derive(Clone)]
    struct CapturedDriveDownloadRequest {
        file_id: String,
        headers: HeaderMap,
        query: String,
    }

    async fn drive_download_list_handler(
        State(state): State<Arc<DriveDownloadServerState>>,
        headers: HeaderMap,
        uri: Uri,
    ) -> Response {
        state
            .list_requests
            .lock()
            .expect("test Drive download list requests lock should not poison")
            .push(CapturedDriveFilesListRequest {
                headers,
                query: uri.query().unwrap_or_default().to_owned(),
            });

        let body = if is_shard_folder_query(uri.query().unwrap_or_default()) {
            r#"{"files":[]}"#.to_owned()
        } else {
            state.list_body.clone()
        };
        (StatusCode::OK, [(CONTENT_TYPE, "application/json")], body).into_response()
    }

    async fn drive_download_media_handler(
        AxumPath(file_id): AxumPath<String>,
        State(state): State<Arc<DriveDownloadServerState>>,
        headers: HeaderMap,
        uri: Uri,
    ) -> Response {
        state
            .download_requests
            .lock()
            .expect("test Drive download media requests lock should not poison")
            .push(CapturedDriveDownloadRequest {
                file_id,
                headers,
                query: uri.query().unwrap_or_default().to_owned(),
            });

        let response_body = if state.download_delay.is_zero() {
            Body::from_stream(ReaderStream::new(Cursor::new(state.download_body.clone())))
        } else {
            let download_delay = state.download_delay;
            let stream = futures_util::stream::unfold(
                Some(state.download_body.clone()),
                move |body| async move {
                    let body = body?;
                    tokio::time::sleep(download_delay).await;
                    Some((Ok::<_, std::io::Error>(Bytes::from(body)), None))
                },
            );
            Body::from_stream(stream)
        };
        let mut response = Response::builder()
            .status(state.download_status)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(response_body)
            .expect("streaming download response should build");
        if let Some(content_length) = state.declared_download_content_length {
            response.headers_mut().insert(
                CONTENT_LENGTH,
                HeaderValue::from_str(&content_length.to_string())
                    .expect("download body length should be a valid header"),
            );
        }
        response
    }

}
