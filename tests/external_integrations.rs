//! Gated external-service integration checks.
//!
//! These tests are ignored by default because they create and delete real
//! provider resources. The scripts in `scripts/manual/` set up the exact cargo
//! invocation and keep normal development runs deterministic.

use std::{
    collections::BTreeMap,
    env, fs,
    net::TcpListener as StdTcpListener,
    process,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use lfs_cloud::{
    GitHubOAuthAccessToken, GitHubProviderConfig, GitHubRepositoryPermissionClient,
    GitHubUserClient, GoogleDriveAccessToken, GoogleDriveCredential, GoogleDriveRootValidator,
    GoogleDriveStorageConfig, GoogleDriveTokenRefresher, LFS_BASIC_TRANSFER, LfsBatchAction,
    LfsBatchHashAlgorithm, LfsBatchOperation, LfsBatchRequest, LfsBatchResponse, LfsObject,
    LfsObjectSize, LfsOid, LocalLfsSessionStore, MetadataDatabase,
    MetadataObjectVerificationStatus, RepositoryIdentity, RepositoryPermission, RepositoryUser,
    ServeOptions, ServerConfig, serve,
};
use reqwest::{
    Client, Method, StatusCode,
    header::{ACCEPT, CONTENT_TYPE, USER_AGENT},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

const GITHUB_API_URL: &str = "https://api.github.com";
const GITHUB_ACCEPT: &str = "application/vnd.github+json";
const DRIVE_API_URL: &str = "https://www.googleapis.com/drive/v3";
const DRIVE_FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";
const LIVE_GITHUB_PROVIDER_ID: &str = "github-live";
const LIVE_DRIVE_PROVIDER_ID: &str = "drive-live";
const LIVE_OAUTH_CLIENT_SECRET: &str = "live-external-integration-session-key";
const LIVE_DRIVE_CREDENTIAL_ENV: &str = "LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_JSON";

#[tokio::test]
#[ignore = "requires LFS_CLOUD_RUN_GITHUB_INTEGRATION=1 and a disposable-capable GitHub token"]
async fn github_disposable_repo_permission_check() {
    if skip_unless_enabled("LFS_CLOUD_RUN_GITHUB_INTEGRATION") {
        return;
    }

    let token = required_env("LFS_CLOUD_GITHUB_TOKEN");
    let repo_owner = env::var("LFS_CLOUD_GITHUB_OWNER").ok();
    let api_url =
        env::var("LFS_CLOUD_GITHUB_API_URL").unwrap_or_else(|_| GITHUB_API_URL.to_owned());
    let repo_host =
        env::var("LFS_CLOUD_GITHUB_HOST").unwrap_or_else(|_| github_host_from_api_url(&api_url));
    let repo_name = disposable_name("lfs-cloud-it");
    let provider = GitHubProviderConfig {
        id: "github-main".to_owned(),
        api_url: api_url.clone(),
        oauth_client_id: "external-integration".to_owned(),
        oauth_client_secret: "external-integration".to_owned(),
        allow_insecure_http: false,
    };
    let oauth_token =
        GitHubOAuthAccessToken::from_secret(token.clone()).expect("GitHub token should validate");
    let http = Client::new();
    let created_repo =
        create_github_repo(&http, &api_url, &token, repo_owner.as_deref(), &repo_name)
            .await
            .expect("disposable GitHub repository should be created");

    let check_result = async {
        let user = GitHubUserClient::new()?
            .fetch_authenticated_user(&provider, &oauth_token)
            .await?;
        let repository = RepositoryIdentity {
            provider_id: provider.id.clone(),
            stable_id: Some(created_repo.id.to_string()),
            host: repo_host,
            owner: created_repo.owner.login.clone(),
            name: created_repo.name.clone(),
        };
        let authorization = GitHubRepositoryPermissionClient::new()?
            .check_permission(
                &provider,
                &oauth_token,
                &repository,
                &user,
                RepositoryPermission::Write,
            )
            .await?;

        Ok::<_, lfs_cloud::ServerError>(authorization)
    }
    .await;

    let delete_result = delete_github_repo(&http, &api_url, &token, &created_repo.full_name).await;
    delete_result.expect("disposable GitHub repository should be deleted");
    let authorization = check_result
        .expect("GitHub permission client should authorize disposable repo write access");
    assert_eq!(authorization.repository.owner, created_repo.owner.login);
    assert_eq!(authorization.repository.name, created_repo.name);
    assert_eq!(authorization.required, RepositoryPermission::Write);
    assert!(
        matches!(
            authorization.granted,
            RepositoryPermission::Write | RepositoryPermission::Admin
        ),
        "disposable repo creator should retain write/admin access"
    );
}

#[tokio::test]
#[ignore = "requires LFS_CLOUD_RUN_GOOGLE_DRIVE_INTEGRATION=1 and disposable Drive credentials"]
async fn google_drive_disposable_folder_root_validation() {
    if skip_unless_enabled("LFS_CLOUD_RUN_GOOGLE_DRIVE_INTEGRATION") {
        return;
    }

    let credential_json = google_drive_credential_json();
    let provider_id =
        env::var("LFS_CLOUD_GOOGLE_DRIVE_PROVIDER_ID").unwrap_or_else(|_| "drive-it".to_owned());
    let credential = GoogleDriveCredential::from_json(
        provider_id.clone(),
        "external-integration",
        &credential_json,
    )
    .expect("Google Drive credential JSON should validate");
    let access_token = GoogleDriveTokenRefresher::new()
        .expect("Google token refresher should build")
        .refresh_access_token(&credential)
        .await
        .expect("Google Drive refresh token should produce an access token");
    let http = Client::new();
    let folder_name = disposable_name("lfs-cloud-drive-it");
    let parent_folder_id = env::var("LFS_CLOUD_GOOGLE_DRIVE_PARENT_FOLDER_ID").ok();
    let folder = create_drive_folder(
        &http,
        access_token.as_str(),
        &folder_name,
        parent_folder_id.as_deref(),
    )
    .await
    .expect("disposable Google Drive folder should be created");

    let storage = GoogleDriveStorageConfig {
        id: provider_id,
        credential_ref: "external-integration".to_owned(),
        root_folder_id: folder.id.clone(),
        display_name: Some(folder.name.clone()),
    };
    let validation_result = GoogleDriveRootValidator::new()
        .expect("Drive root validator should build")
        .validate_root_folder(&storage, &access_token)
        .await;
    let delete_result = delete_drive_file(&http, access_token.as_str(), &folder.id).await;

    let validated = validation_result.expect("disposable Drive folder should validate as a root");
    assert_eq!(validated.id, folder.id);
    assert_eq!(validated.name, folder.name);
    assert!(
        validated.can_add_children,
        "newly-created disposable folder should accept child objects"
    );
    delete_result.expect("disposable Google Drive folder should be deleted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires LFS_CLOUD_RUN_LIVE_TRANSFER_INTEGRATION=1 plus disposable GitHub and Drive credentials"]
async fn live_server_upload_download_records_drive_and_sqlite_state() {
    if skip_unless_enabled("LFS_CLOUD_RUN_LIVE_TRANSFER_INTEGRATION") {
        return;
    }

    run_live_provider_transfer_scenario()
        .await
        .expect("live provider transfer scenario should pass and clean up its resources");
}

#[test]
fn live_server_transfer_config_fixture_uses_production_provider_ids() {
    let test_dir = tempfile::tempdir().expect("config fixture temp directory should be created");
    let config_path = test_dir.path().join("lfs-cloud.yml");
    let metadata_path = test_dir.path().join("metadata.sqlite3");
    let repository = GitHubCreatedRepo {
        id: 8675309,
        name: "repo".to_owned(),
        full_name: "owner/repo".to_owned(),
        owner: GitHubOwner {
            login: "owner".to_owned(),
        },
    };
    let folder = DriveFolder {
        id: "drive-root".to_owned(),
        name: "Drive root".to_owned(),
    };
    let repository_id = format!("{LIVE_GITHUB_PROVIDER_ID}:owner/repo");
    let config = live_server_config_json(
        "http://127.0.0.1:8080",
        8080,
        &metadata_path,
        GITHUB_API_URL,
        "github.com",
        &repository,
        &folder,
    );
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).expect("config fixture should serialize"),
    )
    .expect("config fixture should be written");

    let config = ServerConfig::load_from_path(config_path).expect("config fixture should load");

    assert!(
        config
            .repository_providers
            .contains_key(LIVE_GITHUB_PROVIDER_ID)
    );
    assert!(
        config
            .storage_providers
            .contains_key(LIVE_DRIVE_PROVIDER_ID)
    );
    assert_eq!(config.repositories[0].id, repository_id);
}

async fn run_live_provider_transfer_scenario() -> Result<(), String> {
    let github_token = required_env("LFS_CLOUD_GITHUB_TOKEN");
    let github_owner = env::var("LFS_CLOUD_GITHUB_OWNER").ok();
    let github_api_url =
        env::var("LFS_CLOUD_GITHUB_API_URL").unwrap_or_else(|_| GITHUB_API_URL.to_owned());
    let github_host = env::var("LFS_CLOUD_GITHUB_HOST")
        .unwrap_or_else(|_| github_host_from_api_url(&github_api_url));
    let drive_credential_json = required_env(LIVE_DRIVE_CREDENTIAL_ENV);
    let drive_credential = GoogleDriveCredential::from_json(
        LIVE_DRIVE_PROVIDER_ID,
        "external-live-transfer",
        &drive_credential_json,
    )
    .map_err(|error| format!("Google Drive credential JSON should validate: {error}"))?;
    let drive_access_token = GoogleDriveTokenRefresher::new()
        .map_err(|error| format!("Google token refresher should build: {error}"))?
        .refresh_access_token(&drive_credential)
        .await
        .map_err(|error| {
            format!("Google Drive refresh token should produce an access token: {error}")
        })?;
    let client = Client::new();
    let repository = create_github_repo(
        &client,
        &github_api_url,
        &github_token,
        github_owner.as_deref(),
        &disposable_name("lfs-cloud-live-it"),
    )
    .await?;
    let folder = match create_drive_folder(
        &client,
        drive_access_token.as_str(),
        &disposable_name("lfs-cloud-live-drive-it"),
        env::var("LFS_CLOUD_GOOGLE_DRIVE_PARENT_FOLDER_ID")
            .ok()
            .as_deref(),
    )
    .await
    {
        Ok(folder) => folder,
        Err(error) => {
            let cleanup = delete_github_repo(
                &client,
                &github_api_url,
                &github_token,
                &repository.full_name,
            )
            .await;
            return Err(combine_cleanup_error(error, "GitHub repository", cleanup));
        }
    };

    let scenario = exercise_live_server_transfer(
        &client,
        &github_api_url,
        &github_host,
        &github_token,
        &repository,
        &folder,
        &drive_access_token,
    )
    .await;
    let drive_cleanup = delete_drive_file(&client, drive_access_token.as_str(), &folder.id).await;
    let github_cleanup = delete_github_repo(
        &client,
        &github_api_url,
        &github_token,
        &repository.full_name,
    )
    .await;

    finish_scenario_with_cleanup(scenario, drive_cleanup, github_cleanup)
}

async fn exercise_live_server_transfer(
    client: &Client,
    github_api_url: &str,
    github_host: &str,
    github_token: &str,
    repository: &GitHubCreatedRepo,
    folder: &DriveFolder,
    drive_access_token: &GoogleDriveAccessToken,
) -> Result<(), String> {
    let test_dir = tempfile::tempdir()
        .map_err(|error| format!("live transfer temp directory should be created: {error}"))?;
    let metadata_path = test_dir.path().join("metadata.sqlite3");
    let config_path = test_dir.path().join("lfs-cloud.yml");
    let port = available_loopback_port()?;
    let server_url = format!("http://127.0.0.1:{port}");
    let repository_id = format!(
        "{LIVE_GITHUB_PROVIDER_ID}:{}/{}",
        repository.owner.login, repository.name
    );
    let config_json = live_server_config_json(
        &server_url,
        port,
        &metadata_path,
        github_api_url,
        github_host,
        repository,
        folder,
    );
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config_json)
            .map_err(|error| format!("live server config should serialize: {error}"))?,
    )
    .map_err(|error| format!("live server config should be written: {error}"))?;

    let config = ServerConfig::load_from_path(&config_path)
        .map_err(|error| format!("live server config should load: {error}"))?;
    let metadata = Arc::new(
        MetadataDatabase::open(&metadata_path)
            .map_err(|error| format!("live metadata database should open: {error}"))?,
    );
    metadata
        .sync_config(&config)
        .map_err(|error| format!("live config should synchronize to metadata: {error}"))?;
    let github_access_token = GitHubOAuthAccessToken::from_secret(github_token)
        .map_err(|error| format!("GitHub token should validate: {error}"))?;
    let github_user = GitHubUserClient::new()
        .map_err(|error| format!("GitHub user client should build: {error}"))?
        .fetch_authenticated_user(
            match &config.repository_providers[LIVE_GITHUB_PROVIDER_ID] {
                lfs_cloud::RepositoryProviderConfig::GitHub(provider) => provider,
            },
            &github_access_token,
        )
        .await
        .map_err(|error| format!("GitHub token should resolve a stable user: {error}"))?;
    let session = LocalLfsSessionStore::open_durable(
        Arc::clone(&metadata),
        LIVE_OAUTH_CLIENT_SECRET.as_bytes(),
    )
    .and_then(|sessions| {
        sessions.issue_session_with_github_token(&github_user, ["repo"], github_access_token)
    })
    .map_err(|error| format!("durable live LFS session should be issued: {error}"))?;

    let serve_options = ServeOptions::new(Some(config_path), None, None);
    let server = tokio::spawn(async move { serve(serve_options).await });
    let scenario = async {
        wait_for_live_server(port).await?;
        let batch_url = format!(
            "{server_url}{}/objects/batch",
            config.repositories[0].route_path()
        );
        transfer_live_object(
            client,
            &batch_url,
            session.token.as_str(),
            &repository_id,
            &github_user,
            &metadata,
            drive_access_token,
        )
        .await
    }
    .await;
    let server_finished = server.is_finished();
    server.abort();
    let server_result = server.await;

    if server_finished {
        return match server_result {
            Ok(Ok(())) => Err("live LFS server exited before the scenario completed".to_owned()),
            Ok(Err(error)) => Err(format!("live LFS server failed: {error}")),
            Err(error) => Err(format!("live LFS server task failed: {error}")),
        };
    }

    scenario
}

