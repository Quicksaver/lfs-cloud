// This file is included by `mod.rs` so the migration API remains in one module.

/// Read-only discovery result for an existing Git LFS repository.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct GitLfsMigrationDiscovery {
    /// Git worktree root that was inspected.
    pub worktree_root: PathBuf,
    /// Explicit Git remote whose repository and LFS endpoint are the source.
    pub source_remote: String,
    /// Whether the `git lfs` command is available and its version output.
    pub installation: GitLfsInstallation,
    /// Git filter configuration currently visible to `git config`.
    pub filters: GitLfsFilterConfig,
    /// LFS patterns declared in discovered `.gitattributes` files.
    pub tracked_patterns: Vec<GitLfsTrackedPattern>,
    /// Repository-scoped source LFS endpoint, when configured.
    pub source_endpoint: Option<GitLfsSourceEndpoint>,
}

/// Availability and version details for the local `git lfs` command.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct GitLfsInstallation {
    /// True when `git lfs version` exits successfully.
    pub installed: bool,
    /// First line of `git lfs version` output when installation is detected.
    pub version: Option<String>,
    /// Sanitized diagnostic from a failed `git lfs version` probe.
    pub diagnostic: Option<SanitizedMessage>,
}

/// Git LFS filter settings visible to Git for the inspected repository.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct GitLfsFilterConfig {
    /// `filter.lfs.clean`, usually `git-lfs clean -- %f`.
    pub clean: Option<String>,
    /// `filter.lfs.smudge`, usually `git-lfs smudge -- %f`.
    pub smudge: Option<String>,
    /// `filter.lfs.process`, usually `git-lfs filter-process`.
    pub process: Option<String>,
    /// `filter.lfs.required`, commonly `true`.
    pub required: Option<String>,
}

/// A `.gitattributes` pattern that declares `filter=lfs`.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct GitLfsTrackedPattern {
    /// Pattern token from the `.gitattributes` line.
    pub pattern: String,
    /// Attribute tokens from the same line, with known macros expanded.
    pub attributes: Vec<String>,
    /// `.gitattributes` file that declared this pattern.
    pub source: PathBuf,
}

/// Repository-scoped Git LFS source endpoint discovered for migration.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct GitLfsSourceEndpoint {
    /// Source LFS endpoint URL from Git configuration.
    pub url: String,
    /// Config source that supplied the endpoint.
    pub source: GitLfsSourceEndpointSource,
}

/// Git configuration location that supplied a source LFS endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GitLfsSourceEndpointSource {
    /// Repository-local `.git/config`.
    LocalGitConfig,
    /// Repository-local remote-scoped `remote.<name>.lfsurl`.
    RemoteGitConfig,
    /// Remote-scoped `remote.<name>.lfsurl` committed in `.lfsconfig`.
    WorktreeRemoteConfig,
    /// Worktree `.lfsconfig`.
    WorktreeLfsConfig,
    /// Endpoint derived from the selected Git remote URL.
    RemoteUrlDefault,
}

/// Discovers existing Git LFS migration inputs for a worktree.
///
/// This function is intentionally read-only. It runs Git commands that inspect
/// repository state and reads `.gitattributes` files, but it never fetches LFS
/// objects, writes Git config, or mutates the local cache.
///
/// # Errors
///
/// Returns [`MigrationError`] when `start_dir` is not inside a Git worktree,
/// Git cannot be started for required discovery commands, or discovered
/// metadata is too large or non-UTF-8.
pub fn discover_git_lfs_migration(
    start_dir: impl AsRef<Path>,
) -> MigrationResult<GitLfsMigrationDiscovery> {
    discover_git_lfs_migration_from_remote(start_dir, DEFAULT_REMOTE_NAME)
}

/// Discovers existing Git LFS migration inputs for an explicit source remote.
///
/// The named remote controls remote-scoped LFS configuration and the fallback
/// endpoint derived from the Git remote URL. Repository-wide `lfs.url` and
/// worktree `.lfsconfig` settings retain their normal higher precedence.
///
/// This function is intentionally read-only. It runs Git commands that inspect
/// repository state and reads `.gitattributes` files, but it never fetches LFS
/// objects, writes Git config, or mutates the local cache.
///
/// # Errors
///
/// Returns [`MigrationError`] when `start_dir` is not inside a Git worktree,
/// `source_remote` is invalid or unavailable, Git cannot be started for
/// required discovery commands, or discovered metadata is too large or
/// non-UTF-8.
pub fn discover_git_lfs_migration_from_remote(
    start_dir: impl AsRef<Path>,
    source_remote: impl AsRef<str>,
) -> MigrationResult<GitLfsMigrationDiscovery> {
    discover_git_lfs_migration_from_remote_excluding_endpoint(start_dir, source_remote, None)
}

