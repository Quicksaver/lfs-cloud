#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DriveUploadPhase {
    Initiate,
    Transfer,
}

/// Looks up repository-scoped LFS objects in Google Drive.
#[derive(Clone)]
pub struct GoogleDriveObjectStore {
    storage: GoogleDriveStorageConfig,
    repo_namespace: String,
    token: GoogleDriveAccessToken,
    metadata_client: Client,
    upload_client: Client,
    download_client: Client,
    api_base_url: Url,
    transfer_read_idle_timeout: Duration,
    upload_retry_initial_backoff: Duration,
}

impl GoogleDriveObjectStore {
    /// Creates an object store using the default Drive API client.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the HTTP client or repository namespace
    /// cannot be initialized.
    pub fn new(
        storage: GoogleDriveStorageConfig,
        repo_namespace: impl AsRef<str>,
        token: GoogleDriveAccessToken,
    ) -> StorageResult<Self> {
        Self::with_clients_and_api_base_url(
            storage,
            repo_namespace,
            token,
            default_google_drive_object_metadata_http_client()?,
            default_google_drive_object_upload_http_client()?,
            default_google_drive_object_download_http_client()?,
            GOOGLE_DRIVE_API_BASE_URL,
        )
    }

    /// Creates an object store with production HTTP clients and a test API URL.
    #[cfg(test)]
    pub(crate) fn with_api_base_url(
        storage: GoogleDriveStorageConfig,
        repo_namespace: impl AsRef<str>,
        token: GoogleDriveAccessToken,
        api_base_url: impl AsRef<str>,
    ) -> StorageResult<Self> {
        Self::with_clients_and_api_base_url(
            storage,
            repo_namespace,
            token,
            default_google_drive_object_metadata_http_client()?,
            default_google_drive_object_upload_http_client()?,
            default_google_drive_object_download_http_client()?,
            api_base_url,
        )
    }

    /// Creates an object store with an explicit HTTP client and API base URL.
    ///
    /// This is primarily useful for tests that replace Google Drive with a
    /// local HTTP server.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the repository namespace or API base URL is
    /// not safe to use.
    pub fn with_client_and_api_base_url(
        storage: GoogleDriveStorageConfig,
        repo_namespace: impl AsRef<str>,
        token: GoogleDriveAccessToken,
        client: Client,
        api_base_url: impl AsRef<str>,
    ) -> StorageResult<Self> {
        Self::with_clients_and_api_base_url(
            storage,
            repo_namespace,
            token,
            client.clone(),
            client.clone(),
            client,
            api_base_url,
        )
    }

    fn with_clients_and_api_base_url(
        storage: GoogleDriveStorageConfig,
        repo_namespace: impl AsRef<str>,
        token: GoogleDriveAccessToken,
        metadata_client: Client,
        upload_client: Client,
        download_client: Client,
        api_base_url: impl AsRef<str>,
    ) -> StorageResult<Self> {
        Ok(Self {
            storage,
            repo_namespace: validate_repo_namespace(repo_namespace.as_ref())?,
            token,
            metadata_client,
            upload_client,
            download_client,
            api_base_url: validate_drive_api_base_url(api_base_url.as_ref())?,
            transfer_read_idle_timeout: GOOGLE_DRIVE_TRANSFER_READ_IDLE_TIMEOUT,
            upload_retry_initial_backoff: GOOGLE_DRIVE_RESUMABLE_UPLOAD_INITIAL_BACKOFF,
        })
    }

