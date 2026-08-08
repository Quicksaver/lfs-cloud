fn default_google_drive_http_client_from(
    client_slot: &'static OnceLock<Client>,
    timeout: Duration,
) -> StorageResult<Client> {
    if let Some(client) = client_slot.get() {
        return Ok(client.clone());
    }

    let client = Client::builder()
        .timeout(timeout)
        .connect_timeout(GOOGLE_DRIVE_CONNECT_TIMEOUT)
        .redirect(Policy::none())
        .build()
        .map_err(|source| StorageError::Retryable {
            provider: "google_drive".to_owned(),
            message: format!("failed to initialize Google Drive HTTP client: {source}"),
        })?;

    match client_slot.set(client.clone()) {
        Ok(()) => Ok(client),
        Err(client) => Ok(client_slot.get().cloned().unwrap_or(client)),
    }
}

fn default_google_drive_root_validation_http_client() -> StorageResult<Client> {
    default_google_drive_http_client_from(
        &DEFAULT_GOOGLE_DRIVE_ROOT_VALIDATION_HTTP_CLIENT,
        GOOGLE_DRIVE_ROOT_VALIDATION_TIMEOUT,
    )
}

fn default_google_drive_object_metadata_http_client() -> StorageResult<Client> {
    default_google_drive_http_client_from(
        &DEFAULT_GOOGLE_DRIVE_OBJECT_METADATA_HTTP_CLIENT,
        GOOGLE_DRIVE_OBJECT_METADATA_TIMEOUT,
    )
}

fn default_google_drive_object_upload_http_client() -> StorageResult<Client> {
    if let Some(client) = DEFAULT_GOOGLE_DRIVE_OBJECT_UPLOAD_HTTP_CLIENT.get() {
        return Ok(client.clone());
    }

    let client = Client::builder()
        .connect_timeout(GOOGLE_DRIVE_CONNECT_TIMEOUT)
        .redirect(Policy::none())
        .build()
        .map_err(|source| StorageError::Retryable {
            provider: "google_drive".to_owned(),
            message: format!("failed to initialize Google Drive upload HTTP client: {source}"),
        })?;

    match DEFAULT_GOOGLE_DRIVE_OBJECT_UPLOAD_HTTP_CLIENT.set(client.clone()) {
        Ok(()) => Ok(client),
        Err(client) => Ok(DEFAULT_GOOGLE_DRIVE_OBJECT_UPLOAD_HTTP_CLIENT
            .get()
            .cloned()
            .unwrap_or(client)),
    }
}

fn default_google_drive_object_download_http_client() -> StorageResult<Client> {
    if let Some(client) = DEFAULT_GOOGLE_DRIVE_OBJECT_DOWNLOAD_HTTP_CLIENT.get() {
        return Ok(client.clone());
    }

    let client = Client::builder()
        .connect_timeout(GOOGLE_DRIVE_CONNECT_TIMEOUT)
        .read_timeout(GOOGLE_DRIVE_TRANSFER_READ_IDLE_TIMEOUT)
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .redirect(Policy::none())
        .build()
        .map_err(|source| StorageError::Retryable {
            provider: "google_drive".to_owned(),
            message: format!("failed to initialize Google Drive download HTTP client: {source}"),
        })?;

    match DEFAULT_GOOGLE_DRIVE_OBJECT_DOWNLOAD_HTTP_CLIENT.set(client.clone()) {
        Ok(()) => Ok(client),
        Err(client) => Ok(DEFAULT_GOOGLE_DRIVE_OBJECT_DOWNLOAD_HTTP_CLIENT
            .get()
            .cloned()
            .unwrap_or(client)),
    }
}

/// Controls query handling while validating a Google Drive URL.
///
/// Fragments are rejected under both policies. Rejecting queries uses the
/// combined query-or-fragment diagnostic retained for API base URLs.
#[derive(Clone, Copy)]
enum DriveUrlComponentPolicy {
    /// Reject query strings and fragments.
    RejectQuery,
    /// Allow query strings while continuing to reject fragments.
    AllowQuery,
}

fn drive_upstream_error(provider: &str, message: impl Into<String>) -> StorageError {
    StorageError::Upstream {
        provider: provider.to_owned(),
        status: None,
        message: SanitizedMessage::new(message.into()),
    }
}

