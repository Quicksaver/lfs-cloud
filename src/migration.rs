//! Migration discovery helpers for existing Git LFS repositories.
//!
//! Migration planning starts by inspecting the current repository without
//! writing to Git config, the worktree, the local cache, or any storage
//! provider. This module owns that read-only boundary so later migration steps
//! can build dry-run and transfer plans from one consistent snapshot.

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
};

use crate::{LfsObject, LfsPointer, MigrationError, MigrationResult, SanitizedMessage};
use url::Url;

const DEFAULT_REMOTE_NAME: &str = "origin";
const MAX_MIGRATION_GIT_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_GIT_ATTRIBUTES_BYTES: u64 = 256 * 1024;
const MAX_CURRENT_CHECKOUT_POINTER_BYTES: u64 = 64 * 1024;

/// Read-only discovery result for an existing Git LFS repository.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct GitLfsMigrationDiscovery {
    /// Git worktree root that was inspected.
    pub worktree_root: PathBuf,
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

/// Git LFS pointers discovered from the current checkout.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct CurrentCheckoutLfsPointers {
    /// Git worktree root that was inspected.
    pub worktree_root: PathBuf,
    /// Number of tracked checkout paths whose Git attributes use `filter=lfs`.
    pub tracked_path_count: usize,
    /// Pointer files found among the currently checked-out LFS-tracked paths.
    pub pointers: Vec<CurrentCheckoutLfsPointer>,
}

/// A Git LFS pointer file found in the current checkout.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct CurrentCheckoutLfsPointer {
    /// Repository-relative path to the pointer file.
    pub relative_path: PathBuf,
    /// Absolute worktree path to the pointer file.
    pub path: PathBuf,
    /// Object identity referenced by the pointer file.
    pub object: LfsObject,
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
    let start_dir = start_dir.as_ref();
    let worktree_root = detect_worktree_root(start_dir)?;

    Ok(GitLfsMigrationDiscovery {
        installation: detect_git_lfs_installation(&worktree_root),
        filters: discover_lfs_filters(&worktree_root)?,
        tracked_patterns: discover_lfs_tracked_patterns(&worktree_root)?,
        source_endpoint: discover_source_endpoint(&worktree_root)?,
        worktree_root,
    })
}

/// Enumerates Git LFS pointer files in the current checkout.
///
/// This function is intentionally read-only. It asks Git which tracked paths
/// have `filter=lfs`, then parses only small pointer-shaped files in the
/// current worktree. Hydrated files and ordinary files are reported by their
/// absence so migration planning can distinguish current checkout coverage from
/// later history scans.
///
/// # Errors
///
/// Returns [`MigrationError`] when `start_dir` is not inside a Git worktree,
/// Git cannot list tracked files or attributes, or Git returns unsafe path data.
pub fn enumerate_current_checkout_lfs_pointers(
    start_dir: impl AsRef<Path>,
) -> MigrationResult<CurrentCheckoutLfsPointers> {
    let start_dir = start_dir.as_ref();
    let worktree_root = detect_worktree_root(start_dir)?;
    let lfs_tracked_paths = current_checkout_lfs_tracked_paths(&worktree_root)?;
    let mut pointers = Vec::new();

    for relative_path in &lfs_tracked_paths {
        let path = worktree_root.join(relative_path);
        let Some(pointer) = read_current_checkout_pointer_candidate(&path)? else {
            continue;
        };

        pointers.push(CurrentCheckoutLfsPointer {
            relative_path: relative_path.clone(),
            path,
            object: pointer.object,
        });
    }

    Ok(CurrentCheckoutLfsPointers {
        worktree_root,
        tracked_path_count: lfs_tracked_paths.len(),
        pointers,
    })
}

fn detect_worktree_root(start_dir: &Path) -> MigrationResult<PathBuf> {
    let output = run_git(start_dir, ["rev-parse", "--show-toplevel"])?;
    if !output.status.success() {
        return Err(MigrationError::NotGitRepository {
            path: start_dir.to_path_buf(),
        });
    }

    let stdout = output_stdout(output, "git rev-parse --show-toplevel")?;
    Ok(PathBuf::from(stdout.trim_end_matches(['\n', '\r'])))
}

