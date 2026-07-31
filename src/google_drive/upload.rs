#[derive(Debug)]
enum DriveResumableUploadProgress {
    Complete(StoredObject),
    Incomplete(u64),
}

async fn read_drive_upload_chunk(
    storage: &GoogleDriveStorageConfig,
    source: &Path,
    file: &mut tokio::fs::File,
    offset: u64,
    total_size: u64,
) -> StorageResult<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))
        .await
        .map_err(|error| staged_file_read_error(storage, source, error))?;
    let chunk_len = (total_size - offset).min(GOOGLE_DRIVE_RESUMABLE_UPLOAD_CHUNK_SIZE as u64);
    let mut chunk = vec![0_u8; chunk_len as usize];
    file.read_exact(&mut chunk)
        .await
        .map_err(|error| staged_file_read_error(storage, source, error))?;
    Ok(chunk)
}

fn upload_chunk_progress_stream(
    chunk: Vec<u8>,
) -> (
    impl futures_util::Stream<Item = Result<Bytes, io::Error>> + Send + 'static,
    watch::Receiver<()>,
) {
    let (progress_sender, progress_receiver) = watch::channel(());
    let stream = futures_util::stream::once(async move {
        progress_sender.send_modify(|()| {});
        Ok(Bytes::from(chunk))
    });

    (stream, progress_receiver)
}

async fn parse_drive_resumable_upload_response(
    store: &GoogleDriveObjectStore,
    object: &LfsObject,
    key: &GoogleDriveObjectKey,
    expected_properties: &GoogleDriveObjectProperties,
    response: reqwest::Response,
    maximum_committed_offset: u64,
) -> StorageResult<DriveResumableUploadProgress> {
    let status = response.status();
    if status.as_u16() == 308 {
        let committed_offset = parse_drive_resumable_upload_offset(
            &store.storage,
            response.headers(),
            object.size.bytes(),
            maximum_committed_offset,
        )?;
        return Ok(DriveResumableUploadProgress::Incomplete(committed_offset));
    }

    let body = read_drive_response_body_with_idle_timeout(
        &store.storage,
        &store.token,
        response,
        store.transfer_read_idle_timeout,
    )
    .await?;
    if matches!(status, StatusCode::OK | StatusCode::CREATED) {
        return parse_drive_upload_success(&store.storage, key, expected_properties, status, &body)
            .map(DriveResumableUploadProgress::Complete);
    }

    Err(parse_drive_upload_error(
        &store.storage,
        &store.token,
        object,
        DriveUploadPhase::Transfer,
        status,
        &body,
    ))
}

fn parse_drive_resumable_upload_offset(
    storage: &GoogleDriveStorageConfig,
    headers: &HeaderMap,
    total_size: u64,
    maximum_committed_offset: u64,
) -> StorageResult<u64> {
    let Some(range) = headers.get(RANGE) else {
        return Ok(0);
    };
    let range = range.to_str().map_err(|_| {
        drive_resumable_upload_protocol_error(
            storage,
            "Google Drive resumable upload returned a non-text Range header",
        )
    })?;
    let Some(last_byte) = range.trim().strip_prefix("bytes=0-") else {
        return Err(drive_resumable_upload_protocol_error(
            storage,
            "Google Drive resumable upload returned an invalid Range header",
        ));
    };
    let last_byte = last_byte.parse::<u64>().map_err(|_| {
        drive_resumable_upload_protocol_error(
            storage,
            "Google Drive resumable upload returned an invalid Range header",
        )
    })?;
    let committed_offset = last_byte.checked_add(1).ok_or_else(|| {
        drive_resumable_upload_protocol_error(
            storage,
            "Google Drive resumable upload returned an overflowing Range header",
        )
    })?;
    if committed_offset > maximum_committed_offset
        || committed_offset >= total_size
        || total_size == 0
    {
        return Err(drive_resumable_upload_protocol_error(
            storage,
            "Google Drive resumable upload returned an impossible Range header",
        ));
    }
    Ok(committed_offset)
}

