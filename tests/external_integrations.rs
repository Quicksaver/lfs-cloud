//! Gated external-service integration checks.
//!
//! These tests are ignored by default because they create and delete real
//! provider resources. The scripts in `scripts/manual/` set up the exact cargo
//! invocation and keep normal development runs deterministic.

use std::{
    collections::BTreeMap,
    env, fs,
    fs::File,
    io::Write,
    net::TcpListener as StdTcpListener,
    path::{Path, PathBuf},
    process::{self, Child, Command, Output, Stdio},
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::{StatusCode as AxumStatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use lfscloud::{
    GITHUB_PERSONAL_ACCESS_TOKEN_LOGIN_PATH, GitHubAuthenticationConfig, GitHubPersonalAccessToken,
    GitHubProviderConfig, GitHubRepositoryPermissionClient, GitHubUserClient,
    GoogleDriveAccessToken, GoogleDriveGcloudCredentialsConfig, GoogleDriveGcloudTokenProvider,
    GoogleDriveObjectStore, GoogleDriveRootValidator, GoogleDriveStorageConfig, LfsObject,
    LfsObjectSize, LfsOid, MetadataDatabase, MetadataObjectVerificationStatus, RepositoryIdentity,
    RepositoryPermission, RepositoryUser, ServerConfig,
};
use reqwest::{
    Client, Method, StatusCode,
    header::{ACCEPT, USER_AGENT},
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
const LIVE_SESSION_ENCRYPTION_SECRET: &str =
    "lfscloud-live-session-encryption-secret-at-least-32-characters";
const LIVE_GITHUB_PAT_ENV: &str = "LFS_CLOUD_GITHUB_PAT";
const LIVE_DRIVE_CONFIG_DIR_ENV: &str = "LFS_CLOUD_GOOGLE_DRIVE_CONFIG_DIR";
#[cfg(not(windows))]
const LIVE_GCLOUD_EXECUTABLE: &str = "gcloud";
#[cfg(windows)]
const LIVE_GCLOUD_EXECUTABLE: &str = "gcloud.cmd";

#[derive(Deserialize)]
struct GitHubPersonalAccessTokenLoginResponse {
    lfs_token: String,
}

struct LiveGitHubCredentials {
    personal_access_token: String,
}

#[derive(Clone)]
struct LegacyLfsSourceState {
    base_url: String,
    objects: Arc<BTreeMap<String, Vec<u8>>>,
}

struct AbortTaskOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn legacy_lfs_batch(
    State(state): State<LegacyLfsSourceState>,
    Json(request): Json<Value>,
) -> Json<Value> {
    let operation = request["operation"].as_str().unwrap_or_default();
    let objects = request["objects"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|object| {
            let oid = object["oid"].as_str().unwrap_or_default();
            let size = object["size"].as_u64().unwrap_or_default();
            match state.objects.get(oid) {
                Some(_) if operation == "download" => json!({
                    "oid": oid,
                    "size": size,
                    "actions": {
                        "download": {
                            "href": format!("{}/objects/{oid}", state.base_url),
                            "header": {},
                        }
                    }
                }),
                Some(_) => json!({ "oid": oid, "size": size, "actions": {} }),
                None => json!({
                    "oid": oid,
                    "size": size,
                    "error": { "code": 404, "message": "missing legacy object" }
                }),
            }
        })
        .collect::<Vec<_>>();

    Json(json!({ "transfer": "basic", "objects": objects }))
}

async fn legacy_lfs_object(
    State(state): State<LegacyLfsSourceState>,
    AxumPath(oid): AxumPath<String>,
) -> Response {
    match state.objects.get(&oid) {
        Some(bytes) => {
            ([(CONTENT_TYPE, "application/octet-stream")], bytes.clone()).into_response()
        }
        None => AxumStatusCode::NOT_FOUND.into_response(),
    }
}

impl LiveGitHubCredentials {
    fn authentication(&self) -> GitHubAuthenticationConfig {
        GitHubAuthenticationConfig::new(self.personal_access_token.clone())
    }

    fn personal_access_token(&self) -> &str {
        &self.personal_access_token
    }
}

#[tokio::test]
#[ignore = "requires LFS_CLOUD_RUN_GITHUB_INTEGRATION=1 and LFS_CLOUD_GITHUB_PAT"]
async fn github_disposable_repo_permission_check() {
    require_enabled("LFS_CLOUD_RUN_GITHUB_INTEGRATION");

    let credentials = live_github_credentials();
    let repo_owner = env::var("LFS_CLOUD_GITHUB_OWNER").ok();
    let api_url =
        env::var("LFS_CLOUD_GITHUB_API_URL").unwrap_or_else(|_| GITHUB_API_URL.to_owned());
    let repo_host =
        env::var("LFS_CLOUD_GITHUB_HOST").unwrap_or_else(|_| github_host_from_api_url(&api_url));
    let repo_name = disposable_name("lfscloud-it");
    let provider = GitHubProviderConfig {
        id: "github-main".to_owned(),
        api_url: api_url.clone(),
        authentication: credentials.authentication(),
        allow_insecure_http: false,
    };
    let github_pat =
        GitHubPersonalAccessToken::from_secret(credentials.personal_access_token().to_owned())
            .expect("GitHub PAT should validate");
    let http = Client::new();
    let created_repo = create_github_repo(
        &http,
        &api_url,
        credentials.personal_access_token(),
        repo_owner.as_deref(),
        &repo_name,
    )
    .await
    .expect("disposable GitHub repository should be created");

    let check_result = async {
        let user = GitHubUserClient::new()?
            .fetch_authenticated_user(&provider, &github_pat)
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
                &github_pat,
                &repository,
                &user,
                RepositoryPermission::Write,
            )
            .await?;

        Ok::<_, lfscloud::ServerError>(authorization)
    }
    .await;

    let delete_result = delete_github_repo(
        &http,
        &api_url,
        credentials.personal_access_token(),
        &created_repo.full_name,
    )
    .await;
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
    require_enabled("LFS_CLOUD_RUN_GOOGLE_DRIVE_INTEGRATION");

    let gcloud_credentials = google_drive_gcloud_credentials();
    let provider_id =
        env::var("LFS_CLOUD_GOOGLE_DRIVE_PROVIDER_ID").unwrap_or_else(|_| "drive-it".to_owned());
    let access_token = GoogleDriveGcloudTokenProvider::new()
        .access_token(&provider_id, &gcloud_credentials)
        .await
        .expect("gcloud ADC should produce a Google Drive access token");
    let http = Client::new();
    let folder_name = disposable_name("lfscloud-drive-it");
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
        credentials: gcloud_credentials,
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
#[ignore = "requires LFS_CLOUD_RUN_LIVE_TRANSFER_INTEGRATION=1 plus selected GitHub auth and Drive credentials"]
async fn black_box_git_lfs_push_fetch_uses_live_github_and_drive() {
    require_enabled("LFS_CLOUD_RUN_LIVE_TRANSFER_INTEGRATION");

    run_black_box_git_lfs_transfer_scenario()
        .await
        .expect("black-box Git LFS scenario should pass and clean up its resources");
}

#[test]
fn live_server_transfer_config_fixture_uses_production_provider_ids() {
    let test_dir = tempfile::tempdir().expect("config fixture temp directory should be created");
    let config_path = test_dir.path().join("lfscloud.yml");
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
    let gcloud_credentials = GoogleDriveGcloudCredentialsConfig {
        config_dir: test_dir.path().join("gcloud-drive"),
        executable: PathBuf::from(LIVE_GCLOUD_EXECUTABLE),
    };
    let repository_id = format!("{LIVE_GITHUB_PROVIDER_ID}:owner/repo");
    let config = live_server_config_json(LiveServerConfigFixture {
        server_url: "http://127.0.0.1:8080",
        port: 8080,
        metadata_path: &metadata_path,
        github_api_url: GITHUB_API_URL,
        github_host: "github.com",
        repository: &repository,
        folder: &folder,
        gcloud_credentials: &gcloud_credentials,
    });
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
    let lfscloud::RepositoryProviderConfig::GitHub(provider) =
        &config.repository_providers[LIVE_GITHUB_PROVIDER_ID];
    assert_eq!(provider.authentication.personal_access_token(), None);
    assert!(config.server.session_encryption_secret.is_some());
    assert!(
        config
            .storage_providers
            .contains_key(LIVE_DRIVE_PROVIDER_ID)
    );
    let lfscloud::StorageProviderConfig::GoogleDrive(storage) =
        &config.storage_providers[LIVE_DRIVE_PROVIDER_ID];
    let credentials = &storage.credentials;
    assert_eq!(credentials.config_dir, gcloud_credentials.config_dir);
    assert_eq!(credentials.executable, gcloud_credentials.executable);
    assert_eq!(config.repositories[0].id, repository_id);
}

async fn run_black_box_git_lfs_transfer_scenario() -> Result<(), String> {
    let git_lfs = Command::new("git")
        .args(["lfs", "version"])
        .output()
        .map_err(|error| format!("Git LFS prerequisite check should start: {error}"))?;
    require_command_success(git_lfs, "Git LFS prerequisite check", &[])?;

    let github_credentials = live_github_credentials_result()?;
    let github_owner = env::var("LFS_CLOUD_GITHUB_OWNER").ok();
    let github_api_url =
        env::var("LFS_CLOUD_GITHUB_API_URL").unwrap_or_else(|_| GITHUB_API_URL.to_owned());
    let github_host = env::var("LFS_CLOUD_GITHUB_HOST")
        .unwrap_or_else(|_| github_host_from_api_url(&github_api_url));
    let gcloud_credentials = google_drive_gcloud_credentials_result()?;
    let drive_access_token = GoogleDriveGcloudTokenProvider::new()
        .access_token(LIVE_DRIVE_PROVIDER_ID, &gcloud_credentials)
        .await
        .map_err(|error| format!("gcloud ADC should produce a Drive access token: {error}"))?;
    let client = Client::new();
    let repository = create_github_repo(
        &client,
        &github_api_url,
        github_credentials.personal_access_token(),
        github_owner.as_deref(),
        &disposable_name("lfscloud-live-it"),
    )
    .await?;
    let folder = match create_drive_folder(
        &client,
        drive_access_token.as_str(),
        &disposable_name("lfscloud-live-drive-it"),
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
                github_credentials.personal_access_token(),
                &repository.full_name,
            )
            .await;
            return Err(combine_cleanup_error(error, "GitHub repository", cleanup));
        }
    };

    let scenario = exercise_black_box_git_lfs_transfer(
        &client,
        &github_api_url,
        &github_host,
        &github_credentials,
        &repository,
        LiveDriveFixture {
            gcloud_credentials: &gcloud_credentials,
            folder: &folder,
            access_token: &drive_access_token,
        },
    )
    .await;
    let drive_cleanup = delete_drive_file(&client, drive_access_token.as_str(), &folder.id).await;
    let github_cleanup = delete_github_repo(
        &client,
        &github_api_url,
        github_credentials.personal_access_token(),
        &repository.full_name,
    )
    .await;

    finish_scenario_with_cleanup(scenario, drive_cleanup, github_cleanup)
}

struct LiveDriveFixture<'a> {
    gcloud_credentials: &'a GoogleDriveGcloudCredentialsConfig,
    folder: &'a DriveFolder,
    access_token: &'a GoogleDriveAccessToken,
}