fn detect_git_lfs_installation(worktree_root: &Path) -> GitLfsInstallation {
    match Command::new("git")
        .args(["lfs", "version"])
        .current_dir(worktree_root)
        .output()
    {
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
            diagnostic: Some(SanitizedMessage::new(format!(
                "failed to start git lfs version: {source}"
            ))),
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

fn discover_source_endpoint(worktree_root: &Path) -> MigrationResult<Option<GitLfsSourceEndpoint>> {
    if let Some(url) = git_config_get(worktree_root, ["config", "--local", "--get", "lfs.url"])? {
        return Ok(Some(GitLfsSourceEndpoint {
            url,
            source: GitLfsSourceEndpointSource::LocalGitConfig,
        }));
    }

    let remote_name = source_remote_name(worktree_root)?;
    let remote_lfsurl_key = format!("remote.{remote_name}.lfsurl");
    if let Some(url) = git_config_get_os(
        worktree_root,
        [
            OsStr::new("config"),
            OsStr::new("--local"),
            OsStr::new("--get"),
            OsStr::new(&remote_lfsurl_key),
        ],
        &format!("git config --local --get remote.{remote_name}.lfsurl"),
    )? {
        return Ok(Some(GitLfsSourceEndpoint {
            url,
            source: GitLfsSourceEndpointSource::RemoteGitConfig,
        }));
    }

    let lfsconfig_path = worktree_root.join(".lfsconfig");
    if is_regular_file_without_following_symlinks(&lfsconfig_path)?
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
    {
        return Ok(Some(GitLfsSourceEndpoint {
            url,
            source: GitLfsSourceEndpointSource::WorktreeLfsConfig,
        }));
    }

    let remote_url_key = format!("remote.{remote_name}.url");
    let Some(remote_url) = git_config_get_os(
        worktree_root,
        [
            OsStr::new("config"),
            OsStr::new("--local"),
            OsStr::new("--get"),
            OsStr::new(&remote_url_key),
        ],
        &format!("git config --local --get remote.{remote_name}.url"),
    )?
    else {
        return Ok(None);
    };

    Ok(
        default_lfs_endpoint_for_remote_url(&remote_url).map(|url| GitLfsSourceEndpoint {
            url,
            source: GitLfsSourceEndpointSource::RemoteUrlDefault,
        }),
    )
}

fn source_remote_name(worktree_root: &Path) -> MigrationResult<String> {
    let Some(branch_name) = git_config_get(
        worktree_root,
        ["symbolic-ref", "--quiet", "--short", "HEAD"],
    )?
    else {
        return Ok(DEFAULT_REMOTE_NAME.to_owned());
    };

    let remote_key = format!("branch.{branch_name}.remote");
    Ok(git_config_get_os(
        worktree_root,
        [
            OsStr::new("config"),
            OsStr::new("--local"),
            OsStr::new("--get"),
            OsStr::new(&remote_key),
        ],
        &format!("git config --local --get branch.{branch_name}.remote"),
    )?
    .unwrap_or_else(|| DEFAULT_REMOTE_NAME.to_owned()))
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

fn current_checkout_lfs_tracked_paths(worktree_root: &Path) -> MigrationResult<Vec<PathBuf>> {
    let output = run_git_os(
        worktree_root,
        [
            OsStr::new("ls-files"),
            OsStr::new("-z"),
            OsStr::new("--cached"),
        ],
        "git ls-files -z --cached",
    )?;
    if !output.status.success() {
        return Err(command_error(
            "git ls-files -z --cached",
            output.status,
            &output.stderr,
        ));
    }
    if output.stdout.is_empty() {
        return Ok(Vec::new());
    }

    let output = git_check_attr_filter(worktree_root, &output.stdout)?;
    parse_git_check_attr_filter_stdout(&output.stdout)
}

fn parse_git_check_attr_filter_stdout(stdout: &[u8]) -> MigrationResult<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut fields = stdout.split(|byte| *byte == b'\0').peekable();
    while let Some(relative_path) = fields.next() {
        if relative_path.is_empty() && fields.peek().is_none() {
            break;
        }

        let Some(attribute) = fields.next() else {
            return Err(git_check_attr_parse_error());
        };
        let Some(value) = fields.next() else {
            return Err(git_check_attr_parse_error());
        };

        if attribute == b"filter" && value == b"lfs" {
            paths.push(safe_git_relative_path(
                relative_path,
                "git check-attr -z --stdin filter",
            )?);
        }
    }

    Ok(paths)
}

fn git_check_attr_filter(worktree_root: &Path, tracked_paths: &[u8]) -> MigrationResult<Output> {
    let mut child = Command::new("git")
        .args(["check-attr", "-z", "--stdin", "filter"])
        .current_dir(worktree_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| MigrationError::Io {
            context: "failed to start git check-attr -z --stdin filter".to_owned(),
            source,
        })?;

    let mut stdin = child.stdin.take().expect("child stdin should be piped");
    let tracked_paths = tracked_paths.to_owned();
    let stdin_writer = std::thread::spawn(move || {
        let write_result = stdin.write_all(&tracked_paths);
        drop(stdin);

        write_result
    });

    let output = child
        .wait_with_output()
        .map_err(|source| MigrationError::Io {
            context: "failed to wait for git check-attr -z --stdin filter".to_owned(),
            source,
        })?;

    let write_result = stdin_writer.join().map_err(|_| MigrationError::Io {
        context: "git check-attr input writer panicked".to_owned(),
        source: io::Error::other("git check-attr input writer panicked"),
    })?;

    if !output.status.success() {
        return Err(command_error(
            "git check-attr -z --stdin filter",
            output.status,
            &output.stderr,
        ));
    }

    write_result.map_err(|source| MigrationError::Io {
        context: "failed to write git check-attr path input".to_owned(),
        source,
    })?;

    Ok(output)
}

fn git_check_attr_parse_error() -> MigrationError {
    MigrationError::ExternalCommandOutput {
        command: "git check-attr -z --stdin filter".to_owned(),
        message: SanitizedMessage::new("git returned malformed attribute output"),
    }
}

fn safe_git_relative_path(relative_path: &[u8], command: &str) -> MigrationResult<PathBuf> {
    let path = git_path_bytes_to_path_buf(relative_path, command)?;
    let valid = !path.is_absolute()
        && path.components().next().is_some()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));

    if valid {
        Ok(path)
    } else {
        Err(MigrationError::ExternalCommandOutput {
            command: command.to_owned(),
            message: SanitizedMessage::new("git returned a path outside the worktree"),
        })
    }
}