/// Discovers migration inputs while ignoring one endpoint that is already the target.
pub(crate) fn discover_git_lfs_migration_from_remote_excluding_endpoint(
    start_dir: impl AsRef<Path>,
    source_remote: impl AsRef<str>,
    excluded_endpoint: Option<&str>,
) -> MigrationResult<GitLfsMigrationDiscovery> {
    let start_dir = start_dir.as_ref();
    let worktree_root = detect_worktree_root(start_dir)?;
    let source_remote = validate_source_remote_name(source_remote.as_ref())?;

    Ok(GitLfsMigrationDiscovery {
        installation: detect_git_lfs_installation(&worktree_root),
        filters: discover_lfs_filters(&worktree_root)?,
        tracked_patterns: discover_lfs_tracked_patterns(&worktree_root)?,
        source_endpoint: discover_source_endpoint(
            &worktree_root,
            &source_remote,
            excluded_endpoint,
        )?,
        worktree_root,
        source_remote,
    })
}
fn detect_git_lfs_installation(worktree_root: &Path) -> GitLfsInstallation {
    let mut command = read_only_git_command();
    command.args(["lfs", "version"]).current_dir(worktree_root);
    match run_bounded_command_output(
        &mut command,
        "git lfs version",
        MAX_MIGRATION_GIT_OUTPUT_BYTES,
    ) {
        Ok(output) if output.status.success() => match String::from_utf8(output.stdout) {
            Ok(stdout) => {
                let version = first_non_empty_line(&stdout).map(str::to_owned);
                let diagnostic = version.is_none().then(|| {
                    SanitizedMessage::new("git lfs version succeeded but printed no version")
                });

                GitLfsInstallation {
                    installed: true,
                    version,
                    diagnostic,
                }
            }
            Err(_) => GitLfsInstallation {
                installed: true,
                version: None,
                diagnostic: Some(SanitizedMessage::new(
                    "git lfs version succeeded but printed non-UTF-8 output",
                )),
            },
        },
        Ok(output) => GitLfsInstallation {
            installed: false,
            version: None,
            diagnostic: Some(SanitizedMessage::new(git_lfs_probe_diagnostic(&output))),
        },
        Err(source) => GitLfsInstallation {
            installed: false,
            version: None,
            diagnostic: Some(SanitizedMessage::new(source.to_string())),
        },
    }
}

fn discover_lfs_filters(worktree_root: &Path) -> MigrationResult<GitLfsFilterConfig> {
    Ok(GitLfsFilterConfig {
        clean: git_config_get(worktree_root, ["config", "--get", "filter.lfs.clean"])?,
        smudge: git_config_get(worktree_root, ["config", "--get", "filter.lfs.smudge"])?,
        process: git_config_get(worktree_root, ["config", "--get", "filter.lfs.process"])?,
        required: git_config_get(worktree_root, ["config", "--get", "filter.lfs.required"])?,
    })
}

