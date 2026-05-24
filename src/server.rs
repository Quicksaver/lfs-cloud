//! HTTP server entrypoint and Git LFS route resolution.
//!
//! This module owns the first server-facing boundary: loading a validated
//! configuration, binding an Axum listener, reporting reachable URLs, and
//! resolving incoming Git LFS request paths to configured repository mappings
//! before requiring a local LFS Cloud session token. Batch-transfer behavior is
//! layered on top of this route and authentication context in later protocol
//! tasks.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::PathBuf,
    sync::Arc,
};

use axum::{
    Router,
    extract::{OriginalUri, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{AUTHORIZATION, WWW_AUTHENTICATE},
    },
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};

use crate::{
    DEFAULT_GIT_CREDENTIAL_USERNAME, LfsOid, LfsSessionMetadata, LfsSessionToken,
    LocalLfsSessionStore, MetadataDatabase, RepositoryMapping, ServerConfig, ServerError,
    ServerResult,
};

const LFS_AUTH_CHALLENGE: &str = "Basic realm=\"lfs-cloud\"";

/// Runtime options supplied by `lfs-cloud serve`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServeOptions {
    /// Optional explicit server config path.
    pub config_path: Option<PathBuf>,
    /// Optional listener host override.
    pub host: Option<String>,
    /// Optional listener port override.
    pub port: Option<u16>,
}

impl ServeOptions {
    /// Creates serve options from optional command-line overrides.
    #[must_use]
    pub fn new(config_path: Option<PathBuf>, host: Option<String>, port: Option<u16>) -> Self {
        Self {
            config_path,
            host,
            port,
        }
    }
}

/// Starts the configured LFS Cloud HTTP server and runs until shutdown.
///
/// The server currently resolves configured LFS routes and returns
/// `401 Unauthorized` for unauthenticated matched endpoints or
/// `501 Not Implemented` after a valid local LFS session token is accepted.
/// Batch and transfer behavior are implemented by later protocol tasks.
///
/// # Errors
///
/// Returns [`ServerError`] when configuration loading, metadata initialization,
/// listener binding, or Axum serving fails.
pub async fn serve(options: ServeOptions) -> ServerResult<()> {
    let config_path = options
        .config_path
        .unwrap_or_else(|| ServerConfig::default_path().to_path_buf());
    let mut config = ServerConfig::load_from_path(config_path)?;
    let bind = ServerBind::from_config_and_overrides(
        &config.server.host,
        config.server.port,
        options.host,
        options.port,
    )?;

    // Keep the metadata connection alive for the server lifetime. Handlers do
    // not use it yet, but startup should fail before listening if server-owned
    // state cannot be opened or migrated.
    let metadata_database = MetadataDatabase::open(config.server.metadata_path.clone())?;
    config.server.host = bind.host.clone();
    config.server.port = bind.port;

    let router = lfs_server_router(config);
    let listener = tokio::net::TcpListener::bind((bind.host.as_str(), bind.port))
        .await
        .map_err(|source| ServerError::Bind {
            host: bind.host.clone(),
            port: bind.port,
            source,
        })?;
    let local_addr = listener
        .local_addr()
        .map_err(|source| ServerError::LocalAddress { source })?;
    let urls = advertised_server_urls(&bind.host, local_addr.port());

    println!("{}", render_server_startup_message(&urls));

    let result = axum::serve(listener, router)
        .await
        .map_err(|source| ServerError::Serve { source });
    drop(metadata_database);
    result
}

/// Builds the Axum router for configured Git LFS paths.
pub fn lfs_server_router(config: ServerConfig) -> Router {
    lfs_server_router_with_sessions(config, LocalLfsSessionStore::new())
}

/// Builds the Axum router with an explicit local LFS session store.
///
/// This constructor lets login/callback wiring and tests share the same
/// [`LocalLfsSessionStore`] used by request authentication. Git LFS endpoint
/// requests must present a valid local LFS session token before protocol
/// handlers receive the resolved route.
pub fn lfs_server_router_with_sessions(
    config: ServerConfig,
    session_store: LocalLfsSessionStore,
) -> Router {
    let state = Arc::new(LfsServerState::new(config, session_store));

    Router::new().fallback(handle_lfs_request).with_state(state)
}

