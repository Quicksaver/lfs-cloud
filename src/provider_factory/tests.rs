use std::{
    collections::BTreeMap,
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::{
        HeaderMap, HeaderValue, StatusCode, Uri,
        header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE},
    },
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use sha2::{Digest, Sha256};

use super::GoogleDriveStorageProvider;
use crate::google_drive::{
    GoogleDriveAccessToken, GoogleDriveAccessTokenCache, GoogleDriveAccessTokenSource,
};
use crate::{
    GoogleDriveObjectStore, GoogleDriveStorageConfig, LfsObject, LfsObjectSize, LfsOid,
    MetadataDatabase, ProviderFuture, StorageDeleteOutcome, StorageError, StorageProvider,
    StorageResult, StoredObject, StreamingStorageProvider,
};

mod storage_provider_contract {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/storage_provider_contract.rs"
    ));
}

use storage_provider_contract::assert_storage_provider_contract;

fn lfs_object_for_bytes(bytes: &[u8]) -> LfsObject {
    let oid = LfsOid::new(format!("{:x}", Sha256::digest(bytes)))
        .expect("test SHA-256 object id should parse");
    LfsObject::new(
        oid,
        LfsObjectSize::new(u64::try_from(bytes.len()).expect("test bytes should fit u64")),
    )
}

struct FixedDriveTokenSource;

impl GoogleDriveAccessTokenSource for FixedDriveTokenSource {
    fn access_token<'a>(
        &'a self,
        _storage: &'a GoogleDriveStorageConfig,
    ) -> ProviderFuture<'a, StorageResult<GoogleDriveAccessToken>> {
        Box::pin(async { Ok(GoogleDriveAccessToken::for_test("contract-access-token")) })
    }
}

struct CountingDriveTokenSource {
    calls: AtomicUsize,
}

impl GoogleDriveAccessTokenSource for CountingDriveTokenSource {
    fn access_token<'a>(
        &'a self,
        _storage: &'a GoogleDriveStorageConfig,
    ) -> ProviderFuture<'a, StorageResult<GoogleDriveAccessToken>> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(GoogleDriveAccessToken::for_test(format!(
                "contract-access-token-{call}"
            )))
        })
    }
}

#[derive(Clone)]
struct DriveContractObject {
    backend_id: String,
    repository_namespace: String,
    oid: String,
    size: u64,
    bytes: Vec<u8>,
}

struct DriveStorageContractServer {
    base_url: String,
    state: Arc<DriveStorageContractState>,
    task: tokio::task::JoinHandle<()>,
}

impl DriveStorageContractServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Drive contract server should bind");
        let address = listener
            .local_addr()
            .expect("Drive contract server address should be available");
        let base_url = format!("http://{address}");
        let state = Arc::new(DriveStorageContractState {
            base_url: base_url.clone(),
            objects: Mutex::new(BTreeMap::new()),
            pending_uploads: Mutex::new(BTreeMap::new()),
            next_upload_session_id: AtomicUsize::new(1),
            next_backend_id: AtomicUsize::new(1),
            upload_count: AtomicUsize::new(0),
        });
        let app = Router::new()
            .route("/drive/v3/files", get(drive_contract_list))
            .route(
                "/upload/drive/v3/files",
                post(drive_contract_initiate_upload),
            )
            .route(
                "/upload_session/{session_id}",
                put(drive_contract_complete_upload),
            )
            .route("/drive/v3/files/{file_id}", get(drive_contract_download))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("Drive contract server should run");
        });

        Self {
            base_url,
            state,
            task,
        }
    }

    fn upload_count(&self) -> usize {
        self.state.upload_count.load(Ordering::SeqCst)
    }
}

impl Drop for DriveStorageContractServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct DriveStorageContractState {
    base_url: String,
    objects: Mutex<BTreeMap<(String, String, u64), DriveContractObject>>,
    pending_uploads: Mutex<BTreeMap<String, DriveContractUploadSession>>,
    next_upload_session_id: AtomicUsize,
    next_backend_id: AtomicUsize,
    upload_count: AtomicUsize,
}