fn discover_source_endpoint(
    worktree_root: &Path,
    source_remote: &str,
    excluded_endpoint: Option<&str>,
) -> MigrationResult<Option<GitLfsSourceEndpoint>> {
    let remote_lfsurl_key = format!("remote.{source_remote}.lfsurl");
    if let Some(url) = git_config_get_os(
        worktree_root,
        [
            OsStr::new("config"),
            OsStr::new("--local"),
            OsStr::new("--get"),
            OsStr::new(&remote_lfsurl_key),
        ],
        &format!("git config --local --get remote.{source_remote}.lfsurl"),
    )?
        && !source_endpoint_is_excluded(&url, excluded_endpoint)
    {
        return Ok(Some(GitLfsSourceEndpoint {
            url,
            source: GitLfsSourceEndpointSource::RemoteGitConfig,
        }));
    }

    let lfsconfig_path = worktree_root.join(".lfsconfig");
    let has_lfsconfig = is_regular_file_without_following_symlinks(&lfsconfig_path)?;
    if has_lfsconfig
        && let Some(url) = git_config_get_os(
            worktree_root,
            [
                OsStr::new("config"),
                OsStr::new("--no-includes"),
                OsStr::new("--file"),
                lfsconfig_path.as_os_str(),
                OsStr::new("--get"),
                OsStr::new(&remote_lfsurl_key),
            ],
            &format!(
                "git config --no-includes --file .lfsconfig --get remote.{source_remote}.lfsurl"
            ),
        )?
        && !source_endpoint_is_excluded(&url, excluded_endpoint)
    {
        return Ok(Some(GitLfsSourceEndpoint {
            url,
            source: GitLfsSourceEndpointSource::WorktreeRemoteConfig,
        }));
    }

    if let Some(url) = git_config_get(worktree_root, ["config", "--local", "--get", "lfs.url"])?
        && !source_endpoint_is_excluded(&url, excluded_endpoint)
    {
        return Ok(Some(GitLfsSourceEndpoint {
            url,
            source: GitLfsSourceEndpointSource::LocalGitConfig,
        }));
    }

    if has_lfsconfig
        && let Some(url) = git_config_get_os(
            worktree_root,
            [
                OsStr::new("config"),
                OsStr::new("--no-includes"),
                OsStr::new("--file"),
                lfsconfig_path.as_os_str(),
                OsStr::new("--get"),
                OsStr::new("lfs.url"),
            ],
            "git config --no-includes --file .lfsconfig --get lfs.url",
        )?
        && !source_endpoint_is_excluded(&url, excluded_endpoint)
    {
        return Ok(Some(GitLfsSourceEndpoint {
            url,
            source: GitLfsSourceEndpointSource::WorktreeLfsConfig,
        }));
    }

    let remote_url_key = format!("remote.{source_remote}.url");
    let Some(remote_url) = git_config_get_os(
        worktree_root,
        [
            OsStr::new("config"),
            OsStr::new("--local"),
            OsStr::new("--get"),
            OsStr::new(&remote_url_key),
        ],
        &format!("git config --local --get remote.{source_remote}.url"),
    )?
    else {
        return Ok(None);
    };

    Ok(default_lfs_endpoint_for_remote_url(&remote_url)
        .filter(|url| !source_endpoint_is_excluded(url, excluded_endpoint))
        .map(|url| GitLfsSourceEndpoint {
            url,
            source: GitLfsSourceEndpointSource::RemoteUrlDefault,
        }))
}

fn source_endpoint_is_excluded(candidate: &str, excluded_endpoint: Option<&str>) -> bool {
    let Some(excluded_endpoint) = excluded_endpoint else {
        return false;
    };
    match (
        validated_migration_source_endpoint(candidate, true),
        validated_migration_source_endpoint(excluded_endpoint, true),
    ) {
        (Ok(candidate), Ok(excluded)) => candidate == excluded,
        _ => candidate == excluded_endpoint,
    }
}

fn validate_source_remote_name(source_remote: &str) -> MigrationResult<String> {
    if source_remote.trim().is_empty()
        || source_remote.trim().len() != source_remote.len()
        || source_remote.chars().any(char::is_control)
        || source_remote.chars().any(char::is_whitespace)
    {
        return Err(MigrationError::InvalidInput {
            message: SanitizedMessage::new(
                "source remote name must not be blank, padded, or contain whitespace or control characters",
            ),
        });
    }

    Ok(source_remote.to_owned())
}

fn default_lfs_endpoint_for_remote_url(remote_url: &str) -> Option<String> {
    let trimmed = remote_url.trim();
    if trimmed.is_empty() || trimmed.len() != remote_url.len() {
        return None;
    }

    if trimmed.contains("://") {
        return default_lfs_endpoint_for_url_remote(trimmed);
    }

    default_lfs_endpoint_for_scp_like_remote(trimmed)
}

fn default_lfs_endpoint_for_url_remote(remote_url: &str) -> Option<String> {
    let url = Url::parse(remote_url).ok()?;
    if url.query().is_some() || url.fragment().is_some() {
        return None;
    }

    match url.scheme() {
        "http" | "https" => append_info_lfs_to_url(url),
        "ssh" => {
            let host = url.host_str()?;
            let path = url.path().trim_matches('/');
            default_https_lfs_endpoint(host, path)
        }
        _ => None,
    }
}

