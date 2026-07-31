/// Metadata proving that a configured Google Drive root folder is usable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleDriveRootFolder {
    /// Configured storage provider ID.
    pub provider_id: String,
    /// Google Drive folder ID from server configuration.
    pub id: String,
    /// Operator-facing Google Drive folder name.
    pub name: String,
    /// Whether the Drive API reports that this credential can create children.
    ///
    /// Successful validation guarantees this is `true`; a false or missing API
    /// value fails validation before returning this metadata.
    pub can_add_children: bool,
}

/// Validates that a configured Google Drive root folder is app-accessible.
///
/// The validator performs a non-mutating `files.get` probe. It confirms that
/// the configured ID resolves to a live folder and that the current credential
/// can add children under it. This is intentionally weaker than an upload
/// smoke test, but it is safe for startup and health checks.
#[derive(Clone)]
pub struct GoogleDriveRootValidator {
    client: Client,
    api_base_url: Url,
}

impl GoogleDriveRootValidator {
    /// Creates a validator using the default Google Drive HTTP client.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the default HTTP client or API base URL
    /// cannot be initialized.
    pub fn new() -> StorageResult<Self> {
        Self::with_client_and_api_base_url(
            default_google_drive_root_validation_http_client()?,
            GOOGLE_DRIVE_API_BASE_URL,
        )
    }

    /// Creates a validator with an explicit HTTP client and API base URL.
    ///
    /// Callers targeting the real Google Drive endpoint should pass
    /// [`GOOGLE_DRIVE_API_BASE_URL`]. Tests and development tools can instead
    /// provide a validated loopback URL for a local HTTP server.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if `api_base_url` is not an absolute HTTP(S)
    /// URL without credentials, query, or fragment components. HTTP is accepted
    /// only for literal loopback IP addresses used by local tests and
    /// development tools.
    pub fn with_client_and_api_base_url(
        client: Client,
        api_base_url: impl AsRef<str>,
    ) -> StorageResult<Self> {
        let api_base_url = validate_drive_api_base_url(api_base_url.as_ref())?;
        Ok(Self {
            client,
            api_base_url,
        })
    }

    /// Validates access to the configured Drive root folder.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the access token cannot authorize Drive
    /// requests, the folder cannot be found, the configured ID is not a live
    /// folder, or the credential cannot create child objects there.
    pub async fn validate_root_folder(
        &self,
        storage: &GoogleDriveStorageConfig,
        token: &GoogleDriveAccessToken,
    ) -> StorageResult<GoogleDriveRootFolder> {
        let response = self
            .client
            .get(drive_file_metadata_url(
                self.api_base_url.clone(),
                &storage.root_folder_id,
            )?)
            .header(ACCEPT, "application/json")
            .header(
                AUTHORIZATION,
                token.authorization_header_value(&storage.id)?,
            )
            .send()
            .await
            .map_err(|source| drive_transport_error(storage, token, source))?;
        let status = response.status();
        let response_body = read_google_response_body(response)
            .await
            .map_err(|source| drive_transport_error(storage, token, source))?;

        if !status.is_success() {
            return Err(parse_drive_root_error(
                storage,
                token,
                status,
                &response_body,
            ));
        }

        parse_drive_root_success(storage, token, status, &response_body)
    }
}

impl fmt::Debug for GoogleDriveRootValidator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleDriveRootValidator")
            .field("client", &"<redacted>")
            .field("api_base_url", &self.api_base_url)
            .finish()
    }
}