struct DriveContractUploadSession {
    metadata: serde_json::Value,
    bytes: Vec<u8>,
}

async fn drive_contract_list(
    State(state): State<Arc<DriveStorageContractState>>,
    uri: Uri,
) -> Response {
    let query = drive_contract_query(&uri);
    if query.contains("lfsCloudFolderKind") {
        let shard =
            drive_contract_property(&query, "lfsCloudShard").unwrap_or_else(|| "00".to_owned());
        return Json(serde_json::json!({
            "files": [{
                "id": format!("drive-shard-{shard}"),
                "name": format!("lfscloud-sha256-{shard}"),
                "mimeType": "application/vnd.google-apps.folder",
                "parents": ["drive-root"],
                "trashed": false,
                "appProperties": {
                    "lfsCloudFolderKind": "objectShard",
                    "lfsCloudShard": shard
                }
            }]
        }))
        .into_response();
    }

    let repository_namespace = drive_contract_property(&query, "lfsCloudRepoNamespace")
        .expect("Drive contract object query must include lfsCloudRepoNamespace");
    let oid = drive_contract_property(&query, "lfsCloudOid")
        .expect("Drive contract object query must include lfsCloudOid");
    let size = drive_contract_property(&query, "lfsCloudSize")
        .expect("Drive contract object query must include lfsCloudSize")
        .parse::<u64>()
        .expect("Drive contract object query size must parse");
    let objects = state
        .objects
        .lock()
        .expect("Drive contract objects lock should not poison");
    let files = objects
        .get(&(repository_namespace, oid, size))
        .map_or_else(Vec::new, |object| vec![drive_contract_object_json(object)]);
    Json(serde_json::json!({ "files": files })).into_response()
}

async fn drive_contract_initiate_upload(
    State(state): State<Arc<DriveStorageContractState>>,
    body: Bytes,
) -> Response {
    let metadata: serde_json::Value =
        serde_json::from_slice(&body).expect("Drive contract upload metadata should be JSON");
    let session_id = format!(
        "session-{}",
        state.next_upload_session_id.fetch_add(1, Ordering::SeqCst)
    );
    state
        .pending_uploads
        .lock()
        .expect("Drive contract pending uploads lock should not poison")
        .insert(
            session_id.clone(),
            DriveContractUploadSession {
                metadata,
                bytes: Vec::new(),
            },
        );

    let mut response = StatusCode::OK.into_response();
    response.headers_mut().insert(
        LOCATION,
        HeaderValue::from_str(&format!("{}/upload_session/{session_id}", state.base_url))
            .expect("Drive contract upload location should be a valid header"),
    );
    response
}