fn default_lfs_endpoint_for_scp_like_remote(remote_url: &str) -> Option<String> {
    let (host_part, path) = remote_url.split_once(':')?;
    if host_part.contains('/') || path.starts_with('/') {
        return None;
    }

    let host = host_part
        .rsplit_once('@')
        .map_or(host_part, |(_, host)| host)
        .trim();

    default_https_lfs_endpoint(host, path.trim_matches('/'))
}

fn default_https_lfs_endpoint(host: &str, path: &str) -> Option<String> {
    if host.is_empty() || path.is_empty() || path.contains('?') || path.contains('#') {
        return None;
    }

    let mut url = Url::parse(&format!("https://{host}/")).ok()?;
    {
        let mut segments = url.path_segments_mut().ok()?;
        segments.extend(path.split('/').filter(|segment| !segment.is_empty()));
        segments.extend(["info", "lfs"]);
    }

    Some(url.to_string())
}

fn append_info_lfs_to_url(mut url: Url) -> Option<String> {
    if url.path().trim_matches('/').is_empty() || url.query().is_some() || url.fragment().is_some()
    {
        return None;
    }

    {
        let mut segments = url.path_segments_mut().ok()?;
        segments.extend(["info", "lfs"]);
    }

    Some(url.to_string())
}

fn discover_lfs_tracked_patterns(
    worktree_root: &Path,
) -> MigrationResult<Vec<GitLfsTrackedPattern>> {
    let attributes_files = git_attributes_files(worktree_root)?;
    let mut patterns = Vec::new();

    for attributes_file in attributes_files {
        let path = worktree_root.join(&attributes_file);
        if !is_regular_file_without_following_symlinks(&path)? {
            continue;
        }

        let metadata = fs::metadata(&path).map_err(|source| MigrationError::Io {
            context: format!("failed to inspect {}", path.display()),
            source,
        })?;
        if metadata.len() > MAX_GIT_ATTRIBUTES_BYTES {
            return Err(MigrationError::ExternalCommandOutput {
                command: format!("read {}", attributes_file.display()),
                message: SanitizedMessage::new(".gitattributes file is too large"),
            });
        }

        let contents = fs::read(&path).map_err(|source| MigrationError::Io {
            context: format!("failed to read {}", path.display()),
            source,
        })?;
        let contents = String::from_utf8_lossy(&contents);

        patterns.extend(parse_lfs_patterns_from_attributes(
            contents.as_ref(),
            attributes_file.clone(),
        ));
    }

    Ok(patterns)
}

fn git_attributes_files(worktree_root: &Path) -> MigrationResult<Vec<PathBuf>> {
    let output = run_git_os(
        worktree_root,
        [
            OsStr::new("ls-files"),
            OsStr::new("-z"),
            OsStr::new("--cached"),
            OsStr::new("--others"),
            OsStr::new("--exclude-standard"),
            OsStr::new("--"),
            OsStr::new(".gitattributes"),
            OsStr::new(":(glob)**/.gitattributes"),
        ],
        "git ls-files -z --cached --others --exclude-standard -- .gitattributes ':(glob)**/.gitattributes'",
    )?;

    let stdout = required_success_stdout(
        output,
        "git ls-files -z --cached --others --exclude-standard -- .gitattributes ':(glob)**/.gitattributes'",
    )?;

    let mut paths = BTreeSet::new();
    for value in stdout.split('\0').filter(|value| !value.is_empty()) {
        paths.insert(repo_relative_path_from_git_output(value)?);
    }

    Ok(paths.into_iter().collect())
}


#[cfg(test)]
mod discovery_tests {
    use super::discover_git_lfs_migration_from_remote_excluding_endpoint;
    use super::test_support::*;