#[cfg(unix)]
fn git_path_bytes_to_path_buf(relative_path: &[u8], _command: &str) -> MigrationResult<PathBuf> {
    Ok(PathBuf::from(OsString::from_vec(relative_path.to_owned())))
}

#[cfg(not(unix))]
fn git_path_bytes_to_path_buf(relative_path: &[u8], command: &str) -> MigrationResult<PathBuf> {
    String::from_utf8(relative_path.to_owned())
        .map(PathBuf::from)
        .map_err(|_| MigrationError::ExternalCommandOutput {
            command: command.to_owned(),
            message: SanitizedMessage::new("git returned non-UTF-8 path output"),
        })
}

fn read_current_checkout_pointer_candidate(path: &Path) -> MigrationResult<Option<LfsPointer>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(MigrationError::Io {
                context: format!("failed to inspect checkout path {}", path.display()),
                source,
            });
        }
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_CURRENT_CHECKOUT_POINTER_BYTES {
        return Ok(None);
    }

    let contents = fs::read(path).map_err(|source| MigrationError::Io {
        context: format!("failed to read checkout path {}", path.display()),
        source,
    })?;
    let Ok(contents) = std::str::from_utf8(&contents) else {
        return Ok(None);
    };

    Ok(LfsPointer::parse(contents).ok())
}