async fn drive_contract_complete_upload(
    AxumPath(session_id): AxumPath<String>,
    State(state): State<Arc<DriveStorageContractState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_range = headers
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .expect("Drive contract upload must include a valid Content-Range");
    let mut pending_uploads = state
        .pending_uploads
        .lock()
        .expect("Drive contract pending uploads lock should not poison");
    let session = pending_uploads
        .get_mut(&session_id)
        .expect("Drive contract upload session should have metadata");
    let properties = &session.metadata["appProperties"];
    let repository_namespace = properties["lfsCloudRepoNamespace"]
        .as_str()
        .expect("Drive contract namespace should be present")
        .to_owned();
    let oid = properties["lfsCloudOid"]
        .as_str()
        .expect("Drive contract OID should be present")
        .to_owned();
    let size = properties["lfsCloudSize"]
        .as_str()
        .expect("Drive contract size should be present")
        .parse::<u64>()
        .expect("Drive contract size should parse");
    if content_range == format!("bytes */{size}") {
        return drive_contract_incomplete_upload_response(session.bytes.len());
    }
    let (start, end, total) = drive_contract_upload_range(content_range);
    assert_eq!(
        total, size,
        "Drive contract upload total must match metadata"
    );
    assert_eq!(
        start,
        u64::try_from(session.bytes.len()).expect("session byte count should fit u64"),
        "Drive contract upload chunks must be contiguous"
    );
    assert_eq!(
        end - start + 1,
        u64::try_from(body.len()).expect("chunk length should fit u64"),
        "Drive contract Content-Range must match the chunk body"
    );
    session.bytes.extend_from_slice(&body);
    let committed_size =
        u64::try_from(session.bytes.len()).expect("session byte count should fit u64");
    assert!(
        committed_size <= size,
        "Drive contract upload must not exceed its declared size"
    );
    if committed_size < size {
        return drive_contract_incomplete_upload_response(session.bytes.len());
    }
    let completed = pending_uploads
        .remove(&session_id)
        .expect("completed Drive contract session should still exist");
    drop(pending_uploads);
    let backend_id = format!(
        "drive-contract-{}",
        state.next_backend_id.fetch_add(1, Ordering::SeqCst)
    );
    let object = DriveContractObject {
        backend_id,
        repository_namespace: repository_namespace.clone(),
        oid: oid.clone(),
        size,
        bytes: completed.bytes,
    };
    state
        .objects
        .lock()
        .expect("Drive contract objects lock should not poison")
        .insert((repository_namespace, oid, size), object.clone());
    state.upload_count.fetch_add(1, Ordering::SeqCst);

    (
        StatusCode::CREATED,
        [(CONTENT_TYPE, "application/json")],
        Json(drive_contract_object_json(&object)),
    )
        .into_response()
}

fn drive_contract_upload_range(value: &str) -> (u64, u64, u64) {
    let (range, total) = value
        .strip_prefix("bytes ")
        .and_then(|value| value.split_once('/'))
        .expect("Drive contract Content-Range must use bytes start-end/total");
    let (start, end) = range
        .split_once('-')
        .expect("Drive contract Content-Range must include a byte range");
    (
        start
            .parse::<u64>()
            .expect("Drive contract range start should parse"),
        end.parse::<u64>()
            .expect("Drive contract range end should parse"),
        total
            .parse::<u64>()
            .expect("Drive contract range total should parse"),
    )
}

fn drive_contract_incomplete_upload_response(committed_size: usize) -> Response {
    let mut response = StatusCode::from_u16(308)
        .expect("308 should be a valid status")
        .into_response();
    if committed_size > 0 {
        response.headers_mut().insert(
            RANGE,
            HeaderValue::from_str(&format!("bytes=0-{}", committed_size - 1))
                .expect("Drive contract committed range should be a valid header"),
        );
    }
    response
}

async fn drive_contract_download(
    AxumPath(file_id): AxumPath<String>,
    State(state): State<Arc<DriveStorageContractState>>,
    uri: Uri,
) -> Response {
    let object = state
        .objects
        .lock()
        .expect("Drive contract objects lock should not poison")
        .values()
        .find(|object| object.backend_id == file_id)
        .cloned();
    let Some(object) = object else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if drive_contract_query_pair(&uri, "alt").as_deref() == Some("media") {
        let mut response = (
            StatusCode::OK,
            [(CONTENT_TYPE, "application/octet-stream")],
            object.bytes,
        )
            .into_response();
        response.headers_mut().insert(
            CONTENT_LENGTH,
            HeaderValue::from_str(&object.size.to_string())
                .expect("Drive contract content length should be a valid header"),
        );
        response
    } else {
        Json(drive_contract_object_json(&object)).into_response()
    }
}

fn drive_contract_query(uri: &Uri) -> String {
    drive_contract_query_pair(uri, "q").unwrap_or_default()
}

fn drive_contract_query_pair(uri: &Uri, expected_key: &str) -> Option<String> {
    url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .find_map(|(key, value)| (key == expected_key).then(|| value.into_owned()))
}