    #[test]
    fn discovers_lfs_filters_patterns_and_local_endpoint() {
        let repo = TempRepo::new();
        repo.git(["config", "filter.lfs.clean", "git-lfs clean -- %f"]);
        repo.git(["config", "filter.lfs.smudge", "git-lfs smudge -- %f"]);
        repo.git(["config", "filter.lfs.process", "git-lfs filter-process"]);
        repo.git(["config", "filter.lfs.required", "true"]);
        repo.git([
            "config",
            "--local",
            "lfs.url",
            "https://source.example/owner/repo.git/info/lfs",
        ]);
        repo.write_file(
            ".gitattributes",
            "*.bin filter=lfs diff=lfs merge=lfs -text\n*.txt text\n",
        );
        repo.write_file("assets/.gitattributes", "*.psd -text filter=lfs diff=lfs\n");

        let discovery =
            discover_git_lfs_migration(repo.path()).expect("migration discovery should succeed");

        assert_eq!(
            discovery
                .worktree_root
                .canonicalize()
                .expect("Git worktree root should canonicalize"),
            repo.path()
                .canonicalize()
                .expect("temporary repo path should canonicalize")
        );
        assert_eq!(
            discovery.filters.clean.as_deref(),
            Some("git-lfs clean -- %f")
        );
        assert_eq!(
            discovery.filters.smudge.as_deref(),
            Some("git-lfs smudge -- %f")
        );
        assert_eq!(
            discovery.filters.process.as_deref(),
            Some("git-lfs filter-process")
        );
        assert_eq!(discovery.filters.required.as_deref(), Some("true"));

        let endpoint = discovery
            .source_endpoint
            .expect("local lfs.url should be detected");
        assert_eq!(
            endpoint.url,
            "https://source.example/owner/repo.git/info/lfs"
        );
        assert_eq!(endpoint.source, GitLfsSourceEndpointSource::LocalGitConfig);

        assert_eq!(discovery.tracked_patterns.len(), 2);
        assert!(discovery.tracked_patterns.iter().any(|pattern| {
            pattern.pattern == "*.bin" && pattern.source == Path::new(".gitattributes")
        }));
        assert!(discovery.tracked_patterns.iter().any(|pattern| {
            pattern.pattern == "*.psd" && pattern.source == Path::new("assets/.gitattributes")
        }));
    }

    #[test]
    fn source_endpoint_falls_back_to_lfsconfig() {
        let repo = TempRepo::new();
        repo.write_file(
            ".lfsconfig",
            "[lfs]\n    url = https://source.example/from-lfsconfig.git/info/lfs\n",
        );

        let discovery =
            discover_git_lfs_migration(repo.path()).expect("migration discovery should succeed");
        let endpoint = discovery
            .source_endpoint
            .expect(".lfsconfig lfs.url should be detected");

        assert_eq!(
            endpoint.url,
            "https://source.example/from-lfsconfig.git/info/lfs"
        );
        assert_eq!(
            endpoint.source,
            GitLfsSourceEndpointSource::WorktreeLfsConfig
        );
    }

    #[test]
    fn source_endpoint_falls_back_to_remote_lfsurl() {
        let repo = TempRepo::new();
        repo.git([
            "config",
            "--local",
            "remote.origin.lfsurl",
            "https://source.example/from-remote.git/info/lfs",
        ]);

        let discovery =
            discover_git_lfs_migration(repo.path()).expect("migration discovery should succeed");
        let endpoint = discovery
            .source_endpoint
            .expect("remote origin lfsurl should be detected");

        assert_eq!(
            endpoint.url,
            "https://source.example/from-remote.git/info/lfs"
        );
        assert_eq!(endpoint.source, GitLfsSourceEndpointSource::RemoteGitConfig);
    }

    #[test]
    fn source_endpoint_falls_back_to_remote_url_default() {
        let repo = TempRepo::new();
        repo.git([
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ]);

        let discovery =
            discover_git_lfs_migration(repo.path()).expect("migration discovery should succeed");
        let endpoint = discovery
            .source_endpoint
            .expect("origin remote URL should provide a default LFS endpoint");

        assert_eq!(endpoint.url, "https://github.com/owner/repo.git/info/lfs");
        assert_eq!(
            endpoint.source,
            GitLfsSourceEndpointSource::RemoteUrlDefault
        );
    }

    #[test]
    fn source_endpoint_defaults_to_origin_instead_of_current_branch_remote() {
        let repo = TempRepo::new();
        repo.git([
            "remote",
            "add",
            "origin",
            "https://github.com/origin/repo.git",
        ]);
        repo.git([
            "remote",
            "add",
            "upstream",
            "https://github.com/upstream/repo.git",
        ]);
        repo.git(["checkout", "-b", "feature"]);
        repo.git(["config", "--local", "branch.feature.remote", "upstream"]);

        let discovery =
            discover_git_lfs_migration(repo.path()).expect("migration discovery should succeed");
        let endpoint = discovery
            .source_endpoint
            .expect("origin remote URL should provide a default LFS endpoint");

        assert_eq!(endpoint.url, "https://github.com/origin/repo.git/info/lfs");
        assert_eq!(discovery.source_remote, "origin");
        assert_eq!(
            endpoint.source,
            GitLfsSourceEndpointSource::RemoteUrlDefault
        );
    }