fn live_server_config_json(
    server_url: &str,
    port: u16,
    metadata_path: &std::path::Path,
    github_api_url: &str,
    github_host: &str,
    repository: &GitHubCreatedRepo,
    folder: &DriveFolder,
) -> Value {
    let repository_id = format!(
        "{LIVE_GITHUB_PROVIDER_ID}:{}/{}",
        repository.owner.login, repository.name
    );
    json!({
        "server": {
            "host": "127.0.0.1",
            "port": port,
            "public_url": server_url,
            "metadata_path": metadata_path.to_string_lossy(),
        },
        "repository_providers": {
            LIVE_GITHUB_PROVIDER_ID: {
                "type": "github",
                "api_url": github_api_url,
                "oauth_client_id": "live-external-integration",
                "oauth_client_secret": LIVE_OAUTH_CLIENT_SECRET,
            }
        },
        "storage_providers": {
            LIVE_DRIVE_PROVIDER_ID: {
                "type": "google_drive",
                "credentials_ref": format!("env:{LIVE_DRIVE_CREDENTIAL_ENV}"),
                "root_folder_id": folder.id,
                "display_name": folder.name,
            }
        },
        "repositories": [{
            "id": repository_id,
            "repo_provider": LIVE_GITHUB_PROVIDER_ID,
            "host": github_host,
            "owner": repository.owner.login,
            "name": repository.name,
            "provider_repository_id": repository.id.to_string(),
            "storage_provider": LIVE_DRIVE_PROVIDER_ID,
        }]
    })
}