#[derive(Clone, Debug)]
struct LfsServerState {
    routes: LfsRouteResolver,
    session_store: LocalLfsSessionStore,
}

impl LfsServerState {
    fn new(config: ServerConfig, session_store: LocalLfsSessionStore) -> Self {
        Self {
            routes: LfsRouteResolver::new(&config),
            session_store,
        }
    }
}

async fn handle_lfs_request(
    State(state): State<Arc<LfsServerState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    match state.routes.resolve_path(uri.path()) {
        Ok(_route) => match authenticate_lfs_session(&headers, &state.session_store) {
            Ok(_session) => (
                StatusCode::NOT_IMPLEMENTED,
                "Git LFS endpoint routing is configured; protocol handling is not implemented yet.\n",
            )
                .into_response(),
            Err(error @ ServerError::Unauthorized { .. }) => {
                tracing::debug!(path = uri.path(), %error, "LFS route request was not authenticated");
                authentication_required_response()
            }
            Err(error) => {
                tracing::error!(path = uri.path(), %error, "failed to authenticate LFS route request");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "LFS Cloud authentication failed.\n",
                )
                    .into_response()
            }
        },
        Err(ServerError::RouteNotConfigured { .. }) => (
            StatusCode::NOT_FOUND,
            "No configured LFS Cloud repository route matches this path.\n",
        )
            .into_response(),
        Err(error @ ServerError::InvalidRequest { .. }) => {
            tracing::debug!(path = uri.path(), %error, "invalid LFS route request");
            (StatusCode::BAD_REQUEST, "Invalid LFS Cloud route.\n").into_response()
        }
        Err(error) => {
            tracing::error!(path = uri.path(), %error, "failed to resolve LFS route");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "LFS Cloud route handling failed.\n",
            )
                .into_response()
        }
    }
}

fn authenticate_lfs_session(
    headers: &HeaderMap,
    session_store: &LocalLfsSessionStore,
) -> ServerResult<LfsSessionMetadata> {
    let token = lfs_session_token_from_authorization_header(headers)?;

    session_store
        .verify(&token)
        .ok_or_else(|| unauthorized("invalid or expired lfs session token"))
}

fn lfs_session_token_from_authorization_header(
    headers: &HeaderMap,
) -> ServerResult<LfsSessionToken> {
    let Some(value) = headers.get(AUTHORIZATION) else {
        return Err(unauthorized("missing authorization header"));
    };
    let value = value
        .to_str()
        .map_err(|_| unauthorized("authorization header is not valid ASCII"))?;

    if let Some(token) = authorization_credentials(value, "Bearer") {
        return LfsSessionToken::from_secret(token.to_owned()).map_err(|_| {
            unauthorized("bearer authorization did not contain a valid lfs session token")
        });
    }

    if let Some(credentials) = authorization_credentials(value, "Basic") {
        return lfs_session_token_from_basic_credentials(credentials);
    }

    Err(unauthorized("unsupported authorization scheme"))
}

fn lfs_session_token_from_basic_credentials(credentials: &str) -> ServerResult<LfsSessionToken> {
    let decoded = BASE64_STANDARD
        .decode(credentials)
        .map_err(|_| unauthorized("basic authorization credentials were not valid base64"))?;
    let decoded = String::from_utf8(decoded)
        .map_err(|_| unauthorized("basic authorization credentials were not valid UTF-8"))?;
    let Some((username, password)) = decoded.split_once(':') else {
        return Err(unauthorized(
            "basic authorization credentials were malformed",
        ));
    };

    if username != DEFAULT_GIT_CREDENTIAL_USERNAME {
        return Err(unauthorized(
            "basic authorization username was not accepted",
        ));
    }

    LfsSessionToken::from_secret(password.to_owned())
        .map_err(|_| unauthorized("basic authorization did not contain a valid lfs session token"))
}

fn authorization_credentials<'a>(value: &'a str, scheme: &str) -> Option<&'a str> {
    let value = value.trim();
    let scheme_end = value.find(char::is_whitespace).unwrap_or(value.len());
    let (actual_scheme, rest) = value.split_at(scheme_end);

    if !actual_scheme.eq_ignore_ascii_case(scheme) {
        return None;
    }

    let credentials = rest.trim_start();
    (!credentials.is_empty()).then_some(credentials)
}