async fn exercise_black_box_git_lfs_transfer(
    client: &Client,
    github_api_url: &str,
    github_host: &str,
    github_credentials: &LiveGitHubCredentials,
    repository: &GitHubCreatedRepo,
    drive: LiveDriveFixture<'_>,
) -> Result<(), String> {
    let test_dir = tempfile::tempdir()
        .map_err(|error| format!("live transfer temp directory should be created: {error}"))?;
    let metadata_path = test_dir.path().join("metadata.sqlite3");
    let config_path = test_dir.path().join("lfscloud.yml");
    let port = available_loopback_port()?;
    let server_url = format!("http://127.0.0.1:{port}");
    let repository_id = format!(
        "{LIVE_GITHUB_PROVIDER_ID}:{}/{}",
        repository.owner.login, repository.name
    );
    let config_json = live_server_config_json(LiveServerConfigFixture {
        server_url: &server_url,
        port,
        metadata_path: &metadata_path,
        github_api_url,
        github_host,
        repository,
        folder: drive.folder,
        gcloud_credentials: drive.gcloud_credentials,
    });
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
    let github_personal_access_token =
        GitHubPersonalAccessToken::from_secret(github_credentials.personal_access_token())
            .map_err(|error| format!("GitHub PAT should validate: {error}"))?;
    let lfscloud::RepositoryProviderConfig::GitHub(github_provider) =
        &config.repository_providers[LIVE_GITHUB_PROVIDER_ID];
    let github_user = GitHubUserClient::new()
        .map_err(|error| format!("GitHub user client should build: {error}"))?
        .fetch_authenticated_user(github_provider, &github_personal_access_token)
        .await
        .map_err(|error| format!("GitHub PAT should resolve a stable user: {error}"))?;
    let lfs_url = format!("{server_url}{}", config.repositories[0].route_path());
    let server_stdout = test_dir.path().join("lfscloud.stdout.log");
    let server_stderr = test_dir.path().join("lfscloud.stderr.log");
    let secrets = vec![
        github_credentials.personal_access_token(),
        drive.access_token.as_str(),
    ];
    let mut server =
        LiveServerProcess::spawn(&config_path, &server_stdout, &server_stderr, &[], &secrets)?;
    let scenario = async {
        wait_for_live_server(port, &mut server).await?;
        let response = client
            .post(format!(
                "{server_url}{GITHUB_PERSONAL_ACCESS_TOKEN_LOGIN_PATH}"
            ))
            .bearer_auth(github_credentials.personal_access_token())
            .send()
            .await
            .map_err(|error| format!("PAT login request should send: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("PAT login should succeed, received HTTP {status}"));
        }
        let response = response
            .json::<GitHubPersonalAccessTokenLoginResponse>()
            .await
            .map_err(|error| format!("PAT login response should parse: {error}"))?;
        let session_token = lfscloud::LfsSessionToken::from_secret(response.lfs_token)
            .map_err(|error| format!("PAT login local token should validate: {error}"))?;
        server.add_secret(session_token.as_str());
        let bytes = b"live Git LFS bytes crossing the compiled LFS Cloud process";
        let object = lfs_object_for_bytes(bytes)?;
        git_lfs_push_fetch_round_trip(test_dir.path(), &lfs_url, session_token.as_str(), bytes)?;
        verify_live_object_storage(
            client,
            &repository_id,
            &github_user,
            &metadata,
            drive.access_token,
            &object,
        )
        .await?;
        git_lfs_historical_migration_round_trip(
            test_dir.path(),
            &server_url,
            &lfs_url,
            session_token.as_str(),
            github_host,
            repository,
            &config,
            drive.access_token,
        )
        .await
    }
    .await;
    let stop = server.stop();

    combine_process_result(scenario, stop)
}

struct LiveServerConfigFixture<'a> {
    server_url: &'a str,
    port: u16,
    metadata_path: &'a Path,
    github_api_url: &'a str,
    github_host: &'a str,
    repository: &'a GitHubCreatedRepo,
    folder: &'a DriveFolder,
    gcloud_credentials: &'a GoogleDriveGcloudCredentialsConfig,
}

