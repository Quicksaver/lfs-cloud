//! Local end-to-end coverage for the fake-provider MVP path.
//!
//! This test keeps real GitHub, Google Drive, and Git LFS binaries out of the
//! loop while still exercising the repository-init, provider authorization,
//! object transfer, and checkout-materialization boundaries together.

mod support;

use std::{fs, sync::Arc};

use axum::{
    body::{Body, to_bytes},
    http::{
        Method, Request, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE},
    },
};
use lfs_cloud::{
    GitLfsConfigTarget, GitRepository, LfsBatchResponse, LfsInitRoute, LocalCacheDehydrationStatus,
    LocalCacheLayout, LocalLfsSessionStore, RepositoryPermission, RepositoryUser, ServerConfig,
    ServerError, lfs_server_router_with_provider_adapters,
};
use support::{
    FakeRepositoryProvider, FakeStorageProvider, TempGitRepo, lfs_object_for_bytes,
    lfs_pointer_file,
};
use tower::ServiceExt;
use url::Url;

const TEST_DOWNLOAD_BODY_LIMIT: usize = 4 * 1024 * 1024;
const TEST_BATCH_BODY_LIMIT: usize = 64 * 1024;

#[tokio::test]
async fn local_init_upload_download_and_checkout_flow_uses_fake_providers() {
    let repo = TempGitRepo::new();
    repo.git([
        "remote",
        "add",
        "origin",
        "https://github.com/owner/repo.git",
    ]);
    repo.write_file(
        ".gitattributes",
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
    );

    let repository = GitRepository::discover(repo.path()).expect("repository should be discovered");
    let route = LfsInitRoute::resolve("http://127.0.0.1:8080", &repository.remote)
        .expect("init route should resolve");
    let init_change = repository
        .write_lfs_url(GitLfsConfigTarget::WorktreeFile, &route.lfs_url)
        .expect("init should write .lfsconfig");

    assert_eq!(init_change.previous_url, None);
    assert_eq!(
        repo.git(["config", "--file", ".lfsconfig", "--get", "lfs.url"])
            .stdout,
        b"http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs\n"
    );

    let github = Arc::new(FakeRepositoryProvider::new("github-main"));
    github.add_repository("github.com", "owner", "repo", Some("repo-123".to_owned()));
    github.grant_permission(
        "github.com",
        "owner",
        "repo",
        "octocat",
        RepositoryPermission::Write,
    );
    let user = RepositoryUser::new("github-main", "octocat", Some("user-123".to_owned()));
    let sessions = LocalLfsSessionStore::new();
    let session = sessions
        .issue_session(&user, ["repo"])
        .expect("local LFS session should be issued");

    let bytes = b"large model bytes fetched through lfs-cloud";
    let object = lfs_object_for_bytes(bytes);
    let drive = Arc::new(FakeStorageProvider::new("drive-user-a"));
    let router = lfs_server_router_with_provider_adapters(
        local_server_config(),
        sessions,
        github,
        drive.clone(),
    )
    .expect("fake-provider router should validate injected provider IDs");

    let upload_batch = router
        .clone()
        .oneshot(lfs_json_request(
            Method::POST,
            "/github.com/owner/repo.git/info/lfs/objects/batch",
            session.token.as_str(),
            lfs_batch_request("upload", &object),
        ))
        .await
        .expect("upload batch request should route through the server");
    assert_eq!(upload_batch.status(), StatusCode::OK);
    let upload_batch = lfs_batch_response(upload_batch).await;
    let upload_href = upload_batch.objects[0]
        .actions
        .get("upload")
        .expect("missing fake object should receive an upload action")
        .href
        .clone();
    assert_action_href(&upload_href, &object);

    let upload = router
        .clone()
        .oneshot(authenticated_request(
            Method::PUT,
            &action_path_and_query(&upload_href),
            session.token.as_str(),
            Body::from(bytes.to_vec()),
        ))
        .await
        .expect("upload action should route through the server");
    assert_eq!(upload.status(), StatusCode::OK);
    assert_eq!(drive.object_bytes(&object), Some(bytes.to_vec()));

    let cache_root = tempfile::tempdir().expect("cache tempdir should be created");
    let cache = LocalCacheLayout::new(cache_root.path());
    let cached_object_path = cache.object_path(&object);
    let download_batch = router
        .clone()
        .oneshot(lfs_json_request(
            Method::POST,
            "/github.com/owner/repo.git/info/lfs/objects/batch",
            session.token.as_str(),
            lfs_batch_request("download", &object),
        ))
        .await
        .expect("download batch request should route through the server");
    assert_eq!(download_batch.status(), StatusCode::OK);
    let download_batch = lfs_batch_response(download_batch).await;
    let download_href = download_batch.objects[0]
        .actions
        .get("download")
        .expect("uploaded fake object should receive a download action")
        .href
        .clone();
    assert_action_href(&download_href, &object);

    let download = router
        .oneshot(authenticated_request(
            Method::GET,
            &action_path_and_query(&download_href),
            session.token.as_str(),
            Body::empty(),
        ))
        .await
        .expect("download action should route through the server");
    assert_eq!(download.status(), StatusCode::OK);
    let downloaded_bytes = to_bytes(download.into_body(), TEST_DOWNLOAD_BODY_LIMIT)
        .await
        .expect("downloaded object body should collect");
    let downloaded_object_path = cache_root.path().join("downloaded-model.bin");
    fs::write(&downloaded_object_path, &downloaded_bytes)
        .expect("downloaded bytes should be written for cache ingest");
    let dehydration = cache
        .dehydrate_file(&object, &downloaded_object_path)
        .expect("downloaded bytes should be verified into the shared cache");
    assert_eq!(
        dehydration.status,
        LocalCacheDehydrationStatus::CachedAndReplacedWithPointer
    );
    assert_eq!(dehydration.cache_path, cached_object_path);
    assert_eq!(
        fs::read(&cached_object_path).expect("cached object should be readable"),
        bytes
    );

    let checkout_path = repo.write_file(
        "assets/model.bin",
        &lfs_pointer_file(object.oid.as_hex(), object.size.bytes()),
    );
    let materialized = cache
        .hydrate_pointer_file(&checkout_path)
        .expect("checkout pointer should hydrate from verified cache bytes");

    assert_eq!(materialized.object, object);
    assert_eq!(
        fs::read(repo.path().join("assets/model.bin"))
            .expect("hydrated checkout bytes should be readable"),
        bytes
    );
}