fn unauthorized(reason: impl Into<String>) -> ServerError {
    ServerError::Unauthorized {
        reason: reason.into(),
    }
}

fn authentication_required_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            WWW_AUTHENTICATE,
            HeaderValue::from_static(LFS_AUTH_CHALLENGE),
        )],
        "LFS Cloud authentication required.\n",
    )
        .into_response()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ServerBind {
    host: String,
    port: u16,
}

impl ServerBind {
    fn from_config_and_overrides(
        config_host: &str,
        config_port: u16,
        host_override: Option<String>,
        port_override: Option<u16>,
    ) -> ServerResult<Self> {
        let host = host_override.unwrap_or_else(|| config_host.to_owned());
        let port = port_override.unwrap_or(config_port);

        if host.trim().is_empty() {
            return Err(ServerError::InvalidConfiguration {
                message: "server.host must not be empty".to_owned(),
            });
        }
        if host.trim() != host {
            return Err(ServerError::InvalidConfiguration {
                message: "server.host must not include leading or trailing whitespace".to_owned(),
            });
        }
        if !is_valid_bind_host(&host) {
            return Err(ServerError::InvalidConfiguration {
                message: "server.host must be an IP address or DNS hostname".to_owned(),
            });
        }
        if port == 0 {
            // User-facing server config should advertise a stable URL instead
            // of silently choosing an OS-assigned ephemeral listener.
            return Err(ServerError::InvalidConfiguration {
                message: "server.port must be greater than zero".to_owned(),
            });
        }

        Ok(Self { host, port })
    }
}

/// Repository route and endpoint resolved from an incoming request path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedLfsRoute {
    /// Configured repository mapping matched by the request path.
    pub repository: RepositoryMapping,
    /// Git LFS endpoint beneath the repository's `/info/lfs` base path.
    pub endpoint: LfsRouteEndpoint,
}

/// Git LFS endpoint beneath a configured repository route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LfsRouteEndpoint {
    /// The repository's base `/info/lfs` path.
    Info,
    /// The Git LFS batch API at `/objects/batch`.
    Batch,
    /// An object transfer endpoint at `/objects/{oid}`.
    Object {
        /// SHA-256 object identifier from the transfer path.
        oid: LfsOid,
    },
}

/// Resolves request paths to configured repository LFS routes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LfsRouteResolver {
    routes: Vec<ConfiguredLfsRoute>,
}

impl LfsRouteResolver {
    /// Builds a resolver from validated server configuration.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::{LfsRouteEndpoint, LfsRouteResolver, ServerConfig};
    ///
    /// let config = ServerConfig::load_from_str(
    ///     r#"
    /// server:
    ///   public_url: http://127.0.0.1:8080
    /// repository_providers:
    ///   github-main:
    ///     type: github
    ///     api_url: https://api.github.com
    ///     oauth_client_id: test-client
    ///     oauth_client_secret: test-secret
    /// storage_providers:
    ///   drive-user-a:
    ///     type: google_drive
    ///     credentials_ref: google-drive-user-a
    ///     root_folder_id: root
    /// repositories:
    ///   - id: github-main:owner/repo
    ///     repo_provider: github-main
    ///     host: github.com
    ///     owner: owner
    ///     name: repo
    ///     storage_provider: drive-user-a
    /// "#,
    /// )?;
    /// let resolver = LfsRouteResolver::new(&config);
    /// let route = resolver.resolve_path("/github.com/owner/repo.git/info/lfs/objects/batch")?;
    ///
    /// assert_eq!(route.endpoint, LfsRouteEndpoint::Batch);
    /// # Ok::<(), lfs_cloud::ServerError>(())
    /// ```
    #[must_use]
    pub fn new(config: &ServerConfig) -> Self {
        let mut routes = config
            .repositories
            .iter()
            .cloned()
            .map(|repository| {
                let route_path = repository.route_path();
                ConfiguredLfsRoute {
                    route_path_with_slash: format!("{route_path}/"),
                    route_path,
                    repository,
                }
            })
            .collect::<Vec<_>>();

        routes.sort_by(|left, right| right.route_path.len().cmp(&left.route_path.len()));

        Self { routes }
    }