fn live_server_config_json(fixture: LiveServerConfigFixture<'_>) -> Value {
    let LiveServerConfigFixture {
        server_url,
        port,
        metadata_path,
        github_api_url,
        github_host,
        repository,
        folder,
        gcloud_credentials,
    } = fixture;
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
            "session_encryption_secret": LIVE_SESSION_ENCRYPTION_SECRET,
        },
        "repository_providers": {
            LIVE_GITHUB_PROVIDER_ID: {
                "type": "github",
                "api_url": github_api_url,
            }
        },
        "storage_providers": {
            LIVE_DRIVE_PROVIDER_ID: {
                "type": "google_drive",
                "credentials": {
                    "type": "gcloud",
                    "config_dir": gcloud_credentials.config_dir,
                    "executable": gcloud_credentials.executable,
                },
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

struct LiveServerProcess {
    child: Option<Child>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    secrets: Vec<String>,
}

impl LiveServerProcess {
    fn spawn(
        config_path: &Path,
        stdout_path: &Path,
        stderr_path: &Path,
        environment: &[(&str, &str)],
        secrets: &[&str],
    ) -> Result<Self, String> {
        let stdout = File::create(stdout_path)
            .map_err(|error| format!("live server stdout log should be created: {error}"))?;
        let stderr = File::create(stderr_path)
            .map_err(|error| format!("live server stderr log should be created: {error}"))?;
        let lfscloud_binary = env::var_os("LFS_CLOUD_SMOKE_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_lfscloud")));
        let mut command = Command::new(lfscloud_binary);
        command
            .args(["--config"])
            .arg(config_path)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        for (name, value) in environment {
            command.env(name, value);
        }
        let child = command
            .spawn()
            .map_err(|error| format!("compiled lfscloud server should start: {error}"))?;

        Ok(Self {
            child: Some(child),
            stdout_path: stdout_path.to_owned(),
            stderr_path: stderr_path.to_owned(),
            secrets: secrets.iter().map(|secret| (*secret).to_owned()).collect(),
        })
    }

    fn add_secret(&mut self, secret: &str) {
        self.secrets.push(secret.to_owned());
    }

    fn try_wait(&mut self) -> Result<Option<process::ExitStatus>, String> {
        self.child
            .as_mut()
            .ok_or_else(|| "live server process is no longer available".to_owned())?
            .try_wait()
            .map_err(|error| format!("live server process status should be readable: {error}"))
    }

    fn unexpected_exit(&self, status: process::ExitStatus) -> String {
        let diagnostics = self.diagnostics();
        if diagnostics.is_empty() {
            format!("compiled lfscloud server exited unexpectedly with {status}")
        } else {
            format!("compiled lfscloud server exited unexpectedly with {status}\n{diagnostics}")
        }
    }

    fn diagnostics(&self) -> String {
        let stdout = fs::read_to_string(&self.stdout_path).unwrap_or_default();
        let stderr = fs::read_to_string(&self.stderr_path).unwrap_or_default();
        let combined = format!("server stdout:\n{stdout}\nserver stderr:\n{stderr}");
        tail_lines(&redact_secrets(&combined, &self.secrets), 80)
    }

    fn stop(&mut self) -> Result<(), String> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("live server process status should be readable: {error}"))?
        {
            return Err(self.unexpected_exit(status));
        }

        child
            .kill()
            .map_err(|error| format!("live server process should stop: {error}"))?;
        child
            .wait()
            .map_err(|error| format!("live server process should be reaped: {error}"))?;
        Ok(())
    }
}