    /// Returns this store's configured storage-provider ID.
    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.storage.id
    }

    /// Returns the stable repository namespace bound to this store instance.
    #[must_use]
    pub fn repository_namespace(&self) -> &str {
        &self.repo_namespace
    }

    fn ensure_repository_namespace(&self, repository_namespace: &str) -> StorageResult<()> {
        if self.repo_namespace == repository_namespace {
            Ok(())
        } else {
            Err(StorageError::RepositoryNamespaceMismatch {
                provider: self.storage.id.clone(),
            })
        }
    }

    /// Creates a deterministic Drive object key for the configured repository.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the repository namespace cannot be rendered
    /// safely. This should only happen if the store was constructed with
    /// invalid state.
    pub fn object_key(&self, object: &LfsObject) -> StorageResult<GoogleDriveObjectKey> {
        GoogleDriveObjectKey::new(&self.repo_namespace, object.clone())
    }

    /// Checks whether the object exists under the configured Drive root.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] for backend authentication failures, retryable
    /// Drive failures or malformed Drive responses. Exact duplicate files are
    /// reconciled by selecting the lexicographically smallest Drive file ID.
    pub async fn object_exists(&self, object: &LfsObject) -> StorageResult<bool> {
        Ok(self.lookup_object(object).await?.is_some())
    }

    /// Returns verified backend metadata for an existing Drive object.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] for backend authentication failures, retryable
    /// Drive failures or malformed Drive responses. Exact duplicate files are
    /// reconciled by selecting the lexicographically smallest Drive file ID.
    pub async fn lookup_object(&self, object: &LfsObject) -> StorageResult<Option<StoredObject>> {
        let key = self.object_key(object)?;
        let expected_properties = key.expected_app_properties();
        let mut stored_objects = Vec::new();

        stored_objects.extend(
            self.lookup_objects_in_parent(&self.storage.root_folder_id, &key, &expected_properties)
                .await?,
        );
        for shard_folder_id in self.lookup_shard_folder_ids(&key).await? {
            stored_objects.extend(
                self.lookup_objects_in_parent(&shard_folder_id, &key, &expected_properties)
                    .await?,
            );
        }

        stored_objects.sort_unstable_by(|left, right| left.backend_id.cmp(&right.backend_id));
        Ok(stored_objects.into_iter().next())
    }

    /// Resolves a stored Drive file ID without scanning a folder.
    ///
    /// A missing ID or one that no longer identifies the expected immutable
    /// object returns `None`, allowing the caller to perform indexed discovery
    /// and repair its metadata mapping.
    pub(crate) async fn lookup_object_by_backend_id(
        &self,
        object: &LfsObject,
        backend_id: &str,
    ) -> StorageResult<Option<StoredObject>> {
        let key = self.object_key(object)?;
        let expected_properties = key.expected_app_properties();
        let response = self
            .metadata_client
            .get(drive_object_metadata_url(
                self.api_base_url.clone(),
                backend_id,
            )?)
            .header(ACCEPT, "application/json")
            .header(
                AUTHORIZATION,
                self.token.authorization_header_value(&self.storage.id)?,
            )
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
        let status = response.status();
        let response_body = read_google_response_body(response)
            .await
            .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;

        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(parse_drive_object_lookup_error(
                &self.storage,
                &self.token,
                status,
                &response_body,
            ));
        }
        let file = serde_json::from_str::<GoogleDriveObjectFile>(&response_body).map_err(|_| {
            StorageError::Upstream {
                provider: self.storage.id.clone(),
                status: Some(status.as_u16()),
                message: SanitizedMessage::new(
                    "Google Drive object metadata response was invalid JSON",
                ),
            }
        })?;
        let parents = file.parents.clone();
        let stored_object =
            match verify_drive_object_file(&self.storage, &key, &expected_properties, status, file)
            {
                Ok(stored_object) if stored_object.backend_id == backend_id => stored_object,
                Ok(_) | Err(StorageError::Conflict { .. }) => return Ok(None),
                Err(error) => return Err(error),
            };
        if parents.as_slice() == [self.storage.root_folder_id.as_str()] {
            return Ok(Some(stored_object));
        }
        let [parent_folder_id] = parents.as_slice() else {
            return Ok(None);
        };
        if self
            .shard_folder_id_matches_key(parent_folder_id, &key)
            .await?
        {
            Ok(Some(stored_object))
        } else {
            Ok(None)
        }
    }

    async fn shard_folder_id_matches_key(
        &self,
        folder_id: &str,
        key: &GoogleDriveObjectKey,
    ) -> StorageResult<bool> {
        let response = self
            .metadata_client
            .get(drive_shard_folder_metadata_url(
                self.api_base_url.clone(),
                folder_id,
            )?)
            .header(ACCEPT, "application/json")
            .header(
                AUTHORIZATION,
                self.token.authorization_header_value(&self.storage.id)?,
            )
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
        let status = response.status();
        let response_body = read_google_response_body(response)
            .await
            .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
        if status == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !status.is_success() {
            return Err(parse_drive_object_lookup_error(
                &self.storage,
                &self.token,
                status,
                &response_body,
            ));
        }
        let file = serde_json::from_str::<GoogleDriveObjectFile>(&response_body).map_err(|_| {
            StorageError::Upstream {
                provider: self.storage.id.clone(),
                status: Some(status.as_u16()),
                message: SanitizedMessage::new(
                    "Google Drive shard-folder metadata response was invalid JSON",
                ),
            }
        })?;

        match verify_drive_shard_folder(&self.storage, key, status, file) {
            Ok(returned_id) => Ok(returned_id == folder_id),
            Err(StorageError::Upstream {
                status: Some(200), ..
            }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn lookup_objects_in_parent(
        &self,
        parent_folder_id: &str,
        key: &GoogleDriveObjectKey,
        expected_properties: &GoogleDriveObjectProperties,
    ) -> StorageResult<Vec<StoredObject>> {
        let mut stored_objects = Vec::new();
        let mut page_token = None;
        let mut seen_page_tokens = BTreeSet::new();

        loop {
            let response = self
                .metadata_client
                .get(drive_object_lookup_url(
                    self.api_base_url.clone(),
                    parent_folder_id,
                    key,
                    expected_properties,
                    page_token.as_deref(),
                )?)
                .header(ACCEPT, "application/json")
                .header(
                    AUTHORIZATION,
                    self.token.authorization_header_value(&self.storage.id)?,
                )
                .header(ACCEPT_ENCODING, "identity")
                .send()
                .await
                .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
            let status = response.status();
            let response_body = read_google_response_body(response)
                .await
                .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;

            if !status.is_success() {
                return Err(parse_drive_object_lookup_error(
                    &self.storage,
                    &self.token,
                    status,
                    &response_body,
                ));
            }

            let page = parse_drive_object_lookup_success(
                &self.storage,
                key,
                expected_properties,
                status,
                &response_body,
            )?;
            stored_objects.extend(page.stored_objects);

            let Some(next_page_token) = page.next_page_token else {
                break;
            };
            if !seen_page_tokens.insert(next_page_token.clone()) {
                return Err(StorageError::Retryable {
                    provider: self.storage.id.clone(),
                    message: "Google Drive object lookup repeated a page token".to_owned(),
                });
            }
            page_token = Some(next_page_token);
        }

        Ok(stored_objects)
    }

    async fn lookup_shard_folder_ids(
        &self,
        key: &GoogleDriveObjectKey,
    ) -> StorageResult<Vec<String>> {
        let mut folder_ids = Vec::new();
        let mut page_token = None;
        let mut seen_page_tokens = BTreeSet::new();

        loop {
            let response = self
                .metadata_client
                .get(drive_shard_folder_lookup_url(
                    self.api_base_url.clone(),
                    &self.storage.root_folder_id,
                    key,
                    page_token.as_deref(),
                )?)
                .header(ACCEPT, "application/json")
                .header(
                    AUTHORIZATION,
                    self.token.authorization_header_value(&self.storage.id)?,
                )
                .header(ACCEPT_ENCODING, "identity")
                .send()
                .await
                .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
            let status = response.status();
            let response_body = read_google_response_body(response)
                .await
                .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
            if !status.is_success() {
                return Err(parse_drive_object_lookup_error(
                    &self.storage,
                    &self.token,
                    status,
                    &response_body,
                ));
            }
            let page = parse_drive_shard_folder_lookup_success(
                &self.storage,
                key,
                status,
                &response_body,
            )?;
            folder_ids.extend(page.folder_ids);
            let Some(next_page_token) = page.next_page_token else {
                break;
            };
            if !seen_page_tokens.insert(next_page_token.clone()) {
                return Err(StorageError::Retryable {
                    provider: self.storage.id.clone(),
                    message: "Google Drive shard-folder lookup repeated a page token".to_owned(),
                });
            }
            page_token = Some(next_page_token);
        }

        folder_ids.sort_unstable();
        folder_ids.dedup();
        Ok(folder_ids)
    }

    async fn upload_parent_folder_id(&self, key: &GoogleDriveObjectKey) -> StorageResult<String> {
        if let Some(folder_id) = self.lookup_shard_folder_ids(key).await?.into_iter().next() {
            return Ok(folder_id);
        }

        let response = self
            .metadata_client
            .post(drive_file_create_url(self.api_base_url.clone())?)
            .timeout(GOOGLE_DRIVE_OBJECT_METADATA_TIMEOUT)
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, "application/json")
            .header(
                AUTHORIZATION,
                self.token.authorization_header_value(&self.storage.id)?,
            )
            .json(&drive_shard_folder_metadata(
                &self.storage.root_folder_id,
                key,
            ))
            .send()
            .await
            .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
        let status = response.status();
        let response_body = read_google_response_body(response)
            .await
            .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
        if !status.is_success() {
            return Err(parse_drive_upload_error(
                &self.storage,
                &self.token,
                key.object(),
                DriveUploadPhase::Initiate,
                status,
                &response_body,
            ));
        }
        parse_drive_shard_folder_create_success(&self.storage, key, status, &response_body)
    }

    /// Uploads a staged and locally verified object file through Drive resumable upload.
    ///
    /// The staged file is read before any Drive request so its SHA-256 and
    /// byte count can be checked against the LFS pointer metadata. Uploads use
    /// bounded 256 KiB-aligned chunks. Interrupted transfers query the existing
    /// session's committed offset and continue from Drive's authoritative
    /// `Range` response instead of creating a new backend file.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the staged file cannot be read, its bytes
    /// do not match the requested object identity, Drive cannot create a
    /// resumable session, or the upload completion response is malformed.
    pub async fn upload_object(
        &self,
        object: &LfsObject,
        source: impl AsRef<Path>,
    ) -> StorageResult<StoredObject> {
        let source = source.as_ref().to_path_buf();
        let verified_file =
            Self::open_verified_staged_upload_file(&self.storage, object, &source).await?;
        self.upload_verified_object(object, &source, verified_file)
            .await
    }

    /// Uploads verified bytes unless the exact namespaced object already exists.
    ///
    /// Source verification happens before the existence lookup so callers
    /// cannot use idempotency to bypass the staged-file integrity contract.
    /// That deliberately re-reads the full staged file even when Drive already
    /// contains the object. The lookup and upload are only sequentially
    /// idempotent; concurrent writers to one Drive root must hold the shared
    /// object upload lock across this operation.
    pub(crate) async fn upload_object_idempotent(
        &self,
        object: &LfsObject,
        source: impl AsRef<Path>,
    ) -> StorageResult<StoredObject> {
        let source = source.as_ref().to_path_buf();
        let verified_file =
            Self::open_verified_staged_upload_file(&self.storage, object, &source).await?;
        self.upload_verified_object_idempotent(object, &source, verified_file)
            .await
    }

    /// Opens and verifies a staged upload without requiring a Drive token.
    pub(crate) async fn open_verified_staged_upload_file(
        storage: &GoogleDriveStorageConfig,
        object: &LfsObject,
        source: &Path,
    ) -> StorageResult<File> {
        open_verified_staged_upload_file_on_blocking_thread(storage, object, source).await
    }

    /// Performs the idempotent lookup and upload using an already-verified file.
    pub(crate) async fn upload_verified_object_idempotent(
        &self,
        object: &LfsObject,
        source: &Path,
        verified_file: File,
    ) -> StorageResult<StoredObject> {
        if let Some(stored_object) = self.lookup_object(object).await? {
            return Ok(stored_object);
        }
        self.upload_verified_object(object, source, verified_file)
            .await
    }

    async fn upload_verified_object(
        &self,
        object: &LfsObject,
        source: &Path,
        verified_file: File,
    ) -> StorageResult<StoredObject> {
        let key = self.object_key(object)?;
        let expected_properties = key.expected_app_properties();
        let upload_parent_folder_id = self.upload_parent_folder_id(&key).await?;
        let metadata = drive_upload_metadata(&upload_parent_folder_id, &key);
        let initiate_response = self
            .upload_client
            .post(drive_resumable_upload_url(self.api_base_url.clone())?)
            .timeout(GOOGLE_DRIVE_OBJECT_METADATA_TIMEOUT)
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, "application/json")
            .header(
                AUTHORIZATION,
                self.token.authorization_header_value(&self.storage.id)?,
            )
            .header("X-Upload-Content-Type", GOOGLE_DRIVE_OBJECT_CONTENT_TYPE)
            .header("X-Upload-Content-Length", object.size.bytes().to_string())
            .json(&metadata)
            .send()
            .await
            .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
        let initiate_status = initiate_response.status();
        let session_url = if initiate_status.is_success() {
            let session_url = initiate_response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| StorageError::Upstream {
                    provider: self.storage.id.clone(),
                    status: Some(initiate_status.as_u16()),
                    message: SanitizedMessage::new(
                        "Google Drive resumable upload response did not include Location",
                    ),
                })?;
            validate_drive_resumable_upload_session_url(
                &self.storage,
                &self.api_base_url,
                session_url,
            )?
        } else {
            let response_body = read_google_response_body(initiate_response)
                .await
                .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
            return Err(parse_drive_upload_error(
                &self.storage,
                &self.token,
                object,
                DriveUploadPhase::Initiate,
                initiate_status,
                &response_body,
            ));
        };

        let mut file = tokio::fs::File::from_std(verified_file);
        let total_size = object.size.bytes();
        let mut committed_offset = 0_u64;
        let mut recovery_attempts = 0_u32;

        loop {
            let chunk = read_drive_upload_chunk(
                &self.storage,
                source,
                &mut file,
                committed_offset,
                total_size,
            )
            .await?;
            let chunk_end = committed_offset
                .checked_add(chunk.len() as u64)
                .and_then(|end| end.checked_sub(1));
            let (upload_stream, upload_progress) = upload_chunk_progress_stream(chunk);
            let mut upload_request = self
                .upload_client
                .put(session_url.clone())
                .header(ACCEPT, "application/json")
                .header(
                    AUTHORIZATION,
                    self.token.authorization_header_value(&self.storage.id)?,
                )
                .header(CONTENT_TYPE, GOOGLE_DRIVE_OBJECT_CONTENT_TYPE)
                .header(
                    CONTENT_LENGTH,
                    chunk_end
                        .map_or(0, |end| end - committed_offset + 1)
                        .to_string(),
                );
            if let Some(chunk_end) = chunk_end {
                upload_request = upload_request.header(
                    CONTENT_RANGE,
                    format!("bytes {committed_offset}-{chunk_end}/{total_size}"),
                );
            }
            let upload_result = match send_drive_upload_with_idle_timeout(
                &self.storage,
                &self.token,
                upload_request.body(ReqwestBody::wrap_stream(upload_stream)),
                upload_progress,
                self.transfer_read_idle_timeout,
            )
            .await
            {
                Ok(response) => {
                    parse_drive_resumable_upload_response(
                        self,
                        object,
                        &key,
                        &expected_properties,
                        response,
                        chunk_end.map_or(0, |end| end + 1),
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            let upload_progress = match upload_result {
                Ok(progress) => progress,
                Err(error) if is_retryable_storage_error(&error) => {
                    recover_drive_resumable_upload(
                        self,
                        object,
                        &key,
                        &expected_properties,
                        &session_url,
                        &mut recovery_attempts,
                        error,
                    )
                    .await?
                }
                Err(error) => return Err(error),
            };

            match upload_progress {
                DriveResumableUploadProgress::Complete(stored_object) => {
                    return Ok(stored_object);
                }
                DriveResumableUploadProgress::Incomplete(next_offset) => {
                    if next_offset < committed_offset {
                        return Err(drive_resumable_upload_protocol_error(
                            &self.storage,
                            "Google Drive resumable upload moved its committed offset backwards",
                        ));
                    }
                    if next_offset > committed_offset {
                        committed_offset = next_offset;
                        recovery_attempts = 0;
                        continue;
                    }
                    if recovery_attempts >= GOOGLE_DRIVE_RESUMABLE_UPLOAD_MAX_RECOVERY_ATTEMPTS {
                        return Err(StorageError::Retryable {
                            provider: self.storage.id.clone(),
                            message: "Google Drive resumable upload made no committed progress"
                                .to_owned(),
                        });
                    }
                    sleep_drive_upload_backoff(self, recovery_attempts).await;
                    recovery_attempts += 1;
                }
            }
        }
    }

    /// Downloads a verified Drive object into a local destination path.
    ///
    /// This accepts only Drive files whose private object metadata and streamed
    /// bytes match the requested repository-scoped LFS object before publishing
    /// the bytes to the destination path.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the object is missing, Drive rejects the
    /// media request, the response omits or conflicts with the requested object
    /// size, streamed bytes fail integrity verification, or the destination
    /// path cannot be written.
    pub async fn download_object(
        &self,
        object: &LfsObject,
        destination: impl AsRef<Path>,
    ) -> StorageResult<StoredObject> {
        let destination = destination.as_ref();
        let (stored_object, verified_file) = self.download_object_to_verified_file(object).await?;
        persist_verified_drive_download_file(&self.storage, verified_file, destination).await?;

        Ok(stored_object)
    }

    /// Streams a verified Drive object as an HTTP response.
    ///
    /// This performs a metadata lookup first, so the Drive file ID is accepted
    /// only when private app properties and binary size match the requested
    /// repository-scoped LFS object.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the object is missing, Drive rejects the
    /// media request, the response omits or conflicts with the requested object
    /// size, or the HTTP response cannot be built.
    pub async fn download_object_response(
        &self,
        object: &LfsObject,
    ) -> StorageResult<StorageDownloadResponse> {
        let stored_object =
            self.lookup_object(object)
                .await?
                .ok_or_else(|| StorageError::ObjectNotFound {
                    provider: self.storage.id.clone(),
                    oid: object.oid.as_hex().to_owned(),
                    size: object.size.bytes(),
                })?;
        self.download_object_response_for_stored_object(object, stored_object)
            .await
    }

    /// Streams an object whose Drive file ID was already metadata-verified.
    pub(crate) async fn download_object_response_for_stored_object(
        &self,
        object: &LfsObject,
        stored_object: StoredObject,
    ) -> StorageResult<StorageDownloadResponse> {
        self.ensure_repository_namespace(&stored_object.repository_namespace)?;
        if stored_object.provider_id != self.storage.id || stored_object.object != *object {
            return Err(StorageError::Conflict {
                provider: self.storage.id.clone(),
                oid: object.oid.as_hex().to_owned(),
            });
        }
        let download_response = self
            .download_client
            .get(drive_media_download_url(
                self.api_base_url.clone(),
                &stored_object.backend_id,
            )?)
            .header(ACCEPT, GOOGLE_DRIVE_OBJECT_CONTENT_TYPE)
            .header(
                AUTHORIZATION,
                self.token.authorization_header_value(&self.storage.id)?,
            )
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
        let status = download_response.status();
        if !status.is_success() {
            let response_body = read_google_response_body(download_response)
                .await
                .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
            return Err(parse_drive_download_error(
                &self.storage,
                &self.token,
                object,
                status,
                &response_body,
            ));
        }
        let Some(actual_size) = download_response.content_length() else {
            return Err(drive_upstream_error(
                &self.storage.id,
                "Google Drive download response omitted Content-Length",
            ));
        };
        if actual_size != object.size.bytes() {
            return Err(drive_upstream_error(
                &self.storage.id,
                format!(
                    "Google Drive download response Content-Length {actual_size} did not match requested size {}",
                    object.size.bytes()
                ),
            ));
        }

        let expected_oid = object.oid.as_hex().to_owned();
        let expected_size = object.size.bytes();
        let stream = futures_util::stream::try_unfold(
            (
                download_response.bytes_stream(),
                Sha256::new(),
                0_u64,
                false,
            ),
            move |(mut source, mut hasher, mut actual_size, finished)| {
                let expected_oid = expected_oid.clone();
                async move {
                    if finished {
                        return Ok(None);
                    }
                    match source.next().await {
                        Some(Ok(chunk)) => {
                            hasher.update(&chunk);
                            actual_size = actual_size.saturating_add(chunk.len() as u64);
                            if actual_size > expected_size {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "Google Drive download exceeded the requested object size",
                                ));
                            }
                            Ok(Some((chunk, (source, hasher, actual_size, false))))
                        }
                        Some(Err(_)) => {
                            Err(io::Error::other("Google Drive download stream failed"))
                        }
                        None => {
                            let actual_oid = format!("{:x}", hasher.finalize());
                            if actual_size != expected_size || actual_oid != expected_oid {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "Google Drive download failed LFS integrity verification",
                                ));
                            }
                            Ok(None)
                        }
                    }
                }
            },
        );
        let response_body = AxumBody::from_stream(stream);
        let response = AxumResponse::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, GOOGLE_DRIVE_OBJECT_CONTENT_TYPE)
            .header(CONTENT_LENGTH, object.size.bytes().to_string())
            .body(response_body)
            .map_err(|source| {
                drive_upstream_error(
                    &self.storage.id,
                    format!("Google Drive download response could not be built: {source}"),
                )
            })?;

        Ok(StorageDownloadResponse::new(stored_object, response))
    }

    async fn download_object_to_verified_file(
        &self,
        object: &LfsObject,
    ) -> StorageResult<(StoredObject, File)> {
        let stored_object =
            self.lookup_object(object)
                .await?
                .ok_or_else(|| StorageError::ObjectNotFound {
                    provider: self.storage.id.clone(),
                    oid: object.oid.as_hex().to_owned(),
                    size: object.size.bytes(),
                })?;
        let download_response = self
            .download_client
            .get(drive_media_download_url(
                self.api_base_url.clone(),
                &stored_object.backend_id,
            )?)
            .header(ACCEPT, GOOGLE_DRIVE_OBJECT_CONTENT_TYPE)
            .header(
                AUTHORIZATION,
                self.token.authorization_header_value(&self.storage.id)?,
            )
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
        let download_status = download_response.status();

        if !download_status.is_success() {
            let response_body = read_google_response_body(download_response)
                .await
                .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
            return Err(parse_drive_download_error(
                &self.storage,
                &self.token,
                object,
                download_status,
                &response_body,
            ));
        }

        let Some(actual_size) = download_response.content_length() else {
            return Err(drive_upstream_error(
                &self.storage.id,
                "Google Drive download response omitted Content-Length",
            ));
        };
        if actual_size != object.size.bytes() {
            return Err(drive_upstream_error(
                &self.storage.id,
                format!(
                    "Google Drive download response Content-Length {actual_size} did not match requested size {}",
                    object.size.bytes()
                ),
            ));
        }

        let verified_file = verify_drive_download_response_to_tempfile(
            &self.storage,
            &self.token,
            object,
            download_response,
        )
        .await?;

        Ok((stored_object, verified_file))
    }
}