    /// Resolves an HTTP request path to a configured Git LFS route.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::RouteNotConfigured`] for unknown repositories and
    /// [`ServerError::InvalidRequest`] for malformed endpoints under a known
    /// repository route.
    pub fn resolve_path(&self, path: &str) -> ServerResult<ResolvedLfsRoute> {
        if !path.starts_with('/') {
            return Err(ServerError::InvalidRequest {
                message: "route path must start with '/'".to_owned(),
            });
        }

        for route in &self.routes {
            if path == route.route_path || path == route.route_path_with_slash {
                return Ok(ResolvedLfsRoute {
                    repository: route.repository.clone(),
                    endpoint: LfsRouteEndpoint::Info,
                });
            }

            let Some(suffix) = path.strip_prefix(&route.route_path_with_slash) else {
                continue;
            };

            return Ok(ResolvedLfsRoute {
                repository: route.repository.clone(),
                endpoint: parse_lfs_route_endpoint(suffix)?,
            });
        }

        Err(ServerError::RouteNotConfigured {
            path: path.to_owned(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConfiguredLfsRoute {
    route_path: String,
    route_path_with_slash: String,
    repository: RepositoryMapping,
}

fn parse_lfs_route_endpoint(suffix: &str) -> ServerResult<LfsRouteEndpoint> {
    if suffix == "objects/batch" {
        return Ok(LfsRouteEndpoint::Batch);
    }

    if let Some(oid) = suffix.strip_prefix("objects/") {
        if oid.contains('/') || oid.is_empty() {
            return Err(ServerError::InvalidRequest {
                message: format!("unsupported LFS object endpoint {suffix:?}"),
            });
        }

        return Ok(LfsRouteEndpoint::Object {
            oid: LfsOid::new(oid).map_err(|source| ServerError::InvalidRequest {
                message: format!("invalid LFS object id in route: {source}"),
            })?,
        });
    }

    Err(ServerError::InvalidRequest {
        message: format!("unsupported LFS endpoint {suffix:?}"),
    })
}

/// URLs printed when the server starts listening.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvertisedServerUrls {
    /// URL suitable for same-machine Git LFS clients.
    pub local: String,
    /// URL suitable for trusted LAN clients, when it can be detected.
    pub network: Option<String>,
}

/// Computes local and LAN URLs for a bound listener.
#[must_use]
pub fn advertised_server_urls(bind_host: &str, port: u16) -> AdvertisedServerUrls {
    let local_host = if is_unspecified_host(bind_host) {
        "127.0.0.1".to_owned()
    } else {
        advertised_url_host(bind_host)
    };
    let network = if is_unspecified_host(bind_host) {
        detect_lan_ipv4().map(|ip| format!("http://{ip}:{port}"))
    } else {
        None
    };

    AdvertisedServerUrls {
        local: format!("http://{local_host}:{port}"),
        network,
    }
}

fn advertised_url_host(host: &str) -> String {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(ip)) => format!("[{ip}]"),
        Ok(IpAddr::V4(ip)) => ip.to_string(),
        Err(_) => host.to_owned(),
    }
}

fn is_valid_bind_host(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok() || is_valid_dns_hostname(host)
}

fn is_valid_dns_hostname(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.bytes().enumerate().all(|(index, byte)| {
                    let is_alphanumeric = byte.is_ascii_alphanumeric();
                    let is_inner_hyphen = byte == b'-' && index > 0 && index + 1 < label.len();
                    is_alphanumeric || is_inner_hyphen
                })
        })
}

/// Renders the startup message shown by `lfs-cloud serve`.
#[must_use]
pub fn render_server_startup_message(urls: &AdvertisedServerUrls) -> String {
    let network = urls.network.as_deref().unwrap_or("(not detected)");

    format!(
        "lfs-cloud server running\n  local:   {}\n  network: {}",
        urls.local, network
    )
}

fn is_unspecified_host(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::" | "[::]")
}