impl Drop for LiveServerProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct IsolatedGit {
    global_config: PathBuf,
    credential_store: PathBuf,
    secrets: Vec<String>,
}

impl IsolatedGit {
    fn initialize(root: &Path, secrets: &[&str]) -> Result<Self, String> {
        let git = Self {
            global_config: root.join("gitconfig"),
            credential_store: root.join("credentials"),
            secrets: secrets.iter().map(|secret| (*secret).to_owned()).collect(),
        };
        // Git evaluates helper values as shell snippets, including under Git
        // Bash on Windows, where unescaped backslashes would be consumed.
        let helper_path = git.credential_store.to_string_lossy().replace('\\', "/");
        let helper = format!("store --file={helper_path}");
        git.run(None, "isolated Git credential helper setup", |command| {
            command.args(["config", "--global", "credential.helper", &helper]);
        })?;
        git.run(None, "isolated Git path-scoping setup", |command| {
            command.args(["config", "--global", "credential.useHttpPath", "true"]);
        })?;
        Ok(git)
    }

    fn approve(&self, url: &str, username: &str, password: &str) -> Result<(), String> {
        if [url, username, password]
            .iter()
            .any(|field| field.contains(['\r', '\n']))
        {
            return Err("Git credential fixture fields must not contain line breaks".to_owned());
        }
        let input = format!("url={url}\nusername={username}\npassword={password}\n\n");
        let mut command = self.command();
        command
            .args(["credential", "approve"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| format!("Git credential approval should start: {error}"))?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| "Git credential approval stdin should be available".to_owned())?
            .write_all(input.as_bytes())
            .map_err(|error| format!("Git credential approval input should be written: {error}"))?;
        let output = child
            .wait_with_output()
            .map_err(|error| format!("Git credential approval should finish: {error}"))?;
        require_command_success(output, "Git credential approval", &self.secrets)?;
        Ok(())
    }