    #[test]
    fn source_endpoint_uses_the_explicit_source_remote() {
        let repo = TempRepo::new();
        repo.git([
            "remote",
            "add",
            "origin",
            "https://github.com/target/repo.git",
        ]);
        repo.git([
            "remote",
            "add",
            "upstream",
            "https://github.com/source/repo.git",
        ]);
        repo.git(["checkout", "-b", "feature"]);
        repo.git(["config", "--local", "branch.feature.remote", "origin"]);

        let discovery = discover_git_lfs_migration_from_remote(repo.path(), "upstream")
            .expect("migration discovery should use the selected source remote");
        let endpoint = discovery
            .source_endpoint
            .expect("explicit source remote should provide a default LFS endpoint");

        assert_eq!(discovery.source_remote, "upstream");
        assert_eq!(endpoint.url, "https://github.com/source/repo.git/info/lfs");
        assert_eq!(
            endpoint.source,
            GitLfsSourceEndpointSource::RemoteUrlDefault
        );
    }

    #[test]
    fn lfsconfig_symlink_is_not_used_as_source_endpoint() {
        let repo = TempRepo::new();
        repo.git([
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ]);
        repo.write_file(
            "outside-lfsconfig",
            "[lfs]\n    url = https://source.example/symlink.git/info/lfs\n",
        );
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            repo.path().join("outside-lfsconfig"),
            repo.path().join(".lfsconfig"),
        )
        .expect("test symlink should be created");

        let discovery =
            discover_git_lfs_migration(repo.path()).expect("migration discovery should succeed");
        let endpoint = discovery
            .source_endpoint
            .expect("origin remote URL should provide a default LFS endpoint");

