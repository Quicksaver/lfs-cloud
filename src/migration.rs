//! Migration discovery helpers for existing Git LFS repositories.
//!
//! Migration planning starts by inspecting the current repository without
//! writing to Git config, the worktree, the local cache, or any storage
//! provider. This module owns that read-only boundary so later migration steps
//! can build dry-run and transfer plans from one consistent snapshot.

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::{
    borrow::Borrow,
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    fs::File,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

use crate::{
    LfsObject, LfsObjectSize, LfsOid, LfsPointer, LocalCacheLayout, MigrationError,
    MigrationResult, SanitizedMessage,
};
use url::Url;

const DEFAULT_REMOTE_NAME: &str = "origin";
const MAX_MIGRATION_GIT_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_GIT_ATTRIBUTES_BYTES: u64 = 256 * 1024;
const MAX_CURRENT_CHECKOUT_POINTER_BYTES: u64 = 64 * 1024;
const MAX_CURRENT_CHECKOUT_ATTR_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_HISTORY_REF_LIST_BYTES: usize = 2 * 1024 * 1024;
const MAX_HISTORY_COMMIT_LIST_BYTES: usize = 32 * 1024 * 1024;
const MAX_HISTORY_TREE_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_HISTORY_CHECK_ATTR_INPUT_BYTES: usize = 1024 * 1024;
const MAX_HISTORY_POINTER_BYTES: u64 = 64 * 1024;
const MIGRATION_SOURCE_FETCH_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
const MIGRATION_SOURCE_FETCH_POLL_INTERVAL: Duration = Duration::from_millis(25);

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

/// Git LFS pointers discovered by scanning Git history for one or more refs.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct GitLfsHistoryPointers {
    /// Git worktree root whose object database was inspected.
    pub worktree_root: PathBuf,
    /// Refs that were resolved and scanned.
    pub refs: Vec<GitLfsScannedRef>,
    /// Pointer occurrences found in commits reachable from the scanned refs.
    pub pointers: Vec<GitLfsHistoryPointer>,
}

/// A Git ref that was resolved for migration history scanning.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct GitLfsScannedRef {
    /// Ref name requested or discovered for scanning.
    pub name: String,
    /// Commit object ID that the ref resolved to when scanning started.
    pub commit: String,
}

/// A Git LFS pointer found in a commit reachable from a scanned ref.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct GitLfsHistoryPointer {
    /// Ref whose reachable history contained this pointer.
    pub ref_name: String,
    /// Commit object ID containing this pointer at `relative_path`.
    pub commit: String,
    /// Repository-relative path to the pointer file in that commit.
    pub relative_path: PathBuf,
    /// Object identity referenced by the pointer file.
    pub object: LfsObject,
}

/// Local availability check for discovered migration objects.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct LocalMigrationObjectAvailability {
    /// Git worktree root whose local object stores were inspected.
    pub worktree_root: PathBuf,
    /// Repository-local Git LFS media object directory that was inspected.
    pub git_lfs_objects_dir: PathBuf,
    /// Shared LFS Cloud cache root that was inspected, when supplied.
    pub shared_cache_root: Option<PathBuf>,
    /// Deduplicated object availability records in stable object order.
    pub objects: Vec<LocalMigrationObject>,
}

impl LocalMigrationObjectAvailability {
    /// Returns objects with at least one verified local copy.
    #[must_use]
    pub fn available_objects(&self) -> Vec<&LocalMigrationObject> {
        self.objects
            .iter()
            .filter(|object| object.is_available())
            .collect()
    }

    /// Returns objects without any verified local copy.
    #[must_use]
    pub fn unavailable_objects(&self) -> Vec<&LocalMigrationObject> {
        self.objects
            .iter()
            .filter(|object| !object.is_available())
            .collect()
    }
}

/// Ref scope used when fetching source Git LFS objects for migration.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MigrationFetchMode {
    /// Fetch objects required by the currently checked-out ref.
    CurrentCheckout,
    /// Fetch objects required by the supplied branch, tag, or ref names.
    SelectedRefs {
        /// Refs to pass to `git lfs fetch` after validation.
        refs: Vec<String>,
    },
    /// Fetch all objects reachable from fetched local refs.
    AllFetchedRefs,
}

impl MigrationFetchMode {
    /// Builds a selected-ref fetch mode from caller-supplied ref names.
    ///
    /// Validation happens when the fetch command is prepared so callers can
    /// keep raw CLI arguments in one place until migration execution begins.
    #[must_use]
    pub fn selected_refs<I, S>(refs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::SelectedRefs {
            refs: refs.into_iter().map(Into::into).collect(),
        }
    }

    /// Returns the selected ref names when this mode targets explicit refs.
    #[must_use]
    pub fn selected_ref_names(&self) -> Option<&[String]> {
        match self {
            Self::SelectedRefs { refs } => Some(refs),
            Self::CurrentCheckout | Self::AllFetchedRefs => None,
        }
    }
}

/// Result of fetching missing source Git LFS objects into local media storage.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct MigrationSourceFetch {
    /// Git worktree root where the source fetch ran.
    pub worktree_root: PathBuf,
    /// Ref scope used for the source fetch.
    pub mode: MigrationFetchMode,
    /// Safe display form of the `git lfs fetch` command that ran.
    ///
    /// This is a human-readable diagnostic string, not a shell script. Arguments
    /// that need quoting are single-quoted, and non-UTF-8 argument bytes are
    /// rendered lossily because Git accepts platform-native paths and ref names.
    ///
    /// `None` means every requested object was already locally available, so no
    /// source-provider fetch was needed.
    pub command: Option<String>,
    /// Availability snapshot before fetching from the source provider.
    pub before: LocalMigrationObjectAvailability,
    /// Availability snapshot after fetching from the source provider.
    pub after: LocalMigrationObjectAvailability,
    /// Objects that were unavailable before the fetch and available afterward.
    pub fetched_objects: Vec<LfsObject>,
    /// Objects that still have no verified local bytes after the fetch.
    pub unavailable_objects: Vec<LfsObject>,
}

/// Local availability details for one discovered Git LFS object.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct LocalMigrationObject {
    /// Git LFS object identity being checked.
    pub object: LfsObject,
    /// Locations checked for a verified copy of this object.
    pub locations: Vec<LocalMigrationObjectLocation>,
}

impl LocalMigrationObject {
    /// Returns true when any checked location contains verified object bytes.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.locations.iter().any(|location| {
            matches!(
                location.status,
                LocalMigrationObjectLocationStatus::Available
            )
        })
    }
}

/// One local storage location checked for a migration object.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct LocalMigrationObjectLocation {
    /// Kind of local object store inspected.
    pub kind: LocalMigrationObjectLocationKind,
    /// Filesystem path expected to contain the object bytes.
    pub path: PathBuf,
    /// Availability status for this location.
    pub status: LocalMigrationObjectLocationStatus,
}

/// Kind of local object store inspected during migration planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LocalMigrationObjectLocationKind {
    /// Repository-local Git LFS media storage, normally `.git/lfs/objects`.
    GitLfsMedia,
    /// Shared LFS Cloud content-addressed cache.
    SharedCache,
}

/// Availability state for a checked local migration object location.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LocalMigrationObjectLocationStatus {
    /// The location contains bytes whose SHA-256 and size match the pointer.
    Available,
    /// The expected local object path does not exist.
    Missing,
    /// The path exists but cannot be used as the requested object.
    Invalid {
        /// Safe diagnostic explaining why the local bytes are unusable.
        message: SanitizedMessage,
    },
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

/// Enumerates Git LFS pointer files reachable from selected refs.
///
/// This function is intentionally read-only. It resolves each ref to a commit,
/// walks reachable commits, asks Git to evaluate `filter=lfs` attributes at
/// each historical tree, and parses only small LFS pointer blobs at matching
/// paths. It does not fetch objects, check out refs, or mutate repository
/// state.
///
/// # Errors
///
/// Returns [`MigrationError`] when `start_dir` is not inside a Git worktree,
/// any selected ref cannot be resolved to a commit, or Git returns malformed
/// history, attribute, or object data.
pub fn enumerate_selected_ref_lfs_pointers<I, S>(
    start_dir: impl AsRef<Path>,
    refs: I,
) -> MigrationResult<GitLfsHistoryPointers>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let start_dir = start_dir.as_ref();
    let worktree_root = detect_worktree_root(start_dir)?;
    let mut scanned_refs = Vec::new();
    let mut pointers = Vec::new();
    let mut seen = BTreeSet::new();
    let mut commit_cache = BTreeMap::new();
    let mut blob_pointer_cache = BTreeMap::new();

    for ref_name in refs {
        let ref_name = ref_name.as_ref();
        validate_history_ref_name(ref_name)?;
        let commit = resolve_ref_commit(&worktree_root, ref_name)?;
        scanned_refs.push(GitLfsScannedRef {
            name: ref_name.to_owned(),
            commit: commit.clone(),
        });
        enumerate_ref_history_lfs_pointers(
            &worktree_root,
            ref_name,
            &commit,
            &mut pointers,
            &mut seen,
            &mut commit_cache,
            &mut blob_pointer_cache,
        )?;
    }

    Ok(GitLfsHistoryPointers {
        worktree_root,
        refs: scanned_refs,
        pointers,
    })
}