fn drive_contract_property(query: &str, key: &str) -> Option<String> {
    let marker = format!("key='{key}' and value='");
    query
        .split_once(&marker)?
        .1
        .split_once('\'')
        .map(|(value, _)| value.to_owned())
}

fn drive_contract_object_json(object: &DriveContractObject) -> serde_json::Value {
    let oid_prefix = object
        .oid
        .get(..2)
        .expect("Drive contract OID must contain a two-character shard prefix");
    serde_json::json!({
        "id": object.backend_id,
        "name": format!("sha256-{}-{}.lfs", object.oid, object.size),
        "size": object.size.to_string(),
        "parents": [format!("drive-shard-{oid_prefix}")],
        "trashed": false,
        "appProperties": {
            "lfsCloudVersion": "1",
            "lfsCloudRepoNamespace": object.repository_namespace,
            "lfsCloudOid": object.oid,
            "lfsCloudSize": object.size.to_string()
        }
    })
}

#[tokio::test]
async fn google_drive_object_store_satisfies_shared_storage_contract() {
    let server = DriveStorageContractServer::start().await;
    let store = GoogleDriveObjectStore::with_api_base_url(
        drive_contract_storage_config(),
        "github.com/owner/repo",
        GoogleDriveAccessToken::for_test("contract-access-token"),
        &server.base_url,
    )
    .expect("Drive contract store should build");

    let report = assert_storage_provider_contract(
        &store,
        "github.com/owner/repo",
        "github.com/owner/isolated",
    )
    .await;

    assert!(!report.isolated_object_was_created);
    assert!(matches!(
        report.deletion,
        StorageDeleteOutcome::Retained { .. }
    ));
    assert_eq!(
        server.upload_count(),
        1,
        "verified idempotent re-upload must not create another Drive object"
    );
}

#[tokio::test]
async fn configured_google_drive_storage_satisfies_shared_storage_contract() {
    let server = DriveStorageContractServer::start().await;
    let storage = GoogleDriveStorageProvider::with_test_dependencies(
        drive_contract_storage_config(),
        "github.com/owner/repo",
        Arc::new(FixedDriveTokenSource),
        GoogleDriveAccessTokenCache::default(),
        Arc::new(MetadataDatabase::open_in_memory().expect("Drive contract metadata should open")),
        Some(server.base_url.clone()),
    );

    let report = assert_storage_provider_contract(
        &storage,
        "github.com/owner/repo",
        "github.com/owner/isolated",
    )
    .await;

    assert!(!report.isolated_object_was_created);
    assert!(matches!(
        report.deletion,
        StorageDeleteOutcome::Retained { .. }
    ));
    assert_eq!(
        server.upload_count(),
        1,
        "configured provider's locked idempotent re-upload must reuse the Drive object"
    );
}

#[tokio::test]
async fn configured_google_drive_streaming_rejects_another_repository_namespace() {
    let object = lfs_object_for_bytes(b"configured provider namespace isolation");
    let storage = GoogleDriveStorageProvider::with_test_dependencies(
        drive_contract_storage_config(),
        "github.com/owner/repo-a",
        Arc::new(FixedDriveTokenSource),
        GoogleDriveAccessTokenCache::default(),
        Arc::new(MetadataDatabase::open_in_memory().expect("Drive metadata should open")),
        Some("http://127.0.0.1:1".to_owned()),
    );
    let stored_object = StoredObject::new(
        "drive-user-a",
        "github.com/owner/repo-b",
        object.clone(),
        "foreign-drive-file",
    );

    let error = match StreamingStorageProvider::download_object_response(
        &storage,
        "github.com/owner/repo-b",
        &object,
        stored_object,
    )
    .await
    {
        Ok(_) => panic!("configured provider should reject another repository namespace"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StorageError::RepositoryNamespaceMismatch { ref provider }
            if provider == "drive-user-a"
    ));
}