async fn recover_drive_resumable_upload(
    store: &GoogleDriveObjectStore,
    object: &LfsObject,
    key: &GoogleDriveObjectKey,
    expected_properties: &GoogleDriveObjectProperties,
    session_url: &Url,
    recovery_attempts: &mut u32,
    mut last_error: StorageError,
) -> StorageResult<DriveResumableUploadProgress> {
    let total_size = object.size.bytes();
    loop {
        if *recovery_attempts >= GOOGLE_DRIVE_RESUMABLE_UPLOAD_MAX_RECOVERY_ATTEMPTS {
            return Err(last_error);
        }
        sleep_drive_upload_backoff(store, *recovery_attempts).await;
        *recovery_attempts += 1;

        let (progress_sender, progress_receiver) = watch::channel(());
        drop(progress_sender);
        let probe_request = store
            .upload_client
            .put(session_url.clone())
            .header(ACCEPT, "application/json")
            .header(
                AUTHORIZATION,
                store.token.authorization_header_value(&store.storage.id)?,
            )
            .header(CONTENT_LENGTH, "0")
            .header(CONTENT_RANGE, format!("bytes */{total_size}"));
        let probe_result = match send_drive_upload_with_idle_timeout(
            &store.storage,
            &store.token,
            probe_request,
            progress_receiver,
            store.transfer_read_idle_timeout,
        )
        .await
        {
            Ok(response) => {
                parse_drive_resumable_upload_response(
                    store,
                    object,
                    key,
                    expected_properties,
                    response,
                    total_size,
                )
                .await
            }
            Err(error) => Err(error),
        };
        match probe_result {
            Ok(progress) => return Ok(progress),
            Err(error) if is_retryable_storage_error(&error) => last_error = error,
            Err(error) => return Err(error),
        }
    }
}

async fn sleep_drive_upload_backoff(store: &GoogleDriveObjectStore, attempt: u32) {
    let multiplier = 1_u32 << attempt.min(8);
    tokio::time::sleep(
        store
            .upload_retry_initial_backoff
            .saturating_mul(multiplier),
    )
    .await;
}

fn is_retryable_storage_error(error: &StorageError) -> bool {
    matches!(error, StorageError::Retryable { .. })
}

fn drive_resumable_upload_protocol_error(
    storage: &GoogleDriveStorageConfig,
    message: &'static str,
) -> StorageError {
    StorageError::Upstream {
        provider: storage.id.clone(),
        status: Some(308),
        message: SanitizedMessage::new(message),
    }
}

async fn send_drive_upload_with_idle_timeout(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    request: reqwest::RequestBuilder,
    mut progress: watch::Receiver<()>,
    idle_timeout: Duration,
) -> StorageResult<reqwest::Response> {
    let mut request = Box::pin(request.send());
    let idle = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle);
    let mut body_is_streaming = true;

    loop {
        tokio::select! {
            response = &mut request => {
                return response.map_err(|source| drive_transport_error(storage, token, source));
            }
            progress_result = progress.changed(), if body_is_streaming => {
                if progress_result.is_err() {
                    body_is_streaming = false;
                }
                idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
            }
            () = &mut idle => {
                return Err(StorageError::Retryable {
                    provider: storage.id.clone(),
                    message: "Google Drive upload made no progress before the idle timeout"
                        .to_owned(),
                });
            }
        }
    }
}

async fn open_verified_staged_upload_file_on_blocking_thread(
    storage: &GoogleDriveStorageConfig,
    object: &LfsObject,
    source: &Path,
) -> StorageResult<File> {
    let storage = storage.clone();
    let object = object.clone();
    let source = source.to_path_buf();
    let provider = storage.id.clone();

    tokio::task::spawn_blocking(move || {
        open_verified_staged_upload_file(&storage, &object, &source)
    })
    .await
    .map_err(|error| StorageError::Retryable {
        provider,
        message: format!("staged upload file verification task failed: {error}"),
    })?
}

fn open_verified_staged_upload_file(
    storage: &GoogleDriveStorageConfig,
    object: &LfsObject,
    source: &Path,
) -> StorageResult<File> {
    let file =
        File::open(source).map_err(|error| staged_file_read_error(storage, source, error))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut actual_size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];

    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .map_err(|error| staged_file_read_error(storage, source, error))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
        actual_size += bytes_read as u64;
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

    let mut file = reader.into_inner();
    file.seek(SeekFrom::Start(0))
        .map_err(|error| staged_file_read_error(storage, source, error))?;

    Ok(file)
}