/// Enumerates Git LFS pointer files reachable from all fetched repository refs.
///
/// The scan includes local branches, remote-tracking branches, and tags under
/// `refs/heads`, `refs/remotes`, and `refs/tags`. Symbolic refs are skipped so
/// aliases such as `refs/remotes/origin/HEAD` do not duplicate another ref's
/// history.
///
/// # Errors
///
/// Returns [`MigrationError`] when `start_dir` is not inside a Git worktree, Git
/// cannot list refs, or any discovered ref cannot be scanned.
pub fn enumerate_all_fetched_ref_lfs_pointers(
    start_dir: impl AsRef<Path>,
) -> MigrationResult<GitLfsHistoryPointers> {
    let start_dir = start_dir.as_ref();
    let worktree_root = detect_worktree_root(start_dir)?;
    let refs = all_fetched_ref_names(&worktree_root)?;
    let mut scanned_refs = Vec::new();
    let mut pointers = Vec::new();
    let mut seen = BTreeSet::new();
    let mut commit_cache = BTreeMap::new();
    let mut blob_pointer_cache = BTreeMap::new();

    for ref_name in refs {
        let commit = resolve_ref_commit(&worktree_root, &ref_name)?;
        scanned_refs.push(GitLfsScannedRef {
            name: ref_name.clone(),
            commit: commit.clone(),
        });
        enumerate_ref_history_lfs_pointers(
            &worktree_root,
            &ref_name,
            &commit,
            &mut pointers,
            &mut seen,
            &mut commit_cache,
            &mut blob_pointer_cache,
        )?;
    }

    Ok(GitLfsHistoryPointers {
        worktree_root,
        refs: scanned_refs,
        pointers,
    })
}

/// Checks whether discovered migration objects already have verified local bytes.
///
/// The repository's stock Git LFS media directory is always checked. When a
/// shared LFS Cloud cache layout is supplied, that cache is checked too. The
/// helper is intentionally read-only: missing or corrupt objects are reported
/// in the returned availability records instead of fetching or rewriting local
/// state.
///
/// # Errors
///
/// Returns [`MigrationError`] when `start_dir` is not inside a Git worktree or
/// Git cannot resolve the repository's local object storage configuration.
pub fn check_local_migration_objects<I, O>(
    start_dir: impl AsRef<Path>,
    objects: I,
    shared_cache: Option<&LocalCacheLayout>,
) -> MigrationResult<LocalMigrationObjectAvailability>
where
    I: IntoIterator<Item = O>,
    O: Borrow<LfsObject>,
{
    let start_dir = start_dir.as_ref();
    let worktree_root = detect_worktree_root(start_dir)?;
    let git_lfs_objects_dir = migration_git_lfs_objects_dir(&worktree_root)?;
    let shared_cache_root = shared_cache.map(|layout| layout.root().to_path_buf());
    let mut seen_objects = BTreeSet::new();
    let objects = objects
        .into_iter()
        .filter_map(|object| {
            let object = object.borrow().clone();
            seen_objects.insert(object.clone()).then_some(object)
        })
        .collect::<Vec<_>>();

    let objects = objects
        .into_iter()
        .map(|object| {
            let mut locations = vec![check_local_migration_object_location(
                LocalMigrationObjectLocationKind::GitLfsMedia,
                git_lfs_object_path(&git_lfs_objects_dir, &object.oid)?,
                &object,
            )?];

            if let Some(layout) = shared_cache {
                locations.push(check_local_migration_object_location(
                    LocalMigrationObjectLocationKind::SharedCache,
                    layout.object_path(&object),
                    &object,
                )?);
            }

            Ok(LocalMigrationObject { object, locations })
        })
        .collect::<MigrationResult<Vec<_>>>()?;

    Ok(LocalMigrationObjectAvailability {
        worktree_root,
        git_lfs_objects_dir,
        shared_cache_root,
        objects,
    })
}

/// Fetches missing source Git LFS objects without updating worktree files.
///
/// The helper first checks repository-local Git LFS media storage and the
/// optional shared LFS Cloud cache. If any requested object lacks verified local
/// bytes, it runs `git lfs fetch` for the requested ref scope and then checks
/// availability again. Git LFS fetch populates local media storage and does not
/// smudge or checkout files, so callers can use this before upload planning
/// without mutating worktree contents.
///
/// # Errors
///
/// Returns [`MigrationError`] when the start directory is not a Git worktree,
/// selected refs use invalid revision syntax, `git lfs fetch` cannot be
/// started, or the source provider fetch exits unsuccessfully.
pub fn fetch_missing_migration_objects<I, O>(
    start_dir: impl AsRef<Path>,
    objects: I,
    shared_cache: Option<&LocalCacheLayout>,
    mode: MigrationFetchMode,
) -> MigrationResult<MigrationSourceFetch>
where
    I: IntoIterator<Item = O>,
    O: Borrow<LfsObject>,
{
    fetch_missing_migration_objects_with_runner(
        start_dir,
        objects,
        shared_cache,
        mode,
        run_git_lfs_fetch_command,
    )
}

