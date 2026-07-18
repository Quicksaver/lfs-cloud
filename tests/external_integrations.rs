//! Gated external-service integration checks.
//!
//! These tests are ignored by default because they create and delete real
//! provider resources. The scripts in `scripts/manual/` set up the exact cargo
//! invocation and keep normal development runs deterministic.

use std::{env, fs, process, time::SystemTime};

use lfs_cloud::{
    GitHubOAuthAccessToken, GitHubProviderConfig, GitHubRepositoryPermissionClient,
    GitHubUserClient, GoogleDriveCredential, GoogleDriveRootValidator, GoogleDriveStorageConfig,
    GoogleDriveTokenRefresher, RepositoryIdentity, RepositoryPermission,
};
use reqwest::{
    Client, Method, StatusCode,
    header::{ACCEPT, USER_AGENT},
};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

const GITHUB_API_URL: &str = "https://api.github.com";
const GITHUB_ACCEPT: &str = "application/vnd.github+json";
const DRIVE_API_URL: &str = "https://www.googleapis.com/drive/v3";
const DRIVE_FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";

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