fn parse_lfs_patterns_from_attributes(
    contents: &str,
    source: PathBuf,
) -> Vec<GitLfsTrackedPattern> {
    let mut attribute_macros = BTreeMap::new();
    let mut patterns = Vec::new();

    for line in contents.lines() {
        if let Some(pattern) = parse_lfs_pattern_line(line, &source, &mut attribute_macros) {
            patterns.push(pattern);
        }
    }

    patterns
}

fn parse_lfs_pattern_line(
    line: &str,
    source: &Path,
    attribute_macros: &mut BTreeMap<String, Vec<String>>,
) -> Option<GitLfsTrackedPattern> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }

    let tokens = split_gitattributes_line(trimmed);
    let (pattern, attributes) = tokens.split_first()?;
    if let Some(macro_name) = pattern.strip_prefix("[attr]") {
        if !macro_name.is_empty() {
            attribute_macros.insert(macro_name.to_owned(), attributes.to_vec());
        }
        return None;
    }

    let attributes = expand_attribute_macros(attributes, attribute_macros);
    if !attributes.iter().any(|attribute| attribute == "filter=lfs") {
        return None;
    }

    Some(GitLfsTrackedPattern {
        pattern: pattern.clone(),
        attributes,
        source: source.to_path_buf(),
    })
}

fn expand_attribute_macros(
    attributes: &[String],
    attribute_macros: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let mut expanded = Vec::new();

    for attribute in attributes {
        expand_attribute_macro(
            attribute,
            attribute_macros,
            &mut BTreeSet::new(),
            &mut expanded,
        );
    }

    expanded
}

fn expand_attribute_macro(
    attribute: &str,
    attribute_macros: &BTreeMap<String, Vec<String>>,
    expanding: &mut BTreeSet<String>,
    expanded: &mut Vec<String>,
) {
    expanded.push(attribute.to_owned());

    let Some(macro_attributes) = attribute_macros.get(attribute) else {
        return;
    };
    if !expanding.insert(attribute.to_owned()) {
        return;
    }

    for macro_attribute in macro_attributes {
        expand_attribute_macro(macro_attribute, attribute_macros, expanding, expanded);
    }

    expanding.remove(attribute);
}

fn split_gitattributes_line(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut in_quotes = false;
    let mut escaped = false;

    for ch in line.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
            continue;
        }

        match ch {
            '\\' => escaped = true,
            '"' => in_quotes = !in_quotes,
            ch if ch.is_whitespace() && !in_quotes => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
            }
            _ => token.push(ch),
        }
    }

    if escaped {
        token.push('\\');
    }
    if !token.is_empty() {
        tokens.push(token);
    }

    tokens
}

fn git_config_get<const N: usize>(
    worktree_root: &Path,
    args: [&str; N],
) -> MigrationResult<Option<String>> {
    let command_name = args.join(" ");
    let output = run_git(worktree_root, args)?;
    optional_stdout(output, &format!("git {command_name}"))
}

fn git_config_get_os<const N: usize>(
    worktree_root: &Path,
    args: [&OsStr; N],
    command_name: &str,
) -> MigrationResult<Option<String>> {
    let output = run_git_os(worktree_root, args, command_name)?;
    optional_stdout(output, command_name)
}

fn run_git<const N: usize>(current_dir: &Path, args: [&str; N]) -> MigrationResult<Output> {
    Command::new("git")
        .args(args)
        .current_dir(current_dir)
        .output()
        .map_err(|source| MigrationError::Io {
            context: "failed to start git".to_owned(),
            source,
        })
}

fn run_git_os<const N: usize>(
    current_dir: &Path,
    args: [&OsStr; N],
    command_name: &str,
) -> MigrationResult<Output> {
    Command::new("git")
        .args(args)
        .current_dir(current_dir)
        .output()
        .map_err(|source| MigrationError::Io {
            context: format!("failed to start {command_name}"),
            source,
        })
}

