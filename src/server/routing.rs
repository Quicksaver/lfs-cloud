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

    fn validate_transport(&self, config: &ServerConfig) -> ServerResult<()> {
        if config.server.allow_insecure_http {
            return Ok(());
        }

        let Some(configured_public_url) = &config.server.public_url else {
            return Ok(());
        };
        let public_url = Url::parse(configured_public_url).map_err(|source| {
            ServerError::InvalidConfiguration {
                message: format!("server.public_url must be a valid absolute URL: {source}"),
            }
        })?;
        if public_url.scheme() == "https"
            || self
                .host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
        {
            return Ok(());
        }

        Err(ServerError::InvalidConfiguration {
            message: "server.host must be an exact loopback IP when server.public_url uses HTTP; set server.allow_insecure_http to true only for a trusted development network or use HTTPS through trusted TLS termination".to_owned(),
        })
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
    /// The repository's base `/info/lfs` path, which identifies the repository
    /// but is not itself a Git LFS operation endpoint.
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
    /// use lfscloud::{LfsRouteEndpoint, LfsRouteResolver, ServerConfig};
    ///
    /// let config = ServerConfig::load_from_str(
    ///     r#"
    /// server: {}
    /// repository_providers:
    ///   github-main:
    ///     type: github
    /// storage_providers:
    ///   drive-user-a:
    ///     type: google_drive
    ///     credentials:
    ///       type: gcloud
    ///       config_dir: .gcloud-drive
    ///     root_folder_id: root
    /// repositories:
    ///   - id: github-main:owner/repo
    ///     repo_provider: github-main
    ///     host: github.com
    ///     owner: owner
    ///     name: repo
    ///     provider_repository_id: "8675309"
    ///     storage_provider: drive-user-a
    /// "#,
    /// )?;
    /// let resolver = LfsRouteResolver::new(&config);
    /// let route = resolver.resolve_path("/github.com/owner/repo.git/info/lfs/objects/batch")?;
    ///
    /// assert_eq!(route.endpoint, LfsRouteEndpoint::Batch);
    /// # Ok::<(), lfscloud::ServerError>(())
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
                    repository_identity_path: format!(
                        "/{}/{}/{}",
                        repository.host, repository.owner, repository.name
                    ),
                    route_path_with_slash: format!("{route_path}/"),
                    route_path,
                    case_insensitive_identity: config
                        .repository_mapping_is_case_insensitive(&repository),
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
            if route.path_matches(path, &route.route_path)
                || route.path_matches(path, &route.route_path_with_slash)
            {
                return Ok(ResolvedLfsRoute {
                    repository: route.repository.clone(),
                    endpoint: LfsRouteEndpoint::Info,
                });
            }

            let Some(suffix) = route.strip_path_prefix(path) else {
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
    repository_identity_path: String,
    route_path: String,
    route_path_with_slash: String,
    case_insensitive_identity: bool,
    repository: RepositoryMapping,
}

impl ConfiguredLfsRoute {
    fn path_matches(&self, candidate: &str, configured: &str) -> bool {
        if !self.case_insensitive_identity {
            return candidate == configured;
        }

        let identity_length = self.repository_identity_path.len();
        let Some(candidate_identity) = candidate.get(..identity_length) else {
            return false;
        };
        let Some(candidate_suffix) = candidate.get(identity_length..) else {
            return false;
        };
        let Some(configured_suffix) = configured.get(identity_length..) else {
            return false;
        };

        candidate_identity.eq_ignore_ascii_case(&self.repository_identity_path)
            && candidate_suffix == configured_suffix
    }

    fn strip_path_prefix<'a>(&self, path: &'a str) -> Option<&'a str> {
        let prefix = &self.route_path_with_slash;
        let candidate = path.get(..prefix.len())?;
        self.path_matches(candidate, prefix)
            .then(|| &path[prefix.len()..])
    }
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

/// Renders the startup message shown by `lfscloud serve`.
#[must_use]
pub fn render_server_startup_message(urls: &AdvertisedServerUrls) -> String {
    let network = urls.network.as_deref().unwrap_or("(not detected)");

    format!(
        "LFS Cloud server running\n  local:   {}\n  network: {}",
        urls.local, network
    )
}

fn is_exact_loopback_host(host: &str) -> bool {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
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