fn staged_file_read_error(
    storage: &GoogleDriveStorageConfig,
    path: &Path,
    source: std::io::Error,
) -> StorageError {
    StorageError::StagedFileRead {
        provider: storage.id.clone(),
        path: path.to_path_buf(),
        source,
    }
}

fn parse_drive_upload_error(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    object: &LfsObject,
    phase: DriveUploadPhase,
    status: StatusCode,
    body: &str,
) -> StorageError {
    let diagnostic = drive_error_message(token, body);
    if let Some(error) = classify_common_drive_error(storage, status, &diagnostic) {
        return error;
    }
    if status == StatusCode::CONFLICT {
        return StorageError::Conflict {
            provider: storage.id.clone(),
            oid: object.oid.as_hex().to_owned(),
        };
    }
    if status.as_u16() == 308
        || (phase == DriveUploadPhase::Transfer && status == StatusCode::NOT_FOUND)
    {
        // Retrying the full upload operation starts a fresh resumable session.
        return StorageError::Retryable {
            provider: storage.id.clone(),
            message: diagnostic.message,
        };
    }

    StorageError::Upstream {
        provider: storage.id.clone(),
        status: Some(status.as_u16()),
        message: SanitizedMessage::new(diagnostic.message),
    }
}


#[cfg(test)]
pub(super) mod upload_tests {
    use super::*;