    fn run(
        &self,
        cwd: Option<&Path>,
        context: &str,
        configure: impl FnOnce(&mut Command),
    ) -> Result<Output, String> {
        let mut command = self.command();
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        configure(&mut command);
        let output = command
            .output()
            .map_err(|error| format!("{context} should start: {error}"))?;
        require_command_success(output, context, &self.secrets)
    }

    fn run_program(
        &self,
        cwd: Option<&Path>,
        program: impl AsRef<Path>,
        context: &str,
        configure: impl FnOnce(&mut Command),
    ) -> Result<Output, String> {
        let mut command = Command::new(program.as_ref());
        self.configure_environment(&mut command);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        configure(&mut command);
        let output = command
            .output()
            .map_err(|error| format!("{context} should start: {error}"))?;
        require_command_success(output, context, &self.secrets)
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        self.configure_environment(&mut command);
        command
    }

    fn configure_environment(&self, command: &mut Command) {
        command
            .env("GIT_CONFIG_GLOBAL", &self.global_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "Never")
            .env("LC_ALL", "C")
            .env("NO_COLOR", "1")
            .env_remove("GIT_CONFIG_COUNT");
    }
}

fn git_lfs_push_fetch_round_trip(
    root: &Path,
    lfs_url: &str,
    session_token: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let bare_remote = root.join("remote.git");
    let source = root.join("source");
    let checkout = root.join("checkout");
    let git = IsolatedGit::initialize(root, &[session_token])?;
    git.approve(lfs_url, "lfscloud", session_token)?;

    git.run(None, "bare Git remote initialization", |command| {
        command
            .args(["init", "--bare", "--initial-branch", "main"])
            .arg(&bare_remote);
    })?;
    git.run(None, "source Git repository initialization", |command| {
        command
            .args(["init", "--initial-branch", "main"])
            .arg(&source);
    })?;
    git.run(Some(&source), "source Git remote setup", |command| {
        command
            .arg("remote")
            .arg("add")
            .arg("origin")
            .arg(&bare_remote);
    })?;
    git.run(Some(&source), "Git test identity setup", |command| {
        command.args(["config", "user.name", "LFS Cloud Smoke Test"]);
    })?;
    git.run(Some(&source), "Git test email setup", |command| {
        command.args(["config", "user.email", "smoke@example.invalid"]);
    })?;
    git.run(
        Some(&source),
        "repository-local Git LFS installation",
        |command| {
            command.args(["lfs", "install", "--local"]);
        },
    )?;
    git.run(Some(&source), "Git LFS tracking setup", |command| {
        command.args(["lfs", "track", "assets/*.bin"]);
    })?;
    git.run(Some(&source), "repository LFS endpoint setup", |command| {
        command.args(["config", "--file", ".lfsconfig", "lfs.url", lfs_url]);
    })?;
    fs::create_dir_all(source.join("assets"))
        .map_err(|error| format!("Git LFS fixture directory should be created: {error}"))?;
    fs::write(source.join("assets/model.bin"), bytes)
        .map_err(|error| format!("Git LFS fixture bytes should be written: {error}"))?;
    git.run(Some(&source), "Git LFS fixture staging", |command| {
        command.args(["add", ".gitattributes", ".lfsconfig", "assets/model.bin"]);
    })?;
    git.run(Some(&source), "Git LFS fixture commit", |command| {
        command.args(["commit", "-m", "Add live LFS fixture"]);
    })?;
    git.run(Some(&source), "Git push with Git LFS pre-push", |command| {
        command.args(["push", "origin", "HEAD:refs/heads/main"]);
    })?;

    git.run(None, "pointer-only Git clone", |command| {
        command
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .args(["clone", "--branch", "main"])
            .arg(&bare_remote)
            .arg(&checkout);
    })?;
    git.run(
        Some(&checkout),
        "checkout-local Git LFS installation",
        |command| {
            command.args(["lfs", "install", "--local"]);
        },
    )?;
    git.run(
        Some(&checkout),
        "Git LFS download through LFS Cloud",
        |command| {
            command.args(["lfs", "pull"]);
        },
    )?;
    let downloaded = fs::read(checkout.join("assets/model.bin"))
        .map_err(|error| format!("downloaded Git LFS fixture should be readable: {error}"))?;
    require_equal(downloaded.as_slice(), bytes, "downloaded Git LFS bytes")
}