fn fetch_missing_migration_objects_with_runner<I, O, F>(
    start_dir: impl AsRef<Path>,
    objects: I,
    shared_cache: Option<&LocalCacheLayout>,
    mode: MigrationFetchMode,
    mut runner: F,
) -> MigrationResult<MigrationSourceFetch>
where
    I: IntoIterator<Item = O>,
    O: Borrow<LfsObject>,
    F: FnMut(&Path, &MigrationSourceFetchCommand) -> MigrationResult<()>,
{
    let start_dir = start_dir.as_ref();
    let before = check_local_migration_objects(start_dir, objects, shared_cache)?;
    let worktree_root = before.worktree_root.clone();
    let mut command = None;

    if before.unavailable_objects().is_empty() {
        let after = before.clone();
        return Ok(MigrationSourceFetch {
            worktree_root,
            mode,
            command,
            fetched_objects: Vec::new(),
            unavailable_objects: Vec::new(),
            before,
            after,
        });
    }

    let fetch_command = migration_source_fetch_command(&worktree_root, &mode)?;
    runner(&worktree_root, &fetch_command)?;
    command = Some(fetch_command.display.clone());

    let after = check_local_migration_objects(
        &worktree_root,
        before.objects.iter().map(|object| &object.object),
        shared_cache,
    )?;
    let fetched_objects = fetched_migration_objects(&before, &after);
    let unavailable_objects = unavailable_migration_objects(&after);

    Ok(MigrationSourceFetch {
        worktree_root,
        mode,
        command,
        before,
        after,
        fetched_objects,
        unavailable_objects,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MigrationSourceFetchCommand {
    args: Vec<OsString>,
    display: String,
}

fn migration_source_fetch_command(
    worktree_root: &Path,
    mode: &MigrationFetchMode,
) -> MigrationResult<MigrationSourceFetchCommand> {
    let mut args = vec![OsString::from("lfs"), OsString::from("fetch")];

    match mode {
        MigrationFetchMode::CurrentCheckout => {
            args.push(OsString::from("--include="));
            args.push(OsString::from("--exclude="));
        }
        MigrationFetchMode::SelectedRefs { refs } => {
            if refs.is_empty() {
                return Err(MigrationError::InvalidInput {
                    message: SanitizedMessage::new(
                        "selected-ref migration fetch requires at least one ref",
                    ),
                });
            }

            for ref_name in refs {
                validate_history_ref_name(ref_name)?;
            }

            args.push(OsString::from("--include="));
            args.push(OsString::from("--exclude="));
            args.push(OsString::from(source_remote_name(worktree_root)?));
            args.extend(
                refs.iter()
                    .map(|ref_name| OsString::from(ref_name.as_str())),
            );
        }
        MigrationFetchMode::AllFetchedRefs => {
            args.push(OsString::from("--all"));
        }
    }

    let display = display_git_command(&args);
    Ok(MigrationSourceFetchCommand { args, display })
}

fn run_git_lfs_fetch_command(
    worktree_root: &Path,
    command: &MigrationSourceFetchCommand,
) -> MigrationResult<()> {
    let mut child = Command::new("git")
        .args(&command.args)
        .current_dir(worktree_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| MigrationError::Io {
            context: "failed to start git lfs fetch".to_owned(),
            source,
        })?;
    let (status, stderr) = wait_for_git_lfs_fetch_command(
        &mut child,
        &command.display,
        MIGRATION_SOURCE_FETCH_TIMEOUT,
    )?;

    if status.success() {
        Ok(())
    } else {
        Err(command_error(&command.display, status, &stderr))
    }
}

fn wait_for_git_lfs_fetch_command(
    child: &mut Child,
    command: &str,
    timeout: Duration,
) -> MigrationResult<(ExitStatus, Vec<u8>)> {
    let stderr = child.stderr.take().ok_or_else(|| MigrationError::Io {
        context: "git lfs fetch stderr was not piped".to_owned(),
        source: io::Error::other("git lfs fetch stderr was not piped"),
    })?;
    let stderr_reader =
        thread::spawn(move || read_pipe_with_limit(stderr, MAX_MIGRATION_GIT_OUTPUT_BYTES + 1));
    let deadline = Instant::now() + timeout;

    loop {
        if let Some(status) = child.try_wait().map_err(|source| MigrationError::Io {
            context: format!("failed to wait for {command}"),
            source,
        })? {
            let stderr = join_git_lfs_fetch_stderr_reader(stderr_reader)?;
            return Ok((status, stderr.bytes));
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let stderr = join_git_lfs_fetch_stderr_reader(stderr_reader)?;
            return Err(MigrationError::ExternalCommand {
                command: command.to_owned(),
                status: format!("timed out after {} seconds", timeout.as_secs()),
                stderr: SanitizedMessage::new(truncated_lossy_message(&stderr.bytes)),
            });
        }

        thread::sleep(MIGRATION_SOURCE_FETCH_POLL_INTERVAL);
    }
}

fn join_git_lfs_fetch_stderr_reader(
    reader: thread::JoinHandle<io::Result<PipeReadResult>>,
) -> MigrationResult<PipeReadResult> {
    reader
        .join()
        .map_err(|_| MigrationError::Io {
            context: "git lfs fetch stderr reader panicked".to_owned(),
            source: io::Error::other("git lfs fetch stderr reader panicked"),
        })?
        .map_err(|source| MigrationError::Io {
            context: "failed to read git lfs fetch stderr".to_owned(),
            source,
        })
}

fn fetched_migration_objects(
    before: &LocalMigrationObjectAvailability,
    after: &LocalMigrationObjectAvailability,
) -> Vec<LfsObject> {
    before
        .objects
        .iter()
        .zip(after.objects.iter())
        .filter(|(before, after)| {
            before.object == after.object && !before.is_available() && after.is_available()
        })
        .map(|(before, _)| before.object.clone())
        .collect()
}

fn unavailable_migration_objects(
    availability: &LocalMigrationObjectAvailability,
) -> Vec<LfsObject> {
    availability
        .objects
        .iter()
        .filter(|object| !object.is_available())
        .map(|object| object.object.clone())
        .collect()
}

fn display_git_command(args: &[OsString]) -> String {
    std::iter::once(OsStr::new("git"))
        .chain(args.iter().map(OsString::as_os_str))
        .map(display_git_command_arg)
        .collect::<Vec<_>>()
        .join(" ")
}

fn display_git_command_arg(arg: &OsStr) -> String {
    let arg = arg.to_string_lossy();
    if arg.is_empty() {
        return "''".to_owned();
    }

    if arg
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_./:=@,".contains(character))
    {
        return arg.into_owned();
    }

    format!("'{}'", arg.replace('\'', "'\\''"))
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

fn migration_git_lfs_objects_dir(worktree_root: &Path) -> MigrationResult<PathBuf> {
    let git_common_dir = git_absolute_path(
        worktree_root,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        "git rev-parse --path-format=absolute --git-common-dir",
    )?;
    let storage_dir = match configured_git_lfs_storage_dir(worktree_root)? {
        Some(storage_dir) if storage_dir.is_absolute() => storage_dir,
        Some(storage_dir) => git_common_dir.join(storage_dir),
        None => git_common_dir.join("lfs"),
    };

    Ok(storage_dir.join("objects"))
}

fn git_absolute_path<const N: usize>(
    worktree_root: &Path,
    args: [&str; N],
    command_name: &str,
) -> MigrationResult<PathBuf> {
    let output = run_git(worktree_root, args)?;
    let stdout = required_success_stdout(output, command_name)?;

    Ok(PathBuf::from(stdout.trim_end_matches(['\n', '\r'])))
}

fn configured_git_lfs_storage_dir(worktree_root: &Path) -> MigrationResult<Option<PathBuf>> {
    git_config_get(worktree_root, ["config", "--get", "lfs.storage"]).map(|storage| {
        storage.and_then(|storage| {
            let storage = storage.trim();
            (!storage.is_empty()).then(|| PathBuf::from(storage))
        })
    })
}

fn check_local_migration_object_location(
    kind: LocalMigrationObjectLocationKind,
    path: PathBuf,
    object: &LfsObject,
) -> MigrationResult<LocalMigrationObjectLocation> {
    let status = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            LocalMigrationObjectLocationStatus::Invalid {
                message: SanitizedMessage::new("local object path is a symbolic link"),
            }
        }
        Ok(metadata) if metadata.is_file() => {
            let metadata_size = LfsObjectSize::new(metadata.len());
            if metadata_size != object.size {
                LocalMigrationObjectLocationStatus::Invalid {
                    message: SanitizedMessage::new(format!(
                        "expected sha256:{} ({} bytes), got local object with {} bytes",
                        object.oid, object.size, metadata_size
                    )),
                }
            } else {
                match hash_migration_object_file(&path) {
                    Ok((actual_oid, actual_size)) => {
                        if actual_oid == object.oid && actual_size == object.size {
                            LocalMigrationObjectLocationStatus::Available
                        } else {
                            LocalMigrationObjectLocationStatus::Invalid {
                                message: SanitizedMessage::new(format!(
                                    "expected sha256:{} ({} bytes), got sha256:{} ({} bytes)",
                                    object.oid, object.size, actual_oid, actual_size
                                )),
                            }
                        }
                    }
                    Err(source) => LocalMigrationObjectLocationStatus::Invalid {
                        message: local_object_verification_failure_message(&source),
                    },
                }
            }
        }
        Ok(_) => LocalMigrationObjectLocationStatus::Invalid {
            message: SanitizedMessage::new("local object path is not a regular file"),
        },
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            LocalMigrationObjectLocationStatus::Missing
        }
        Err(source) => LocalMigrationObjectLocationStatus::Invalid {
            message: SanitizedMessage::new(format!(
                "failed to inspect local object path: {}",
                source.kind()
            )),
        },
    };

    Ok(LocalMigrationObjectLocation { kind, path, status })
}

fn local_object_verification_failure_message(error: &MigrationError) -> SanitizedMessage {
    match error {
        MigrationError::Io { source, .. } => SanitizedMessage::new(format!(
            "failed to verify local object bytes: {}",
            source.kind()
        )),
        _ => SanitizedMessage::new("failed to verify local object bytes"),
    }
}

fn hash_migration_object_file(path: &Path) -> MigrationResult<(LfsOid, LfsObjectSize)> {
    let mut file = File::open(path).map_err(|source| MigrationError::Io {
        context: format!("failed to open local migration object {}", path.display()),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut total_size = 0u64;
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| MigrationError::Io {
                context: format!("failed to read local migration object {}", path.display()),
                source,
            })?;
        if read == 0 {
            break;
        }

        hasher.update(&buffer[..read]);
        total_size = total_size
            .checked_add(read as u64)
            .ok_or_else(|| MigrationError::Io {
                context: format!(
                    "failed to measure local migration object {}",
                    path.display()
                ),
                source: io::Error::other("local migration object is too large to measure"),
            })?;
    }

    Ok((
        LfsOid::new(format!("{:x}", hasher.finalize())).expect("SHA-256 hex should be valid"),
        LfsObjectSize::new(total_size),
    ))
}

fn git_lfs_object_path(git_lfs_objects_dir: &Path, oid: &LfsOid) -> MigrationResult<PathBuf> {
    let hex = oid.as_hex();
    let first_shard = hex.get(..2).ok_or_else(|| MigrationError::InvalidInput {
        message: SanitizedMessage::new("validated SHA-256 object ID is too short"),
    })?;
    let second_shard = hex.get(2..4).ok_or_else(|| MigrationError::InvalidInput {
        message: SanitizedMessage::new("validated SHA-256 object ID is too short"),
    })?;

    Ok(git_lfs_objects_dir
        .join(first_shard)
        .join(second_shard)
        .join(hex))
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

    let output = git_check_attr_filter(worktree_root, output.stdout)?;
    let lfs_tracked_paths = parse_git_check_attr_filter_stdout(
        &output.stdout,
        &git_check_attr_filter_command_name(None),
    )?;
    current_checkout_existing_paths(worktree_root, lfs_tracked_paths)
}

fn parse_git_check_attr_filter_stdout(
    stdout: &[u8],
    command_name: &str,
) -> MigrationResult<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut fields = stdout.split(|byte| *byte == b'\0').peekable();
    while let Some(relative_path) = fields.next() {
        if relative_path.is_empty() {
            if fields.peek().is_none() {
                break;
            }

            return Err(git_check_attr_parse_error(command_name));
        }

        let Some(attribute) = fields.next() else {
            return Err(git_check_attr_parse_error(command_name));
        };
        let Some(value) = fields.next() else {
            return Err(git_check_attr_parse_error(command_name));
        };

        if attribute == b"filter" && value == b"lfs" {
            paths.push(safe_git_relative_path(relative_path, command_name)?);
        }
    }

    Ok(paths)
}

fn current_checkout_existing_paths(
    worktree_root: &Path,
    paths: Vec<PathBuf>,
) -> MigrationResult<Vec<PathBuf>> {
    let mut existing_paths = Vec::with_capacity(paths.len());
    for relative_path in paths {
        let path = worktree_root.join(&relative_path);
        match fs::symlink_metadata(&path) {
            Ok(_) => existing_paths.push(relative_path),
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(MigrationError::Io {
                    context: format!("failed to inspect checkout path {}", path.display()),
                    source,
                });
            }
        }
    }

    Ok(existing_paths)
}

fn git_check_attr_filter(worktree_root: &Path, tracked_paths: Vec<u8>) -> MigrationResult<Output> {
    git_check_attr_filter_with_source(worktree_root, tracked_paths, None)
}

