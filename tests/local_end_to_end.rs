//! Local end-to-end coverage for the fake-provider MVP path.
//!
//! These tests keep real GitHub and Google Drive services out of the loop while
//! exercising repository initialization, provider authorization, object
//! transfer, and checkout materialization. The TCP-boundary test additionally
//! drives those routes through the real Git LFS client.

mod support;

use std::{
    fs,
    path::Path,
    process::{Command, Output},
    sync::Arc,
};

use axum::{
    body::{Body, to_bytes},
    http::{
        Method, Request, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE},
    },
};
use lfscloud::{
    GitCredentialApproval, GitHubPersonalAccessToken, GitLfsConfigTarget, GitRepository,
    LfsBatchResponse, LfsInitRoute, LocalCacheDehydrationStatus, LocalCacheLayout,
    LocalLfsSessionStore, RepositoryPermission, RepositoryUser, ServerConfig, ServerError,
    lfs_server_router_with_provider_adapters,
};
use support::{
    FakeRepositoryProvider, FakeStorageProvider, TempGitRepo, lfs_object_for_bytes,
    lfs_pointer_file,
};
use tower::ServiceExt;
use url::Url;

const TEST_DOWNLOAD_BODY_LIMIT: usize = 4 * 1024 * 1024;
const TEST_BATCH_BODY_LIMIT: usize = 64 * 1024;
const TEST_REPOSITORY_NAMESPACE: &str = "github-main:owner/repo";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_git_lfs_push_fetch_and_checkout_cross_the_tcp_boundary() {
    assert_command_success(
        Command::new("git").args(["lfs", "version"]),
        "Git LFS prerequisite check",
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("local Git LFS test listener should bind");
    let address = listener
        .local_addr()
        .expect("local Git LFS test listener address should resolve");
    let server_url = format!("http://{address}");
    let lfs_url = format!("{server_url}/github.com/owner/repo.git/info/lfs");

    let github = Arc::new(FakeRepositoryProvider::new("github-main"));
    github.add_repository("github.com", "owner", "repo", Some("8675309".to_owned()));
    github.grant_permission(
        "github.com",
        "owner",
        "repo",
        "octocat",
        RepositoryPermission::Write,
    );
    let sessions = LocalLfsSessionStore::new();
    let session = sessions
        .issue_session_with_github_pat(
            &RepositoryUser::new("github-main", "octocat", Some("user-123".to_owned())),
            ["repo"],
            GitHubPersonalAccessToken::from_secret("fake-provider-token")
                .expect("fake provider token should parse"),
        )
        .expect("local LFS session should be issued");
    let drive = Arc::new(FakeStorageProvider::new("drive-user-a"));
    let router = lfs_server_router_with_provider_adapters(
        local_server_config_for_public_url(&server_url),
        sessions,
        github,
        drive.clone(),
    )
    .expect("fake-provider router should build for the TCP listener");

    let source = TempGitRepo::new();
    let bare_remote = tempfile::tempdir().expect("bare remote tempdir should be created");
    assert_command_success(
        Command::new("git")
            .args(["init", "--bare", "--initial-branch=main"])
            .arg(bare_remote.path()),
        "bare Git remote initialization",
    );
    source.git(["lfs", "install", "--local"]);
    source.git(["remote", "add", "origin", path_str(bare_remote.path())]);
    source.git(["config", "--file", ".lfsconfig", "lfs.url", &lfs_url]);
    source.write_file(
        ".gitattributes",
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
    );
    let bytes = b"real Git LFS bytes crossing an ephemeral TCP listener";
    fs::create_dir_all(source.path().join("assets"))
        .expect("source asset directory should be created");
    fs::write(source.path().join("assets/model.bin"), bytes)
        .expect("source LFS bytes should be written");
    source.commit_all("Add LFS fixture");
    configure_test_credential(&source, &lfs_url, session.token.clone());

    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("local Git LFS test server should run");
    });

    assert_command_success(
        Command::new("git")
            .env("GIT_TERMINAL_PROMPT", "0")
            .arg("-C")
            .arg(source.path())
            .args(["lfs", "push", "origin", "main"]),
        "Git LFS push through lfscloud",
    );
    let object = lfs_object_for_bytes(bytes);
    assert_eq!(
        drive.object_bytes(TEST_REPOSITORY_NAMESPACE, &object),
        Some(bytes.to_vec())
    );
    source.git(["push", "--no-verify", "origin", "main"]);

    let checkout_parent = tempfile::tempdir().expect("checkout tempdir should be created");
    let checkout = checkout_parent.path().join("checkout");
    assert_command_success(
        Command::new("git")
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .args(["clone"])
            .arg(bare_remote.path())
            .arg(&checkout),
        "Git LFS pointer-only clone",
    );
    assert_eq!(
        fs::read(checkout.join("assets/model.bin"))
            .expect("pointer-only checkout file should be readable"),
        lfs_pointer_file(object.oid.as_hex(), object.size.bytes()).as_bytes()
    );
    assert_command_success(
        Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["lfs", "install", "--local"]),
        "checkout-local Git LFS installation",
    );
    configure_test_credential_path(&checkout, &lfs_url, session.token);
    assert_command_success(
        Command::new("git")
            .env("GIT_TERMINAL_PROMPT", "0")
            .arg("-C")
            .arg(&checkout)
            .args(["lfs", "fetch", "origin", "main"]),
        "Git LFS fetch through lfscloud",
    );
    assert_command_success(
        Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["lfs", "checkout", "assets/model.bin"]),
        "Git LFS checkout from fetched object bytes",
    );
    assert_eq!(
        fs::read(checkout.join("assets/model.bin"))
            .expect("materialized Git LFS checkout should be readable"),
        bytes
    );

    server.abort();
    let _ = server.await;
}

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
    github.add_repository("github.com", "owner", "repo", Some("8675309".to_owned()));
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
        .issue_session_with_github_pat(
            &user,
            ["repo"],
            GitHubPersonalAccessToken::from_secret("fake-provider-token")
                .expect("fake provider token should parse"),
        )
        .expect("local LFS session should be issued");

    let bytes = b"large model bytes fetched through lfscloud";
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
    assert_eq!(
        drive.object_bytes(TEST_REPOSITORY_NAMESPACE, &object),
        Some(bytes.to_vec())
    );

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

