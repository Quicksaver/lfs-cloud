//! Shared repository-provider contract tests.
//!
//! These checks deliberately exercise both the reusable fake and the real
//! GitHub adapter so permission and repository-isolation semantics cannot
//! silently diverge between test and production implementations.

mod support;

use std::sync::Arc;

use axum::{Json, Router, extract::State, routing::get};
use lfscloud::{
    GitHubAuthenticationConfig, GitHubProviderConfig, GitHubRepositoryPermissionClient,
    GitHubRepositoryProvider, RepositoryAuthentication, RepositoryIdentity, RepositoryPermission,
    RepositoryUser,
};
use serde_json::{Value, json};
use support::storage_provider_contract::assert_storage_provider_contract;
use support::{
    FakeRepositoryProvider, FakeStorageProvider, assert_repository_isolation_contract,
    assert_repository_permission_contract,
};
use tokio::task::JoinHandle;

const PROVIDER_ID: &str = "github-main";
const REPOSITORY_ID: &str = "8675309";
const USER_ID: &str = "583231";

#[tokio::test]
async fn fake_repository_provider_satisfies_shared_permission_contract() {
    for granted in permission_levels() {
        let provider = configured_fake_provider(granted);
        assert_repository_permission_contract(
            &provider,
            &repository_identity(REPOSITORY_ID),
            &authentication(),
            granted,
        )
        .await;
    }
}

#[tokio::test]
async fn github_repository_provider_satisfies_shared_permission_contract() {
    for granted in permission_levels() {
        let server = GitHubContractServer::start(granted).await;
        let provider = github_provider(server.api_url());
        assert_repository_permission_contract(
            &provider,
            &repository_identity(REPOSITORY_ID),
            &authentication(),
            granted,
        )
        .await;
    }
}

#[tokio::test]
async fn fake_repository_provider_satisfies_shared_isolation_contract() {
    let provider = configured_fake_provider(RepositoryPermission::Write);
    assert_repository_isolation_contract(
        &provider,
        &repository_identity(REPOSITORY_ID),
        &repository_identity("8675310"),
        &authentication(),
    )
    .await;
}

#[tokio::test]
async fn github_repository_provider_satisfies_shared_isolation_contract() {
    let server = GitHubContractServer::start(RepositoryPermission::Write).await;
    let provider = github_provider(server.api_url());
    assert_repository_isolation_contract(
        &provider,
        &repository_identity(REPOSITORY_ID),
        &repository_identity("8675310"),
        &authentication(),
    )
    .await;
}

#[tokio::test]
async fn fake_storage_provider_satisfies_shared_contract() {
    let provider = FakeStorageProvider::new("drive-user-a");

    assert_storage_provider_contract(
        &provider,
        "github.com/owner/repo",
        "github.com/owner/isolated",
    )
    .await;

    assert_eq!(
        provider.object_count(),
        1,
        "primary deletion must leave only the isolated backend object"
    );
}

fn permission_levels() -> [RepositoryPermission; 3] {
    [
        RepositoryPermission::Read,
        RepositoryPermission::Write,
        RepositoryPermission::Admin,
    ]
}

fn configured_fake_provider(granted: RepositoryPermission) -> FakeRepositoryProvider {
    let provider = FakeRepositoryProvider::new(PROVIDER_ID);
    provider.add_repository(
        "github.com",
        "owner",
        "repo",
        Some(REPOSITORY_ID.to_owned()),
    );
    provider.grant_permission("github.com", "owner", "repo", "octocat", granted);
    provider
}

fn github_provider(api_url: String) -> GitHubRepositoryProvider {
    GitHubRepositoryProvider::with_client(
        GitHubProviderConfig {
            id: PROVIDER_ID.to_owned(),
            api_url,
            authentication: GitHubAuthenticationConfig::new("github-pat"),
            allow_insecure_http: true,
        },
        GitHubRepositoryPermissionClient::new().expect("GitHub API client should build"),
    )
}

fn repository_identity(stable_id: &str) -> RepositoryIdentity {
    RepositoryIdentity {
        provider_id: PROVIDER_ID.to_owned(),
        stable_id: Some(stable_id.to_owned()),
        host: "github.com".to_owned(),
        owner: "owner".to_owned(),
        name: "repo".to_owned(),
    }
}

fn authentication() -> RepositoryAuthentication {
    RepositoryAuthentication::new(
        RepositoryUser::new(PROVIDER_ID, "octocat", Some(USER_ID.to_owned())),
        "github_pat_contract_token",
    )
}

#[derive(Clone)]
struct GitHubContractState {
    granted: RepositoryPermission,
}

struct GitHubContractServer {
    api_url: String,
    task: JoinHandle<()>,
}

impl GitHubContractServer {
    async fn start(granted: RepositoryPermission) -> Self {
        let app = Router::new()
            .route(
                "/api/v3/repos/{owner}/{repo}",
                get(repository_identity_response),
            )
            .route(
                "/api/v3/repos/{owner}/{repo}/collaborators/{username}/permission",
                get(repository_permission_response),
            )
            .with_state(Arc::new(GitHubContractState { granted }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("GitHub contract server should bind");
        let address = listener
            .local_addr()
            .expect("GitHub contract server address should be available");
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("GitHub contract server should run");
        });

        Self {
            api_url: format!("http://{address}/api/v3"),
            task,
        }
    }

    fn api_url(&self) -> String {
        self.api_url.clone()
    }
}

impl Drop for GitHubContractServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn repository_identity_response() -> Json<Value> {
    Json(json!({ "id": REPOSITORY_ID.parse::<u64>().unwrap() }))
}

async fn repository_permission_response(
    State(state): State<Arc<GitHubContractState>>,
) -> Json<Value> {
    Json(json!({
        "permission": permission_label(state.granted),
        "user": { "id": USER_ID.parse::<u64>().unwrap() }
    }))
}

fn permission_label(permission: RepositoryPermission) -> &'static str {
    match permission {
        RepositoryPermission::Read => "read",
        RepositoryPermission::Write => "write",
        RepositoryPermission::Admin => "admin",
        _ => panic!("contract fixture requires a known repository permission"),
    }
}