fn parse_drive_root_success(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    status: StatusCode,
    body: &str,
) -> StorageResult<GoogleDriveRootFolder> {
    let metadata = serde_json::from_str::<GoogleDriveFileMetadata>(body).map_err(|_| {
        StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new("Google Drive root folder response was invalid JSON"),
        }
    })?;
    let id = metadata
        .id
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new("Google Drive root folder response did not include id"),
        })?;
    let name = metadata
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| storage.root_folder_id.clone());
    if id != storage.root_folder_id {
        return Err(StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(format!(
                "Google Drive returned folder id {id:?} for configured root_folder_id {:?}",
                storage.root_folder_id
            )),
        });
    }
    if metadata.mime_type.as_deref() != Some(GOOGLE_DRIVE_FOLDER_MIME_TYPE) {
        return Err(StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(format!(
                "Google Drive root_folder_id {:?} is not a folder",
                storage.root_folder_id
            )),
        });
    }
    if metadata.trashed {
        return Err(StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(format!(
                "Google Drive root_folder_id {:?} is trashed",
                storage.root_folder_id
            )),
        });
    }
    let can_add_children =
        metadata
            .capabilities
            .can_add_children
            .ok_or_else(|| StorageError::Upstream {
                provider: storage.id.clone(),
                status: Some(status.as_u16()),
                message: SanitizedMessage::new(
                    "Google Drive root folder response did not include capabilities.canAddChildren",
                ),
            })?;
    if !can_add_children {
        return Err(StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(sanitize_drive_diagnostic(
                token,
                &format!(
                    "Google Drive root folder {:?} is visible but cannot accept child objects",
                    storage.root_folder_id
                ),
            )),
        });
    }

    Ok(GoogleDriveRootFolder {
        provider_id: storage.id.clone(),
        id,
        name,
        can_add_children,
    })
}

fn parse_drive_root_error(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    status: StatusCode,
    body: &str,
) -> StorageError {
    let diagnostic = drive_error_message(token, body);
    if let Some(error) = classify_common_drive_error(storage, status, &diagnostic) {
        return error;
    }
    if status == StatusCode::NOT_FOUND {
        return StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(format!(
                "Google Drive root_folder_id {:?} was not found or is not accessible",
                storage.root_folder_id
            )),
        };
    }

    StorageError::Upstream {
        provider: storage.id.clone(),
        status: Some(status.as_u16()),
        message: SanitizedMessage::new(diagnostic.message),
    }
}


#[cfg(test)]
pub(super) mod root_tests {
    use super::*;