async fn transfer_live_object(
    client: &Client,
    batch_url: &str,
    session_token: &str,
    repository_id: &str,
    github_user: &RepositoryUser,
    metadata: &MetadataDatabase,
    drive_access_token: &GoogleDriveAccessToken,
) -> Result<(), String> {
    let bytes = b"live GitHub-authorized LFS bytes stored and read through Google Drive";
    let object = lfs_object_for_bytes(bytes)?;
    let upload = request_batch_action(
        client,
        batch_url,
        session_token,
        LfsBatchOperation::Upload,
        &object,
        "upload",
    )
    .await?;
    let upload_response = action_request(client, Method::PUT, &upload)
        .header(CONTENT_TYPE, "application/octet-stream")
        .body(bytes.as_slice().to_vec())
        .send()
        .await
        .map_err(|error| format!("live LFS upload request should complete: {error}"))?;
    require_status(upload_response, StatusCode::OK, "live LFS upload").await?;

    let record = metadata
        .lookup_object(repository_id, LIVE_DRIVE_PROVIDER_ID, &object)
        .map_err(|error| format!("live upload metadata lookup should succeed: {error}"))?
        .ok_or_else(|| "live upload should record an SQLite object row".to_owned())?;
    require_equal(
        record.verification_status,
        MetadataObjectVerificationStatus::Verified,
        "SQLite verification status",
    )?;
    require_equal(
        record.created_by.clone(),
        github_user.clone(),
        "SQLite object creator",
    )?;
    let drive_metadata =
        drive_object_metadata(client, drive_access_token.as_str(), &record.backend_id).await?;
    require_equal(drive_metadata.id, record.backend_id, "Drive backend ID")?;
    let expected_size = object.size.bytes().to_string();
    require_equal(
        drive_metadata.size.as_deref(),
        Some(expected_size.as_str()),
        "Drive byte size",
    )?;
    require_property(&drive_metadata.app_properties, "lfsCloudVersion", "1")?;
    require_property(
        &drive_metadata.app_properties,
        "lfsCloudRepoNamespace",
        repository_id,
    )?;
    require_property(
        &drive_metadata.app_properties,
        "lfsCloudOid",
        object.oid.as_hex(),
    )?;
    require_property(
        &drive_metadata.app_properties,
        "lfsCloudSize",
        &object.size.bytes().to_string(),
    )?;

    let download = request_batch_action(
        client,
        batch_url,
        session_token,
        LfsBatchOperation::Download,
        &object,
        "download",
    )
    .await?;
    let response = action_request(client, Method::GET, &download)
        .send()
        .await
        .map_err(|error| format!("live LFS download request should complete: {error}"))?;
    if response.status() != StatusCode::OK {
        return Err(response_error("live LFS download", response).await);
    }
    let downloaded = response
        .bytes()
        .await
        .map_err(|error| format!("live LFS download body should be readable: {error}"))?;
    require_equal(
        downloaded.as_ref(),
        bytes.as_slice(),
        "downloaded object bytes",
    )
}