#[tokio::test]
async fn shared_storage_provider_does_not_expose_another_repository_object() {
    let github = Arc::new(FakeRepositoryProvider::new("github-main"));
    for (name, stable_id) in [("repo", "8675309"), ("repo-b", "8675310")] {
        github.add_repository("github.com", "owner", name, Some(stable_id.to_owned()));
        github.grant_permission(
            "github.com",
            "owner",
            name,
            "octocat",
            RepositoryPermission::Write,
        );
    }
    let user = RepositoryUser::new("github-main", "octocat", Some("user-123".to_owned()));
    let sessions = LocalLfsSessionStore::new();
    let session = sessions
        .issue_session_with_github_pat(
            &user,
            ["repo"],
            GitHubPersonalAccessToken::from_secret("fake-provider-token")
                .expect("fake provider token should parse"),
        )
        .expect("local LFS session should be issued");
    let drive = Arc::new(FakeStorageProvider::new("drive-user-a"));
    let router = lfs_server_router_with_provider_adapters(
        local_server_config(),
        sessions,
        github,
        drive.clone(),
    )
    .expect("two repositories may share one namespaced storage provider");
    let bytes = b"repository A private LFS bytes";
    let object = lfs_object_for_bytes(bytes);

    let upload_batch = router
        .clone()
        .oneshot(lfs_json_request(
            Method::POST,
            "/github.com/owner/repo.git/info/lfs/objects/batch",
            session.token.as_str(),
            lfs_batch_request("upload", &object),
        ))
        .await
        .expect("repository A upload batch should complete");
    let upload_batch = lfs_batch_response(upload_batch).await;
    let upload_href = upload_batch.objects[0]
        .actions
        .get("upload")
        .expect("repository A should receive an upload action")
        .href
        .clone();
    let upload = router
        .clone()
        .oneshot(authenticated_request(
            Method::PUT,
            &action_path_and_query(&upload_href),
            session.token.as_str(),
            Body::from(bytes.to_vec()),
        ))
        .await
        .expect("repository A upload should complete");
    assert_eq!(upload.status(), StatusCode::OK);
    assert_eq!(
        drive.object_bytes("github-main:owner/repo", &object),
        Some(bytes.to_vec())
    );
    assert_eq!(
        drive.object_bytes("github-main:owner/repo-b", &object),
        None
    );

    let repository_b_batch = router
        .oneshot(lfs_json_request(
            Method::POST,
            "/github.com/owner/repo-b.git/info/lfs/objects/batch",
            session.token.as_str(),
            lfs_batch_request("download", &object),
        ))
        .await
        .expect("authorized repository B batch should complete");
    assert_eq!(repository_b_batch.status(), StatusCode::OK);
    let repository_b_batch = lfs_batch_response(repository_b_batch).await;
    assert!(repository_b_batch.objects[0].actions.is_empty());
    assert_eq!(
        repository_b_batch.objects[0]
            .error
            .as_ref()
            .expect("repository B object should remain missing")
            .code,
        404
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
    local_server_config_for_public_url("http://127.0.0.1:8080")
}

fn local_server_config_for_public_url(public_url: &str) -> ServerConfig {
    ServerConfig::load_from_str(&format!(
        r#"
server:
  public_url: {public_url}
repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
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
  - id: github-main:owner/repo-b
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo-b
    provider_repository_id: "8675310"
    storage_provider: drive-user-a
"#
    ))
    .expect("local fake-provider server config should load")
}

fn configure_test_credential(
    repository: &TempGitRepo,
    lfs_url: &str,
    token: lfscloud::LfsSessionToken,
) {
    repository.git([
        "config",
        "--local",
        "credential.helper",
        "store --file=.git/lfscloud-test-credentials",
    ]);
    GitCredentialApproval::new(lfs_url, token)
        .expect("loopback LFS credential URL should be accepted")
        .approve_in_dir(repository.path())
        .expect("source repository credential should be approved");
}

fn configure_test_credential_path(
    repository: &Path,
    lfs_url: &str,
    token: lfscloud::LfsSessionToken,
) {
    assert_command_success(
        Command::new("git").arg("-C").arg(repository).args([
            "config",
            "--local",
            "credential.helper",
            "store --file=.git/lfscloud-test-credentials",
        ]),
        "checkout-local credential helper configuration",
    );
    GitCredentialApproval::new(lfs_url, token)
        .expect("loopback LFS credential URL should be accepted")
        .approve_in_dir(repository)
        .expect("checkout repository credential should be approved");
}

fn assert_command_success(command: &mut Command, description: &str) -> Output {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{description} should start: {error}"));
    assert!(
        output.status.success(),
        "{description} should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn path_str(path: &Path) -> &str {
    path.to_str()
        .expect("temporary test repository paths should be valid UTF-8")
}

fn lfs_batch_request(operation: &str, object: &lfscloud::LfsObject) -> String {
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

fn assert_action_href(href: &str, object: &lfscloud::LfsObject) {
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