#[expect(
    clippy::too_many_arguments,
    reason = "the live migration fixture keeps every external resource and secret boundary explicit"
)]
async fn git_lfs_historical_migration_round_trip(
    root: &Path,
    server_url: &str,
    lfscloud_url: &str,
    session_token: &str,
    github_host: &str,
    repository: &GitHubCreatedRepo,
    config: &ServerConfig,
    drive_access_token: &GoogleDriveAccessToken,
) -> Result<(), String> {
    let source_remote = root.join("migration-source.git");
    let source = root.join("migration-source");
    let checkout = root.join("migration-checkout");
    let migration_git_root = root.join("migration-git");
    fs::create_dir_all(&migration_git_root)
        .map_err(|error| format!("migration Git state directory should be created: {error}"))?;
    let git = IsolatedGit::initialize(&migration_git_root, &[session_token])?;
    let github_remote_url = format!(
        "git@{github_host}:{}/{}.git",
        repository.owner.login, repository.name
    );
    // Keep Git refs in a disposable local bare remote and source LFS bytes on a
    // loopback HTTP fixture. Migration must use the legacy HTTP endpoint while
    // every destination write still crosses the compiled LFS Cloud server and
    // its real GitHub permission plus Drive storage boundaries.
    git.run(None, "migration source bare repository setup", |command| {
        command
            .args(["init", "--bare", "--initial-branch", "main"])
            .arg(&source_remote);
    })?;
    let source_git_url = Url::from_file_path(&source_remote)
        .map_err(|()| "migration source path should convert to a file URL".to_owned())?;
    let first_bytes = b"historical live migration asset bytes\n";
    let latest_bytes = b"latest live migration asset bytes with a changed payload\n";
    let first_object = lfs_object_for_bytes(first_bytes)?;
    let latest_object = lfs_object_for_bytes(latest_bytes)?;
    let legacy_objects = Arc::new(BTreeMap::from([
        (first_object.oid.as_hex().to_owned(), first_bytes.to_vec()),
        (latest_object.oid.as_hex().to_owned(), latest_bytes.to_vec()),
    ]));
    let legacy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("legacy LFS fixture should bind: {error}"))?;
    let legacy_address = legacy_listener
        .local_addr()
        .map_err(|error| format!("legacy LFS fixture address should resolve: {error}"))?;
    let source_lfs_url = format!("http://{legacy_address}/legacy");
    let legacy_state = LegacyLfsSourceState {
        base_url: source_lfs_url.clone(),
        objects: legacy_objects,
    };
    let legacy_router = Router::new()
        .route("/legacy/objects/batch", post(legacy_lfs_batch))
        .route("/legacy/objects/{oid}", get(legacy_lfs_object))
        .with_state(legacy_state);
    let _legacy_server = AbortTaskOnDrop(tokio::spawn(async move {
        axum::serve(legacy_listener, legacy_router)
            .await
            .expect("legacy LFS fixture should run");
    }));
    git.run(None, "migration Git remote rewrite setup", |command| {
        command.args([
            "config",
            "--global",
            &format!("url.{}.insteadOf", source_git_url.as_str()),
            &github_remote_url,
        ]);
    })?;
    git.approve(lfscloud_url, "lfscloud", session_token)?;

    git.run(
        None,
        "migration source repository initialization",
        |command| {
            command
                .args(["init", "--initial-branch", "main"])
                .arg(&source);
        },
    )?;
    git.run(Some(&source), "migration source remote setup", |command| {
        command
            .args(["remote", "add", "origin"])
            .arg(&github_remote_url);
    })?;
    git.run(
        Some(&source),
        "migration source LFS endpoint setup",
        |command| {
            command.args(["config", "--local", "lfs.url", &source_lfs_url]);
        },
    )?;
    git.run(
        Some(&source),
        "migration source lock verification setup",
        |command| {
            command.args(["config", "--local", "lfs.locksverify", "false"]);
        },
    )?;
    for (name, value) in [
        ("user.name", "LFS Cloud Smoke Test"),
        ("user.email", "smoke@example.invalid"),
        ("commit.gpgSign", "false"),
    ] {
        git.run(Some(&source), "migration Git identity setup", |command| {
            command.args(["config", name, value]);
        })?;
    }
    git.run(Some(&source), "migration Git LFS installation", |command| {
        command.args(["lfs", "install", "--local"]);
    })?;
    git.run(
        Some(&source),
        "migration Git LFS tracking setup",
        |command| {
            command.args(["lfs", "track", "assets/*.bin"]);
        },
    )?;
    fs::create_dir_all(source.join("assets"))
        .map_err(|error| format!("migration LFS fixture directory should be created: {error}"))?;

    fs::write(source.join("assets/model.bin"), first_bytes)
        .map_err(|error| format!("first migration asset version should be written: {error}"))?;
    git.run(Some(&source), "first migration asset staging", |command| {
        command.args(["add", ".gitattributes", "assets/model.bin"]);
    })?;
    git.run(Some(&source), "first migration asset commit", |command| {
        command.args(["commit", "-m", "Add first historical LFS asset"]);
    })?;
    let first_commit = git
        .run(Some(&source), "first migration commit lookup", |command| {
            command.args(["rev-parse", "HEAD"]);
        })?
        .stdout;
    let first_commit = String::from_utf8(first_commit)
        .map_err(|_| "first migration commit should be UTF-8".to_owned())?
        .trim()
        .to_owned();
    git.run(Some(&source), "first source LFS version push", |command| {
        command.args(["push", "origin", "HEAD:refs/heads/main"]);
    })?;

    fs::write(source.join("assets/model.bin"), latest_bytes)
        .map_err(|error| format!("latest migration asset version should be written: {error}"))?;
    git.run(Some(&source), "latest migration asset staging", |command| {
        command.args(["add", "assets/model.bin"]);
    })?;
    git.run(Some(&source), "latest migration asset commit", |command| {
        command.args(["commit", "-m", "Change historical LFS asset bytes"]);
    })?;
    git.run(Some(&source), "latest source LFS version push", |command| {
        command.args(["push", "origin", "HEAD:refs/heads/main"]);
    })?;

    let source_objects = source.join(".git/lfs/objects");
    fs::remove_dir_all(&source_objects)
        .map_err(|error| format!("source LFS media should be cleared before migration: {error}"))?;
    fs::create_dir_all(&source_objects)
        .map_err(|error| format!("source LFS media should be recreated: {error}"))?;

    let lfscloud_binary = env::var_os("LFS_CLOUD_SMOKE_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_lfscloud")));
    let migration = git.run_program(
        Some(&source),
        &lfscloud_binary,
        "compiled historical migration",
        |command| {
            command.args(["migrate", "--server", server_url, "--all-refs"]);
        },
    )?;
    let migration_output = String::from_utf8_lossy(&migration.stdout);
    for marker in [
        "lfscloud migrate complete",
        "mode: all-refs",
        "objects discovered: 2",
        "target objects: 2 uploaded",
        "repository configuration:",
        "remote.origin.lfsurl (legacy migration source)",
        "local Git config:",
    ] {
        if !migration_output.contains(marker) {
            return Err(format!("historical migration output omitted {marker:?}"));
        }
    }
    require_equal(
        fs::read(source.join("assets/model.bin"))
            .map_err(|error| format!("latest source asset should be readable: {error}"))?,
        latest_bytes.to_vec(),
        "latest source worktree bytes after migration",
    )?;

    let lfscloud::StorageProviderConfig::GoogleDrive(storage) =
        &config.storage_providers[LIVE_DRIVE_PROVIDER_ID];
    let store = GoogleDriveObjectStore::new(
        storage.clone(),
        &config.repositories[0].id,
        drive_access_token.clone(),
    )
    .map_err(|error| format!("migration Drive object store should build: {error}"))?;
    for object in [&first_object, &latest_object] {
        if store
            .lookup_object(object)
            .await
            .map_err(|error| format!("migrated Drive object lookup should succeed: {error}"))?
            .is_none()
        {
            return Err(format!(
                "migration did not store historical object sha256:{}",
                object.oid.as_hex()
            ));
        }
    }

    git.run(
        Some(&source),
        "migration endpoint commit staging",
        |command| {
            command.args(["add", ".lfsconfig"]);
        },
    )?;
    git.run(Some(&source), "migration endpoint commit", |command| {
        command.args(["commit", "-m", "Route Git LFS through LFS Cloud"]);
    })?;
    git.run(Some(&source), "migration endpoint push", |command| {
        command.args(["push", "origin", "HEAD:refs/heads/main"]);
    })?;

    git.run(None, "migrated repository pointer-only clone", |command| {
        command
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .args(["clone", "--branch", "main"])
            .arg(&github_remote_url)
            .arg(&checkout);
    })?;
    git.run(
        Some(&checkout),
        "migrated checkout Git LFS installation",
        |command| {
            command.args(["lfs", "install", "--local"]);
        },
    )?;
    git.run(Some(&checkout), "latest migrated LFS pull", |command| {
        command.args(["lfs", "pull"]);
    })?;
    require_equal(
        fs::read(checkout.join("assets/model.bin"))
            .map_err(|error| format!("latest migrated asset should be readable: {error}"))?,
        latest_bytes.to_vec(),
        "latest migrated checkout bytes",
    )?;

    git.run_program(
        Some(&checkout),
        &lfscloud_binary,
        "historical checkout local endpoint setup",
        |command| {
            command.args(["init", "--server", server_url, "--local"]);
        },
    )?;
    fs::remove_dir_all(checkout.join(".git/lfs/objects"))
        .map_err(|error| format!("checkout LFS media should be cleared: {error}"))?;
    git.run(
        Some(&checkout),
        "historical pointer-only checkout",
        |command| {
            command
                .env("GIT_LFS_SKIP_SMUDGE", "1")
                .args(["checkout", "--quiet", &first_commit]);
        },
    )?;
    git.run(Some(&checkout), "historical migrated LFS pull", |command| {
        command.args(["lfs", "pull"]);
    })?;
    require_equal(
        fs::read(checkout.join("assets/model.bin"))
            .map_err(|error| format!("historical migrated asset should be readable: {error}"))?,
        first_bytes.to_vec(),
        "historical migrated checkout bytes",
    )
}