fn git_check_attr_filter_with_source(
    worktree_root: &Path,
    mut tracked_paths: Vec<u8>,
    source: Option<&str>,
) -> MigrationResult<Output> {
    if !tracked_paths.ends_with(b"\0") {
        tracked_paths.push(b'\0');
    }

    let mut args = vec![
        OsString::from("check-attr"),
        OsString::from("-z"),
        OsString::from("--stdin"),
    ];
    let command_name = git_check_attr_filter_command_name(source);
    if let Some(source) = source {
        args.push(OsString::from(format!("--source={source}")));
    }
    args.push(OsString::from("filter"));

    let mut child = Command::new("git")
        .args(&args)
        .current_dir(worktree_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| MigrationError::Io {
            context: format!("failed to start {command_name}"),
            source,
        })?;

    let mut stdin = child.stdin.take().ok_or_else(|| MigrationError::Io {
        context: "git check-attr stdin was not piped".to_owned(),
        source: io::Error::other("git check-attr stdin was not piped"),
    })?;
    let stdout = child.stdout.take().ok_or_else(|| MigrationError::Io {
        context: "git check-attr stdout was not piped".to_owned(),
        source: io::Error::other("git check-attr stdout was not piped"),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| MigrationError::Io {
        context: "git check-attr stderr was not piped".to_owned(),
        source: io::Error::other("git check-attr stderr was not piped"),
    })?;
    let stdin_writer = std::thread::spawn(move || {
        let write_result = stdin.write_all(&tracked_paths);
        drop(stdin);

        write_result
    });
    let stdout_reader = std::thread::spawn(move || {
        read_pipe_with_limit(stdout, MAX_CURRENT_CHECKOUT_ATTR_OUTPUT_BYTES)
    });
    let stderr_reader = std::thread::spawn(move || {
        read_pipe_with_limit(stderr, MAX_MIGRATION_GIT_OUTPUT_BYTES + 1)
    });

    let status = child.wait().map_err(|source| MigrationError::Io {
        context: format!("failed to wait for {command_name}"),
        source,
    })?;

    let write_result = stdin_writer.join().map_err(|_| MigrationError::Io {
        context: "git check-attr input writer panicked".to_owned(),
        source: io::Error::other("git check-attr input writer panicked"),
    })?;

    write_result.map_err(|source| MigrationError::Io {
        context: "failed to write git check-attr path input".to_owned(),
        source,
    })?;

    let stdout = stdout_reader
        .join()
        .map_err(|_| MigrationError::Io {
            context: "git check-attr stdout reader panicked".to_owned(),
            source: io::Error::other("git check-attr stdout reader panicked"),
        })?
        .map_err(|source| MigrationError::Io {
            context: "failed to read git check-attr stdout".to_owned(),
            source,
        })?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| MigrationError::Io {
            context: "git check-attr stderr reader panicked".to_owned(),
            source: io::Error::other("git check-attr stderr reader panicked"),
        })?
        .map_err(|source| MigrationError::Io {
            context: "failed to read git check-attr stderr".to_owned(),
            source,
        })?;

    if !status.success() {
        return Err(command_error(&command_name, status, &stderr.bytes));
    }

    if stdout.exceeded_limit {
        return Err(MigrationError::ExternalCommandOutput {
            command: command_name,
            message: SanitizedMessage::new("git returned too much attribute output"),
        });
    }

    Ok(Output {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

struct PipeReadResult {
    bytes: Vec<u8>,
    exceeded_limit: bool,
}

fn read_pipe_with_limit(mut reader: impl Read, limit: usize) -> io::Result<PipeReadResult> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    let mut exceeded_limit = false;

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }

        let remaining = limit.saturating_sub(bytes.len());
        if remaining >= read {
            bytes.extend_from_slice(&buffer[..read]);
        } else {
            bytes.extend_from_slice(&buffer[..remaining]);
            exceeded_limit = true;
        }
    }

    Ok(PipeReadResult {
        bytes,
        exceeded_limit,
    })
}

fn git_check_attr_filter_command_name(source: Option<&str>) -> String {
    source.map_or_else(
        || "git check-attr -z --stdin filter".to_owned(),
        |source| format!("git check-attr -z --stdin --source={source} filter"),
    )
}

fn git_check_attr_parse_error(command_name: &str) -> MigrationError {
    MigrationError::ExternalCommandOutput {
        command: command_name.to_owned(),
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

    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(MigrationError::Io {
                context: format!("failed to read checkout path {}", path.display()),
                source,
            });
        }
    };
    let Ok(contents) = std::str::from_utf8(&contents) else {
        return Ok(None);
    };

    Ok(LfsPointer::parse(contents).ok())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitTreeBlob {
    object_id: String,
    relative_path: PathBuf,
    relative_path_bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitLfsHistoryPointerOccurrence {
    commit: String,
    relative_path: PathBuf,
    object: LfsObject,
}

fn validate_history_ref_name(ref_name: &str) -> MigrationResult<()> {
    let has_invalid_byte = ref_name.bytes().any(|byte| {
        byte.is_ascii_control()
            || matches!(byte, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
    });
    let has_invalid_sequence =
        ref_name.contains("..") || ref_name.contains("@{") || ref_name.contains("//");
    let has_invalid_boundary = ref_name.starts_with('/')
        || ref_name.ends_with('/')
        || ref_name.ends_with('.')
        || ref_name.ends_with(".lock");

    if !ref_name.is_empty()
        && !has_invalid_byte
        && !has_invalid_sequence
        && !has_invalid_boundary
        && ref_name != "@"
        && ref_name != "HEAD"
    {
        return Ok(());
    }

    Err(MigrationError::InvalidInput {
        message: SanitizedMessage::new("selected ref name is empty or contains invalid ref syntax"),
    })
}

fn resolve_ref_commit(worktree_root: &Path, ref_name: &str) -> MigrationResult<String> {
    let revision = format!("{ref_name}^{{commit}}");
    let command_name = format!("git rev-parse --verify --end-of-options {revision}");
    let output = run_git_os_vec(
        worktree_root,
        vec![
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("--end-of-options"),
            OsString::from(revision),
        ],
        &command_name,
    )?;
    let stdout = required_success_stdout(output, &command_name)?;
    let commit = stdout.trim_end_matches(['\n', '\r']);

    if is_git_object_id(commit) {
        Ok(commit.to_owned())
    } else {
        Err(MigrationError::ExternalCommandOutput {
            command: command_name,
            message: SanitizedMessage::new("git returned an invalid commit object ID"),
        })
    }
}

fn all_fetched_ref_names(worktree_root: &Path) -> MigrationResult<Vec<String>> {
    let command_name =
        "git for-each-ref --format=%(refname)%00%(symref) refs/heads refs/remotes refs/tags";
    let output = run_git(
        worktree_root,
        [
            "for-each-ref",
            "--format=%(refname)%00%(symref)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
    )?;
    let stdout =
        required_success_stdout_with_limit(output, command_name, MAX_HISTORY_REF_LIST_BYTES)?;
    let mut refs = Vec::new();

    for line in stdout.lines().filter(|line| !line.is_empty()) {
        let Some((ref_name, symref)) = line.split_once('\0') else {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned malformed ref output"),
            });
        };

        if symref.is_empty() {
            refs.push(ref_name.to_owned());
        }
    }

    refs.sort();
    refs.dedup();
    Ok(refs)
}

fn enumerate_ref_history_lfs_pointers(
    worktree_root: &Path,
    ref_name: &str,
    root_commit: &str,
    pointers: &mut Vec<GitLfsHistoryPointer>,
    seen: &mut BTreeSet<(String, PathBuf, LfsObject)>,
    commit_cache: &mut BTreeMap<String, Vec<GitLfsHistoryPointerOccurrence>>,
    blob_pointer_cache: &mut BTreeMap<String, Option<LfsPointer>>,
) -> MigrationResult<()> {
    for commit in rev_list_commits(worktree_root, root_commit)? {
        if !commit_cache.contains_key(&commit) {
            let occurrences =
                scan_history_commit_lfs_pointers(worktree_root, &commit, blob_pointer_cache)?;
            commit_cache.insert(commit.clone(), occurrences);
        }

        for occurrence in commit_cache
            .get(&commit)
            .expect("history commit cache should contain scanned commit")
        {
            let key = (
                occurrence.commit.clone(),
                occurrence.relative_path.clone(),
                occurrence.object.clone(),
            );
            if seen.insert(key) {
                pointers.push(GitLfsHistoryPointer {
                    ref_name: ref_name.to_owned(),
                    commit: occurrence.commit.clone(),
                    relative_path: occurrence.relative_path.clone(),
                    object: occurrence.object.clone(),
                });
            }
        }
    }

    Ok(())
}

fn scan_history_commit_lfs_pointers(
    worktree_root: &Path,
    commit: &str,
    blob_pointer_cache: &mut BTreeMap<String, Option<LfsPointer>>,
) -> MigrationResult<Vec<GitLfsHistoryPointerOccurrence>> {
    let blobs = tree_blobs_at_commit(worktree_root, commit)?;
    if blobs.is_empty() {
        return Ok(Vec::new());
    }

    let lfs_paths = git_check_attr_lfs_paths_for_tree_blobs(worktree_root, &blobs, commit)?;
    let mut occurrences = Vec::new();
    for blob in blobs
        .into_iter()
        .filter(|blob| lfs_paths.contains(&blob.relative_path))
    {
        let Some(pointer) = read_history_pointer_blob_candidate_cached(
            worktree_root,
            &blob.object_id,
            blob_pointer_cache,
        )?
        else {
            continue;
        };

        occurrences.push(GitLfsHistoryPointerOccurrence {
            commit: commit.to_owned(),
            relative_path: blob.relative_path,
            object: pointer.object,
        });
    }

    Ok(occurrences)
}

fn rev_list_commits(worktree_root: &Path, root_commit: &str) -> MigrationResult<Vec<String>> {
    let command_name = format!("git rev-list --topo-order {root_commit}");
    let output = run_git_os_vec(
        worktree_root,
        vec![
            OsString::from("rev-list"),
            OsString::from("--topo-order"),
            OsString::from(root_commit),
        ],
        &command_name,
    )?;
    let stdout =
        required_success_stdout_with_limit(output, &command_name, MAX_HISTORY_COMMIT_LIST_BYTES)?;
    let mut commits = Vec::new();

    for line in stdout.lines().filter(|line| !line.is_empty()) {
        if !is_git_object_id(line) {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name,
                message: SanitizedMessage::new("git returned an invalid commit object ID"),
            });
        }
        commits.push(line.to_owned());
    }

    Ok(commits)
}