impl StorageProvider for GoogleDriveObjectStore {
    fn provider_id(&self) -> &str {
        GoogleDriveObjectStore::provider_id(self)
    }

    fn lookup_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<Option<StoredObject>>> {
        Box::pin(async move {
            self.ensure_repository_namespace(repository_namespace)?;
            GoogleDriveObjectStore::lookup_object(self, object).await
        })
    }

    fn upload_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        source: &'a Path,
    ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
        Box::pin(async move {
            self.ensure_repository_namespace(repository_namespace)?;
            self.upload_object_idempotent(object, source).await
        })
    }

    fn download_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        destination: &'a Path,
    ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
        // Delegate through the inherent method so the trait adapter keeps the
        // Drive-specific verification and atomic publication behavior in one path.
        Box::pin(async move {
            self.ensure_repository_namespace(repository_namespace)?;
            GoogleDriveObjectStore::download_object(self, object, destination).await
        })
    }

    fn delete_or_mark_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        _object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<StorageDeleteOutcome>> {
        Box::pin(async move {
            self.ensure_repository_namespace(repository_namespace)?;
            Ok(StorageDeleteOutcome::Retained {
                reason: "Google Drive object deletion is not implemented".to_owned(),
            })
        })
    }
}

impl BackendIdLookup for GoogleDriveObjectStore {
    fn lookup_object_by_backend_id<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        backend_id: &'a str,
    ) -> ProviderFuture<'a, StorageResult<Option<StoredObject>>> {
        Box::pin(async move {
            self.ensure_repository_namespace(repository_namespace)?;
            GoogleDriveObjectStore::lookup_object_by_backend_id(self, object, backend_id).await
        })
    }
}