async fn request_batch_action(
    client: &Client,
    batch_url: &str,
    session_token: &str,
    operation: LfsBatchOperation,
    object: &LfsObject,
    action_name: &str,
) -> Result<LfsBatchAction, String> {
    let request = LfsBatchRequest {
        operation,
        transfers: vec![LFS_BASIC_TRANSFER.to_owned()],
        ref_context: None,
        hash_algo: LfsBatchHashAlgorithm::Sha256,
        objects: vec![object.clone()],
    };
    let response = client
        .post(batch_url)
        .header(ACCEPT, "application/vnd.git-lfs+json")
        .header(CONTENT_TYPE, "application/vnd.git-lfs+json")
        .bearer_auth(session_token)
        .json(&request)
        .send()
        .await
        .map_err(|error| format!("live LFS {action_name} batch should complete: {error}"))?;
    if response.status() != StatusCode::OK {
        return Err(response_error(&format!("live LFS {action_name} batch"), response).await);
    }
    let response: LfsBatchResponse = response
        .json()
        .await
        .map_err(|error| format!("live LFS {action_name} batch JSON should decode: {error}"))?;
    require_equal(
        response.transfer.as_str(),
        LFS_BASIC_TRANSFER,
        "batch transfer",
    )?;
    let mut objects = response.objects.into_iter();
    let object_response = objects
        .next()
        .ok_or_else(|| format!("live LFS {action_name} batch should return one object"))?;
    if objects.next().is_some() {
        return Err(format!(
            "live LFS {action_name} batch returned extra objects"
        ));
    }
    if let Some(error) = object_response.error {
        return Err(format!(
            "live LFS {action_name} batch returned object error {}: {}",
            error.code, error.message
        ));
    }
    object_response
        .actions
        .get(action_name)
        .cloned()
        .ok_or_else(|| format!("live LFS batch should advertise a {action_name} action"))
}