    #[tokio::test]
    async fn root_validator_confirms_app_accessible_writable_folder() {
        let server = DriveMetadataServer::start(StatusCode::OK, drive_folder_json()).await;
        let validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("validator should build");

        let folder = validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect("root folder should validate");

        assert_eq!(folder.provider_id, "drive-user-a");
        assert_eq!(folder.id, "drive-root");
        assert_eq!(folder.name, "LFS Cloud Root");
        assert!(folder.can_add_children);

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].file_id, "drive-root");
        assert_eq!(
            requests[0].headers.get(AUTHORIZATION).unwrap(),
            "Bearer access-token"
        );
        let query = form_pairs(&requests[0].query);
        assert_eq!(
            query["fields"],
            "id,name,mimeType,trashed,capabilities(canAddChildren)"
        );
        assert_eq!(query["supportsAllDrives"], "true");
    }

    #[tokio::test]
    async fn root_validator_uses_existing_drive_api_base_path_once() {
        let server = DriveMetadataServer::start(StatusCode::OK, drive_folder_json()).await;
        let validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            format!("{}/drive/v3", server.base_url),
        )
        .expect("validator should build");

        validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect("root folder should validate through Drive API base path");

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].file_id, "drive-root");
    }

    #[tokio::test]
    async fn root_validator_rejects_non_folder_root_ids() {
        let server = DriveMetadataServer::start(
            StatusCode::OK,
            r#"{
                "id":"drive-root",
                "name":"not-a-folder.bin",
                "mimeType":"application/octet-stream",
                "capabilities":{"canAddChildren":true}
            }"#,
        )
        .await;
        let validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("validator should build");

        let error = validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect_err("non-folder root should fail");

        assert!(matches!(
            error,
            StorageError::Upstream {
                ref provider,
                status: Some(200),
                ..
            } if provider == "drive-user-a"
        ));
        assert!(error.to_string().contains("is not a folder"));
    }

    #[tokio::test]
    async fn root_validator_rejects_visible_folder_without_child_write_access() {
        let server = DriveMetadataServer::start(
            StatusCode::OK,
            r#"{
                "id":"drive-root",
                "name":"Read Only",
                "mimeType":"application/vnd.google-apps.folder",
                "capabilities":{"canAddChildren":false}
            }"#,
        )
        .await;
        let validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("validator should build");

        let error = validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect_err("read-only root should fail");

        assert!(matches!(
            error,
            StorageError::Upstream {
                ref provider,
                status: Some(200),
                ..
            } if provider == "drive-user-a"
        ));
        assert!(error.to_string().contains("cannot accept child objects"));
    }

    #[tokio::test]
    async fn root_validator_reports_missing_child_write_capability() {
        let server = DriveMetadataServer::start(
            StatusCode::OK,
            r#"{
                "id":"drive-root",
                "name":"Unexpected Shape",
                "mimeType":"application/vnd.google-apps.folder"
            }"#,
        )
        .await;
        let validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("validator should build");

        let error = validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect_err("missing canAddChildren should fail");

        assert!(error.to_string().contains("capabilities.canAddChildren"));
    }

    #[tokio::test]
    async fn root_validator_maps_missing_root_to_clear_upstream_error() {
        let server = DriveMetadataServer::start(
            StatusCode::NOT_FOUND,
            r#"{"error":{"message":"File not found: drive-root"}}"#,
        )
        .await;
        let validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("validator should build");

        let error = validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect_err("missing root should fail");

        assert!(matches!(
            error,
            StorageError::Upstream {
                ref provider,
                status: Some(404),
                ..
            } if provider == "drive-user-a"
        ));
        assert!(
            error
                .to_string()
                .contains("was not found or is not accessible")
        );
    }

    #[tokio::test]
    async fn root_validator_maps_auth_and_rate_limit_failures() {
        let auth_server = DriveMetadataServer::start(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"missing scope access-token","errors":[{"reason":"insufficientPermissions"}]}}"#,
        )
        .await;
        let auth_validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            &auth_server.base_url,
        )
        .expect("validator should build");

        let auth_error = auth_validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect_err("insufficient scope should fail");
        assert!(matches!(
            auth_error,
            StorageError::AuthenticationRequired { ref provider } if provider == "drive-user-a"
        ));
        assert!(!auth_error.to_string().contains("access-token"));

        let rate_limit_server = DriveMetadataServer::start(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"try later access-token","errors":[{"reason":"rateLimitExceeded"}]}}"#,
        )
        .await;
        let rate_limit_validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            &rate_limit_server.base_url,
        )
        .expect("validator should build");

        let rate_limit_error = rate_limit_validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect_err("rate limit should fail");
        assert!(matches!(
            rate_limit_error,
            StorageError::Retryable {
                provider,
                message,
            } if provider == "drive-user-a"
                && message.contains("try later")
                && !message.contains("access-token")
        ));
    }

    pub(super) struct DriveMetadataServer {
        pub(super) base_url: String,
        state: Arc<DriveMetadataServerState>,
        server_task: tokio::task::JoinHandle<()>,
    }

    impl DriveMetadataServer {
        pub(super) async fn start(status: StatusCode, body: impl Into<String>) -> Self {
            let state = Arc::new(DriveMetadataServerState {
                status,
                body: body.into(),
                requests: Mutex::new(Vec::new()),
            });
            let app = Router::new()
                .route("/drive/v3/files/{file_id}", get(drive_metadata_handler))
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test Drive metadata server should bind");
            let address = listener
                .local_addr()
                .expect("test Drive metadata server address should be available");
            let server_task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("test Drive metadata server should run");
            });

            Self {
                base_url: format!("http://{address}"),
                state,
                server_task,
            }
        }

        pub(super) fn requests(&self) -> Vec<CapturedDriveMetadataRequest> {
            self.state
                .requests
                .lock()
                .expect("test Drive metadata requests lock should not poison")
                .clone()
        }
    }

    impl Drop for DriveMetadataServer {
        fn drop(&mut self) {
            self.server_task.abort();
        }
    }

    struct DriveMetadataServerState {
        status: StatusCode,
        body: String,
        requests: Mutex<Vec<CapturedDriveMetadataRequest>>,
    }

    #[derive(Clone)]
    pub(super) struct CapturedDriveMetadataRequest {
        pub(super) file_id: String,
        pub(super) headers: HeaderMap,
        pub(super) query: String,
    }

    async fn drive_metadata_handler(
        AxumPath(file_id): AxumPath<String>,
        State(state): State<Arc<DriveMetadataServerState>>,
        headers: HeaderMap,
        uri: Uri,
    ) -> Response {
        state
            .requests
            .lock()
            .expect("test Drive metadata requests lock should not poison")
            .push(CapturedDriveMetadataRequest {
                file_id,
                headers,
                query: uri.query().unwrap_or_default().to_owned(),
            });

        (
            state.status,
            [(CONTENT_TYPE, "application/json")],
            state.body.clone(),
        )
            .into_response()
    }

}