impl StreamingStorageProvider for GoogleDriveObjectStore {
    fn download_object_response<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        stored_object: StoredObject,
    ) -> ProviderFuture<'a, StorageResult<StorageDownloadResponse>> {
        Box::pin(async move {
            self.ensure_repository_namespace(repository_namespace)?;
            self.download_object_response_for_stored_object(object, stored_object)
                .await
        })
    }
}

impl fmt::Debug for GoogleDriveObjectStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleDriveObjectStore")
            .field("storage", &self.storage)
            .field("repo_namespace", &self.repo_namespace)
            .field("token", &"<redacted>")
            .field("metadata_client", &"<redacted>")
            .field("upload_client", &"<redacted>")
            .field("download_client", &"<redacted>")
            .field("api_base_url", &self.api_base_url)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleDriveFileMetadata {
    id: Option<String>,
    name: Option<String>,
    mime_type: Option<String>,
    #[serde(default)]
    trashed: bool,
    #[serde(default)]
    capabilities: GoogleDriveFileCapabilities,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleDriveFileCapabilities {
    can_add_children: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleDriveFileList {
    #[serde(default)]
    files: Vec<GoogleDriveObjectFile>,
    #[serde(default)]
    next_page_token: Option<String>,
    #[serde(default)]
    incomplete_search: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleDriveObjectFile {
    id: Option<String>,
    name: Option<String>,
    mime_type: Option<String>,
    size: Option<String>,
    #[serde(default)]
    parents: Vec<String>,
    #[serde(default)]
    trashed: bool,
    #[serde(default)]
    app_properties: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct GoogleDriveErrorResponse {
    #[serde(default)]
    error: Option<GoogleDriveErrorBody>,
}

#[derive(Deserialize)]
struct GoogleDriveErrorBody {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    errors: Vec<GoogleDriveErrorDetail>,
}

#[derive(Deserialize)]
struct GoogleDriveErrorDetail {
    #[serde(default)]
    reason: Option<String>,
}

fn parse_drive_object_lookup_success(
    storage: &GoogleDriveStorageConfig,
    key: &GoogleDriveObjectKey,
    expected_properties: &GoogleDriveObjectProperties,
    status: StatusCode,
    body: &str,
) -> StorageResult<VerifiedGoogleDriveObjectPage> {
    let response =
        serde_json::from_str::<GoogleDriveFileList>(body).map_err(|_| StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new("Google Drive object lookup response was invalid JSON"),
        })?;
    if response.incomplete_search {
        return Err(StorageError::Retryable {
            provider: storage.id.clone(),
            message: "Google Drive object lookup returned incomplete search results".to_owned(),
        });
    }
    let next_page_token = response
        .next_page_token
        .map(|token| {
            if token.trim().is_empty() {
                return Err(StorageError::Upstream {
                    provider: storage.id.clone(),
                    status: Some(status.as_u16()),
                    message: SanitizedMessage::new(
                        "Google Drive object lookup returned a blank page token",
                    ),
                });
            }
            Ok(token)
        })
        .transpose()?;

    let stored_objects = response
        .files
        .into_iter()
        .map(|file| verify_drive_object_file(storage, key, expected_properties, status, file))
        .collect::<StorageResult<Vec<_>>>()?;
    Ok(VerifiedGoogleDriveObjectPage {
        stored_objects,
        next_page_token,
    })
}

struct VerifiedGoogleDriveObjectPage {
    stored_objects: Vec<StoredObject>,
    next_page_token: Option<String>,
}

fn parse_drive_shard_folder_lookup_success(
    storage: &GoogleDriveStorageConfig,
    key: &GoogleDriveObjectKey,
    status: StatusCode,
    body: &str,
) -> StorageResult<VerifiedGoogleDriveShardFolderPage> {
    let response =
        serde_json::from_str::<GoogleDriveFileList>(body).map_err(|_| StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(
                "Google Drive shard-folder lookup response was invalid JSON",
            ),
        })?;
    if response.incomplete_search {
        return Err(StorageError::Retryable {
            provider: storage.id.clone(),
            message: "Google Drive shard-folder lookup returned incomplete search results"
                .to_owned(),
        });
    }
    let next_page_token = validated_drive_page_token(
        storage,
        status,
        response.next_page_token,
        "Google Drive shard-folder lookup",
    )?;
    let folder_ids = response
        .files
        .into_iter()
        .map(|file| verify_drive_shard_folder(storage, key, status, file))
        .collect::<StorageResult<Vec<_>>>()?;

    Ok(VerifiedGoogleDriveShardFolderPage {
        folder_ids,
        next_page_token,
    })
}

fn parse_drive_shard_folder_create_success(
    storage: &GoogleDriveStorageConfig,
    key: &GoogleDriveObjectKey,
    status: StatusCode,
    body: &str,
) -> StorageResult<String> {
    let file = serde_json::from_str::<GoogleDriveObjectFile>(body).map_err(|_| {
        StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(
                "Google Drive shard-folder creation response was invalid JSON",
            ),
        }
    })?;
    verify_drive_shard_folder(storage, key, status, file)
}

fn verify_drive_shard_folder(
    storage: &GoogleDriveStorageConfig,
    key: &GoogleDriveObjectKey,
    status: StatusCode,
    file: GoogleDriveObjectFile,
) -> StorageResult<String> {
    let id = file
        .id
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new("Google Drive shard-folder response did not include id"),
        })?;
    let valid = file.name.as_deref() == Some(key.shard_folder_name().as_str())
        && file.mime_type.as_deref() == Some(GOOGLE_DRIVE_FOLDER_MIME_TYPE)
        && !file.trashed
        && file
            .parents
            .iter()
            .any(|parent| parent == &storage.root_folder_id)
        && file
            .app_properties
            .get(GOOGLE_DRIVE_SHARD_KIND_PROPERTY)
            .map(String::as_str)
            == Some(GOOGLE_DRIVE_SHARD_KIND)
        && file
            .app_properties
            .get(GOOGLE_DRIVE_SHARD_PREFIX_PROPERTY)
            .map(String::as_str)
            == Some(key.shard_prefix());
    if !valid {
        return Err(StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(
                "Google Drive shard-folder response did not match its deterministic identity",
            ),
        });
    }

    Ok(id)
}