#[tokio::test]
async fn configured_google_drive_upload_acquires_token_after_upload_lock() {
    let server = DriveStorageContractServer::start().await;
    let source_root = tempfile::tempdir().expect("provider source root should be created");
    let source = source_root.path().join("object.bin");
    let object_bytes = b"migration upload lock token refresh";
    fs::write(&source, object_bytes).expect("provider source should be written");
    let object = lfs_object_for_bytes(object_bytes);
    let repository_namespace = "github.com/owner/repo";
    let storage_config = drive_contract_storage_config();
    let metadata = Arc::new(
        MetadataDatabase::open(source_root.path().join("metadata.sqlite3"))
            .expect("provider metadata should open"),
    );
    let held_lock = metadata
        .acquire_object_upload_lock(
            repository_namespace.to_owned(),
            storage_config.id.clone(),
            object.clone(),
        )
        .await
        .expect("migration upload lock should be acquired")
        .expect("file-backed metadata should return an upload lock");
    let token_source = Arc::new(CountingDriveTokenSource {
        calls: AtomicUsize::new(0),
    });
    let storage = GoogleDriveStorageProvider::with_test_dependencies(
        storage_config,
        repository_namespace,
        token_source.clone(),
        GoogleDriveAccessTokenCache::default(),
        metadata,
        Some(server.base_url.clone()),
    );

    let upload = tokio::spawn(async move {
        StorageProvider::upload_object(&storage, repository_namespace, &object, &source).await
    });
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        token_source.calls.load(Ordering::SeqCst),
        0,
        "provider must not capture a Drive token before the upload lock is available"
    );

    drop(held_lock);
    tokio::time::timeout(Duration::from_secs(10), upload)
        .await
        .expect("provider upload should complete after the lock is released")
        .expect("provider upload task should join")
        .expect("provider upload should succeed");
    assert_eq!(
        token_source.calls.load(Ordering::SeqCst),
        1,
        "provider should acquire a current Drive token after the upload lock"
    );
}

#[tokio::test]
async fn server_google_drive_provider_uses_the_server_owned_upload_lock() {
    let server = DriveStorageContractServer::start().await;
    let source_root = tempfile::tempdir().expect("provider source root should be created");
    let source = source_root.path().join("object.bin");
    let object_bytes = b"server-owned upload lock";
    fs::write(&source, object_bytes).expect("provider source should be written");
    let object = lfs_object_for_bytes(object_bytes);
    let repository_namespace = "github.com/owner/repo";
    let storage_config = drive_contract_storage_config();
    let metadata = Arc::new(
        MetadataDatabase::open(source_root.path().join("metadata.sqlite3"))
            .expect("provider metadata should open"),
    );
    let held_lock = metadata
        .acquire_object_upload_lock(
            repository_namespace.to_owned(),
            storage_config.id.clone(),
            object.clone(),
        )
        .await
        .expect("server upload lock should be acquired")
        .expect("file-backed metadata should return an upload lock");
    let storage = GoogleDriveStorageProvider::with_test_dependencies(
        storage_config,
        repository_namespace,
        Arc::new(FixedDriveTokenSource),
        GoogleDriveAccessTokenCache::default(),
        metadata,
        Some(server.base_url.clone()),
    )
    .without_provider_upload_lock();

    tokio::time::timeout(
        Duration::from_secs(10),
        StorageProvider::upload_object(&storage, repository_namespace, &object, &source),
    )
    .await
    .expect("server provider must not reacquire the lock already held by the handler")
    .expect("server provider upload should succeed");
    drop(held_lock);

    assert_eq!(server.upload_count(), 1);
}

fn drive_contract_storage_config() -> GoogleDriveStorageConfig {
    GoogleDriveStorageConfig {
        id: "drive-user-a".to_owned(),
        credentials: crate::GoogleDriveGcloudCredentialsConfig {
            config_dir: ".gcloud-drive".into(),
            executable: "gcloud".into(),
        },
        root_folder_id: "drive-root".to_owned(),
        display_name: None,
    }
}