fn required_success_stdout(output: Output, command_name: &str) -> MigrationResult<String> {
    if !output.status.success() {
        return Err(command_error(command_name, output.status, &output.stderr));
    }

    output_stdout(output, command_name)
}

fn optional_stdout(output: Output, command_name: &str) -> MigrationResult<Option<String>> {
    if !output.status.success() {
        if output.status.code() == Some(1) && output.stderr.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }

        return Err(command_error(command_name, output.status, &output.stderr));
    }

    output_stdout(output, command_name).map(|stdout| {
        let trimmed = stdout.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

fn output_stdout(output: Output, command_name: &str) -> MigrationResult<String> {
    if output.stdout.len() > MAX_MIGRATION_GIT_OUTPUT_BYTES {
        return Err(MigrationError::ExternalCommandOutput {
            command: command_name.to_owned(),
            message: SanitizedMessage::new("git returned too much output"),
        });
    }

    String::from_utf8(output.stdout).map_err(|_| MigrationError::ExternalCommandOutput {
        command: command_name.to_owned(),
        message: SanitizedMessage::new("git returned non-UTF-8 output"),
    })
}

fn command_error(command: &str, status: ExitStatus, stderr: &[u8]) -> MigrationError {
    MigrationError::ExternalCommand {
        command: command.to_owned(),
        status: command_status_text(status),
        stderr: SanitizedMessage::new(truncated_lossy_message(stderr)),
    }
}

fn git_lfs_probe_diagnostic(output: &Output) -> String {
    let stderr = truncated_lossy_message(&output.stderr);
    if stderr.trim().is_empty() {
        format!(
            "git lfs version exited with status {}",
            command_status_text(output.status)
        )
    } else {
        stderr.trim().to_owned()
    }
}

fn command_status_text(status: ExitStatus) -> String {
    status.code().map_or_else(
        || "terminated by signal".to_owned(),
        |code| code.to_string(),
    )
}

fn truncated_lossy_message(bytes: &[u8]) -> String {
    if bytes.len() <= MAX_MIGRATION_GIT_OUTPUT_BYTES {
        return String::from_utf8_lossy(bytes).into_owned();
    }

    let mut message =
        String::from_utf8_lossy(&bytes[..MAX_MIGRATION_GIT_OUTPUT_BYTES]).into_owned();
    message.push_str("\n[truncated]");
    message
}

fn first_non_empty_line(value: &str) -> Option<&str> {
    value.lines().find(|line| !line.trim().is_empty())
}

fn is_regular_file_without_following_symlinks(path: &Path) -> MigrationResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_file()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(MigrationError::Io {
            context: format!("failed to inspect {}", path.display()),
            source,
        }),
    }
}