fn validated_drive_page_token(
    storage: &GoogleDriveStorageConfig,
    status: StatusCode,
    page_token: Option<String>,
    operation: &str,
) -> StorageResult<Option<String>> {
    page_token
        .map(|token| {
            if token.trim().is_empty() {
                return Err(StorageError::Upstream {
                    provider: storage.id.clone(),
                    status: Some(status.as_u16()),
                    message: SanitizedMessage::new(format!(
                        "{operation} returned a blank page token"
                    )),
                });
            }
            Ok(token)
        })
        .transpose()
}

struct VerifiedGoogleDriveShardFolderPage {
    folder_ids: Vec<String>,
    next_page_token: Option<String>,
}

fn verify_drive_object_file(
    storage: &GoogleDriveStorageConfig,
    key: &GoogleDriveObjectKey,
    expected_properties: &GoogleDriveObjectProperties,
    status: StatusCode,
    file: GoogleDriveObjectFile,
) -> StorageResult<StoredObject> {
    let id = file
        .id
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(
                "Google Drive object lookup response did not include id",
            ),
        })?;
    if file.name.as_deref() != Some(&key.file_name()) {
        return Err(StorageError::Conflict {
            provider: storage.id.clone(),
            oid: key.object.oid.as_hex().to_owned(),
        });
    }
    if file.trashed {
        return Err(StorageError::Conflict {
            provider: storage.id.clone(),
            oid: key.object.oid.as_hex().to_owned(),
        });
    }
    for (property, expected) in expected_properties.pairs() {
        if file.app_properties.get(property).map(String::as_str) != Some(expected) {
            return Err(StorageError::Conflict {
                provider: storage.id.clone(),
                oid: key.object.oid.as_hex().to_owned(),
            });
        }
    }
    let actual_size = file
        .size
        .as_deref()
        .ok_or_else(|| StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(
                "Google Drive object lookup response did not include size",
            ),
        })?
        .parse::<u64>()
        .map_err(|_| StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new("Google Drive object lookup response size was invalid"),
        })?;
    if actual_size != key.object.size.bytes() {
        return Err(StorageError::IntegrityMismatch {
            expected_oid: key.object.oid.as_hex().to_owned(),
            expected_size: key.object.size.bytes(),
            actual_oid: key.object.oid.as_hex().to_owned(),
            actual_size,
        });
    }

    Ok(StoredObject::new(
        storage.id.clone(),
        key.repo_namespace.clone(),
        key.object.clone(),
        id,
    ))
}