fn action_request(
    client: &Client,
    method: Method,
    action: &LfsBatchAction,
) -> reqwest::RequestBuilder {
    action.header.iter().fold(
        client.request(method, &action.href),
        |request, (name, value)| request.header(name.as_str(), value.as_str()),
    )
}

async fn drive_object_metadata(
    client: &Client,
    access_token: &str,
    file_id: &str,
) -> Result<DriveObjectMetadata, String> {
    let mut endpoint = Url::parse(DRIVE_API_URL).map_err(|error| error.to_string())?;
    endpoint
        .path_segments_mut()
        .map_err(|_| "Drive API URL cannot be a base".to_owned())?
        .extend(["files", file_id]);
    endpoint
        .query_pairs_mut()
        .append_pair("fields", "id,name,size,parents,trashed,appProperties")
        .append_pair("supportsAllDrives", "true");

    drive_json(client, Method::GET, endpoint, access_token, None).await
}

fn lfs_object_for_bytes(bytes: &[u8]) -> Result<LfsObject, String> {
    let oid = format!("{:x}", Sha256::digest(bytes));
    Ok(LfsObject::new(
        LfsOid::from_str(&oid).map_err(|error| format!("fixture OID should validate: {error}"))?,
        LfsObjectSize::new(
            u64::try_from(bytes.len())
                .map_err(|_| "fixture byte length should fit in u64".to_owned())?,
        ),
    ))
}