fn repo_relative_path_from_git_output(path: &str) -> MigrationResult<PathBuf> {
    let path = PathBuf::from(path);
    let is_safe_relative_path = !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir));

    if is_safe_relative_path {
        Ok(path)
    } else {
        Err(MigrationError::ExternalCommandOutput {
            command: "git ls-files".to_owned(),
            message: SanitizedMessage::new("git returned a path outside the worktree"),
        })
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
    };

    use tempfile::TempDir;

    use crate::{LfsObject, LfsObjectSize, LfsOid, LfsPointer};

    use super::{
        GitLfsSourceEndpointSource, MAX_GIT_ATTRIBUTES_BYTES, MAX_MIGRATION_GIT_OUTPUT_BYTES,
        MigrationError, default_lfs_endpoint_for_remote_url, discover_git_lfs_migration,
        enumerate_current_checkout_lfs_pointers, parse_git_check_attr_filter_stdout,
        parse_lfs_patterns_from_attributes, repo_relative_path_from_git_output,
        split_gitattributes_line,
    };

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
    fn source_endpoint_uses_current_branch_remote_before_origin() {
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
            .expect("branch remote URL should provide a default LFS endpoint");

        assert_eq!(
            endpoint.url,
            "https://github.com/upstream/repo.git/info/lfs"
        );
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
    fn enumerates_current_checkout_lfs_pointer_files() {
        let repo = TempRepo::new();
        let pointer_object = test_lfs_object('a', 123);
        let ordinary_lfs_object = test_lfs_object('b', 456);
        let non_lfs_pointer_object = test_lfs_object('c', 789);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n*.txt text\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(pointer_object.clone()).to_pointer_file(),
        );
        repo.write_file("asset/hydrated.bin", "already hydrated bytes");
        repo.write_file(
            "docs/pointer-example.txt",
            &LfsPointer::new(non_lfs_pointer_object).to_pointer_file(),
        );
        repo.git([
            "add",
            ".gitattributes",
            "asset/model.bin",
            "asset/hydrated.bin",
            "docs/pointer-example.txt",
        ]);

        let scan = enumerate_current_checkout_lfs_pointers(repo.path())
            .expect("current checkout pointer scan should succeed");

        assert_eq!(scan.tracked_path_count, 2);
        assert_eq!(scan.pointers.len(), 1);
        assert_eq!(scan.pointers[0].relative_path, Path::new("asset/model.bin"));
        assert_eq!(
            scan.pointers[0]
                .path
                .canonicalize()
                .expect("discovered pointer path should canonicalize"),
            repo.path()
                .join("asset/model.bin")
                .canonicalize()
                .expect("expected pointer path should canonicalize")
        );
        assert_eq!(scan.pointers[0].object, pointer_object);
        assert_ne!(scan.pointers[0].object, ordinary_lfs_object);
    }

    #[test]
    fn current_checkout_pointer_scan_ignores_untracked_lfs_files() {
        let repo = TempRepo::new();
        let tracked_object = test_lfs_object('a', 123);
        let untracked_object = test_lfs_object('b', 456);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/tracked.bin",
            &LfsPointer::new(tracked_object.clone()).to_pointer_file(),
        );
        repo.write_file(
            "asset/untracked.bin",
            &LfsPointer::new(untracked_object).to_pointer_file(),
        );
        repo.git(["add", ".gitattributes", "asset/tracked.bin"]);

        let scan = enumerate_current_checkout_lfs_pointers(repo.path())
            .expect("current checkout pointer scan should succeed");

        assert_eq!(scan.tracked_path_count, 1);
        assert_eq!(scan.pointers.len(), 1);
        assert_eq!(
            scan.pointers[0].relative_path,
            Path::new("asset/tracked.bin")
        );
        assert_eq!(scan.pointers[0].object, tracked_object);
    }

    #[cfg(unix)]
    #[test]
    fn current_checkout_pointer_scan_accepts_non_utf8_lfs_paths() {
        let repo = TempRepo::new();
        let object = test_lfs_object('d', 321);
        let relative_path = PathBuf::from(OsString::from_vec(b"asset/nonutf-\xFF.bin".to_vec()));
        let worktree_file = repo.path().join(&relative_path);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        fs::create_dir_all(worktree_file.parent().expect("path should have parent"))
            .expect("non-UTF-8 path parent should be created");
        if fs::write(
            &worktree_file,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        )
        .is_err()
        {
            return;
        }
        repo.git(["add", "-A"]);

        let scan = enumerate_current_checkout_lfs_pointers(repo.path())
            .expect("current checkout pointer scan should accept non-UTF-8 paths");

        assert_eq!(scan.tracked_path_count, 1);
        assert_eq!(scan.pointers.len(), 1);
        assert_eq!(scan.pointers[0].relative_path, relative_path);
        assert_eq!(scan.pointers[0].object, object);
    }

    #[test]
    fn current_checkout_pointer_scan_accepts_large_attribute_output() {
        let mut stdout = Vec::new();
        for index in 0..8_000 {
            stdout.extend_from_slice(format!("docs/file-{index:05}.txt").as_bytes());
            stdout.extend_from_slice(b"\0filter\0unspecified\0");
        }
        stdout.extend_from_slice(b"asset/model.bin\0filter\0lfs\0");
        assert!(stdout.len() > MAX_MIGRATION_GIT_OUTPUT_BYTES);

        let paths = parse_git_check_attr_filter_stdout(&stdout)
            .expect("large check-attr output should not fail before parsing");

        assert_eq!(paths, vec![PathBuf::from("asset/model.bin")]);
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
    fn parses_lfs_patterns_from_gitattributes_lines() {
        let patterns = parse_lfs_patterns_from_attributes(
            "# ignored\n\"assets/big file.bin\" filter=lfs diff=lfs -text\n*.txt text\n*.zip -text filter=lfs\n",
            Path::new(".gitattributes").to_path_buf(),
        );

        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[0].pattern, "assets/big file.bin");
        assert_eq!(
            patterns[0].attributes,
            vec!["filter=lfs", "diff=lfs", "-text"]
        );
        assert_eq!(patterns[1].pattern, "*.zip");
    }

    #[test]
    fn parses_lfs_patterns_declared_with_attribute_macros() {
        let patterns = parse_lfs_patterns_from_attributes(
            "[attr]lfs filter=lfs diff=lfs merge=lfs -text\n*.bin lfs\n",
            Path::new(".gitattributes").to_path_buf(),
        );

        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].pattern, "*.bin");
        assert_eq!(
            patterns[0].attributes,
            vec!["lfs", "filter=lfs", "diff=lfs", "merge=lfs", "-text"]
        );
    }

    #[test]
    fn parses_lfs_patterns_declared_with_nested_attribute_macros() {
        let patterns = parse_lfs_patterns_from_attributes(
            "[attr]lfs filter=lfs diff=lfs merge=lfs -text\n[attr]lfs2 lfs\n*.bin lfs2\n",
            Path::new(".gitattributes").to_path_buf(),
        );

        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].pattern, "*.bin");
        assert_eq!(
            patterns[0].attributes,
            vec![
                "lfs2",
                "lfs",
                "filter=lfs",
                "diff=lfs",
                "merge=lfs",
                "-text"
            ]
        );
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

    #[test]
    fn splits_quoted_and_escaped_gitattributes_tokens() {
        assert_eq!(
            split_gitattributes_line(r#""assets/big file.bin" filter=lfs -text"#),
            vec!["assets/big file.bin", "filter=lfs", "-text"]
        );
        assert_eq!(
            split_gitattributes_line(r#"assets/big\ file.bin filter=lfs"#),
            vec!["assets/big file.bin", "filter=lfs"]
        );
    }

    struct TempRepo {
        root: TempDir,
    }

    impl TempRepo {
        fn new() -> Self {
            let root =
                tempfile::tempdir().expect("temporary repository directory should be created");
            let repo = Self { root };
            repo.git(["init", "--initial-branch", "main"]);
            repo.git(["config", "user.email", "lfs-cloud@example.invalid"]);
            repo.git(["config", "user.name", "LFS Cloud Test"]);
            repo
        }

        fn path(&self) -> PathBuf {
            self.root.path().to_path_buf()
        }

        fn write_file(&self, relative_path: impl AsRef<Path>, contents: &str) {
            self.write_bytes(relative_path, contents.as_bytes());
        }

        fn write_bytes(&self, relative_path: impl AsRef<Path>, contents: &[u8]) {
            let path = self.root.path().join(relative_path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("test file parent should be created");
            }
            fs::write(path, contents).expect("test file should be written");
        }

        fn git<const N: usize>(&self, args: [&str; N]) {
            let output = Command::new("git")
                .args(args)
                .current_dir(self.root.path())
                .output()
                .expect("git command should start");

            assert!(
                output.status.success(),
                "git command failed: {}\nstderr: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    fn test_lfs_object(hex_digit: char, size: u64) -> LfsObject {
        let oid = hex_digit.to_string().repeat(64);
        LfsObject::new(
            LfsOid::new(oid).expect("test OID should be valid"),
            LfsObjectSize::new(size),
        )
    }
}