fn parse_drive_upload_success(
    storage: &GoogleDriveStorageConfig,
    key: &GoogleDriveObjectKey,
    expected_properties: &GoogleDriveObjectProperties,
    status: StatusCode,
    body: &str,
) -> StorageResult<StoredObject> {
    let file = serde_json::from_str::<GoogleDriveObjectFile>(body).map_err(|_| {
        StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(
                "Google Drive upload completion response was invalid JSON",
            ),
        }
    })?;

    verify_drive_object_file(storage, key, expected_properties, status, file)
}

fn parse_drive_object_lookup_error(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    status: StatusCode,
    body: &str,
) -> StorageError {
    let diagnostic = drive_error_message(token, body);
    if let Some(error) = classify_common_drive_error(storage, status, &diagnostic) {
        return error;
    }

    StorageError::Upstream {
        provider: storage.id.clone(),
        status: Some(status.as_u16()),
        message: SanitizedMessage::new(diagnostic.message),
    }
}


#[cfg(test)]
pub(super) mod object_store_tests {
    use super::*;
    use super::root_tests::DriveMetadataServer;
    use super::upload_tests::is_shard_folder_query;

    #[tokio::test]
    async fn object_store_finds_verified_object_by_repo_oid_and_size() {
        let server = DriveFilesListServer::start(
            StatusCode::OK,
            drive_object_list_json("drive-file-123", OBJECT_OID, 42),
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

        let found = store
            .lookup_object(&lfs_object())
            .await
            .expect("object lookup should succeed")
            .expect("object should exist");

        assert_eq!(found.provider_id, "drive-user-a");
        assert_eq!(found.object, lfs_object());
        assert_eq!(found.backend_id, "drive-file-123");
        assert!(
            store
                .object_exists(&lfs_object())
                .await
                .expect("exists should succeed")
        );

        let requests = server.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests[0].headers.get(AUTHORIZATION).unwrap(),
            "Bearer access-token"
        );
        let query = form_pairs(&requests[0].query);
        assert_eq!(query["corpora"], "user");
        assert_eq!(query["includeItemsFromAllDrives"], "true");
        assert_eq!(query["supportsAllDrives"], "true");
        assert!(query["q"].contains("'drive-root' in parents"));
        assert!(query["q"].contains(
            "appProperties has { key='lfsCloudRepoNamespace' and value='github.com/owner/repo' }"
        ));
    }