fn available_loopback_port() -> Result<u16, String> {
    let listener = StdTcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| format!("temporary loopback port should bind: {error}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| format!("temporary loopback port should resolve: {error}"))
}

async fn wait_for_live_server(port: u16) -> Result<(), String> {
    for _ in 0..600 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Err("live LFS server did not become ready within 60 seconds".to_owned())
}

async fn require_status(
    response: reqwest::Response,
    expected: StatusCode,
    context: &str,
) -> Result<(), String> {
    if response.status() == expected {
        Ok(())
    } else {
        Err(response_error(context, response).await)
    }
}

async fn response_error(context: &str, response: reqwest::Response) -> String {
    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "<unreadable body>".to_owned());
    format!("{context} failed with HTTP {status}: {body}")
}

fn require_property(
    properties: &BTreeMap<String, String>,
    name: &str,
    expected: &str,
) -> Result<(), String> {
    require_equal(
        properties.get(name).map(String::as_str),
        Some(expected),
        &format!("Drive app property {name}"),
    )
}

fn require_equal<T>(actual: T, expected: T, context: &str) -> Result<(), String>
where
    T: std::fmt::Debug + PartialEq,
{
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "{context} mismatch: expected {expected:?}, got {actual:?}"
        ))
    }
}

fn combine_cleanup_error(
    scenario_error: String,
    cleanup_name: &str,
    cleanup: Result<(), String>,
) -> String {
    match cleanup {
        Ok(()) => scenario_error,
        Err(cleanup_error) => {
            format!("{scenario_error}; {cleanup_name} cleanup also failed: {cleanup_error}")
        }
    }
}