fn validate_drive_url(
    value: &str,
    provider: &str,
    label: &str,
    component_policy: DriveUrlComponentPolicy,
) -> StorageResult<Url> {
    let url = Url::parse(value)
        .map_err(|_| drive_upstream_error(provider, format!("{label} must be valid")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(drive_upstream_error(
            provider,
            format!("{label} must be an absolute http or https URL"),
        ));
    }
    if url.scheme() == "http" && !has_exact_loopback_host(&url) {
        return Err(drive_upstream_error(
            provider,
            format!("{label} must use https unless it targets an exact literal loopback IP"),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(drive_upstream_error(
            provider,
            format!("{label} must not include credentials"),
        ));
    }
    if matches!(component_policy, DriveUrlComponentPolicy::RejectQuery) && url.query().is_some() {
        return Err(drive_upstream_error(
            provider,
            format!("{label} must not include query strings or fragments"),
        ));
    }
    if url.fragment().is_some() {
        let message = match component_policy {
            DriveUrlComponentPolicy::RejectQuery => {
                format!("{label} must not include query strings or fragments")
            }
            DriveUrlComponentPolicy::AllowQuery => format!("{label} must not include fragments"),
        };
        return Err(drive_upstream_error(provider, message));
    }

    Ok(url)
}

fn validate_drive_api_base_url(value: &str) -> StorageResult<Url> {
    validate_drive_url(
        value,
        "google_drive",
        "Google Drive API base URL",
        DriveUrlComponentPolicy::RejectQuery,
    )
}

fn drive_api_base_path_already_targets_drive_api(api_base_url: &Url) -> bool {
    api_base_url
        .path_segments()
        .map(|segments| {
            let segments = segments
                .filter(|segment| !segment.is_empty())
                .collect::<Vec<_>>();
            segments.ends_with(&["drive", "v3"])
        })
        .unwrap_or(false)
}

/// Google Drive endpoint appended to a configurable API base path.
///
/// Resumable uploads replace an existing trailing `/drive/v3` suffix before
/// appending `/upload/drive/v3/files`, preserving any preceding proxy prefix.
enum DriveApiEndpoint<'a> {
    /// A metadata or media endpoint beneath `/drive/v3/files`.
    Files(&'a [&'a str]),
    /// The resumable-upload creation endpoint beneath `/upload/drive/v3/files`.
    ResumableUpload,
}

fn drive_api_url(mut api_base_url: Url, endpoint: DriveApiEndpoint<'_>) -> StorageResult<Url> {
    let already_targets_drive_api = drive_api_base_path_already_targets_drive_api(&api_base_url);
    let mut segments = api_base_url.path_segments_mut().map_err(|_| {
        drive_upstream_error(
            "google_drive",
            "Google Drive API base URL cannot be used for path construction",
        )
    })?;
    segments.pop_if_empty();
    match endpoint {
        DriveApiEndpoint::Files(extra_segments) => {
            if !already_targets_drive_api {
                segments.extend(["drive", "v3"]);
            }
            segments.push("files");
            segments.extend(extra_segments.iter().copied());
        }
        DriveApiEndpoint::ResumableUpload => {
            if already_targets_drive_api {
                segments.pop();
                segments.pop();
            }
            segments.extend(["upload", "drive", "v3", "files"]);
        }
    }
    drop(segments);

    Ok(api_base_url)
}

fn drive_files_url(api_base_url: Url, extra_segments: &[&str]) -> StorageResult<Url> {
    drive_api_url(api_base_url, DriveApiEndpoint::Files(extra_segments))
}

fn require_drive_identifier(value: &str, label: &str) -> StorageResult<()> {
    if value.trim().is_empty() {
        return Err(drive_upstream_error(
            "google_drive",
            format!("Google Drive {label} must not be blank"),
        ));
    }

    Ok(())
}

fn drive_file_metadata_url(api_base_url: Url, root_folder_id: &str) -> StorageResult<Url> {
    require_drive_identifier(root_folder_id, "root_folder_id")?;
    let mut api_base_url = drive_files_url(api_base_url, &[root_folder_id])?;
    api_base_url
        .query_pairs_mut()
        .append_pair(
            "fields",
            "id,name,mimeType,trashed,capabilities(canAddChildren)",
        )
        .append_pair("supportsAllDrives", "true");

    Ok(api_base_url)
}

fn drive_object_metadata_url(api_base_url: Url, file_id: &str) -> StorageResult<Url> {
    require_drive_identifier(file_id, "file ID")?;
    let mut api_base_url = drive_files_url(api_base_url, &[file_id])?;
    api_base_url
        .query_pairs_mut()
        .append_pair("fields", "id,name,size,parents,trashed,appProperties")
        .append_pair("supportsAllDrives", "true");

    Ok(api_base_url)
}

fn drive_shard_folder_metadata_url(api_base_url: Url, folder_id: &str) -> StorageResult<Url> {
    require_drive_identifier(folder_id, "folder ID")?;
    let mut api_base_url = drive_files_url(api_base_url, &[folder_id])?;
    api_base_url
        .query_pairs_mut()
        .append_pair("fields", "id,name,mimeType,parents,trashed,appProperties")
        .append_pair("supportsAllDrives", "true");

    Ok(api_base_url)
}

fn drive_object_lookup_url(
    api_base_url: Url,
    root_folder_id: &str,
    key: &GoogleDriveObjectKey,
    expected_properties: &GoogleDriveObjectProperties,
    page_token: Option<&str>,
) -> StorageResult<Url> {
    require_drive_identifier(root_folder_id, "root_folder_id")?;
    let mut api_base_url = drive_files_url(api_base_url, &[])?;
    api_base_url
        .query_pairs_mut()
        .append_pair(
            "fields",
            "files(id,name,size,appProperties),nextPageToken,incompleteSearch",
        )
        .append_pair("pageSize", "2")
        .append_pair("spaces", "drive")
        .append_pair("corpora", "user")
        .append_pair("includeItemsFromAllDrives", "true")
        .append_pair("supportsAllDrives", "true")
        .append_pair(
            "q",
            &drive_object_lookup_query(root_folder_id, key, expected_properties),
        );
    if let Some(page_token) = page_token {
        api_base_url
            .query_pairs_mut()
            .append_pair("pageToken", page_token);
    }

    Ok(api_base_url)
}

fn drive_shard_folder_lookup_url(
    api_base_url: Url,
    root_folder_id: &str,
    key: &GoogleDriveObjectKey,
    page_token: Option<&str>,
) -> StorageResult<Url> {
    require_drive_identifier(root_folder_id, "root_folder_id")?;
    let mut api_base_url = drive_files_url(api_base_url, &[])?;
    api_base_url
        .query_pairs_mut()
        .append_pair(
            "fields",
            "files(id,name,mimeType,parents,trashed,appProperties),nextPageToken,incompleteSearch",
        )
        .append_pair("pageSize", "100")
        .append_pair("spaces", "drive")
        .append_pair("corpora", "user")
        .append_pair("includeItemsFromAllDrives", "true")
        .append_pair("supportsAllDrives", "true")
        .append_pair("q", &drive_shard_folder_lookup_query(root_folder_id, key));
    if let Some(page_token) = page_token {
        api_base_url
            .query_pairs_mut()
            .append_pair("pageToken", page_token);
    }

    Ok(api_base_url)
}

fn drive_file_create_url(api_base_url: Url) -> StorageResult<Url> {
    let mut api_base_url = drive_files_url(api_base_url, &[])?;
    api_base_url
        .query_pairs_mut()
        .append_pair("fields", "id,name,mimeType,parents,trashed,appProperties")
        .append_pair("supportsAllDrives", "true");

    Ok(api_base_url)
}

fn drive_default_root_folder_lookup_url(
    api_base_url: Url,
    page_token: Option<&str>,
) -> StorageResult<Url> {
    let mut api_base_url = drive_files_url(api_base_url, &[])?;
    api_base_url
        .query_pairs_mut()
        .append_pair(
            "fields",
            "files(id,name,mimeType,parents,trashed,appProperties),nextPageToken,incompleteSearch",
        )
        .append_pair("pageSize", "100")
        .append_pair("spaces", "drive")
        .append_pair("corpora", "user")
        .append_pair("includeItemsFromAllDrives", "true")
        .append_pair("supportsAllDrives", "true")
        .append_pair("q", &drive_default_root_folder_lookup_query());
    if let Some(page_token) = page_token {
        api_base_url
            .query_pairs_mut()
            .append_pair("pageToken", page_token);
    }

    Ok(api_base_url)
}

fn drive_resumable_upload_url(api_base_url: Url) -> StorageResult<Url> {
    let mut api_base_url = drive_api_url(api_base_url, DriveApiEndpoint::ResumableUpload)?;
    api_base_url
        .query_pairs_mut()
        .append_pair("uploadType", "resumable")
        .append_pair("fields", "id,name,size,appProperties")
        .append_pair("supportsAllDrives", "true");

    Ok(api_base_url)
}

fn drive_media_download_url(api_base_url: Url, file_id: &str) -> StorageResult<Url> {
    require_drive_identifier(file_id, "file ID")?;
    let mut api_base_url = drive_files_url(api_base_url, &[file_id])?;
    api_base_url
        .query_pairs_mut()
        .append_pair("alt", "media")
        .append_pair("supportsAllDrives", "true");

    Ok(api_base_url)
}

fn validate_drive_resumable_upload_session_url(
    storage: &GoogleDriveStorageConfig,
    api_base_url: &Url,
    value: &str,
) -> StorageResult<Url> {
    let url = validate_drive_url(
        value,
        &storage.id,
        "Google Drive resumable upload session URL",
        DriveUrlComponentPolicy::AllowQuery,
    )?;
    if !url_origins_match(&url, api_base_url) {
        return Err(drive_upstream_error(
            &storage.id,
            "Google Drive resumable upload session URL must match the configured Drive API origin",
        ));
    }

    Ok(url)
}

fn url_origins_match(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host() == right.host()
        && left.port_or_known_default() == right.port_or_known_default()
}


#[cfg(test)]
pub(super) mod drive_api_tests {
    use super::*;

    #[test]
    fn default_drive_transfer_clients_bound_idle_reads_without_total_deadlines() {
        let upload_client = super::default_google_drive_object_upload_http_client()
            .expect("default Drive upload client should build");
        let download_client = super::default_google_drive_object_download_http_client()
            .expect("default Drive download client should build");

        let upload_debug = format!("{upload_client:?}");
        assert!(
            !upload_debug.contains("read_timeout"),
            "upload progress needs a body-aware watchdog, not a time-to-response limit: {upload_debug}"
        );
        assert!(
            !upload_debug.contains("total_timeout"),
            "large uploads must not impose a total request deadline: {upload_debug}"
        );

        let download_debug = format!("{download_client:?}");
        assert!(
            download_debug.contains("read_timeout: 30s"),
            "downloads should reset a 30-second idle watchdog after each read: {download_debug}"
        );
        assert!(
            !download_debug.contains("total_timeout"),
            "large downloads must not impose a total request deadline: {download_debug}"
        );
    }

    #[test]
    fn root_validator_requires_https_api_base_except_literal_loopback_ip() {
        let error = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            "http://drive.example.com/drive/v3",
        )
        .expect_err("non-loopback HTTP API base should fail");

        assert!(error.to_string().contains(
            "Google Drive API base URL must use https unless it targets an exact literal loopback IP"
        ));

        let error = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            "http://localhost/drive/v3",
        )
        .expect_err("localhost HTTP API base should fail");

        assert!(error.to_string().contains(
            "Google Drive API base URL must use https unless it targets an exact literal loopback IP"
        ));

        let validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            "http://127.0.0.1/drive/v3",
        )
        .expect("literal loopback HTTP API base should be accepted for local testing");

        assert_eq!(validator.api_base_url.as_str(), "http://127.0.0.1/drive/v3");
    }

    #[test]
    fn drive_file_metadata_url_does_not_duplicate_existing_drive_api_path() {
        let url = super::drive_file_metadata_url(
            url::Url::parse("http://localhost/proxy/drive/v3").expect("base URL should parse"),
            "drive-root",
        )
        .expect("metadata URL should build");

        assert_eq!(url.path(), "/proxy/drive/v3/files/drive-root");
    }

    #[test]
    fn drive_object_lookup_url_searches_with_private_app_properties() {
        let key = GoogleDriveObjectKey::new("github.com/owner/repo", lfs_object())
            .expect("key should build");
        let url = super::drive_object_lookup_url(
            url::Url::parse("http://localhost/proxy/drive/v3").expect("base URL should parse"),
            "drive-root",
            &key,
            &key.expected_app_properties(),
            None,
        )
        .expect("lookup URL should build");

        assert_eq!(url.path(), "/proxy/drive/v3/files");
        let query = form_pairs(url.query().expect("lookup URL should include query"));
        assert_eq!(
            query["fields"],
            "files(id,name,size,appProperties),nextPageToken,incompleteSearch"
        );
        assert_eq!(query["pageSize"], "2");
        assert_eq!(query["spaces"], "drive");
        assert_eq!(query["corpora"], "user");
        assert_eq!(query["includeItemsFromAllDrives"], "true");
        assert_eq!(query["supportsAllDrives"], "true");
        assert!(query["q"].contains("'drive-root' in parents"));
        assert!(query["q"].contains("trashed = false"));
        assert!(query["q"].contains(&format!("name = 'sha256-{OBJECT_OID}-42.lfs'")));
        assert!(query["q"].contains(
            "appProperties has { key='lfsCloudRepoNamespace' and value='github.com/owner/repo' }"
        ));
        assert!(query["q"].contains(&format!(
            "appProperties has {{ key='lfsCloudOid' and value='{OBJECT_OID}' }}"
        )));
        assert!(query["q"].contains("appProperties has { key='lfsCloudSize' and value='42' }"));
    }

    #[test]
    fn drive_shard_folder_lookup_is_root_scoped_and_deterministic() {
        let key = GoogleDriveObjectKey::new("github.com/owner/repo", lfs_object())
            .expect("key should build");
        let url = super::drive_shard_folder_lookup_url(
            url::Url::parse("http://localhost/proxy/drive/v3").expect("base URL should parse"),
            "drive-root",
            &key,
            None,
        )
        .expect("shard lookup URL should build");
        let query = form_pairs(url.query().expect("shard lookup URL should include query"));

        assert_eq!(url.path(), "/proxy/drive/v3/files");
        assert!(query["q"].contains("'drive-root' in parents"));
        assert!(query["q"].contains("name = 'lfscloud-sha256-aa'"));
        assert!(
            query["q"]
                .contains("appProperties has { key='lfsCloudFolderKind' and value='objectShard' }")
        );
        assert!(query["q"].contains("appProperties has { key='lfsCloudShard' and value='aa' }"));

        let metadata = super::drive_shard_folder_metadata("drive-root", &key);
        assert_eq!(metadata["name"], "lfscloud-sha256-aa");
        assert_eq!(metadata["mimeType"], "application/vnd.google-apps.folder");
        assert_eq!(metadata["parents"], serde_json::json!(["drive-root"]));
        assert_eq!(
            metadata["appProperties"]["lfsCloudFolderKind"],
            "objectShard"
        );
        assert_eq!(metadata["appProperties"]["lfsCloudShard"], "aa");
    }

    #[test]
    fn drive_resumable_upload_url_does_not_duplicate_existing_drive_api_path() {
        let url = super::drive_resumable_upload_url(
            url::Url::parse("http://localhost/proxy/drive/v3").expect("base URL should parse"),
        )
        .expect("upload URL should build");

        assert_eq!(url.path(), "/proxy/upload/drive/v3/files");
        let query = form_pairs(url.query().expect("upload URL should include query"));
        assert_eq!(query["uploadType"], "resumable");
        assert_eq!(query["fields"], "id,name,size,appProperties");
        assert_eq!(query["supportsAllDrives"], "true");
    }

    #[test]
    fn drive_media_download_url_does_not_duplicate_existing_drive_api_path() {
        let url = super::drive_media_download_url(
            url::Url::parse("http://localhost/proxy/drive/v3").expect("base URL should parse"),
            "drive-file-123",
        )
        .expect("download URL should build");

        assert_eq!(url.path(), "/proxy/drive/v3/files/drive-file-123");
        let query = form_pairs(url.query().expect("download URL should include query"));
        assert_eq!(query["alt"], "media");
        assert_eq!(query["supportsAllDrives"], "true");
    }

    #[test]
    fn drive_media_download_url_encodes_opaque_file_ids_as_one_segment() {
        let url = super::drive_media_download_url(
            url::Url::parse("http://localhost/proxy/drive/v3").expect("base URL should parse"),
            "drive-file-123/../../other",
        )
        .expect("opaque file IDs should be path-encoded");

        assert_eq!(
            url.path(),
            "/proxy/drive/v3/files/drive-file-123%2F..%2F..%2Fother"
        );
    }

    #[test]
    fn drive_media_download_url_rejects_blank_file_ids() {
        let error = super::drive_media_download_url(
            url::Url::parse("http://localhost/proxy/drive/v3").expect("base URL should parse"),
            " \t",
        )
        .expect_err("blank file IDs should fail before URL construction");

        assert!(matches!(
            error,
            StorageError::Upstream { ref message, .. }
                if message.as_str().contains("Google Drive file ID must not be blank")
        ));
    }

    #[test]
    fn drive_object_metadata_url_targets_stored_backend_id_directly() {
        let url = super::drive_object_metadata_url(
            url::Url::parse("http://localhost/proxy/drive/v3").expect("base URL should parse"),
            "drive-file-123/opaque",
        )
        .expect("metadata URL should build");

        assert_eq!(url.path(), "/proxy/drive/v3/files/drive-file-123%2Fopaque");
        let query = form_pairs(url.query().expect("metadata URL should include query"));
        assert_eq!(
            query["fields"],
            "id,name,size,parents,trashed,appProperties"
        );
        assert_eq!(query["supportsAllDrives"], "true");
    }

    #[test]
    fn drive_resumable_upload_session_url_requires_https_except_literal_loopback_ip() {
        let storage = storage_config("google-drive-user-a");
        let error = super::validate_drive_resumable_upload_session_url(
            &storage,
            &url::Url::parse("https://www.googleapis.com").expect("API base should parse"),
            "http://drive.example.com/upload/session-1?upload_id=123",
        )
        .expect_err("non-loopback HTTP session URL should fail");

        assert!(error.to_string().contains(
            "Google Drive resumable upload session URL must use https unless it targets an exact literal loopback IP"
        ));

        let url = super::validate_drive_resumable_upload_session_url(
            &storage,
            &url::Url::parse("http://127.0.0.1").expect("API base should parse"),
            "http://127.0.0.1/upload/session-1?upload_id=123",
        )
        .expect("literal loopback HTTP session URL should be accepted for local testing");

        assert_eq!(
            url.as_str(),
            "http://127.0.0.1/upload/session-1?upload_id=123"
        );

        let error = super::validate_drive_resumable_upload_session_url(
            &storage,
            &url::Url::parse("http://localhost").expect("API base should parse"),
            "http://localhost/upload/session-1?upload_id=123",
        )
        .expect_err("localhost HTTP session URL should fail");

        assert!(error.to_string().contains(
            "Google Drive resumable upload session URL must use https unless it targets an exact literal loopback IP"
        ));
    }

    #[test]
    fn drive_resumable_upload_session_url_must_match_api_origin() {
        let storage = storage_config("google-drive-user-a");
        let error = super::validate_drive_resumable_upload_session_url(
            &storage,
            &url::Url::parse("https://www.googleapis.com").expect("API base should parse"),
            "https://attacker.example/upload/session-1?upload_id=123",
        )
        .expect_err("cross-origin session URL should fail before forwarding auth");

        assert!(error.to_string().contains(
            "Google Drive resumable upload session URL must match the configured Drive API origin"
        ));

        let url = super::validate_drive_resumable_upload_session_url(
            &storage,
            &url::Url::parse("https://www.googleapis.com/drive/v3").expect("API base should parse"),
            "https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable&upload_id=123",
        )
        .expect("same-origin session URL should be accepted");

        assert_eq!(
            url.as_str(),
            "https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable&upload_id=123"
        );
    }

}