fn tree_blobs_at_commit(worktree_root: &Path, commit: &str) -> MigrationResult<Vec<GitTreeBlob>> {
    let command_name =
        format!("git ls-tree -r -z --format=%(objecttype)%x00%(objectname)%x00%(path) {commit}");
    let output = run_git_os_vec(
        worktree_root,
        vec![
            OsString::from("ls-tree"),
            OsString::from("-r"),
            OsString::from("-z"),
            OsString::from("--format=%(objecttype)%x00%(objectname)%x00%(path)"),
            OsString::from(commit),
        ],
        &command_name,
    )?;
    if !output.status.success() {
        return Err(command_error(&command_name, output.status, &output.stderr));
    }
    if output.stdout.len() > MAX_HISTORY_TREE_OUTPUT_BYTES {
        return Err(MigrationError::ExternalCommandOutput {
            command: command_name,
            message: SanitizedMessage::new("git returned too much tree output"),
        });
    }

    parse_ls_tree_blob_output(&output.stdout, &command_name)
}

fn parse_ls_tree_blob_output(
    stdout: &[u8],
    command_name: &str,
) -> MigrationResult<Vec<GitTreeBlob>> {
    let mut blobs = Vec::new();
    let mut fields = stdout.split(|byte| *byte == b'\0').peekable();

    while let Some(object_type) = fields.next() {
        if object_type.is_empty() {
            if fields.peek().is_none() {
                break;
            }

            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned malformed tree output"),
            });
        }

        let Some(object_id) = fields.next() else {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned malformed tree output"),
            });
        };
        let Some(relative_path) = fields.next() else {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned malformed tree output"),
            });
        };
        let object_type = std::str::from_utf8(object_type).map_err(|_| {
            MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned non-UTF-8 object type output"),
            }
        })?;
        if object_type != "blob" {
            continue;
        }

        let object_id =
            std::str::from_utf8(object_id).map_err(|_| MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned non-UTF-8 object ID output"),
            })?;
        if !is_git_object_id(object_id) {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned an invalid blob object ID"),
            });
        }

        blobs.push(GitTreeBlob {
            object_id: object_id.to_owned(),
            relative_path: safe_git_relative_path(relative_path, command_name)?,
            relative_path_bytes: relative_path.to_owned(),
        });
    }

    Ok(blobs)
}

fn git_check_attr_lfs_paths_for_tree_blobs(
    worktree_root: &Path,
    blobs: &[GitTreeBlob],
    commit: &str,
) -> MigrationResult<BTreeSet<PathBuf>> {
    let mut lfs_paths = BTreeSet::new();
    let mut path_input = Vec::new();

    for blob in blobs {
        let path_entry_len = blob.relative_path_bytes.len() + 1;
        if path_entry_len > MAX_HISTORY_CHECK_ATTR_INPUT_BYTES {
            return Err(MigrationError::ExternalCommandOutput {
                command: format!(
                    "git ls-tree -r -z --format=%(objecttype)%x00%(objectname)%x00%(path) {commit}"
                ),
                message: SanitizedMessage::new(
                    "git returned an oversized path for attribute lookup",
                ),
            });
        }

        if !path_input.is_empty()
            && path_input.len() + path_entry_len > MAX_HISTORY_CHECK_ATTR_INPUT_BYTES
        {
            append_git_check_attr_lfs_paths(worktree_root, commit, path_input, &mut lfs_paths)?;
            path_input = Vec::new();
        }

        path_input.extend_from_slice(&blob.relative_path_bytes);
        path_input.push(b'\0');
    }

    if !path_input.is_empty() {
        append_git_check_attr_lfs_paths(worktree_root, commit, path_input, &mut lfs_paths)?;
    }

    Ok(lfs_paths)
}

fn append_git_check_attr_lfs_paths(
    worktree_root: &Path,
    commit: &str,
    path_input: Vec<u8>,
    lfs_paths: &mut BTreeSet<PathBuf>,
) -> MigrationResult<()> {
    let attributes = git_check_attr_filter_with_source(worktree_root, path_input, Some(commit))?;
    let command_name = git_check_attr_filter_command_name(Some(commit));
    lfs_paths.extend(parse_git_check_attr_filter_stdout(
        &attributes.stdout,
        &command_name,
    )?);

    Ok(())
}

fn read_history_pointer_blob_candidate_cached(
    worktree_root: &Path,
    object_id: &str,
    blob_pointer_cache: &mut BTreeMap<String, Option<LfsPointer>>,
) -> MigrationResult<Option<LfsPointer>> {
    if let Some(pointer) = blob_pointer_cache.get(object_id) {
        return Ok(pointer.clone());
    }

    let pointer = read_history_pointer_blob_candidate(worktree_root, object_id)?;
    blob_pointer_cache.insert(object_id.to_owned(), pointer.clone());
    Ok(pointer)
}

fn read_history_pointer_blob_candidate(
    worktree_root: &Path,
    object_id: &str,
) -> MigrationResult<Option<LfsPointer>> {
    let size_command = format!("git cat-file -s {object_id}");
    let size_output = run_git_os_vec(
        worktree_root,
        vec![
            OsString::from("cat-file"),
            OsString::from("-s"),
            OsString::from(object_id),
        ],
        &size_command,
    )?;
    let size_stdout = required_success_stdout(size_output, &size_command)?;
    let size = size_stdout
        .trim_end_matches(['\n', '\r'])
        .parse::<u64>()
        .map_err(|_| MigrationError::ExternalCommandOutput {
            command: size_command.clone(),
            message: SanitizedMessage::new("git returned an invalid blob size"),
        })?;
    if size > MAX_HISTORY_POINTER_BYTES {
        return Ok(None);
    }

    let blob_command = format!("git cat-file blob {object_id}");
    let blob_output = run_git_os_vec(
        worktree_root,
        vec![
            OsString::from("cat-file"),
            OsString::from("blob"),
            OsString::from(object_id),
        ],
        &blob_command,
    )?;
    if !blob_output.status.success() {
        return Err(command_error(
            &blob_command,
            blob_output.status,
            &blob_output.stderr,
        ));
    }
    if blob_output.stdout.len() > MAX_HISTORY_POINTER_BYTES as usize {
        return Ok(None);
    }
    let Ok(contents) = std::str::from_utf8(&blob_output.stdout) else {
        return Ok(None);
    };

    Ok(LfsPointer::parse(contents).ok())
}

fn is_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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

fn run_git_os_vec(
    current_dir: &Path,
    args: Vec<OsString>,
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
    required_success_stdout_with_limit(output, command_name, MAX_MIGRATION_GIT_OUTPUT_BYTES)
}

fn required_success_stdout_with_limit(
    output: Output,
    command_name: &str,
    limit: usize,
) -> MigrationResult<String> {
    if !output.status.success() {
        return Err(command_error(command_name, output.status, &output.stderr));
    }

    output_stdout_with_limit(output, command_name, limit)
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
    output_stdout_with_limit(output, command_name, MAX_MIGRATION_GIT_OUTPUT_BYTES)
}