#[test]
fn provider_adapter_router_rejects_mismatched_provider_ids() {
    let sessions = LocalLfsSessionStore::new();
    let drive = Arc::new(FakeStorageProvider::new("drive-user-a"));
    let error = match lfs_server_router_with_provider_adapters(
        local_server_config(),
        sessions,
        Arc::new(FakeRepositoryProvider::new("github-other")),
        drive,
    ) {
        Ok(_) => panic!("mismatched repository provider should fail before routing"),
        Err(error) => error,
    };

    assert!(matches!(error, ServerError::InvalidConfiguration { .. }));
    assert!(
        error
            .to_string()
            .contains("injected provider is github-other")
    );

    let sessions = LocalLfsSessionStore::new();
    let github = Arc::new(FakeRepositoryProvider::new("github-main"));
    let error = match lfs_server_router_with_provider_adapters(
        local_server_config(),
        sessions,
        github,
        Arc::new(FakeStorageProvider::new("drive-other")),
    ) {
        Ok(_) => panic!("mismatched storage provider should fail before routing"),
        Err(error) => error,
    };

    assert!(matches!(error, ServerError::InvalidConfiguration { .. }));
    assert!(
        error
            .to_string()
            .contains("injected provider is drive-other")
    );
}

fn local_server_config() -> ServerConfig {
    ServerConfig::load_from_str(
        r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: test-client
    oauth_client_secret: test-secret
storage_providers:
  drive-user-a:
    type: google_drive
    credentials_ref: google-drive-user-a
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
    .expect("local fake-provider server config should load")
}

fn lfs_batch_request(operation: &str, object: &lfs_cloud::LfsObject) -> String {
    serde_json::json!({
        "operation": operation,
        "transfers": ["basic"],
        "objects": [
            {
                "oid": object.oid.as_hex(),
                "size": object.size.bytes(),
            }
        ],
    })
    .to_string()
}

fn lfs_json_request(
    method: Method,
    uri: &str,
    token: &str,
    body: impl Into<Body>,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header(CONTENT_TYPE, "application/vnd.git-lfs+json")
        .body(body.into())
        .expect("test Git LFS JSON request should build")
}

fn authenticated_request(
    method: Method,
    uri: &str,
    token: &str,
    body: impl Into<Body>,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .body(body.into())
        .expect("test authenticated request should build")
}

async fn lfs_batch_response(response: axum::response::Response) -> LfsBatchResponse {
    assert_eq!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/vnd.git-lfs+json")
    );
    let body = to_bytes(response.into_body(), TEST_BATCH_BODY_LIMIT)
        .await
        .expect("Git LFS batch response body should collect");

    serde_json::from_slice(&body).expect("response should be Git LFS batch JSON")
}

fn assert_action_href(href: &str, object: &lfs_cloud::LfsObject) {
    let url = Url::parse(href).expect("server action href should be an absolute URL");
    let expected_query = format!("size={}", object.size.bytes());

    assert_eq!(url.scheme(), "http");
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(url.port(), Some(8080));
    assert_eq!(
        url.path(),
        format!(
            "/github.com/owner/repo.git/info/lfs/objects/{}",
            object.oid.as_hex()
        )
    );
    assert_eq!(url.query(), Some(expected_query.as_str()));
}

fn action_path_and_query(href: &str) -> String {
    let url = Url::parse(href).expect("server action href should be an absolute URL");
    let query = url
        .query()
        .expect("server action href should include object size");

    format!("{}?{query}", url.path())
}