    #[tokio::test]
    async fn object_store_uploads_staged_file_with_resumable_session() {
        let staged_bytes = b"0123456789abcdef0123456789abcdef0123456789";
        let object = lfs_object_for_bytes(staged_bytes);
        let server = DriveUploadServer::start(drive_object_json(
            "drive-file-uploaded",
            object.oid.as_hex(),
            object.size.bytes(),
        ))
        .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), staged_bytes).expect("staged file should be written");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let uploaded = store
            .upload_object(&object, staged_file.path())
            .await
            .expect("resumable upload should succeed");

        assert_eq!(uploaded.provider_id, "drive-user-a");
        assert_eq!(uploaded.object, object);
        assert_eq!(uploaded.backend_id, "drive-file-uploaded");

        let initiate_requests = server.initiate_requests();
        assert_eq!(initiate_requests.len(), 1);
        assert_eq!(
            initiate_requests[0].headers.get(AUTHORIZATION).unwrap(),
            "Bearer access-token"
        );
        assert_eq!(
            initiate_requests[0].headers.get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(
            initiate_requests[0]
                .headers
                .get("x-upload-content-type")
                .unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            initiate_requests[0]
                .headers
                .get("x-upload-content-length")
                .unwrap(),
            &object.size.bytes().to_string()
        );
        let query = form_pairs(&initiate_requests[0].query);
        assert_eq!(query["uploadType"], "resumable");
        assert_eq!(query["supportsAllDrives"], "true");
        assert_eq!(query["fields"], "id,name,size,appProperties");
        let metadata: serde_json::Value =
            serde_json::from_str(&initiate_requests[0].body).expect("metadata should be JSON");
        assert_eq!(
            metadata["name"],
            format!("sha256-{}-{}.lfs", object.oid.as_hex(), object.size.bytes())
        );
        assert_eq!(
            metadata["parents"],
            serde_json::json!([format!("drive-shard-{}", &object.oid.as_hex()[..2])])
        );
        assert_eq!(
            metadata["appProperties"]["lfsCloudRepoNamespace"],
            "github.com/owner/repo"
        );
        assert_eq!(
            metadata["appProperties"]["lfsCloudOid"],
            object.oid.as_hex()
        );
        assert_eq!(
            metadata["appProperties"]["lfsCloudSize"],
            object.size.bytes().to_string()
        );

        let upload_requests = server.upload_requests();
        assert_eq!(upload_requests.len(), 1);
        assert_eq!(upload_requests[0].session_id, "session-1");
        assert_eq!(
            upload_requests[0].headers.get(AUTHORIZATION).unwrap(),
            "Bearer access-token"
        );
        assert_eq!(
            upload_requests[0].headers.get(CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            upload_requests[0].headers.get(CONTENT_LENGTH).unwrap(),
            &object.size.bytes().to_string()
        );
        assert_eq!(upload_requests[0].body, staged_bytes);
    }

    #[tokio::test]
    async fn object_store_times_out_a_stalled_upload_response() {
        let staged_bytes = b"upload response idle timeout bytes";
        let object = lfs_object_for_bytes(staged_bytes);
        let server = DriveUploadServer::start_with_upload_response_delay(
            drive_object_json(
                "drive-file-uploaded",
                object.oid.as_hex(),
                object.size.bytes(),
            ),
            Duration::from_secs(5),
        )
        .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), staged_bytes).expect("staged file should be written");
        let client = reqwest::Client::new();
        let mut store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            client,
            &server.base_url,
        )
        .expect("object store should build");
        // Leave enough time for each request to reach the mock server under
        // emulated ARM64 load while remaining well below its five-second stall.
        store.transfer_read_idle_timeout = Duration::from_millis(250);
        store.upload_retry_initial_backoff = Duration::ZERO;

        let error = store
            .upload_object(&object, staged_file.path())
            .await
            .expect_err("a stalled upload response should time out");

        assert!(matches!(
            error,
            StorageError::Retryable {
                ref provider,
                ref message,
            } if provider == "drive-user-a"
                && message.contains("idle timeout")
                && !message.contains("access-token")
        ));
        assert_eq!(
            server.upload_requests().len(),
            super::GOOGLE_DRIVE_RESUMABLE_UPLOAD_MAX_RECOVERY_ATTEMPTS as usize + 1
        );
    }

    #[tokio::test]
    async fn object_store_storage_provider_trait_uploads_to_drive() {
        let staged_bytes = b"storage provider migration upload bytes";
        let object = lfs_object_for_bytes(staged_bytes);
        let server = DriveUploadServer::start(drive_object_json(
            "drive-file-uploaded",
            object.oid.as_hex(),
            object.size.bytes(),
        ))
        .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), staged_bytes).expect("staged file should be written");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");
        let storage: &dyn StorageProvider = &store;

        let uploaded = storage
            .upload_object("github.com/owner/repo", &object, staged_file.path())
            .await
            .expect("trait-backed Drive upload should succeed");

        assert_eq!(uploaded.provider_id, "drive-user-a");
        assert_eq!(uploaded.repository_namespace, "github.com/owner/repo");
        assert_eq!(uploaded.object, object);
        assert_eq!(uploaded.backend_id, "drive-file-uploaded");
        assert_eq!(server.upload_requests()[0].body, staged_bytes);
    }

    #[tokio::test]
    async fn object_store_storage_provider_trait_rejects_another_repository_namespace() {
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo-a",
            access_token(),
            reqwest::Client::new(),
            "http://127.0.0.1:1",
        )
        .expect("object store should build");
        let storage: &dyn StorageProvider = &store;

        let error = storage
            .object_exists("github.com/owner/repo-b", &lfs_object())
            .await
            .expect_err("repository-scoped Drive store should reject another namespace");

        assert!(matches!(
            error,
            StorageError::RepositoryNamespaceMismatch { ref provider }
                if provider == "drive-user-a"
        ));
    }

    #[tokio::test]
    async fn object_store_streaming_trait_rejects_stored_object_from_another_repository() {
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo-a",
            access_token(),
            reqwest::Client::new(),
            "http://127.0.0.1:1",
        )
        .expect("object store should build");
        let streaming: &dyn StreamingStorageProvider = &store;
        let object = lfs_object();
        let stored_object = StoredObject::new(
            "drive-user-a",
            "github.com/owner/repo-b",
            object.clone(),
            "foreign-drive-file",
        );

        let error = match streaming
            .download_object_response("github.com/owner/repo-a", &object, stored_object)
            .await
        {
            Ok(_) => panic!("foreign repository metadata should not stream"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            StorageError::RepositoryNamespaceMismatch { ref provider }
                if provider == "drive-user-a"
        ));
    }

    #[tokio::test]
    async fn object_store_uploads_large_files_in_drive_aligned_chunks() {
        let staged_bytes = vec![b'x'; super::GOOGLE_DRIVE_RESUMABLE_UPLOAD_CHUNK_SIZE * 2 + 17];
        let object = lfs_object_for_bytes(&staged_bytes);
        let first_chunk_end = super::GOOGLE_DRIVE_RESUMABLE_UPLOAD_CHUNK_SIZE - 1;
        let second_chunk_end = super::GOOGLE_DRIVE_RESUMABLE_UPLOAD_CHUNK_SIZE * 2 - 1;
        let server = DriveUploadServer::start_with_upload_responses(
            vec![
                StubDriveUploadResponse {
                    status: StatusCode::from_u16(308).expect("308 should be valid"),
                    body: String::new(),
                    range: Some(format!("bytes=0-{first_chunk_end}")),
                    delay: Duration::ZERO,
                },
                StubDriveUploadResponse {
                    status: StatusCode::from_u16(308).expect("308 should be valid"),
                    body: String::new(),
                    range: Some(format!("bytes=0-{second_chunk_end}")),
                    delay: Duration::ZERO,
                },
                StubDriveUploadResponse {
                    status: StatusCode::CREATED,
                    body: drive_object_json(
                        "drive-file-uploaded",
                        object.oid.as_hex(),
                        object.size.bytes(),
                    ),
                    range: None,
                    delay: Duration::ZERO,
                },
            ],
            None,
        )
        .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), &staged_bytes).expect("staged file should be written");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        store
            .upload_object(&object, staged_file.path())
            .await
            .expect("chunked upload should succeed");

        let requests = server.upload_requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].body, staged_bytes[..=first_chunk_end]);
        assert_eq!(
            requests[1].body,
            staged_bytes[first_chunk_end + 1..=second_chunk_end]
        );
        assert_eq!(requests[2].body, staged_bytes[second_chunk_end + 1..]);
        assert_eq!(
            requests[0].headers.get("content-range").unwrap(),
            &format!("bytes 0-{first_chunk_end}/{}", staged_bytes.len())
        );
        assert_eq!(
            requests[1].headers.get("content-range").unwrap(),
            &format!(
                "bytes {}-{second_chunk_end}/{}",
                first_chunk_end + 1,
                staged_bytes.len()
            )
        );
        assert_eq!(
            requests[2].headers.get("content-range").unwrap(),
            &format!(
                "bytes {}-{}/{}",
                second_chunk_end + 1,
                staged_bytes.len() - 1,
                staged_bytes.len()
            )
        );
    }

    #[tokio::test]
    async fn object_store_probes_and_resumes_an_interrupted_upload_session() {
        let chunk_size = super::GOOGLE_DRIVE_RESUMABLE_UPLOAD_CHUNK_SIZE;
        let staged_bytes = vec![b'r'; chunk_size * 2];
        let object = lfs_object_for_bytes(&staged_bytes);
        let first_chunk_end = chunk_size - 1;
        let partial_second_chunk_end = chunk_size + chunk_size / 2 - 1;
        let server = DriveUploadServer::start_with_upload_responses(
            vec![
                StubDriveUploadResponse {
                    status: StatusCode::from_u16(308).expect("308 should be valid"),
                    body: String::new(),
                    range: Some(format!("bytes=0-{first_chunk_end}")),
                    delay: Duration::ZERO,
                },
                StubDriveUploadResponse {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    body: r#"{"error":{"message":"retry access-token"}}"#.to_owned(),
                    range: None,
                    delay: Duration::ZERO,
                },
                StubDriveUploadResponse {
                    status: StatusCode::from_u16(308).expect("308 should be valid"),
                    body: String::new(),
                    range: Some(format!("bytes=0-{partial_second_chunk_end}")),
                    delay: Duration::ZERO,
                },
                StubDriveUploadResponse {
                    status: StatusCode::CREATED,
                    body: drive_object_json(
                        "drive-file-uploaded",
                        object.oid.as_hex(),
                        object.size.bytes(),
                    ),
                    range: None,
                    delay: Duration::ZERO,
                },
            ],
            None,
        )
        .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), &staged_bytes).expect("staged file should be written");
        let mut store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");
        store.upload_retry_initial_backoff = Duration::ZERO;

        store
            .upload_object(&object, staged_file.path())
            .await
            .expect("interrupted upload should resume");

        let requests = server.upload_requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].body, staged_bytes[..chunk_size]);
        assert_eq!(requests[1].body, staged_bytes[chunk_size..]);
        assert!(requests[2].body.is_empty());
        assert_eq!(
            requests[2].headers.get("content-range").unwrap(),
            &format!("bytes */{}", staged_bytes.len())
        );
        assert_eq!(
            requests[3].body,
            staged_bytes[partial_second_chunk_end + 1..]
        );
        assert_eq!(
            requests[3].headers.get("content-range").unwrap(),
            &format!(
                "bytes {}-{}/{}",
                partial_second_chunk_end + 1,
                staged_bytes.len() - 1,
                staged_bytes.len()
            )
        );
    }

    #[tokio::test]
    async fn object_store_bounds_resumable_upload_recovery_attempts() {
        let staged_bytes = b"bounded Drive resumable upload retries";
        let object = lfs_object_for_bytes(staged_bytes);
        let server = DriveUploadServer::start_with_upload_response(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":{"message":"still unavailable access-token"}}"#,
        )
        .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), staged_bytes).expect("staged file should be written");
        let mut store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");
        store.upload_retry_initial_backoff = Duration::ZERO;

        let error = store
            .upload_object(&object, staged_file.path())
            .await
            .expect_err("repeated upload failures should stop");

        assert!(matches!(
            error,
            StorageError::Retryable { provider, message }
                if provider == "drive-user-a"
                    && message.contains("still unavailable")
                    && !message.contains("access-token")
        ));
        assert!(
            server.upload_requests().len()
                <= (super::GOOGLE_DRIVE_RESUMABLE_UPLOAD_MAX_RECOVERY_ATTEMPTS as usize * 2) + 1
        );
    }

    #[test]
    fn drive_upload_404_classification_is_phase_aware() {
        let storage = storage_config("google-drive-user-a");
        let token = access_token();
        let object = lfs_object();

        let initiate_error = super::parse_drive_upload_error(
            &storage,
            &token,
            &object,
            super::DriveUploadPhase::Initiate,
            StatusCode::NOT_FOUND,
            r#"{"error":{"message":"missing initiate endpoint access-token"}}"#,
        );
        assert!(matches!(
            initiate_error,
            StorageError::Upstream {
                ref provider,
                status: Some(404),
                ref message,
            } if provider == "drive-user-a"
                && message.as_str().contains("missing initiate endpoint")
                && !message.as_str().contains("access-token")
        ));

        let transfer_error = super::parse_drive_upload_error(
            &storage,
            &token,
            &object,
            super::DriveUploadPhase::Transfer,
            StatusCode::NOT_FOUND,
            r#"{"error":{"message":"expired session access-token"}}"#,
        );
        assert!(matches!(
            transfer_error,
            StorageError::Retryable {
                ref provider,
                ref message,
            } if provider == "drive-user-a"
                && message.contains("expired session")
                && !message.contains("access-token")
        ));
    }

    #[tokio::test]
    async fn object_store_rejects_cross_origin_upload_session_before_put() {
        let staged_bytes = b"0123456789abcdef0123456789abcdef0123456789";
        let object = lfs_object_for_bytes(staged_bytes);
        let server = DriveUploadServer::start_with_session_url(
            "https://attacker.example/upload_session/session-1",
            drive_object_json(
                "drive-file-unused",
                object.oid.as_hex(),
                object.size.bytes(),
            ),
        )
        .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), staged_bytes).expect("staged file should be written");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let error = store
            .upload_object(&object, staged_file.path())
            .await
            .expect_err("cross-origin session URL should fail before upload PUT");

        assert!(matches!(
            error,
            StorageError::Upstream {
                ref provider,
                status: None,
                ref message,
            } if provider == "drive-user-a"
                && message.as_str()
                    == "Google Drive resumable upload session URL must match the configured Drive API origin"
        ));
        assert_eq!(server.initiate_requests().len(), 1);
        assert!(server.upload_requests().is_empty());
    }

    #[tokio::test]
    async fn object_store_rejects_staged_file_mismatch_before_drive_upload() {
        let server =
            DriveUploadServer::start(drive_object_list_json("drive-file-unused", OBJECT_OID, 42))
                .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), [b'x'; 42]).expect("staged file should be written");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let error = store
            .upload_object(&lfs_object(), staged_file.path())
            .await
            .expect_err("hash mismatch should fail before Drive upload starts");

        assert!(matches!(
            error,
            StorageError::IntegrityMismatch {
                ref expected_oid,
                expected_size: 42,
                actual_size: 42,
                ..
            } if expected_oid == OBJECT_OID
        ));
        assert!(server.initiate_requests().is_empty());
        assert!(server.upload_requests().is_empty());
    }

    struct DriveUploadServer {
        base_url: String,
        state: Arc<DriveUploadServerState>,
        server_task: tokio::task::JoinHandle<()>,
    }

    impl DriveUploadServer {
        async fn start(upload_body: impl Into<String>) -> Self {
            Self::start_with_upload_response(StatusCode::CREATED, upload_body).await
        }

        async fn start_with_upload_response_delay(
            upload_body: impl Into<String>,
            upload_response_delay: Duration,
        ) -> Self {
            Self::start_with_upload_response_session_url_and_delay(
                StatusCode::CREATED,
                upload_body,
                None,
                upload_response_delay,
            )
            .await
        }

        async fn start_with_session_url(
            session_url: impl Into<String>,
            upload_body: impl Into<String>,
        ) -> Self {
            Self::start_with_upload_response_and_session_url(
                StatusCode::CREATED,
                upload_body,
                Some(session_url.into()),
            )
            .await
        }

        async fn start_with_upload_response(
            upload_status: StatusCode,
            upload_body: impl Into<String>,
        ) -> Self {
            Self::start_with_upload_response_and_session_url(upload_status, upload_body, None).await
        }

        async fn start_with_upload_response_and_session_url(
            upload_status: StatusCode,
            upload_body: impl Into<String>,
            session_url: Option<String>,
        ) -> Self {
            Self::start_with_upload_response_session_url_and_delay(
                upload_status,
                upload_body,
                session_url,
                Duration::ZERO,
            )
            .await
        }

        async fn start_with_upload_response_session_url_and_delay(
            upload_status: StatusCode,
            upload_body: impl Into<String>,
            session_url: Option<String>,
            upload_response_delay: Duration,
        ) -> Self {
            Self::start_with_upload_responses(
                vec![StubDriveUploadResponse {
                    status: upload_status,
                    body: upload_body.into(),
                    range: None,
                    delay: upload_response_delay,
                }],
                session_url,
            )
            .await
        }

        async fn start_with_upload_responses(
            upload_responses: Vec<StubDriveUploadResponse>,
            session_url: Option<String>,
        ) -> Self {
            assert!(!upload_responses.is_empty());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test Drive upload server should bind");
            let address = listener
                .local_addr()
                .expect("test Drive upload server address should be available");
            let state = Arc::new(DriveUploadServerState {
                session_url: session_url
                    .unwrap_or_else(|| format!("http://{address}/upload_session/session-1")),
                initiate_status: StatusCode::OK,
                initiate_body: String::new(),
                upload_responses,
                initiate_requests: Mutex::new(Vec::new()),
                upload_requests: Mutex::new(Vec::new()),
            });
            let app = Router::new()
                .route("/drive/v3/files", get(drive_upload_shard_list_handler))
                .route(
                    "/upload/drive/v3/files",
                    post(drive_upload_initiate_handler),
                )
                .route(
                    "/upload_session/{session_id}",
                    put(drive_upload_session_handler),
                )
                .with_state(state.clone());
            let server_task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("test Drive upload server should run");
            });

            Self {
                base_url: format!("http://{address}"),
                state,
                server_task,
            }
        }

        fn initiate_requests(&self) -> Vec<CapturedDriveUploadInitiateRequest> {
            self.state
                .initiate_requests
                .lock()
                .expect("test Drive upload initiate requests lock should not poison")
                .clone()
        }

        fn upload_requests(&self) -> Vec<CapturedDriveUploadRequest> {
            self.state
                .upload_requests
                .lock()
                .expect("test Drive upload requests lock should not poison")
                .clone()
        }
    }

    impl Drop for DriveUploadServer {
        fn drop(&mut self) {
            self.server_task.abort();
        }
    }

    struct DriveUploadServerState {
        session_url: String,
        initiate_status: StatusCode,
        initiate_body: String,
        upload_responses: Vec<StubDriveUploadResponse>,
        initiate_requests: Mutex<Vec<CapturedDriveUploadInitiateRequest>>,
        upload_requests: Mutex<Vec<CapturedDriveUploadRequest>>,
    }

    async fn drive_upload_shard_list_handler(uri: Uri) -> Response {
        let query = uri.query().unwrap_or_default();
        let Some(shard_prefix) = shard_prefix_from_query(query) else {
            if is_object_lookup_query(query) {
                return Json(serde_json::json!({ "files": [] })).into_response();
            }
            return (
                StatusCode::BAD_REQUEST,
                [(CONTENT_TYPE, "application/json")],
                r#"{"error":{"message":"missing shard or object query"}}"#.to_owned(),
            )
                .into_response();
        };
        (
            StatusCode::OK,
            [(CONTENT_TYPE, "application/json")],
            format!(
                r#"{{"files":[{{
                    "id":"drive-shard-{shard_prefix}",
                    "name":"lfscloud-sha256-{shard_prefix}",
                    "mimeType":"application/vnd.google-apps.folder",
                    "parents":["drive-root"],
                    "trashed":false,
                    "appProperties":{{
                        "lfsCloudFolderKind":"objectShard",
                        "lfsCloudShard":"{shard_prefix}"
                    }}
                }}]}}"#
            ),
        )
            .into_response()
    }

    pub(super) fn is_shard_folder_query(query: &str) -> bool {
        form_pairs(query)
            .get("q")
            .is_some_and(|query| query.contains("lfsCloudFolderKind"))
    }

    fn is_object_lookup_query(query: &str) -> bool {
        form_pairs(query)
            .get("q")
            .is_some_and(|query| query.contains("lfsCloudOid"))
    }

    fn shard_prefix_from_query(query: &str) -> Option<String> {
        let query = form_pairs(query).remove("q")?;
        let marker = "key='lfsCloudShard' and value='";
        let prefix = query.split_once(marker)?.1.split_once('\'')?.0;
        // Test requests use the exact two-character SHA-256 prefix contract.
        (prefix.len() == 2).then(|| prefix.to_owned())
    }

    #[derive(Clone)]
    struct StubDriveUploadResponse {
        status: StatusCode,
        body: String,
        range: Option<String>,
        delay: Duration,
    }

    #[derive(Clone)]
    struct CapturedDriveUploadInitiateRequest {
        headers: HeaderMap,
        query: String,
        body: String,
    }

    #[derive(Clone)]
    struct CapturedDriveUploadRequest {
        session_id: String,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    async fn drive_upload_initiate_handler(
        State(state): State<Arc<DriveUploadServerState>>,
        headers: HeaderMap,
        uri: Uri,
        body: Bytes,
    ) -> Response {
        state
            .initiate_requests
            .lock()
            .expect("test Drive upload initiate requests lock should not poison")
            .push(CapturedDriveUploadInitiateRequest {
                headers,
                query: uri.query().unwrap_or_default().to_owned(),
                body: String::from_utf8(body.to_vec())
                    .expect("initiate metadata body should be UTF-8"),
            });

        let mut response = (
            state.initiate_status,
            [(CONTENT_TYPE, "application/json")],
            state.initiate_body.clone(),
        )
            .into_response();
        if state.initiate_status.is_success() {
            response.headers_mut().insert(
                LOCATION,
                HeaderValue::from_str(&state.session_url)
                    .expect("session URL should be a valid header"),
            );
        }
        response
    }

    async fn drive_upload_session_handler(
        AxumPath(session_id): AxumPath<String>,
        State(state): State<Arc<DriveUploadServerState>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let request_index = {
            let mut upload_requests = state
                .upload_requests
                .lock()
                .expect("test Drive upload requests lock should not poison");
            let request_index = upload_requests.len();
            upload_requests.push(CapturedDriveUploadRequest {
                session_id,
                headers,
                body: body.to_vec(),
            });
            request_index
        };

        let stub_response = state
            .upload_responses
            .get(request_index)
            .or_else(|| state.upload_responses.last())
            .expect("test Drive upload response sequence should not be empty")
            .clone();
        tokio::time::sleep(stub_response.delay).await;

        let mut response = (
            stub_response.status,
            [(CONTENT_TYPE, "application/json")],
            stub_response.body.clone(),
        )
            .into_response();
        if let Some(range) = &stub_response.range {
            response.headers_mut().insert(
                "range",
                HeaderValue::from_str(range).expect("test upload range should be valid"),
            );
        }
        response
    }
}