fn require_command_success(
    output: Output,
    context: &str,
    secrets: &[String],
) -> Result<Output, String> {
    if output.status.success() {
        return Ok(output);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = redact_secrets(&format!("stdout:\n{stdout}\nstderr:\n{stderr}"), secrets);
    Err(format!(
        "{context} failed with {}\n{}",
        output.status,
        tail_lines(&detail, 80)
    ))
}

fn redact_secrets(value: &str, secrets: &[String]) -> String {
    secrets.iter().fold(value.to_owned(), |sanitized, secret| {
        if secret.is_empty() {
            sanitized
        } else {
            sanitized.replace(secret, "[redacted]")
        }
    })
}

fn tail_lines(value: &str, limit: usize) -> String {
    let lines = value.lines().collect::<Vec<_>>();
    lines[lines.len().saturating_sub(limit)..].join("\n")
}

fn combine_process_result(
    scenario: Result<(), String>,
    stop: Result<(), String>,
) -> Result<(), String> {
    match (scenario, stop) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(stop_error)) => {
            Err(format!("{error}; server cleanup also failed: {stop_error}"))
        }
    }
}

async fn verify_live_object_storage(
    client: &Client,
    repository_id: &str,
    github_user: &RepositoryUser,
    metadata: &MetadataDatabase,
    drive_access_token: &GoogleDriveAccessToken,
    object: &LfsObject,
) -> Result<(), String> {
    let record = metadata
        .lookup_object(repository_id, LIVE_DRIVE_PROVIDER_ID, object)
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

async fn wait_for_live_server(port: u16, server: &mut LiveServerProcess) -> Result<(), String> {
    for _ in 0..600 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        if let Some(status) = server.try_wait()? {
            return Err(server.unexpected_exit(status));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let diagnostics = server.diagnostics();
    if diagnostics.is_empty() {
        Err("compiled lfscloud server did not become ready within 60 seconds".to_owned())
    } else {
        Err(format!(
            "compiled lfscloud server did not become ready within 60 seconds\n{diagnostics}"
        ))
    }
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

fn require_enabled(env_name: &str) {
    assert!(
        matches!(
            env::var(env_name).as_deref(),
            Ok("1" | "true" | "TRUE" | "yes" | "YES")
        ),
        "set {env_name}=1 to run this explicitly selected external integration test"
    );
}

fn live_github_credentials() -> LiveGitHubCredentials {
    live_github_credentials_result().unwrap_or_else(|error| panic!("{error}"))
}

fn live_github_credentials_result() -> Result<LiveGitHubCredentials, String> {
    live_github_credentials_from_value(env::var(LIVE_GITHUB_PAT_ENV).ok())
}

fn live_github_credentials_from_value(
    personal_access_token: Option<String>,
) -> Result<LiveGitHubCredentials, String> {
    let personal_access_token = personal_access_token
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{LIVE_GITHUB_PAT_ENV} must be set"))?;
    Ok(LiveGitHubCredentials {
        personal_access_token,
    })
}

#[test]
fn live_github_credentials_require_pat() {
    let error = match live_github_credentials_from_value(None) {
        Ok(_) => panic!("missing GitHub PAT should fail"),
        Err(error) => error,
    };

    assert_eq!(error, "LFS_CLOUD_GITHUB_PAT must be set");
}

#[test]
fn live_github_credentials_load_pat() {
    let credentials = live_github_credentials_from_value(Some("ghp_personal_token".to_owned()))
        .expect("PAT smoke credentials should load");

    assert_eq!(credentials.personal_access_token(), "ghp_personal_token");
}

#[test]
fn live_drive_config_directory_is_used_directly() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    fs::write(
        directory
            .path()
            .join("application_default_credentials.json"),
        "{}",
    )
    .expect("ADC credentials fixture should be written");

    let credentials = google_drive_gcloud_credentials_from_directory(directory.path())
        .expect("generated ADC credentials directory should resolve");

    assert_eq!(
        credentials.config_dir,
        fs::canonicalize(directory.path()).expect("temporary directory should canonicalize")
    );
    assert_eq!(
        credentials.executable,
        PathBuf::from(LIVE_GCLOUD_EXECUTABLE)
    );
}

#[test]
fn live_drive_config_directory_requires_generated_adc_state() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");

    let error = google_drive_gcloud_credentials_from_directory(directory.path())
        .expect_err("a directory without generated ADC state must fail");

    assert!(error.contains("application_default_credentials.json"));
}

#[test]
fn live_drive_config_directory_rejects_a_file_path() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let file = directory
        .path()
        .join("application_default_credentials.json");
    fs::write(&file, "{}").expect("ADC credentials fixture should be written");

    let error = google_drive_gcloud_credentials_from_directory(&file)
        .expect_err("the config path itself must be a directory");

    assert!(error.contains("readable directory"));
}