fn output_stdout_with_limit(
    output: Output,
    command_name: &str,
    limit: usize,
) -> MigrationResult<String> {
    if output.stdout.len() > limit {
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
    use std::os::unix::{ffi::OsStringExt, fs::PermissionsExt};
    use std::{
        collections::BTreeSet,
        ffi::OsString,
        fs, io,
        path::{Path, PathBuf},
        process::Command,
    };

    use tempfile::TempDir;

    use sha2::{Digest, Sha256};

    use crate::{LfsObject, LfsObjectSize, LfsOid, LfsPointer, LocalCacheLayout};

    use super::{
        GitLfsSourceEndpointSource, LocalMigrationObjectLocationKind,
        LocalMigrationObjectLocationStatus, MAX_GIT_ATTRIBUTES_BYTES,
        MAX_MIGRATION_GIT_OUTPUT_BYTES, MigrationError, MigrationFetchMode,
        check_local_migration_objects, default_lfs_endpoint_for_remote_url,
        discover_git_lfs_migration, display_git_command, enumerate_all_fetched_ref_lfs_pointers,
        enumerate_current_checkout_lfs_pointers, enumerate_selected_ref_lfs_pointers,
        fetch_missing_migration_objects, fetch_missing_migration_objects_with_runner,
        git_lfs_object_path, migration_source_fetch_command, parse_git_check_attr_filter_stdout,
        parse_lfs_patterns_from_attributes, parse_ls_tree_blob_output,
        repo_relative_path_from_git_output, split_gitattributes_line, validate_history_ref_name,
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

    #[test]
    fn current_checkout_pointer_scan_ignores_missing_tracked_lfs_files() {
        let repo = TempRepo::new();
        let present_object = test_lfs_object('a', 123);
        let missing_object = test_lfs_object('b', 456);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/present.bin",
            &LfsPointer::new(present_object.clone()).to_pointer_file(),
        );
        repo.write_file(
            "asset/missing.bin",
            &LfsPointer::new(missing_object).to_pointer_file(),
        );
        repo.git([
            "add",
            ".gitattributes",
            "asset/present.bin",
            "asset/missing.bin",
        ]);
        fs::remove_file(repo.path().join("asset/missing.bin"))
            .expect("tracked checkout file should be removable");

        let scan = enumerate_current_checkout_lfs_pointers(repo.path())
            .expect("current checkout pointer scan should succeed");

        assert_eq!(scan.tracked_path_count, 1);
        assert_eq!(scan.pointers.len(), 1);
        assert_eq!(
            scan.pointers[0].relative_path,
            Path::new("asset/present.bin")
        );
        assert_eq!(scan.pointers[0].object, present_object);
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

        let paths = parse_git_check_attr_filter_stdout(&stdout, "git check-attr test")
            .expect("large check-attr output should not fail before parsing");

        assert_eq!(paths, vec![PathBuf::from("asset/model.bin")]);
    }

    #[test]
    fn rejects_malformed_check_attr_output() {
        assert!(parse_git_check_attr_filter_stdout(b"asset/model.bin\0filter", "test").is_err());
        assert!(parse_git_check_attr_filter_stdout(b"\0filter\0lfs\0", "test").is_err());
    }

    #[test]
    fn malformed_check_attr_output_reports_supplied_command() {
        let error = parse_git_check_attr_filter_stdout(
            b"asset/model.bin\0filter",
            "git check-attr -z --stdin --source=abc123 filter",
        )
        .expect_err("malformed attribute output should fail");

        assert!(matches!(
            error,
            MigrationError::ExternalCommandOutput { command, .. }
                if command == "git check-attr -z --stdin --source=abc123 filter"
        ));
    }

    #[test]
    fn rejects_check_attr_paths_outside_worktree() {
        assert!(
            parse_git_check_attr_filter_stdout(b"/tmp/model.bin\0filter\0lfs\0", "test").is_err()
        );
        assert!(
            parse_git_check_attr_filter_stdout(b"../model.bin\0filter\0lfs\0", "test").is_err()
        );
        assert!(
            parse_git_check_attr_filter_stdout(b"asset/model.bin\0filter\0lfs\0", "test").is_ok()
        );
    }

    #[test]
    fn rejects_revision_syntax_as_history_ref_names() {
        for ref_name in ["", "main..feature", "HEAD^", "refs/heads/main\n"] {
            assert!(
                matches!(
                    validate_history_ref_name(ref_name),
                    Err(MigrationError::InvalidInput { .. })
                ),
                "{ref_name:?} should be rejected as unsafe revision syntax"
            );
        }

        validate_history_ref_name("refs/heads/feature/assets")
            .expect("normal full ref names should be accepted");
        validate_history_ref_name("feature/assets")
            .expect("normal branch names should be accepted");
    }

    #[test]
    fn selected_ref_pointer_scan_walks_history_and_respects_historical_attributes() {
        let repo = TempRepo::new();
        let old_object = test_lfs_object('a', 123);
        let new_object = test_lfs_object('b', 456);
        let non_lfs_object = test_lfs_object('c', 789);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n*.txt text\n");
        repo.write_file(
            "asset/old.bin",
            &LfsPointer::new(old_object.clone()).to_pointer_file(),
        );
        repo.write_file(
            "docs/pointer-example.txt",
            &LfsPointer::new(non_lfs_object).to_pointer_file(),
        );
        repo.commit_all("add historical pointer");
        repo.git(["rm", "asset/old.bin"]);
        repo.write_file(
            "asset/new.bin",
            &LfsPointer::new(new_object.clone()).to_pointer_file(),
        );
        repo.commit_all("replace pointer");

        let scan = enumerate_selected_ref_lfs_pointers(repo.path(), ["main"])
            .expect("selected ref scan should succeed");
        let objects = history_scan_objects(&scan.pointers);

        assert_eq!(scan.refs.len(), 1);
        assert_eq!(scan.refs[0].name, "main");
        assert!(objects.contains(&old_object));
        assert!(objects.contains(&new_object));
        assert!(!objects.contains(&test_lfs_object('c', 789)));
        assert!(scan.pointers.iter().any(|pointer| {
            pointer.relative_path == Path::new("asset/old.bin") && pointer.object == old_object
        }));
        assert!(
            scan.pointers
                .iter()
                .all(|pointer| pointer.ref_name == "main")
        );
    }

    #[test]
    fn selected_ref_pointer_scan_finds_branch_only_history() {
        let repo = TempRepo::new();
        let main_object = test_lfs_object('d', 111);
        let branch_object = test_lfs_object('e', 222);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/main.bin",
            &LfsPointer::new(main_object.clone()).to_pointer_file(),
        );
        repo.commit_all("add main pointer");
        repo.git(["checkout", "-b", "feature/assets"]);
        repo.write_file(
            "asset/branch.bin",
            &LfsPointer::new(branch_object.clone()).to_pointer_file(),
        );
        repo.commit_all("add branch pointer");
        repo.git(["checkout", "main"]);

        let scan = enumerate_selected_ref_lfs_pointers(repo.path(), ["feature/assets"])
            .expect("selected branch scan should succeed");
        let objects = history_scan_objects(&scan.pointers);

        assert!(objects.contains(&main_object));
        assert!(objects.contains(&branch_object));
        assert!(scan.pointers.iter().any(|pointer| {
            pointer.relative_path == Path::new("asset/branch.bin")
                && pointer.object == branch_object
        }));
    }

    #[test]
    fn selected_ref_pointer_scan_deduplicates_shared_history() {
        let repo = TempRepo::new();
        let object = test_lfs_object('8', 888);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/shared.bin",
            &LfsPointer::new(object.clone()).to_pointer_file(),
        );
        repo.commit_all("add shared pointer");
        repo.git(["tag", "v-shared"]);

        let scan = enumerate_selected_ref_lfs_pointers(repo.path(), ["main", "v-shared"])
            .expect("selected refs with shared history should scan once");

        assert_eq!(scan.refs.len(), 2);
        assert_eq!(scan.pointers.len(), 1);
        assert_eq!(
            scan.pointers[0].relative_path,
            Path::new("asset/shared.bin")
        );
        assert_eq!(scan.pointers[0].object, object);
    }

    #[test]
    fn all_fetched_ref_pointer_scan_includes_local_branches_and_tags() {
        let repo = TempRepo::new();
        let main_object = test_lfs_object('f', 333);
        let branch_object = test_lfs_object('1', 444);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/main.bin",
            &LfsPointer::new(main_object.clone()).to_pointer_file(),
        );
        repo.commit_all("add main pointer");
        repo.git(["tag", "v-main"]);
        repo.git(["checkout", "-b", "feature/assets"]);
        repo.write_file(
            "asset/branch.bin",
            &LfsPointer::new(branch_object.clone()).to_pointer_file(),
        );
        repo.commit_all("add branch pointer");
        repo.git(["checkout", "main"]);

        let scan = enumerate_all_fetched_ref_lfs_pointers(repo.path())
            .expect("all fetched refs scan should succeed");
        let ref_names = scan
            .refs
            .iter()
            .map(|scanned_ref| scanned_ref.name.as_str())
            .collect::<BTreeSet<_>>();
        let objects = history_scan_objects(&scan.pointers);

        assert!(ref_names.contains("refs/heads/main"));
        assert!(ref_names.contains("refs/heads/feature/assets"));
        assert!(ref_names.contains("refs/tags/v-main"));
        assert!(objects.contains(&main_object));
        assert!(objects.contains(&branch_object));
    }

    #[test]
    fn all_fetched_ref_pointer_scan_skips_symbolic_remote_head() {
        let repo = TempRepo::new();
        let object = test_lfs_object('7', 777);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object.clone()).to_pointer_file(),
        );
        repo.commit_all("add model pointer");
        repo.git(["update-ref", "refs/remotes/origin/main", "HEAD"]);
        repo.git([
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);

        let scan = enumerate_all_fetched_ref_lfs_pointers(repo.path())
            .expect("all fetched refs scan should skip symbolic remote HEAD");
        let ref_names = scan
            .refs
            .iter()
            .map(|scanned_ref| scanned_ref.name.as_str())
            .collect::<BTreeSet<_>>();

        assert!(ref_names.contains("refs/remotes/origin/main"));
        assert!(!ref_names.contains("refs/remotes/origin/HEAD"));
        assert_eq!(scan.pointers.len(), 1);
        assert_eq!(scan.pointers[0].object, object);
    }

    #[test]
    fn selected_ref_pointer_scan_skips_lfs_matching_gitlinks() {
        let repo = TempRepo::new();
        let object = test_lfs_object('9', 555);

        repo.write_file(
            ".gitattributes",
            "asset/* filter=lfs\nvendor/* filter=lfs\n",
        );
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object.clone()).to_pointer_file(),
        );
        repo.git(["add", ".gitattributes", "asset/model.bin"]);
        repo.git([
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            "1111111111111111111111111111111111111111",
            "vendor/tooling",
        ]);
        repo.git(["commit", "-m", "add lfs pointer and gitlink"]);

        let scan = enumerate_selected_ref_lfs_pointers(repo.path(), ["main"])
            .expect("gitlinks matching LFS attributes should be ignored");
        let objects = history_scan_objects(&scan.pointers);

        assert!(objects.contains(&object));
        assert!(
            scan.pointers
                .iter()
                .all(|pointer| pointer.relative_path != Path::new("vendor/tooling"))
        );
    }

    #[test]
    fn ls_tree_parser_skips_non_blob_entries() {
        let mut stdout = Vec::new();
        stdout
            .extend_from_slice(format!("commit\0{}\0vendor/tooling\0", "1".repeat(40)).as_bytes());
        stdout.extend_from_slice(format!("blob\0{}\0asset/model.bin\0", "2".repeat(40)).as_bytes());

        let blobs = parse_ls_tree_blob_output(&stdout, "git ls-tree test")
            .expect("non-blob entries should be skipped");

        assert_eq!(blobs.len(), 1);
        assert_eq!(
            blobs[0].object_id,
            "2222222222222222222222222222222222222222"
        );
        assert_eq!(blobs[0].relative_path, Path::new("asset/model.bin"));
    }

    #[test]
    fn local_object_check_verifies_git_lfs_media_and_deduplicates_objects() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"local object bytes");
        write_git_lfs_source_object(&repo, &object, b"local object bytes");

        let availability = check_local_migration_objects(repo.path(), [&object, &object], None)
            .expect("local object check should succeed");

        assert_eq!(availability.objects.len(), 1);
        assert_eq!(availability.available_objects().len(), 1);
        assert_eq!(availability.unavailable_objects().len(), 0);
        assert_eq!(availability.objects[0].object, object);
        assert!(availability.objects[0].is_available());
        assert_eq!(availability.objects[0].locations.len(), 1);
        assert_eq!(
            availability.objects[0].locations[0].kind,
            LocalMigrationObjectLocationKind::GitLfsMedia
        );
        assert_eq!(
            availability.objects[0].locations[0].status,
            LocalMigrationObjectLocationStatus::Available
        );
    }

    #[test]
    fn local_object_check_preserves_first_seen_object_order() {
        let repo = TempRepo::new();
        let later_sorting_object = test_lfs_object('f', 222);
        let earlier_sorting_object = test_lfs_object('a', 111);

        let availability = check_local_migration_objects(
            repo.path(),
            [
                &later_sorting_object,
                &earlier_sorting_object,
                &later_sorting_object,
            ],
            None,
        )
        .expect("local object check should succeed");

        let objects = availability
            .objects
            .iter()
            .map(|record| &record.object)
            .collect::<Vec<_>>();
        assert_eq!(
            objects,
            vec![&later_sorting_object, &earlier_sorting_object]
        );
    }

    #[test]
    fn local_object_check_reports_missing_and_corrupt_git_lfs_media_objects() {
        let repo = TempRepo::new();
        let missing = test_lfs_object_from_bytes(b"missing object bytes");
        let corrupt = test_lfs_object_from_bytes(b"expected object bytes");
        write_git_lfs_source_object(&repo, &corrupt, b"different object bytes");

        let availability = check_local_migration_objects(repo.path(), [&missing, &corrupt], None)
            .expect("local object check should succeed");

        let missing_record = availability
            .objects
            .iter()
            .find(|record| record.object == missing)
            .expect("missing object should be reported");
        assert!(!missing_record.is_available());
        assert_eq!(
            missing_record.locations[0].status,
            LocalMigrationObjectLocationStatus::Missing
        );

        let corrupt_record = availability
            .objects
            .iter()
            .find(|record| record.object == corrupt)
            .expect("corrupt object should be reported");
        assert!(!corrupt_record.is_available());
        assert!(matches!(
            &corrupt_record.locations[0].status,
            LocalMigrationObjectLocationStatus::Invalid { message }
                if message.as_str().contains("expected sha256:")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn local_object_check_rejects_symbolic_link_media_objects() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"linked object bytes");
        let target_path = repo.path().join("linked-object-target");
        let media_object_path =
            git_lfs_object_path(&repo.path().join(".git/lfs/objects"), &object.oid)
                .expect("test object path should be valid");
        write_file(&target_path, b"linked object bytes");
        fs::create_dir_all(
            media_object_path
                .parent()
                .expect("media object path should have a parent"),
        )
        .expect("media object parent should be created");
        std::os::unix::fs::symlink(&target_path, &media_object_path)
            .expect("media object symlink should be created");

        let availability = check_local_migration_objects(repo.path(), [&object], None)
            .expect("local object check should succeed");

        assert!(matches!(
            &availability.objects[0].locations[0].status,
            LocalMigrationObjectLocationStatus::Invalid { message }
                if message.as_str().contains("symbolic link")
        ));
    }

    #[test]
    fn local_object_check_uses_configured_git_lfs_storage_dir() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"configured storage bytes");
        repo.git(["config", "lfs.storage", "custom-lfs-storage"]);
        let storage_objects_dir = repo
            .path()
            .join(".git")
            .join("custom-lfs-storage")
            .join("objects");
        write_git_lfs_source_object_in(&storage_objects_dir, &object, b"configured storage bytes");

        let availability = check_local_migration_objects(repo.path(), [&object], None)
            .expect("local object check should use configured lfs.storage");

        assert_eq!(
            availability
                .git_lfs_objects_dir
                .canonicalize()
                .expect("reported storage path should canonicalize"),
            storage_objects_dir
                .canonicalize()
                .expect("expected storage path should canonicalize")
        );
        assert!(availability.objects[0].is_available());
    }

    #[test]
    fn local_object_check_treats_empty_configured_git_lfs_storage_dir_as_default() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"default storage bytes");
        repo.git(["config", "lfs.storage", ""]);
        let storage_objects_dir = repo.path().join(".git").join("lfs").join("objects");
        write_git_lfs_source_object_in(&storage_objects_dir, &object, b"default storage bytes");

        let availability = check_local_migration_objects(repo.path(), [&object], None)
            .expect("local object check should use default lfs.storage");

        assert_eq!(
            availability
                .git_lfs_objects_dir
                .canonicalize()
                .expect("reported storage path should canonicalize"),
            storage_objects_dir
                .canonicalize()
                .expect("expected storage path should canonicalize")
        );
        assert!(availability.objects[0].is_available());
    }

    #[test]
    fn local_object_check_uses_shared_cache_when_supplied() {
        let repo = TempRepo::new();
        let cache_root = tempfile::tempdir().expect("temporary cache root should be created");
        let layout = LocalCacheLayout::new(cache_root.path());
        let object = test_lfs_object_from_bytes(b"shared cache bytes");
        write_file(&layout.object_path(&object), b"shared cache bytes");

        let availability = check_local_migration_objects(repo.path(), [&object], Some(&layout))
            .expect("local object check should inspect shared cache");

        assert_eq!(
            availability.shared_cache_root.as_deref(),
            Some(cache_root.path())
        );
        assert!(availability.objects[0].is_available());
        assert_eq!(availability.objects[0].locations.len(), 2);
        assert_eq!(
            availability.objects[0].locations[0].status,
            LocalMigrationObjectLocationStatus::Missing
        );
        assert_eq!(
            availability.objects[0].locations[1].kind,
            LocalMigrationObjectLocationKind::SharedCache
        );
        assert_eq!(
            availability.objects[0].locations[1].status,
            LocalMigrationObjectLocationStatus::Available
        );
    }

    #[test]
    fn source_fetch_skips_git_lfs_when_all_objects_are_available() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"already available source bytes");
        write_git_lfs_source_object(&repo, &object, b"already available source bytes");
        let mut fetch_attempted = false;

        let report = fetch_missing_migration_objects_with_runner(
            repo.path(),
            [&object],
            None,
            MigrationFetchMode::CurrentCheckout,
            |_, _| {
                fetch_attempted = true;
                Ok(())
            },
        )
        .expect("available migration objects should not require fetch");

        assert!(!fetch_attempted);
        assert!(report.command.is_none());
        assert!(report.fetched_objects.is_empty());
        assert!(report.unavailable_objects.is_empty());
        assert_eq!(report.before.available_objects().len(), 1);
        assert_eq!(report.after.available_objects().len(), 1);
        assert_eq!(report.before, report.after);
    }

    #[test]
    fn source_fetch_downloads_missing_objects_into_git_lfs_media_storage() {
        let repo = TempRepo::new();
        repo.git([
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ]);
        let object = test_lfs_object_from_bytes(b"downloaded source bytes");
        let object_for_runner = object.clone();
        let mut observed_command = None;

        let report = fetch_missing_migration_objects_with_runner(
            repo.path(),
            [&object],
            None,
            MigrationFetchMode::selected_refs(["main"]),
            |worktree_root, command| {
                observed_command = Some(command.clone());
                write_git_lfs_source_object_in(
                    &worktree_root.join(".git/lfs/objects"),
                    &object_for_runner,
                    b"downloaded source bytes",
                );
                Ok(())
            },
        )
        .expect("source fetch should re-check downloaded objects");

        let command = observed_command.expect("missing object should run git lfs fetch");
        assert_eq!(
            command.args,
            vec![
                OsString::from("lfs"),
                OsString::from("fetch"),
                OsString::from("--include="),
                OsString::from("--exclude="),
                OsString::from("origin"),
                OsString::from("main"),
            ]
        );
        assert_eq!(
            report.command.as_deref(),
            Some("git lfs fetch --include= --exclude= origin main")
        );
        assert_eq!(report.fetched_objects, vec![object]);
        assert!(report.unavailable_objects.is_empty());
    }

    #[test]
    fn source_fetch_downloads_all_fetched_ref_objects_into_git_lfs_media_storage() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"downloaded all-ref source bytes");
        let object_for_runner = object.clone();
        let mut observed_command = None;

        let report = fetch_missing_migration_objects_with_runner(
            repo.path(),
            [&object],
            None,
            MigrationFetchMode::AllFetchedRefs,
            |worktree_root, command| {
                observed_command = Some(command.clone());
                write_git_lfs_source_object_in(
                    &worktree_root.join(".git/lfs/objects"),
                    &object_for_runner,
                    b"downloaded all-ref source bytes",
                );
                Ok(())
            },
        )
        .expect("all-ref source fetch should re-check downloaded objects");

        let command = observed_command.expect("missing object should run git lfs fetch");
        assert_eq!(
            command.args,
            vec![
                OsString::from("lfs"),
                OsString::from("fetch"),
                OsString::from("--all"),
            ]
        );
        assert_eq!(report.command.as_deref(), Some("git lfs fetch --all"));
        assert_eq!(report.fetched_objects, vec![object]);
        assert!(report.unavailable_objects.is_empty());
    }

    #[test]
    fn source_fetch_reports_objects_still_unavailable_after_fetch() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"still missing source bytes");

        let report = fetch_missing_migration_objects_with_runner(
            repo.path(),
            [&object],
            None,
            MigrationFetchMode::CurrentCheckout,
            |_, _| Ok(()),
        )
        .expect("source fetch report should include objects still missing afterward");

        assert_eq!(
            report.command.as_deref(),
            Some("git lfs fetch --include= --exclude=")
        );
        assert!(report.fetched_objects.is_empty());
        assert_eq!(report.unavailable_objects, vec![object]);
    }

    #[test]
    fn source_fetch_commands_match_migration_scope() {
        let repo = TempRepo::new();
        repo.git(["remote", "add", "origin", "git@github.com:owner/repo.git"]);

        let current =
            migration_source_fetch_command(&repo.path(), &MigrationFetchMode::CurrentCheckout)
                .expect("current checkout fetch command should be built");
        assert_eq!(current.display, "git lfs fetch --include= --exclude=");

        let selected = migration_source_fetch_command(
            &repo.path(),
            &MigrationFetchMode::selected_refs(["main", "refs/tags/v1"]),
        )
        .expect("selected-ref fetch command should be built");
        assert_eq!(
            selected.display,
            "git lfs fetch --include= --exclude= origin main refs/tags/v1"
        );

        let all_refs =
            migration_source_fetch_command(&repo.path(), &MigrationFetchMode::AllFetchedRefs)
                .expect("all-ref fetch command should be built");
        assert_eq!(all_refs.display, "git lfs fetch --all");
    }

    #[test]
    fn source_fetch_command_display_quotes_ambiguous_arguments() {
        let display = display_git_command(&[
            OsString::from("lfs"),
            OsString::from("fetch"),
            OsString::from("feature branch"),
            OsString::from("release'candidate"),
        ]);

        assert_eq!(
            display,
            "git lfs fetch 'feature branch' 'release'\\''candidate'"
        );
    }

    #[test]
    fn source_fetch_rejects_empty_or_unsafe_selected_refs() {
        let repo = TempRepo::new();
        let mode = MigrationFetchMode::selected_refs(["main", "refs/tags/v1"]);
        assert_eq!(
            mode.selected_ref_names(),
            Some(&["main".to_owned(), "refs/tags/v1".to_owned()][..])
        );

        assert!(matches!(
            migration_source_fetch_command(
                &repo.path(),
                &MigrationFetchMode::SelectedRefs { refs: Vec::new() }
            ),
            Err(MigrationError::InvalidInput { .. })
        ));
        assert!(matches!(
            migration_source_fetch_command(
                &repo.path(),
                &MigrationFetchMode::selected_refs(["main..feature"])
            ),
            Err(MigrationError::InvalidInput { .. })
        ));
    }

    #[ignore = "manual verification requires git-lfs and a local source repository"]
    #[test]
    fn source_fetch_downloads_missing_objects_without_changing_worktree_files() {
        if !git_lfs_is_available() {
            return;
        }

        let source = TempRepo::new();
        source.git(["lfs", "install", "--local"]);
        source.git(["lfs", "track", "*.bin"]);
        source.write_bytes("asset/model.bin", b"real source lfs bytes");
        source.commit_all("add source lfs object");

        let temp = tempfile::tempdir().expect("temporary clone parent should be created");
        let clone_path = temp.path().join("clone");
        let output = Command::new("git")
            .arg("clone")
            .arg(source.path())
            .arg(&clone_path)
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .output()
            .expect("git clone should start");
        assert!(
            output.status.success(),
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        match fs::remove_dir_all(clone_path.join(".git/lfs/objects")) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => panic!("clone LFS media storage should be removable: {source}"),
        }
        let pointer_path = clone_path.join("asset/model.bin");
        let pointer_before =
            fs::read(&pointer_path).expect("clone pointer file should be readable");
        let pointers = enumerate_current_checkout_lfs_pointers(&clone_path)
            .expect("clone pointer should be discoverable");
        assert_eq!(pointers.pointers.len(), 1);

        let report = fetch_missing_migration_objects(
            &clone_path,
            pointers.pointers.iter().map(|pointer| &pointer.object),
            None,
            MigrationFetchMode::CurrentCheckout,
        )
        .expect("real git lfs fetch should download missing object bytes");

        assert_eq!(
            fs::read(&pointer_path).expect("clone pointer file should remain readable"),
            pointer_before,
            "git lfs fetch must not smudge or replace checkout files"
        );
        assert_eq!(report.fetched_objects.len(), 1);
        assert!(report.unavailable_objects.is_empty());
        assert_git_status_clean(&clone_path);
    }

    #[cfg(unix)]
    #[test]
    fn local_object_check_keeps_checking_after_unreadable_media_object() {
        let repo = TempRepo::new();
        let cache_root = tempfile::tempdir().expect("temporary cache root should be created");
        let layout = LocalCacheLayout::new(cache_root.path());
        let object = test_lfs_object_from_bytes(b"shared cache fallback bytes");
        let media_object_path =
            git_lfs_object_path(&repo.path().join(".git/lfs/objects"), &object.oid)
                .expect("test object path should be valid");
        write_git_lfs_source_object(&repo, &object, b"shared cache fallback bytes");
        write_file(&layout.object_path(&object), b"shared cache fallback bytes");

        let original_permissions = fs::metadata(&media_object_path)
            .expect("media object metadata should be readable")
            .permissions();
        let mut unreadable_permissions = original_permissions.clone();
        unreadable_permissions.set_mode(0o000);
        fs::set_permissions(&media_object_path, unreadable_permissions)
            .expect("media object should be made unreadable");
        if fs::File::open(&media_object_path).is_ok() {
            fs::set_permissions(&media_object_path, original_permissions)
                .expect("media object permissions should be restored");
            return;
        }

        let availability_result =
            check_local_migration_objects(repo.path(), [&object], Some(&layout));
        fs::set_permissions(&media_object_path, original_permissions)
            .expect("media object permissions should be restored");
        let availability =
            availability_result.expect("unreadable media should not abort cache inspection");

        assert!(availability.objects[0].is_available());
        assert!(matches!(
            &availability.objects[0].locations[0].status,
            LocalMigrationObjectLocationStatus::Invalid { message }
                if message.as_str().contains("failed to verify local object bytes")
        ));
        assert_eq!(
            availability.objects[0].locations[1].status,
            LocalMigrationObjectLocationStatus::Available
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

        fn commit_all(&self, message: &str) {
            self.git(["add", "-A"]);
            self.git(["commit", "-m", message]);
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

    fn test_lfs_object_from_bytes(bytes: &[u8]) -> LfsObject {
        let oid = format!("{:x}", Sha256::digest(bytes));
        LfsObject::new(
            LfsOid::new(oid).expect("test OID should be valid"),
            LfsObjectSize::new(bytes.len() as u64),
        )
    }

    fn history_scan_objects(pointers: &[super::GitLfsHistoryPointer]) -> BTreeSet<LfsObject> {
        pointers
            .iter()
            .map(|pointer| pointer.object.clone())
            .collect()
    }

    fn write_git_lfs_source_object(repo: &TempRepo, object: &LfsObject, contents: &[u8]) {
        write_git_lfs_source_object_in(&repo.path().join(".git/lfs/objects"), object, contents);
    }

    fn write_git_lfs_source_object_in(
        git_lfs_objects_dir: &Path,
        object: &LfsObject,
        contents: &[u8],
    ) {
        write_file(
            &git_lfs_object_path(git_lfs_objects_dir, &object.oid)
                .expect("test object path should be valid"),
            contents,
        );
    }

    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("test file parent should be created");
        }
        fs::write(path, contents).expect("test file should be written");
    }

    fn git_lfs_is_available() -> bool {
        Command::new("git")
            .args(["lfs", "version"])
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn assert_git_status_clean(worktree_root: &Path) {
        let output = Command::new("git")
            .args(["status", "--short"])
            .current_dir(worktree_root)
            .output()
            .expect("git status should start");

        assert!(
            output.status.success(),
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.is_empty(),
            "worktree should stay clean after migration source fetch: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