        assert_eq!(endpoint.url, "https://github.com/owner/repo.git/info/lfs");
        assert_eq!(
            endpoint.source,
            GitLfsSourceEndpointSource::RemoteUrlDefault
        );
    }

    #[test]
    fn local_endpoint_takes_precedence_over_lfsconfig() {
        let repo = TempRepo::new();
        repo.write_file(
            ".lfsconfig",
            "[lfs]\n    url = https://source.example/from-lfsconfig.git/info/lfs\n",
        );
        repo.git([
            "config",
            "--local",
            "lfs.url",
            "https://source.example/local.git/info/lfs",
        ]);

        let discovery =
            discover_git_lfs_migration(repo.path()).expect("migration discovery should succeed");
        let endpoint = discovery
            .source_endpoint
            .expect("local lfs.url should be detected");

        assert_eq!(endpoint.url, "https://source.example/local.git/info/lfs");
        assert_eq!(endpoint.source, GitLfsSourceEndpointSource::LocalGitConfig);
    }

    #[test]
    fn committed_remote_endpoint_remains_the_source_after_lfscloud_configuration() {
        let repo = TempRepo::new();
        repo.write_file(
            ".lfsconfig",
            concat!(
                "[lfs]\n",
                "    url = https://cloud.example/github.com/owner/repo.git/info/lfs\n",
                "[remote \"origin\"]\n",
                "    lfsurl = https://legacy.example/owner/repo.git/info/lfs\n",
            ),
        );

        let discovery =
            discover_git_lfs_migration(repo.path()).expect("migration discovery should succeed");
        let endpoint = discovery
            .source_endpoint
            .expect("committed legacy endpoint should be detected");

        assert_eq!(
            endpoint.url,
            "https://legacy.example/owner/repo.git/info/lfs"
        );
        assert_eq!(
            endpoint.source,
            GitLfsSourceEndpointSource::WorktreeRemoteConfig
        );
    }

    #[test]
    fn target_endpoint_is_ignored_in_favor_of_remote_default() {
        let repo = TempRepo::new();
        repo.git([
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ]);
        let target = "https://cloud.example/github.com/owner/repo.git/info/lfs";
        repo.write_file(".lfsconfig", &format!("[lfs]\n    url = {target}\n"));

        let discovery = discover_git_lfs_migration_from_remote_excluding_endpoint(
            repo.path(),
            "origin",
            Some(target),
        )
        .expect("migration discovery should skip the target endpoint");
        let endpoint = discovery
            .source_endpoint
            .expect("the selected remote should supply the legacy default");

        assert_eq!(endpoint.url, "https://github.com/owner/repo.git/info/lfs");
        assert_eq!(endpoint.source, GitLfsSourceEndpointSource::RemoteUrlDefault);
    }

    #[test]
    fn target_endpoint_with_trailing_slash_is_ignored_in_favor_of_legacy_endpoint() {
        let repo = TempRepo::new();
        repo.git([
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ]);
        let target = "https://cloud.example/github.com/owner/repo.git/info/lfs";
        repo.git([
            "config",
            "--local",
            "remote.origin.lfsurl",
            &format!("{target}/"),
        ]);
        repo.write_file(
            ".lfsconfig",
            concat!(
                "[remote \"origin\"]\n",
                "    lfsurl = https://legacy.example/owner/repo.git/info/lfs\n",
            ),
        );

        let discovery = discover_git_lfs_migration_from_remote_excluding_endpoint(
            repo.path(),
            "origin",
            Some(target),
        )
        .expect("migration discovery should skip an equivalent target endpoint");
        let endpoint = discovery
            .source_endpoint
            .expect("the committed legacy endpoint should be selected");

        assert_eq!(
            endpoint.url,
            "https://legacy.example/owner/repo.git/info/lfs"
        );
        assert_eq!(
            endpoint.source,
            GitLfsSourceEndpointSource::WorktreeRemoteConfig
        );
    }

    #[test]
    fn reports_not_git_repository_for_plain_directory() {
        let plain_directory = tempfile::tempdir().expect("temporary directory should be created");

        let error = discover_git_lfs_migration(plain_directory.path())
            .expect_err("plain directory should not discover as Git repository");

        assert!(matches!(error, MigrationError::NotGitRepository { .. }));
    }

    #[test]
    fn rejects_gitattributes_paths_outside_worktree() {
        assert!(repo_relative_path_from_git_output("/tmp/.gitattributes").is_err());
        assert!(repo_relative_path_from_git_output("../.gitattributes").is_err());
        assert!(repo_relative_path_from_git_output("safe/.gitattributes").is_ok());
    }

    #[test]
    fn discovers_lossy_non_utf8_gitattributes_files() {
        let repo = TempRepo::new();
        repo.write_bytes(".gitattributes", b"*.bin filter=lfs diff=lfs\n\xFF\n");

        let discovery =
            discover_git_lfs_migration(repo.path()).expect("migration discovery should succeed");

        assert_eq!(discovery.tracked_patterns.len(), 1);
        assert_eq!(discovery.tracked_patterns[0].pattern, "*.bin");
    }

    #[test]
    fn rejects_oversized_gitattributes_files() {
        let repo = TempRepo::new();
        repo.write_bytes(
            ".gitattributes",
            &vec![b'a'; MAX_GIT_ATTRIBUTES_BYTES as usize + 1],
        );

        let error = discover_git_lfs_migration(repo.path())
            .expect_err("oversized .gitattributes should fail discovery");

        assert!(matches!(
            error,
            MigrationError::ExternalCommandOutput { .. }
        ));
    }

    #[test]
    fn derives_default_lfs_endpoints_for_common_remote_shapes() {
        assert_eq!(
            default_lfs_endpoint_for_remote_url("git@github.com:owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo.git/info/lfs")
        );
        assert_eq!(
            default_lfs_endpoint_for_remote_url("ssh://git@github.com/owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo.git/info/lfs")
        );
        assert_eq!(
            default_lfs_endpoint_for_remote_url("https://github.com/owner/repo.git/info/lfs")
                .as_deref(),
            Some("https://github.com/owner/repo.git/info/lfs/info/lfs")
        );
        assert_eq!(
            default_lfs_endpoint_for_remote_url("https://github.com/info/lfs").as_deref(),
            Some("https://github.com/info/lfs/info/lfs")
        );
        assert_eq!(
            default_lfs_endpoint_for_remote_url("git@github.com:info/lfs").as_deref(),
            Some("https://github.com/info/lfs/info/lfs")
        );
    }

    #[test]
    fn rejects_unsafe_default_lfs_endpoint_remotes() {
        assert!(
            default_lfs_endpoint_for_remote_url(" https://github.com/owner/repo.git").is_none()
        );
        assert!(
            default_lfs_endpoint_for_remote_url("https://github.com/owner/repo.git?token=secret")
                .is_none()
        );
        assert!(
            default_lfs_endpoint_for_remote_url("https://github.com/owner/repo.git#fragment")
                .is_none()
        );
    }

}