fn google_drive_gcloud_credentials() -> GoogleDriveGcloudCredentialsConfig {
    google_drive_gcloud_credentials_result().unwrap_or_else(|error| panic!("{error}"))
}

fn google_drive_gcloud_credentials_result() -> Result<GoogleDriveGcloudCredentialsConfig, String> {
    let config_dir = env::var_os(LIVE_DRIVE_CONFIG_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| format!("{LIVE_DRIVE_CONFIG_DIR_ENV} must be set"))?;
    google_drive_gcloud_credentials_from_directory(&config_dir)
}

fn google_drive_gcloud_credentials_from_directory(
    config_dir: &Path,
) -> Result<GoogleDriveGcloudCredentialsConfig, String> {
    let config_dir = fs::canonicalize(config_dir)
        .map_err(|_| format!("{LIVE_DRIVE_CONFIG_DIR_ENV} must point to a readable directory"))?;
    if !config_dir.is_dir() {
        return Err(format!(
            "{LIVE_DRIVE_CONFIG_DIR_ENV} must point to a readable directory"
        ));
    }
    if !fs::metadata(config_dir.join("application_default_credentials.json"))
        .is_ok_and(|metadata| metadata.is_file())
    {
        return Err(format!(
            "{LIVE_DRIVE_CONFIG_DIR_ENV} must point to a directory containing application_default_credentials.json generated by gcloud"
        ));
    }

    Ok(GoogleDriveGcloudCredentialsConfig {
        config_dir,
        executable: PathBuf::from(LIVE_GCLOUD_EXECUTABLE),
    })
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
        .header(USER_AGENT, concat!("lfscloud/", env!("CARGO_PKG_VERSION")))
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