fn finish_scenario_with_cleanup(
    scenario: Result<(), String>,
    drive_cleanup: Result<(), String>,
    github_cleanup: Result<(), String>,
) -> Result<(), String> {
    let mut errors = scenario.err().into_iter().collect::<Vec<_>>();
    if let Err(error) = drive_cleanup {
        errors.push(format!("Google Drive cleanup failed: {error}"));
    }
    if let Err(error) = github_cleanup {
        errors.push(format!("GitHub repository cleanup failed: {error}"));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn skip_unless_enabled(env_name: &str) -> bool {
    match env::var(env_name).as_deref() {
        Ok("1" | "true" | "TRUE" | "yes" | "YES") => false,
        _ => {
            eprintln!("skipping external integration test; set {env_name}=1 to enable");
            true
        }
    }
}

fn required_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}

fn google_drive_credential_json() -> String {
    if let Ok(contents) = env::var("LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_JSON") {
        return contents;
    }

    let path = required_env("LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_FILE");
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("failed to read {path}: {error}"))
}

fn disposable_name(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system clock should be after the Unix epoch")
        .as_nanos();
    format!("{prefix}-{nanos}-{}", process::id())
}

fn github_host_from_api_url(api_url: &str) -> String {
    let parsed = Url::parse(api_url).expect("GitHub API URL should parse");
    match parsed.host_str() {
        Some("api.github.com") => "github.com".to_owned(),
        Some(host) if host.starts_with("api.") => host.trim_start_matches("api.").to_owned(),
        Some(host) => host.to_owned(),
        None => "github.com".to_owned(),
    }
}

async fn create_github_repo(
    client: &Client,
    api_url: &str,
    token: &str,
    owner: Option<&str>,
    repo_name: &str,
) -> Result<GitHubCreatedRepo, String> {
    let mut endpoint = Url::parse(api_url).map_err(|error| error.to_string())?;
    {
        let mut segments = endpoint
            .path_segments_mut()
            .map_err(|_| "GitHub API URL cannot be a base".to_owned())?;
        if let Some(owner) = owner {
            segments.extend(["orgs", owner, "repos"]);
        } else {
            segments.extend(["user", "repos"]);
        }
    }

    github_json(
        client,
        Method::POST,
        endpoint,
        token,
        Some(json!({
            "name": repo_name,
            "private": true,
            "auto_init": true
        })),
    )
    .await
}

async fn delete_github_repo(
    client: &Client,
    api_url: &str,
    token: &str,
    full_name: &str,
) -> Result<(), String> {
    let mut endpoint = Url::parse(api_url).map_err(|error| error.to_string())?;
    {
        let mut segments = endpoint
            .path_segments_mut()
            .map_err(|_| "GitHub API URL cannot be a base".to_owned())?;
        segments.push("repos");
        for part in full_name.split('/') {
            segments.push(part);
        }
    }

    let response = github_request(client, Method::DELETE, endpoint, token, None)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if response.status() == StatusCode::NO_CONTENT {
        Ok(())
    } else {
        Err(format!(
            "GitHub repository delete failed with HTTP {}: {}",
            response.status(),
            response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_owned())
        ))
    }
}