fn detect_lan_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))).ok()?;
    // UDP connect only asks the OS which local interface would be used; no
    // LFS Cloud payload is sent to this public address.
    socket.connect(SocketAddr::from(([8, 8, 8, 8], 80))).ok()?;

    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::{
        body::Body,
        http::{
            HeaderMap, HeaderValue, Request, StatusCode,
            header::{AUTHORIZATION, WWW_AUTHENTICATE},
        },
    };
    use tower::ServiceExt;

    use super::{
        BASE64_STANDARD, LFS_AUTH_CHALLENGE, LfsRouteEndpoint, LfsRouteResolver, ServerBind,
        advertised_server_urls, authenticate_lfs_session, lfs_server_router_with_sessions,
        render_server_startup_message,
    };
    use base64::Engine as _;

    use crate::{
        DEFAULT_GIT_CREDENTIAL_USERNAME, LocalLfsSessionStore, RepositoryUser, ServerConfig,
        ServerError,
    };

    fn test_config() -> ServerConfig {
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
    storage_provider: drive-user-a
"#,
        )
        .expect("test config should load")
    }

    #[test]
    fn route_resolver_matches_configured_lfs_paths() {
        let resolver = LfsRouteResolver::new(&test_config());

        let info = resolver
            .resolve_path("/github.com/owner/repo.git/info/lfs")
            .expect("base info route should resolve");
        let info_with_trailing_slash = resolver
            .resolve_path("/github.com/owner/repo.git/info/lfs/")
            .expect("base info route with a trailing slash should resolve");
        let batch = resolver
            .resolve_path("/github.com/owner/repo.git/info/lfs/objects/batch")
            .expect("batch route should resolve");
        let object = resolver
            .resolve_path(
                "/github.com/owner/repo.git/info/lfs/objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .expect("object route should resolve");

        assert_eq!(info.repository.id, "github-main:owner/repo");
        assert_eq!(info.endpoint, LfsRouteEndpoint::Info);
        assert_eq!(info_with_trailing_slash.endpoint, LfsRouteEndpoint::Info);
        assert_eq!(batch.endpoint, LfsRouteEndpoint::Batch);
        assert!(
            matches!(object.endpoint, LfsRouteEndpoint::Object { oid } if oid.as_hex() == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn route_resolver_rejects_unknown_repositories_and_endpoints() {
        let resolver = LfsRouteResolver::new(&test_config());

        let unknown_repo = resolver
            .resolve_path("/github.com/owner/other.git/info/lfs/objects/batch")
            .expect_err("unknown route should be denied");
        let unknown_endpoint = resolver
            .resolve_path("/github.com/owner/repo.git/info/lfs/locks")
            .expect_err("unknown endpoint should be invalid");
        let bad_oid = resolver
            .resolve_path("/github.com/owner/repo.git/info/lfs/objects/not-a-sha")
            .expect_err("bad object oid should be invalid");

        assert!(matches!(
            unknown_repo,
            ServerError::RouteNotConfigured { .. }
        ));
        assert!(matches!(
            unknown_endpoint,
            ServerError::InvalidRequest { .. }
        ));
        assert!(matches!(bad_oid, ServerError::InvalidRequest { .. }));
    }

    #[test]
    fn advertised_urls_report_localhost_and_best_effort_network_url() {
        let localhost = advertised_server_urls("127.0.0.1", 8080);
        let all_interfaces = advertised_server_urls("0.0.0.0", 8080);
        let all_ipv6_interfaces = advertised_server_urls("::", 8080);

        assert_eq!(localhost.local, "http://127.0.0.1:8080");
        assert_eq!(localhost.network, None);
        assert_eq!(all_interfaces.local, "http://127.0.0.1:8080");
        assert_eq!(all_ipv6_interfaces.local, "http://127.0.0.1:8080");

        let message = render_server_startup_message(&all_interfaces);
        assert!(message.contains("lfs-cloud server running"));
        assert!(message.contains("local:   http://127.0.0.1:8080"));
        assert!(message.contains("network: "));
    }

    #[test]
    fn advertised_urls_bracket_ipv6_literals() {
        let loopback = advertised_server_urls("::1", 8080);

        assert_eq!(loopback.local, "http://[::1]:8080");
        assert_eq!(loopback.network, None);
    }

    #[test]
    fn server_bind_rejects_invalid_host_before_listener_bind() {
        let error = ServerBind::from_config_and_overrides("bad host", 8080, None, None)
            .expect_err("host with spaces should fail config validation");

        assert!(matches!(error, ServerError::InvalidConfiguration { .. }));
    }

    #[test]
    fn auth_accepts_bearer_and_basic_lfs_session_tokens() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let mut bearer_headers = HeaderMap::new();
        bearer_headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).expect("bearer header should parse"),
        );
        let mut basic_headers = HeaderMap::new();
        basic_headers.insert(
            AUTHORIZATION,
            basic_authorization(DEFAULT_GIT_CREDENTIAL_USERNAME, &token),
        );

        let bearer_session = authenticate_lfs_session(&bearer_headers, &store)
            .expect("bearer token should authenticate");
        let basic_session = authenticate_lfs_session(&basic_headers, &store)
            .expect("basic credential token should authenticate");

        assert_eq!(bearer_session.login, "octocat");
        assert_eq!(basic_session.login, "octocat");
        assert_eq!(basic_session.provider_id, "github-main");
    }

    #[test]
    fn auth_rejects_missing_malformed_wrong_and_expired_tokens() {
        let (store, token) = issued_session_token(Duration::from_millis(1));
        let cases = [
            HeaderMap::new(),
            authorization_headers("Digest abc123"),
            authorization_headers("Bearer local token"),
            authorization_headers("Basic not-base64"),
            {
                let mut headers = HeaderMap::new();
                headers.insert(AUTHORIZATION, basic_authorization("github", &token));
                headers
            },
        ];

        for headers in cases {
            let error = authenticate_lfs_session(&headers, &store)
                .expect_err("invalid credentials should be denied");
            assert!(matches!(error, ServerError::Unauthorized { .. }));
        }

        std::thread::sleep(Duration::from_millis(10));

        let error =
            authenticate_lfs_session(&authorization_headers(&format!("Bearer {token}")), &store)
                .expect_err("expired token should be denied");
        assert!(matches!(error, ServerError::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn configured_lfs_routes_require_valid_session_tokens() {
        let (store, token) = issued_session_token(Duration::from_secs(60));
        let router = lfs_server_router_with_sessions(test_config(), store);

        let unauthenticated = router
            .clone()
            .oneshot(lfs_request(
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                None,
            ))
            .await
            .expect("router should respond");
        let unknown_route = router
            .clone()
            .oneshot(lfs_request(
                "/github.com/owner/other.git/info/lfs/objects/batch",
                None,
            ))
            .await
            .expect("router should respond");
        let authenticated = router
            .oneshot(lfs_request(
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                Some(&format!("Bearer {token}")),
            ))
            .await
            .expect("router should respond");

        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthenticated.headers().get(WWW_AUTHENTICATE),
            Some(&HeaderValue::from_static(LFS_AUTH_CHALLENGE))
        );
        assert_eq!(unknown_route.status(), StatusCode::NOT_FOUND);
        assert_eq!(authenticated.status(), StatusCode::NOT_IMPLEMENTED);
    }

    fn issued_session_token(ttl: Duration) -> (LocalLfsSessionStore, String) {
        let store = LocalLfsSessionStore::new();
        let user = RepositoryUser::new("github-main", "octocat", Some("42".to_owned()));
        let issued = store
            .issue_session_with_ttl(&user, ["read:user"], ttl)
            .expect("session token should be issued");

        (store, issued.token.as_str().to_owned())
    }

    fn authorization_headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(value).expect("test authorization header should parse"),
        );
        headers
    }

    fn basic_authorization(username: &str, password: &str) -> HeaderValue {
        let encoded = BASE64_STANDARD.encode(format!("{username}:{password}"));
        HeaderValue::from_str(&format!("Basic {encoded}"))
            .expect("test basic authorization header should parse")
    }

    fn lfs_request(path: &str, authorization: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().uri(path);
        if let Some(authorization) = authorization {
            builder = builder.header(AUTHORIZATION, authorization);
        }

        builder
            .body(Body::empty())
            .expect("test request should build")
    }
}