    #[tokio::test]
    async fn object_store_resolves_stored_backend_id_without_a_list_query() {
        let server = DriveMetadataServer::start(
            StatusCode::OK,
            drive_object_json("drive-file-123", OBJECT_OID, 42),
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

        let stored_object = store
            .lookup_object_by_backend_id(&lfs_object(), "drive-file-123")
            .await
            .expect("direct backend lookup should succeed")
            .expect("stored backend should match");

        assert_eq!(stored_object.backend_id, "drive-file-123");
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].file_id, "drive-file-123");
        assert_eq!(
            requests[0].headers.get(AUTHORIZATION).unwrap(),
            "Bearer access-token"
        );
        let query = form_pairs(&requests[0].query);
        assert_eq!(
            query["fields"],
            "id,name,size,parents,trashed,appProperties"
        );
        assert_eq!(query["supportsAllDrives"], "true");
    }

    #[tokio::test]
    async fn object_store_treats_missing_stored_backend_id_as_repairable() {
        let server = DriveMetadataServer::start(
            StatusCode::NOT_FOUND,
            r#"{"error":{"message":"not found"}}"#,
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

        assert!(
            store
                .lookup_object_by_backend_id(&lfs_object(), "drive-file-missing")
                .await
                .expect("missing backend lookup should remain repairable")
                .is_none()
        );
    }

    #[tokio::test]
    async fn object_store_verifies_stored_backend_shard_belongs_to_root() {
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let app = Router::new().route(
            "/drive/v3/files/{file_id}",
            get({
                let requests = requests.clone();
                move |AxumPath(file_id): AxumPath<String>| {
                    let requests = requests.clone();
                    async move {
                        requests
                            .lock()
                            .expect("direct metadata requests lock should not poison")
                            .push(file_id.clone());
                        if file_id == "drive-file-123" {
                            return Json(serde_json::json!({
                                "id": "drive-file-123",
                                "name": format!("sha256-{OBJECT_OID}-42.lfs"),
                                "size": "42",
                                "parents": ["drive-shard-aa"],
                                "trashed": false,
                                "appProperties": {
                                    "lfsCloudVersion": "1",
                                    "lfsCloudRepoNamespace": "github.com/owner/repo",
                                    "lfsCloudOid": OBJECT_OID,
                                    "lfsCloudSize": "42"
                                }
                            }));
                        }
                        Json(serde_json::json!({
                            "id": "drive-shard-aa",
                            "name": "lfscloud-sha256-aa",
                            "mimeType": "application/vnd.google-apps.folder",
                            "parents": ["drive-root"],
                            "trashed": false,
                            "appProperties": {
                                "lfsCloudFolderKind": "objectShard",
                                "lfsCloudShard": "aa"
                            }
                        }))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("direct metadata server should bind");
        let address = listener
            .local_addr()
            .expect("direct metadata server address should be available");
        let server_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("direct metadata server should run");
        });
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            format!("http://{address}"),
        )
        .expect("object store should build");

        let found = store
            .lookup_object_by_backend_id(&lfs_object(), "drive-file-123")
            .await
            .expect("sharded direct lookup should succeed")
            .expect("sharded object should remain root-scoped");
        server_task.abort();

        assert_eq!(found.backend_id, "drive-file-123");
        assert_eq!(
            requests
                .lock()
                .expect("direct metadata requests lock should not poison")
                .as_slice(),
            ["drive-file-123", "drive-shard-aa"]
        );
    }

    #[tokio::test]
    async fn object_store_creates_missing_deterministic_shard_folder() {
        let create_bodies = Arc::new(Mutex::new(Vec::<String>::new()));
        let app = Router::new().route(
            "/drive/v3/files",
            get(|| async { Json(serde_json::json!({ "files": [] })) }).post({
                let create_bodies = create_bodies.clone();
                move |body: Bytes| {
                    let create_bodies = create_bodies.clone();
                    async move {
                        create_bodies
                            .lock()
                            .expect("shard create bodies lock should not poison")
                            .push(
                                String::from_utf8(body.to_vec())
                                    .expect("shard create body should be UTF-8"),
                            );
                        Json(serde_json::json!({
                            "id": "drive-shard-aa",
                            "name": "lfscloud-sha256-aa",
                            "mimeType": "application/vnd.google-apps.folder",
                            "parents": ["drive-root"],
                            "trashed": false,
                            "appProperties": {
                                "lfsCloudFolderKind": "objectShard",
                                "lfsCloudShard": "aa"
                            }
                        }))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("shard create server should bind");
        let address = listener
            .local_addr()
            .expect("shard create server address should be available");
        let server_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("shard create server should run");
        });
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            format!("http://{address}"),
        )
        .expect("object store should build");
        let key = store
            .object_key(&lfs_object())
            .expect("object key should build");

        let folder_id = store
            .upload_parent_folder_id(&key)
            .await
            .expect("missing shard should be created");
        server_task.abort();

        assert_eq!(folder_id, "drive-shard-aa");
        let bodies = create_bodies
            .lock()
            .expect("shard create bodies lock should not poison");
        assert_eq!(bodies.len(), 1);
        let metadata: serde_json::Value =
            serde_json::from_str(&bodies[0]).expect("shard create body should be JSON");
        assert_eq!(metadata["name"], "lfscloud-sha256-aa");
        assert_eq!(metadata["parents"], serde_json::json!(["drive-root"]));
    }

    #[tokio::test]
    async fn object_store_reports_missing_object_as_false() {
        let server = DriveFilesListServer::start(StatusCode::OK, r#"{"files":[]}"#).await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        assert!(
            !store
                .object_exists(&lfs_object())
                .await
                .expect("missing object lookup should succeed")
        );
    }

    #[tokio::test]
    async fn object_store_selects_duplicate_drive_matches_deterministically() {
        let server = DriveFilesListServer::start(
            StatusCode::OK,
            format!(
                r#"{{
                    "files":[
                        {},
                        {}
                    ]
                }}"#,
                drive_object_json("drive-file-b", OBJECT_OID, 42),
                drive_object_json("drive-file-a", OBJECT_OID, 42)
            ),
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

        let stored_object = store
            .lookup_object(&lfs_object())
            .await
            .expect("duplicate Drive matches should reconcile")
            .expect("an exact Drive match should be returned");

        assert_eq!(stored_object.backend_id, "drive-file-a");
    }

    #[tokio::test]
    async fn object_store_reconciles_drive_matches_across_all_pages() {
        let server = DriveFilesListServer::start_paginated(
            format!(
                r#"{{
                    "files":[{}],
                    "nextPageToken":"page-2"
                }}"#,
                drive_object_json("drive-file-b", OBJECT_OID, 42)
            ),
            [(
                "page-2",
                drive_object_list_json("drive-file-a", OBJECT_OID, 42),
            )],
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

        let stored_object = store
            .lookup_object(&lfs_object())
            .await
            .expect("paginated Drive matches should reconcile")
            .expect("an exact Drive match should be returned");

        assert_eq!(stored_object.backend_id, "drive-file-a");
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        let first_query = form_pairs(&requests[0].query);
        let second_query = form_pairs(&requests[1].query);
        assert!(!first_query.contains_key("pageToken"));
        assert_eq!(second_query["pageToken"], "page-2");
        assert_eq!(first_query["q"], second_query["q"]);
    }

    #[tokio::test]
    async fn object_store_verifies_drive_binary_size() {
        let server = DriveFilesListServer::start(
            StatusCode::OK,
            format!(
                r#"{{
                    "files":[{{
                        "id":"drive-file-123",
                        "name":"sha256-{OBJECT_OID}-42.lfs",
                        "size":"41",
                        "appProperties":{{
                            "lfsCloudVersion":"1",
                            "lfsCloudRepoNamespace":"github.com/owner/repo",
                            "lfsCloudOid":"{OBJECT_OID}",
                            "lfsCloudSize":"42"
                        }}
                    }}]
                }}"#
            ),
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
            .lookup_object(&lfs_object())
            .await
            .expect_err("wrong Drive binary size should fail integrity");

        assert!(matches!(
            error,
            StorageError::IntegrityMismatch {
                expected_size: 42,
                actual_size: 41,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn object_store_rejects_missing_drive_binary_size_as_upstream_error() {
        let server = DriveFilesListServer::start(
            StatusCode::OK,
            format!(
                r#"{{
                    "files":[{{
                        "id":"drive-file-123",
                        "name":"sha256-{OBJECT_OID}-42.lfs",
                        "appProperties":{{
                            "lfsCloudVersion":"1",
                            "lfsCloudRepoNamespace":"github.com/owner/repo",
                            "lfsCloudOid":"{OBJECT_OID}",
                            "lfsCloudSize":"42"
                        }}
                    }}]
                }}"#
            ),
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
            .lookup_object(&lfs_object())
            .await
            .expect_err("missing size should be an upstream error");

        assert!(matches!(
            error,
            StorageError::Upstream {
                ref provider,
                status: Some(200),
                ref message,
            } if provider == "drive-user-a"
                && message.as_str()
                    == "Google Drive object lookup response did not include size"
        ));
    }

    pub(super) struct DriveFilesListServer {
        pub(super) base_url: String,
        state: Arc<DriveFilesListServerState>,
        server_task: tokio::task::JoinHandle<()>,
    }

    impl DriveFilesListServer {
        pub(super) async fn start(status: StatusCode, body: impl Into<String>) -> Self {
            Self::start_with_pages(status, body, BTreeMap::new()).await
        }

        async fn start_paginated(
            first_body: impl Into<String>,
            subsequent_pages: impl IntoIterator<Item = (&'static str, String)>,
        ) -> Self {
            Self::start_with_pages(
                StatusCode::OK,
                first_body,
                subsequent_pages
                    .into_iter()
                    .map(|(token, body)| (token.to_owned(), body))
                    .collect(),
            )
            .await
        }

        async fn start_with_pages(
            status: StatusCode,
            body: impl Into<String>,
            paginated_bodies: BTreeMap<String, String>,
        ) -> Self {
            let state = Arc::new(DriveFilesListServerState {
                status,
                body: body.into(),
                paginated_bodies,
                requests: Mutex::new(Vec::new()),
            });
            let app = Router::new()
                .route("/drive/v3/files", get(drive_files_list_handler))
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test Drive files-list server should bind");
            let address = listener
                .local_addr()
                .expect("test Drive files-list server address should be available");
            let server_task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("test Drive files-list server should run");
            });

            Self {
                base_url: format!("http://{address}"),
                state,
                server_task,
            }
        }

        fn requests(&self) -> Vec<CapturedDriveFilesListRequest> {
            self.state
                .requests
                .lock()
                .expect("test Drive files-list requests lock should not poison")
                .clone()
        }
    }

    impl Drop for DriveFilesListServer {
        fn drop(&mut self) {
            self.server_task.abort();
        }
    }

    struct DriveFilesListServerState {
        status: StatusCode,
        body: String,
        paginated_bodies: BTreeMap<String, String>,
        requests: Mutex<Vec<CapturedDriveFilesListRequest>>,
    }

    #[derive(Clone)]
    pub(super) struct CapturedDriveFilesListRequest {
        pub(super) headers: HeaderMap,
        pub(super) query: String,
    }

    async fn drive_files_list_handler(
        State(state): State<Arc<DriveFilesListServerState>>,
        headers: HeaderMap,
        uri: Uri,
    ) -> Response {
        let query = uri.query().unwrap_or_default();
        state
            .requests
            .lock()
            .expect("test Drive files-list requests lock should not poison")
            .push(CapturedDriveFilesListRequest {
                headers,
                query: query.to_owned(),
            });
        let page_token = url::form_urlencoded::parse(query.as_bytes())
            .find_map(|(key, value)| (key == "pageToken").then(|| value.into_owned()));
        let folder_query = is_shard_folder_query(query);
        let body = if folder_query {
            r#"{"files":[]}"#
        } else {
            page_token
                .as_deref()
                .and_then(|token| state.paginated_bodies.get(token))
                .unwrap_or(&state.body)
        };

        (
            state.status,
            [(CONTENT_TYPE, "application/json")],
            body.to_owned(),
        )
            .into_response()
    }

}