async fn github_json<T: for<'de> Deserialize<'de>>(
    client: &Client,
    method: Method,
    endpoint: Url,
    token: &str,
    body: Option<Value>,
) -> Result<T, String> {
    let response = github_request(client, method, endpoint, token, body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("GitHub response body could not be read: {error}"))?;

    if !status.is_success() {
        return Err(format!("GitHub request failed with HTTP {status}: {body}"));
    }

    serde_json::from_str(&body)
        .map_err(|error| format!("GitHub response JSON was invalid: {error}"))
}

fn github_request(
    client: &Client,
    method: Method,
    endpoint: Url,
    token: &str,
    body: Option<Value>,
) -> reqwest::RequestBuilder {
    let mut request = client
        .request(method, endpoint)
        .header(ACCEPT, GITHUB_ACCEPT)
        .header(USER_AGENT, concat!("lfs-cloud/", env!("CARGO_PKG_VERSION")))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    request
}

async fn create_drive_folder(
    client: &Client,
    access_token: &str,
    name: &str,
    parent_folder_id: Option<&str>,
) -> Result<DriveFolder, String> {
    let mut endpoint = Url::parse(DRIVE_API_URL).map_err(|error| error.to_string())?;
    {
        let mut segments = endpoint
            .path_segments_mut()
            .map_err(|_| "Drive API URL cannot be a base".to_owned())?;
        segments.push("files");
    }
    endpoint
        .query_pairs_mut()
        .append_pair("fields", "id,name")
        .append_pair("supportsAllDrives", "true");

    let mut metadata = json!({
        "name": name,
        "mimeType": DRIVE_FOLDER_MIME_TYPE
    });
    if let Some(parent) = parent_folder_id {
        metadata["parents"] = json!([parent]);
    }

    drive_json(client, Method::POST, endpoint, access_token, Some(metadata)).await
}

async fn delete_drive_file(
    client: &Client,
    access_token: &str,
    file_id: &str,
) -> Result<(), String> {
    let mut endpoint = Url::parse(DRIVE_API_URL).map_err(|error| error.to_string())?;
    {
        let mut segments = endpoint
            .path_segments_mut()
            .map_err(|_| "Drive API URL cannot be a base".to_owned())?;
        segments.extend(["files", file_id]);
    }
    endpoint
        .query_pairs_mut()
        .append_pair("supportsAllDrives", "true");

    let response = drive_request(client, Method::DELETE, endpoint, access_token, None)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if response.status().is_success() || response.status() == StatusCode::NOT_FOUND {
        Ok(())
    } else {
        Err(format!(
            "Google Drive file delete failed with HTTP {}: {}",
            response.status(),
            response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_owned())
        ))
    }
}

async fn drive_json<T: for<'de> Deserialize<'de>>(
    client: &Client,
    method: Method,
    endpoint: Url,
    access_token: &str,
    body: Option<Value>,
) -> Result<T, String> {
    let response = drive_request(client, method, endpoint, access_token, body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("Google Drive response body could not be read: {error}"))?;

    if !status.is_success() {
        return Err(format!(
            "Google Drive request failed with HTTP {status}: {body}"
        ));
    }

    serde_json::from_str(&body)
        .map_err(|error| format!("Google Drive response JSON was invalid: {error}"))
}

fn drive_request(
    client: &Client,
    method: Method,
    endpoint: Url,
    access_token: &str,
    body: Option<Value>,
) -> reqwest::RequestBuilder {
    let mut request = client
        .request(method, endpoint)
        .header(ACCEPT, "application/json")
        .bearer_auth(access_token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    request
}

#[derive(Debug, Deserialize)]
struct GitHubCreatedRepo {
    id: u64,
    name: String,
    full_name: String,
    owner: GitHubOwner,
}

#[derive(Debug, Deserialize)]
struct GitHubOwner {
    login: String,
}

#[derive(Debug, Deserialize)]
struct DriveFolder {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveObjectMetadata {
    id: String,
    size: Option<String>,
    #[serde(default)]
    app_properties: BTreeMap<String, String>,
}
