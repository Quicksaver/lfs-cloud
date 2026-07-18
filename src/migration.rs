//! Migration discovery helpers for existing Git LFS repositories.
//!
//! Migration planning starts by inspecting the current repository without
//! writing to Git config, the worktree, the local cache, or any storage
//! provider. This module owns that read-only boundary so later migration steps
//! can build dry-run and transfer plans from one consistent snapshot.

#[cfg(unix)]
use std::os::unix::{ffi::OsStringExt, fs::OpenOptionsExt};
use std::{
    borrow::Borrow,
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::File,
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Output, Stdio},
    str::FromStr,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use fs4::FileExt;
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    LFS_POINTER_SIZE_CUTOFF, LfsObject, LfsObjectSize, LfsOid, LfsPointer, LocalCacheLayout,
    MigrationError, MigrationResult, SanitizedMessage, StorageProvider, StoredObject,
};
use url::Url;

const DEFAULT_REMOTE_NAME: &str = "origin";
const MAX_MIGRATION_GIT_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_GIT_ATTRIBUTES_BYTES: u64 = 256 * 1024;
const MAX_CURRENT_CHECKOUT_ATTR_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_HISTORY_REF_LIST_BYTES: usize = 2 * 1024 * 1024;
const MAX_HISTORY_COMMIT_LIST_BYTES: usize = 32 * 1024 * 1024;
const MAX_HISTORY_TREE_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_HISTORY_CHECK_ATTR_INPUT_BYTES: usize = 1024 * 1024;
const GIT_NO_LAZY_FETCH_ENV: &str = "GIT_NO_LAZY_FETCH";
const MIGRATION_SOURCE_FETCH_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
const MIGRATION_SOURCE_FETCH_POLL_INTERVAL: Duration = Duration::from_millis(25);
const MIGRATION_GIT_OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MIGRATION_GIT_OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(500);
const MINIMUM_HISTORICAL_SCAN_GIT_VERSION: GitVersion = GitVersion::new(2, 40, 0);
const MINIMUM_HISTORICAL_SCAN_GIT_VERSION_TEXT: &str = "2.40.0";
/// Default number of migration objects uploaded concurrently.
pub const DEFAULT_MIGRATION_UPLOAD_CONCURRENCY: usize = 4;
const MIGRATION_UPLOAD_CHECKPOINT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct GitVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

impl GitVersion {
    const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

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

/// Git LFS pointers discovered from the current checkout's index.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct CurrentCheckoutLfsPointers {
    /// Git worktree root that was inspected.
    pub worktree_root: PathBuf,
    /// Number of tracked index paths whose Git attributes use `filter=lfs`.
    pub tracked_path_count: usize,
    /// Pointer blobs found among the current index's LFS-tracked paths.
    pub pointers: Vec<CurrentCheckoutLfsPointer>,
}

/// A Git LFS pointer blob found in the current checkout's index.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct CurrentCheckoutLfsPointer {
    /// Repository-relative path to the pointer file.
    pub relative_path: PathBuf,
    /// Corresponding absolute worktree path, which may be absent in a sparse checkout.
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
    /// Shared LFS Cloud cache root available as a fallback, when supplied.
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
    /// Explicit Git remote used for source-provider fetches.
    pub source_remote: String,
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

/// Result of uploading locally available migration objects into LFS Cloud storage.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct MigrationStorageUpload {
    /// Configured storage provider ID that received the upload checks.
    pub storage_provider_id: String,
    /// Objects skipped because the configured storage provider already has them.
    pub already_present_objects: Vec<LfsObject>,
    /// Objects uploaded during this run or restored from its durable checkpoint.
    pub uploaded_objects: Vec<StoredObject>,
    /// Objects that could not complete, with retry-safe diagnostics.
    pub failed_objects: Vec<MigrationObjectUploadFailure>,
    /// One terminal outcome per requested object, in discovery order.
    pub outcomes: Vec<MigrationObjectUploadOutcome>,
    /// Durable checkpoint used to resume completed objects.
    pub checkpoint_path: PathBuf,
}

/// Options controlling bounded migration uploads and durable progress.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationStorageUploadOptions {
    checkpoint_path: PathBuf,
    max_concurrent_uploads: usize,
}

impl MigrationStorageUploadOptions {
    /// Creates upload options using the supplied durable checkpoint path.
    ///
    /// The default concurrency is [`DEFAULT_MIGRATION_UPLOAD_CONCURRENCY`].
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::MigrationStorageUploadOptions;
    ///
    /// let options = MigrationStorageUploadOptions::new(".git/lfs/migration.jsonl")
    ///     .with_max_concurrent_uploads(2);
    /// assert_eq!(options.max_concurrent_uploads(), 2);
    /// ```
    #[must_use]
    pub fn new(checkpoint_path: impl Into<PathBuf>) -> Self {
        Self {
            checkpoint_path: checkpoint_path.into(),
            max_concurrent_uploads: DEFAULT_MIGRATION_UPLOAD_CONCURRENCY,
        }
    }

    /// Sets the maximum number of simultaneous object transfers.
    ///
    /// A zero value is rejected when upload execution starts.
    #[must_use]
    pub fn with_max_concurrent_uploads(mut self, max_concurrent_uploads: usize) -> Self {
        self.max_concurrent_uploads = max_concurrent_uploads;
        self
    }

    /// Returns the durable checkpoint path.
    #[must_use]
    pub fn checkpoint_path(&self) -> &Path {
        &self.checkpoint_path
    }

    /// Returns the maximum number of simultaneous object transfers.
    #[must_use]
    pub fn max_concurrent_uploads(&self) -> usize {
        self.max_concurrent_uploads
    }
}

/// Structured result for one requested migration object.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct MigrationObjectUploadOutcome {
    /// Object requested by the migration inventory.
    pub object: LfsObject,
    /// Terminal status observed during this run.
    pub status: MigrationObjectUploadStatus,
}

/// Terminal migration-upload status for one object.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MigrationObjectUploadStatus {
    /// Storage already contained the object before upload.
    AlreadyPresent {
        /// True when this completion was restored without contacting storage.
        resumed: bool,
    },
    /// Storage accepted and verified the object.
    Uploaded {
        /// Verified provider metadata returned for the stored object.
        stored_object: StoredObject,
        /// True when this completion was restored without contacting storage.
        resumed: bool,
    },
    /// This object failed while other independent objects continued.
    Failed {
        /// Secret-safe failure diagnostic suitable for a retry report.
        message: SanitizedMessage,
    },
}

/// One migration object that should be retried.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct MigrationObjectUploadFailure {
    /// Object whose transfer did not complete durably.
    pub object: LfsObject,
    /// Secret-safe reason the object should be retried.
    pub message: SanitizedMessage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MigrationUploadCheckpointCompletion {
    AlreadyPresent,
    Uploaded { backend_id: String },
}

#[derive(Debug, Deserialize, Serialize)]
struct MigrationUploadCheckpointRecord {
    version: u32,
    storage_provider_id: String,
    oid: String,
    size: u64,
    completion: MigrationUploadCheckpointRecordCompletion,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
enum MigrationUploadCheckpointRecordCompletion {
    AlreadyPresent,
    Uploaded { backend_id: String },
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
    let start_dir = start_dir.as_ref();
    let worktree_root = detect_worktree_root(start_dir)?;
    let source_remote = validate_source_remote_name(source_remote.as_ref())?;

    Ok(GitLfsMigrationDiscovery {
        installation: detect_git_lfs_installation(&worktree_root),
        filters: discover_lfs_filters(&worktree_root)?,
        tracked_patterns: discover_lfs_tracked_patterns(&worktree_root)?,
        source_endpoint: discover_source_endpoint(&worktree_root, &source_remote)?,
        worktree_root,
        source_remote,
    })
}

/// Enumerates Git LFS pointer blobs in the current checkout's index.
///
/// This function is intentionally read-only. It asks Git which index paths have
/// `filter=lfs`, then parses small pointer-shaped blobs directly from the index.
/// The index remains authoritative when a worktree file is hydrated or omitted
/// by sparse checkout, so both states retain complete current-checkout coverage.
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
    let lfs_tracked_blobs = current_checkout_lfs_tracked_blobs(&worktree_root)?;
    let mut pointers = Vec::new();

    for blob in &lfs_tracked_blobs {
        let Some(pointer) = read_index_pointer_blob_candidate(&worktree_root, &blob.object_id)?
        else {
            continue;
        };

        pointers.push(CurrentCheckoutLfsPointer {
            relative_path: blob.relative_path.clone(),
            path: worktree_root.join(&blob.relative_path),
            object: pointer.object,
        });
    }

    Ok(CurrentCheckoutLfsPointers {
        worktree_root,
        tracked_path_count: lfs_tracked_blobs.len(),
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
/// Git is older than 2.40, the repository is shallow, any selected ref cannot
/// be resolved to a commit, or Git returns malformed history, attribute, or
/// object data.
pub fn enumerate_selected_ref_lfs_pointers<I, S>(
    start_dir: impl AsRef<Path>,
    refs: I,
) -> MigrationResult<GitLfsHistoryPointers>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    enumerate_selected_ref_lfs_pointers_with_metrics(start_dir, refs).map(|(pointers, _)| pointers)
}

fn enumerate_selected_ref_lfs_pointers_with_metrics<I, S>(
    start_dir: impl AsRef<Path>,
    refs: I,
) -> MigrationResult<(GitLfsHistoryPointers, HistoryScanMetrics)>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let start_dir = start_dir.as_ref();
    let worktree_root = detect_worktree_root(start_dir)?;
    require_historical_scan_git_version(&worktree_root)?;
    require_complete_history(&worktree_root)?;
    let mut scanned_refs = Vec::new();

    for ref_name in refs {
        let ref_name = ref_name.as_ref();
        validate_history_ref_name(ref_name)?;
        let commit = resolve_ref_commit(&worktree_root, ref_name)?;
        scanned_refs.push(GitLfsScannedRef {
            name: ref_name.to_owned(),
            commit: commit.clone(),
        });
    }

    scan_resolved_history_refs(worktree_root, scanned_refs)
}

/// Enumerates Git LFS pointer files reachable from local branches, tags, and
/// fetched `origin` remote-tracking refs.
///
/// Symbolic refs are skipped so aliases such as `refs/remotes/origin/HEAD` do
/// not duplicate another ref's history. Use
/// [`enumerate_fetched_ref_lfs_pointers_for_remote`] to select another explicit
/// source remote.
///
/// # Errors
///
/// Returns [`MigrationError`] when `start_dir` is not inside a Git worktree, Git
/// cannot list refs, or any discovered ref cannot be scanned.
pub fn enumerate_all_fetched_ref_lfs_pointers(
    start_dir: impl AsRef<Path>,
) -> MigrationResult<GitLfsHistoryPointers> {
    enumerate_fetched_ref_lfs_pointers_for_remote(start_dir, DEFAULT_REMOTE_NAME)
}

/// Enumerates Git LFS pointers reachable from local branches, tags, and one
/// explicit source remote's fetched remote-tracking refs.
///
/// Scoping remote-tracking refs prevents an all-ref migration from silently
/// mixing histories fetched from unrelated repository remotes. Local branches
/// and tags remain included because they are repository-owned refs rather than
/// remote-tracking namespaces.
///
/// # Errors
///
/// Returns [`MigrationError`] when `start_dir` is not inside a Git worktree,
/// Git is older than 2.40, the repository is shallow, `source_remote` is
/// invalid, Git cannot list refs, or a discovered ref cannot be scanned.
pub fn enumerate_fetched_ref_lfs_pointers_for_remote(
    start_dir: impl AsRef<Path>,
    source_remote: impl AsRef<str>,
) -> MigrationResult<GitLfsHistoryPointers> {
    let start_dir = start_dir.as_ref();
    let worktree_root = detect_worktree_root(start_dir)?;
    require_historical_scan_git_version(&worktree_root)?;
    require_complete_history(&worktree_root)?;
    let source_remote = validate_source_remote_name(source_remote.as_ref())?;
    let refs = all_fetched_ref_names(&worktree_root, &source_remote)?;
    let mut scanned_refs = Vec::new();

    for ref_name in refs {
        let commit = resolve_ref_commit(&worktree_root, &ref_name)?;
        scanned_refs.push(GitLfsScannedRef {
            name: ref_name.clone(),
            commit: commit.clone(),
        });
    }

    scan_resolved_history_refs(worktree_root, scanned_refs).map(|(pointers, _)| pointers)
}

/// Checks whether discovered migration objects already have verified local bytes.
///
/// The repository's stock Git LFS media directory is always checked first.
/// When it does not contain a verified copy and a shared LFS Cloud cache layout
/// is supplied, that cache is checked as a fallback. The helper is
/// intentionally read-only: missing or corrupt objects are reported in the
/// returned availability records instead of fetching or rewriting local state.
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
            let git_lfs_location = check_local_migration_object_location(
                LocalMigrationObjectLocationKind::GitLfsMedia,
                git_lfs_object_path(&git_lfs_objects_dir, &object.oid)?,
                &object,
            )?;
            let git_lfs_media_is_available = matches!(
                &git_lfs_location.status,
                LocalMigrationObjectLocationStatus::Available
            );
            let mut locations = vec![git_lfs_location];

            if !git_lfs_media_is_available && let Some(layout) = shared_cache {
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
    fetch_missing_migration_objects_from_remote(
        start_dir,
        objects,
        shared_cache,
        DEFAULT_REMOTE_NAME,
        mode,
    )
}

/// Fetches missing source Git LFS objects from an explicit Git remote.
///
/// This is the remote-selecting variant of [`fetch_missing_migration_objects`].
/// The selected remote is included in every fetch scope so later execution
/// cannot silently follow the current branch's configured remote.
///
/// # Errors
///
/// Returns [`MigrationError`] when the source remote is invalid, the start
/// directory is not a Git worktree, selected refs use invalid revision syntax,
/// `git lfs fetch` cannot be started, or the source fetch exits unsuccessfully.
pub fn fetch_missing_migration_objects_from_remote<I, O>(
    start_dir: impl AsRef<Path>,
    objects: I,
    shared_cache: Option<&LocalCacheLayout>,
    source_remote: impl AsRef<str>,
    mode: MigrationFetchMode,
) -> MigrationResult<MigrationSourceFetch>
where
    I: IntoIterator<Item = O>,
    O: Borrow<LfsObject>,
{
    let source_remote = validate_source_remote_name(source_remote.as_ref())?;
    fetch_missing_migration_objects_with_runner(
        start_dir,
        objects,
        shared_cache,
        &source_remote,
        mode,
        run_git_lfs_fetch_command,
    )
}

/// Uploads locally available migration objects to configured LFS Cloud storage.
///
/// The helper is intentionally idempotent: it checks the destination storage
/// provider before uploading each object and reports already-present objects
/// separately. For objects that do need upload, it re-verifies the selected
/// local source bytes against the pointer OID and size immediately before
/// delegating to the storage provider.
///
/// Uploads run with [`DEFAULT_MIGRATION_UPLOAD_CONCURRENCY`] simultaneous
/// transfers. Each success is appended and synchronized to a provider-specific
/// checkpoint under the repository's Git LFS media directory before it is
/// reported. A later invocation resumes those completions without contacting
/// storage, while failed objects are retried. Outcomes retain discovery order
/// even though provider work completes out of order.
///
/// # Errors
///
/// Returns [`MigrationError`] when the checkpoint cannot be initialized or
/// parsed, or when upload options are invalid. Per-object source, provider, and
/// checkpoint-append failures are returned in
/// [`MigrationStorageUpload::failed_objects`] so independent work can finish.
pub async fn upload_migration_objects_to_storage(
    availability: &LocalMigrationObjectAvailability,
    storage: &dyn StorageProvider,
) -> MigrationResult<MigrationStorageUpload> {
    let checkpoint_path =
        default_migration_upload_checkpoint_path(availability, storage.provider_id());
    let options = MigrationStorageUploadOptions::new(checkpoint_path);
    upload_migration_objects_to_storage_with_options(availability, storage, &options).await
}

/// Uploads migration objects with an explicit checkpoint and concurrency bound.
///
/// This variant is useful when a migration coordinator owns the durable state
/// location or needs a provider-specific concurrency limit. Completed outcomes
/// are appended as JSON Lines records and synchronized individually, making the
/// checkpoint safe to reuse after interruption. A partial final line left by a
/// process crash is ignored; malformed complete records fail closed.
///
/// # Errors
///
/// Returns [`MigrationError`] when the concurrency limit is zero, the
/// checkpoint cannot be initialized or parsed, or its completed records do not
/// match the configured storage provider. Per-object failures are reported as
/// structured outcomes instead of aborting the remaining work.
pub async fn upload_migration_objects_to_storage_with_options(
    availability: &LocalMigrationObjectAvailability,
    storage: &dyn StorageProvider,
    options: &MigrationStorageUploadOptions,
) -> MigrationResult<MigrationStorageUpload> {
    if options.max_concurrent_uploads == 0 {
        return Err(MigrationError::InvalidInput {
            message: SanitizedMessage::new(
                "migration upload concurrency must be greater than zero",
            ),
        });
    }

    let storage_provider_id = storage.provider_id().to_owned();
    let checkpoint_path = options.checkpoint_path.clone();
    let checkpointed =
        load_migration_upload_checkpoint(checkpoint_path.clone(), storage_provider_id.clone())
            .await?;
    let mut indexed_outcomes = stream::iter(availability.objects.iter().cloned().enumerate())
        .map(|(index, local_object)| {
            let checkpointed = checkpointed.get(&local_object.object).cloned();
            let checkpoint_path = checkpoint_path.clone();
            let storage_provider_id = storage_provider_id.clone();
            async move {
                let outcome = if let Some(completion) = checkpointed {
                    resumed_migration_upload_outcome(
                        local_object.object.clone(),
                        &storage_provider_id,
                        completion,
                    )
                } else {
                    let status = upload_one_migration_object(&local_object, storage).await;
                    checkpoint_migration_upload_outcome(
                        &checkpoint_path,
                        &storage_provider_id,
                        local_object.object.clone(),
                        status,
                    )
                    .await
                };
                (index, outcome)
            }
        })
        .buffer_unordered(options.max_concurrent_uploads)
        .collect::<Vec<_>>()
        .await;
    indexed_outcomes.sort_by_key(|(index, _)| *index);
    let outcomes = indexed_outcomes
        .into_iter()
        .map(|(_, outcome)| outcome)
        .collect::<Vec<_>>();

    let already_present_objects = outcomes
        .iter()
        .filter(|outcome| {
            matches!(
                outcome.status,
                MigrationObjectUploadStatus::AlreadyPresent { .. }
            )
        })
        .map(|outcome| outcome.object.clone())
        .collect();
    let uploaded_objects = outcomes
        .iter()
        .filter_map(|outcome| match &outcome.status {
            MigrationObjectUploadStatus::Uploaded { stored_object, .. } => {
                Some(stored_object.clone())
            }
            MigrationObjectUploadStatus::AlreadyPresent { .. }
            | MigrationObjectUploadStatus::Failed { .. } => None,
        })
        .collect();
    let failed_objects = outcomes
        .iter()
        .filter_map(|outcome| match &outcome.status {
            MigrationObjectUploadStatus::Failed { message } => Some(MigrationObjectUploadFailure {
                object: outcome.object.clone(),
                message: message.clone(),
            }),
            MigrationObjectUploadStatus::AlreadyPresent { .. }
            | MigrationObjectUploadStatus::Uploaded { .. } => None,
        })
        .collect();

    Ok(MigrationStorageUpload {
        storage_provider_id,
        already_present_objects,
        uploaded_objects,
        failed_objects,
        outcomes,
        checkpoint_path,
    })
}

async fn upload_one_migration_object(
    local_object: &LocalMigrationObject,
    storage: &dyn StorageProvider,
) -> MigrationResult<MigrationObjectUploadStatus> {
    let object = &local_object.object;
    if storage.object_exists(object).await? {
        return Ok(MigrationObjectUploadStatus::AlreadyPresent { resumed: false });
    }

    let source = verified_migration_upload_source_path(local_object)?;
    verify_migration_upload_source(source, object).await?;
    let stored_object = storage.upload_object(object, source).await?;
    validate_migration_uploaded_object(object, storage.provider_id(), &stored_object)?;
    Ok(MigrationObjectUploadStatus::Uploaded {
        stored_object,
        resumed: false,
    })
}

async fn checkpoint_migration_upload_outcome(
    checkpoint_path: &Path,
    storage_provider_id: &str,
    object: LfsObject,
    status: MigrationResult<MigrationObjectUploadStatus>,
) -> MigrationObjectUploadOutcome {
    let status = match status {
        Ok(status) => {
            let completion = match &status {
                MigrationObjectUploadStatus::AlreadyPresent { .. } => {
                    Some(MigrationUploadCheckpointCompletion::AlreadyPresent)
                }
                MigrationObjectUploadStatus::Uploaded { stored_object, .. } => {
                    Some(MigrationUploadCheckpointCompletion::Uploaded {
                        backend_id: stored_object.backend_id.clone(),
                    })
                }
                MigrationObjectUploadStatus::Failed { .. } => None,
            };
            if let Some(completion) = completion
                && let Err(error) = append_migration_upload_checkpoint(
                    checkpoint_path.to_path_buf(),
                    storage_provider_id.to_owned(),
                    object.clone(),
                    completion,
                )
                .await
            {
                MigrationObjectUploadStatus::Failed {
                    message: SanitizedMessage::new(format!(
                        "object completed in storage but durable checkpoint failed: {error}"
                    )),
                }
            } else {
                status
            }
        }
        Err(error) => MigrationObjectUploadStatus::Failed {
            message: SanitizedMessage::new(error.to_string()),
        },
    };

    MigrationObjectUploadOutcome { object, status }
}

fn resumed_migration_upload_outcome(
    object: LfsObject,
    storage_provider_id: &str,
    completion: MigrationUploadCheckpointCompletion,
) -> MigrationObjectUploadOutcome {
    let status = match completion {
        MigrationUploadCheckpointCompletion::AlreadyPresent => {
            MigrationObjectUploadStatus::AlreadyPresent { resumed: true }
        }
        MigrationUploadCheckpointCompletion::Uploaded { backend_id } => {
            MigrationObjectUploadStatus::Uploaded {
                stored_object: StoredObject::new(storage_provider_id, object.clone(), backend_id),
                resumed: true,
            }
        }
    };
    MigrationObjectUploadOutcome { object, status }
}

fn default_migration_upload_checkpoint_path(
    availability: &LocalMigrationObjectAvailability,
    storage_provider_id: &str,
) -> PathBuf {
    let provider_digest = Sha256::digest(storage_provider_id.as_bytes());
    let filename = format!("lfs-cloud-migration-upload-{provider_digest:x}.jsonl");
    availability
        .git_lfs_objects_dir
        .parent()
        .unwrap_or(&availability.git_lfs_objects_dir)
        .join(filename)
}

async fn load_migration_upload_checkpoint(
    checkpoint_path: PathBuf,
    storage_provider_id: String,
) -> MigrationResult<BTreeMap<LfsObject, MigrationUploadCheckpointCompletion>> {
    tokio::task::spawn_blocking(move || {
        load_migration_upload_checkpoint_blocking(&checkpoint_path, &storage_provider_id)
    })
    .await
    .map_err(|error| MigrationError::InvalidInput {
        message: SanitizedMessage::new(format!(
            "migration checkpoint loading task failed: {error}"
        )),
    })?
}

fn load_migration_upload_checkpoint_blocking(
    checkpoint_path: &Path,
    storage_provider_id: &str,
) -> MigrationResult<BTreeMap<LfsObject, MigrationUploadCheckpointCompletion>> {
    create_migration_checkpoint_parent(checkpoint_path)?;
    let mut file = open_migration_checkpoint(checkpoint_path)?;
    FileExt::lock(&file).map_err(|source| {
        migration_checkpoint_io_error(
            checkpoint_path,
            "failed to lock migration upload checkpoint",
            source,
        )
    })?;
    let result = (|| {
        let mut contents = String::new();
        file.read_to_string(&mut contents).map_err(|source| {
            migration_checkpoint_io_error(
                checkpoint_path,
                "failed to read migration upload checkpoint",
                source,
            )
        })?;
        parse_migration_upload_checkpoint(&contents, checkpoint_path, storage_provider_id)
    })();
    let unlock_result = FileExt::unlock(&file).map_err(|source| {
        migration_checkpoint_io_error(
            checkpoint_path,
            "failed to unlock migration upload checkpoint",
            source,
        )
    });
    match (result, unlock_result) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(completed), Ok(())) => Ok(completed),
    }
}

fn parse_migration_upload_checkpoint(
    contents: &str,
    checkpoint_path: &Path,
    storage_provider_id: &str,
) -> MigrationResult<BTreeMap<LfsObject, MigrationUploadCheckpointCompletion>> {
    let mut completed = BTreeMap::new();
    let chunks = contents.split_inclusive('\n').collect::<Vec<_>>();
    for (index, chunk) in chunks.iter().enumerate() {
        let line = chunk
            .strip_suffix('\n')
            .unwrap_or(chunk)
            .trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let record = match serde_json::from_str::<MigrationUploadCheckpointRecord>(line) {
            Ok(record) => record,
            Err(_) if index + 1 == chunks.len() && !chunk.ends_with('\n') => break,
            Err(source) => {
                return Err(MigrationError::InvalidInput {
                    message: SanitizedMessage::new(format!(
                        "migration upload checkpoint {} contains invalid record {}: {source}",
                        checkpoint_path.display(),
                        index + 1
                    )),
                });
            }
        };
        if record.version != MIGRATION_UPLOAD_CHECKPOINT_VERSION {
            return Err(MigrationError::InvalidInput {
                message: SanitizedMessage::new(format!(
                    "migration upload checkpoint {} uses unsupported version {}",
                    checkpoint_path.display(),
                    record.version
                )),
            });
        }
        if record.storage_provider_id != storage_provider_id {
            return Err(MigrationError::InvalidInput {
                message: SanitizedMessage::new(format!(
                    "migration upload checkpoint {} belongs to a different storage provider",
                    checkpoint_path.display()
                )),
            });
        }
        let oid = LfsOid::from_str(&record.oid).map_err(|source| MigrationError::InvalidInput {
            message: SanitizedMessage::new(format!(
                "migration upload checkpoint {} contains an invalid object ID: {source}",
                checkpoint_path.display()
            )),
        })?;
        let object = LfsObject::new(oid, LfsObjectSize::new(record.size));
        let completion = match record.completion {
            MigrationUploadCheckpointRecordCompletion::AlreadyPresent => {
                MigrationUploadCheckpointCompletion::AlreadyPresent
            }
            MigrationUploadCheckpointRecordCompletion::Uploaded { backend_id } => {
                if backend_id.trim().is_empty() {
                    return Err(MigrationError::InvalidInput {
                        message: SanitizedMessage::new(format!(
                            "migration upload checkpoint {} contains an empty backend object ID",
                            checkpoint_path.display()
                        )),
                    });
                }
                MigrationUploadCheckpointCompletion::Uploaded { backend_id }
            }
        };
        completed.insert(object, completion);
    }
    Ok(completed)
}

async fn append_migration_upload_checkpoint(
    checkpoint_path: PathBuf,
    storage_provider_id: String,
    object: LfsObject,
    completion: MigrationUploadCheckpointCompletion,
) -> MigrationResult<()> {
    tokio::task::spawn_blocking(move || {
        append_migration_upload_checkpoint_blocking(
            &checkpoint_path,
            &storage_provider_id,
            &object,
            completion,
        )
    })
    .await
    .map_err(|error| MigrationError::InvalidInput {
        message: SanitizedMessage::new(format!("migration checkpoint write task failed: {error}")),
    })?
}

fn append_migration_upload_checkpoint_blocking(
    checkpoint_path: &Path,
    storage_provider_id: &str,
    object: &LfsObject,
    completion: MigrationUploadCheckpointCompletion,
) -> MigrationResult<()> {
    create_migration_checkpoint_parent(checkpoint_path)?;
    let mut file = open_migration_checkpoint(checkpoint_path)?;
    FileExt::lock(&file).map_err(|source| {
        migration_checkpoint_io_error(
            checkpoint_path,
            "failed to lock migration upload checkpoint",
            source,
        )
    })?;
    let result = (|| {
        let completion = match completion {
            MigrationUploadCheckpointCompletion::AlreadyPresent => {
                MigrationUploadCheckpointRecordCompletion::AlreadyPresent
            }
            MigrationUploadCheckpointCompletion::Uploaded { backend_id } => {
                MigrationUploadCheckpointRecordCompletion::Uploaded { backend_id }
            }
        };
        let record = MigrationUploadCheckpointRecord {
            version: MIGRATION_UPLOAD_CHECKPOINT_VERSION,
            storage_provider_id: storage_provider_id.to_owned(),
            oid: object.oid.as_hex().to_owned(),
            size: object.size.bytes(),
            completion,
        };
        let mut encoded =
            serde_json::to_vec(&record).map_err(|source| MigrationError::InvalidInput {
                message: SanitizedMessage::new(format!(
                    "failed to encode migration upload checkpoint record: {source}"
                )),
            })?;
        encoded.push(b'\n');
        file.write_all(&encoded).map_err(|source| {
            migration_checkpoint_io_error(
                checkpoint_path,
                "failed to append migration upload checkpoint",
                source,
            )
        })?;
        file.sync_data().map_err(|source| {
            migration_checkpoint_io_error(
                checkpoint_path,
                "failed to synchronize migration upload checkpoint",
                source,
            )
        })
    })();
    let unlock_result = FileExt::unlock(&file).map_err(|source| {
        migration_checkpoint_io_error(
            checkpoint_path,
            "failed to unlock migration upload checkpoint",
            source,
        )
    });
    result.and(unlock_result)
}

fn create_migration_checkpoint_parent(checkpoint_path: &Path) -> MigrationResult<()> {
    let parent = checkpoint_path
        .parent()
        .ok_or_else(|| MigrationError::InvalidInput {
            message: SanitizedMessage::new("migration upload checkpoint has no parent directory"),
        })?;
    fs::create_dir_all(parent).map_err(|source| {
        migration_checkpoint_io_error(
            checkpoint_path,
            "failed to create migration upload checkpoint directory",
            source,
        )
    })
}

fn open_migration_checkpoint(checkpoint_path: &Path) -> MigrationResult<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(checkpoint_path).map_err(|source| {
        migration_checkpoint_io_error(
            checkpoint_path,
            "failed to open migration upload checkpoint",
            source,
        )
    })
}

fn migration_checkpoint_io_error(
    checkpoint_path: &Path,
    context: &str,
    source: io::Error,
) -> MigrationError {
    MigrationError::Io {
        context: format!("{context} {}", checkpoint_path.display()),
        source,
    }
}

fn verified_migration_upload_source_path(
    local_object: &LocalMigrationObject,
) -> MigrationResult<&Path> {
    [
        LocalMigrationObjectLocationKind::GitLfsMedia,
        LocalMigrationObjectLocationKind::SharedCache,
    ]
    .into_iter()
    .find_map(|preferred_kind| {
        local_object
            .locations
            .iter()
            .find(|location| {
                location.kind == preferred_kind
                    && matches!(
                        location.status,
                        LocalMigrationObjectLocationStatus::Available
                    )
            })
            .map(|location| location.path.as_path())
    })
    .ok_or_else(|| MigrationError::SourceObjectMissing {
        oid: local_object.object.oid.as_hex().to_owned(),
        size: local_object.object.size.bytes(),
    })
}

async fn verify_migration_upload_source(path: &Path, object: &LfsObject) -> MigrationResult<()> {
    let path = path.to_path_buf();
    let object = object.clone();
    tokio::task::spawn_blocking(move || verify_migration_upload_source_blocking(&path, &object))
        .await
        .map_err(|error| MigrationError::InvalidInput {
            message: SanitizedMessage::new(format!(
                "migration source verification task failed: {error}"
            )),
        })?
}

fn verify_migration_upload_source_blocking(path: &Path, object: &LfsObject) -> MigrationResult<()> {
    let (actual_oid, actual_size) = hash_migration_object_file(path)?;
    if actual_oid == object.oid && actual_size == object.size {
        return Ok(());
    }

    Err(MigrationError::InvalidInput {
        message: SanitizedMessage::new(format!(
            "local migration source {} no longer matches sha256:{} ({} bytes): got sha256:{} ({} bytes)",
            path.display(),
            object.oid,
            object.size.bytes(),
            actual_oid,
            actual_size.bytes()
        )),
    })
}

fn validate_migration_uploaded_object(
    expected: &LfsObject,
    expected_provider_id: &str,
    stored: &StoredObject,
) -> MigrationResult<()> {
    if stored.provider_id != expected_provider_id {
        return Err(MigrationError::InvalidInput {
            message: SanitizedMessage::new(format!(
                "storage provider returned provider ID {}, expected {}",
                stored.provider_id, expected_provider_id
            )),
        });
    }

    if stored.backend_id.trim().is_empty() {
        return Err(MigrationError::InvalidInput {
            message: SanitizedMessage::new(format!(
                "storage provider {expected_provider_id} returned an empty backend object ID"
            )),
        });
    }

    if stored.object != *expected {
        return Err(MigrationError::InvalidInput {
            message: SanitizedMessage::new(format!(
                "storage provider returned object sha256:{} ({} bytes), expected sha256:{} ({} bytes)",
                stored.object.oid,
                stored.object.size.bytes(),
                expected.oid,
                expected.size.bytes()
            )),
        });
    }

    Ok(())
}

fn fetch_missing_migration_objects_with_runner<I, O, F>(
    start_dir: impl AsRef<Path>,
    objects: I,
    shared_cache: Option<&LocalCacheLayout>,
    source_remote: &str,
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
            source_remote: source_remote.to_owned(),
            mode,
            command,
            fetched_objects: Vec::new(),
            unavailable_objects: Vec::new(),
            before,
            after,
        });
    }

    let fetch_command = migration_source_fetch_command(source_remote, &mode)?;
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
        source_remote: source_remote.to_owned(),
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
    source_remote: &str,
    mode: &MigrationFetchMode,
) -> MigrationResult<MigrationSourceFetchCommand> {
    let source_remote = validate_source_remote_name(source_remote)?;
    // Migration fetch scope must be determined only by `mode`. Repository or
    // user configuration can otherwise turn every fetch into `--recent`,
    // expanding downloads beyond the reviewed migration inventory and making
    // `--all` fail because Git LFS forbids combining it with recent mode.
    let mut args = vec![
        OsString::from("-c"),
        OsString::from("lfs.fetchrecentalways=false"),
        OsString::from("-c"),
        OsString::from("lfs.fetchrecentrefsdays=0"),
        OsString::from("-c"),
        OsString::from("lfs.fetchrecentremoterefs=false"),
        OsString::from("-c"),
        OsString::from("lfs.fetchrecentcommitsdays=0"),
        OsString::from("lfs"),
        OsString::from("fetch"),
    ];

    match mode {
        MigrationFetchMode::CurrentCheckout => {
            args.push(OsString::from("--include="));
            args.push(OsString::from("--exclude="));
            args.push(OsString::from(&source_remote));
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
            args.push(OsString::from(&source_remote));
            args.extend(
                refs.iter()
                    .map(|ref_name| OsString::from(ref_name.as_str())),
            );
        }
        MigrationFetchMode::AllFetchedRefs => {
            args.push(OsString::from("--all"));
            args.push(OsString::from(&source_remote));
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
            stop_timed_out_git_lfs_fetch_child(child, command)?;
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

fn stop_timed_out_git_lfs_fetch_child(child: &mut Child, command: &str) -> MigrationResult<()> {
    stop_timed_out_git_lfs_fetch_process_tree(child);

    if child
        .try_wait()
        .map_err(|source| MigrationError::Io {
            context: format!("failed to wait for timed-out {command}"),
            source,
        })?
        .is_none()
    {
        child.kill().map_err(|source| MigrationError::Io {
            context: format!("failed to stop timed-out {command}"),
            source,
        })?;
    }
    child.wait().map_err(|source| MigrationError::Io {
        context: format!("failed to reap timed-out {command}"),
        source,
    })?;

    Ok(())
}

#[cfg(unix)]
fn stop_timed_out_git_lfs_fetch_process_tree(child: &Child) {
    let descendants = collect_git_lfs_fetch_descendant_pids(child.id());
    for pid in descendants.iter().rev() {
        signal_process("TERM", *pid);
    }
    thread::sleep(Duration::from_millis(50));
    for pid in descendants.iter().rev() {
        signal_process("KILL", *pid);
    }
}

#[cfg(unix)]
fn collect_git_lfs_fetch_descendant_pids(root_pid: u32) -> Vec<u32> {
    let mut descendants = Vec::new();
    let mut pending = child_pids(root_pid);

    while let Some(pid) = pending.pop() {
        descendants.push(pid);
        pending.extend(child_pids(pid));
    }

    descendants
}

#[cfg(target_os = "linux")]
fn child_pids(parent_pid: u32) -> Vec<u32> {
    let children_path = format!("/proc/{parent_pid}/task/{parent_pid}/children");
    let Ok(children) = fs::read_to_string(children_path) else {
        return Vec::new();
    };

    children
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn child_pids(parent_pid: u32) -> Vec<u32> {
    let Ok(output) = Command::new("pgrep")
        .args(["-P", &parent_pid.to_string()])
        .stdin(Stdio::null())
        .output()
    else {
        return Vec::new();
    };

    if !output.status.success() {
        return Vec::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

#[cfg(unix)]
fn signal_process(signal: &str, pid: u32) {
    let _ = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(windows)]
fn stop_timed_out_git_lfs_fetch_process_tree(child: &Child) {
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(any(unix, windows)))]
fn stop_timed_out_git_lfs_fetch_process_tree(_child: &Child) {}

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

fn require_historical_scan_git_version(worktree_root: &Path) -> MigrationResult<()> {
    const COMMAND: &str = "git --version";
    let output = run_git(worktree_root, ["--version"])?;
    let stdout = required_success_stdout(output, COMMAND)?;
    validate_historical_scan_git_version(&stdout)
}

fn validate_historical_scan_git_version(output: &str) -> MigrationResult<()> {
    let version_text = output
        .trim()
        .strip_prefix("git version ")
        .and_then(|version| version.split_ascii_whitespace().next())
        .ok_or_else(git_version_parse_error)?;
    let version = parse_git_version(version_text).ok_or_else(git_version_parse_error)?;

    if version < MINIMUM_HISTORICAL_SCAN_GIT_VERSION {
        return Err(MigrationError::UnsupportedGitVersion {
            installed: version_text.to_owned(),
            required: MINIMUM_HISTORICAL_SCAN_GIT_VERSION_TEXT,
            feature: "historical migration attribute discovery",
        });
    }

    Ok(())
}

fn parse_git_version(version: &str) -> Option<GitVersion> {
    let mut components = version.split('.');
    let major = components.next()?.parse().ok()?;
    let minor = components.next()?.parse().ok()?;
    let patch_text = components.next()?;
    let patch_digits = patch_text
        .bytes()
        .take_while(u8::is_ascii_digit)
        .collect::<Vec<_>>();
    let patch = std::str::from_utf8(&patch_digits).ok()?.parse().ok()?;

    Some(GitVersion::new(major, minor, patch))
}

fn git_version_parse_error() -> MigrationError {
    MigrationError::ExternalCommandOutput {
        command: "git --version".to_owned(),
        message: SanitizedMessage::new(
            "could not determine whether Git 2.40.0 or newer is installed; upgrade Git before scanning selected refs or all refs",
        ),
    }
}

fn require_complete_history(worktree_root: &Path) -> MigrationResult<()> {
    const COMMAND: &str = "git rev-parse --is-shallow-repository";
    let output = run_git(worktree_root, ["rev-parse", "--is-shallow-repository"])?;
    let stdout = required_success_stdout(output, COMMAND)?;

    match stdout.trim() {
        "false" => Ok(()),
        "true" => Err(MigrationError::ShallowRepository {
            path: worktree_root.to_path_buf(),
        }),
        _ => Err(MigrationError::ExternalCommandOutput {
            command: COMMAND.to_owned(),
            message: SanitizedMessage::new("git returned an invalid shallow-repository state"),
        }),
    }
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
) -> MigrationResult<Option<GitLfsSourceEndpoint>> {
    if let Some(url) = git_config_get(worktree_root, ["config", "--local", "--get", "lfs.url"])? {
        return Ok(Some(GitLfsSourceEndpoint {
            url,
            source: GitLfsSourceEndpointSource::LocalGitConfig,
        }));
    }

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

    Ok(
        default_lfs_endpoint_for_remote_url(&remote_url).map(|url| GitLfsSourceEndpoint {
            url,
            source: GitLfsSourceEndpointSource::RemoteUrlDefault,
        }),
    )
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

fn current_checkout_lfs_tracked_blobs(worktree_root: &Path) -> MigrationResult<Vec<GitIndexBlob>> {
    const COMMAND: &str = "git ls-files -z --cached --stage";
    let output = run_git_os_with_limit(
        worktree_root,
        [
            OsStr::new("ls-files"),
            OsStr::new("-z"),
            OsStr::new("--cached"),
            OsStr::new("--stage"),
        ],
        COMMAND,
        MAX_CURRENT_CHECKOUT_ATTR_OUTPUT_BYTES,
    )?;
    if !output.status.success() {
        return Err(command_error(COMMAND, output.status, &output.stderr));
    }
    if output.stdout.is_empty() {
        return Ok(Vec::new());
    }

    let index_blobs = parse_ls_files_stage_blob_output(&output.stdout, COMMAND)?;
    if index_blobs.is_empty() {
        return Ok(Vec::new());
    }
    let tracked_paths = index_blobs
        .iter()
        .flat_map(|blob| blob.relative_path_bytes.iter().copied().chain([b'\0']))
        .collect();
    let attributes = git_check_attr_filter(worktree_root, tracked_paths)?;
    let lfs_tracked_paths = parse_git_check_attr_filter_stdout(
        &attributes.stdout,
        &git_check_attr_filter_command_name(None),
    )?
    .into_iter()
    .collect::<BTreeSet<_>>();

    Ok(index_blobs
        .into_iter()
        .filter(|blob| lfs_tracked_paths.contains(&blob.relative_path))
        .collect())
}

fn parse_ls_files_stage_blob_output(
    stdout: &[u8],
    command_name: &str,
) -> MigrationResult<Vec<GitIndexBlob>> {
    let mut blobs = Vec::new();

    for record in stdout
        .split(|byte| *byte == b'\0')
        .filter(|record| !record.is_empty())
    {
        let Some(separator) = record.iter().position(|byte| *byte == b'\t') else {
            return Err(index_entry_parse_error(command_name));
        };
        let metadata = &record[..separator];
        let fields = metadata
            .split(|byte| *byte == b' ')
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        let [mode, object_id, stage] = fields.as_slice() else {
            return Err(index_entry_parse_error(command_name));
        };
        if *stage != b"0" {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("Git index contains an unmerged entry"),
            });
        }
        if !matches!(*mode, b"100644" | b"100755") {
            continue;
        }
        let object_id = std::str::from_utf8(object_id)
            .map_err(|_| index_entry_parse_error(command_name))?
            .to_owned();
        let relative_path_bytes = record[separator + 1..].to_owned();
        let relative_path = safe_git_relative_path(&relative_path_bytes, command_name)?;

        blobs.push(GitIndexBlob {
            object_id,
            relative_path,
            relative_path_bytes,
        });
    }

    Ok(blobs)
}

fn index_entry_parse_error(command_name: &str) -> MigrationError {
    MigrationError::ExternalCommandOutput {
        command: command_name.to_owned(),
        message: SanitizedMessage::new("git returned malformed index metadata"),
    }
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

    let mut child = read_only_git_command()
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

fn read_index_pointer_blob_candidate(
    worktree_root: &Path,
    object_id: &str,
) -> MigrationResult<Option<LfsPointer>> {
    read_history_pointer_blob_candidate(worktree_root, object_id)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitIndexBlob {
    object_id: String,
    relative_path: PathBuf,
    relative_path_bytes: Vec<u8>,
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct HistoryScanMetrics {
    cat_file_processes: usize,
    attribute_processes: usize,
    tree_entries_inspected: usize,
    blobs_inspected: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitHistoryCommit {
    object_id: String,
    tree_id: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct HistoryTreeSummary {
    pointer_blobs: Vec<GitTreeBlob>,
    attribute_blobs: Vec<(PathBuf, String)>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct HistoryAttributeQueryKey {
    attribute_blobs: Vec<(PathBuf, String)>,
    pointer_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RawGitTreeEntry {
    mode: Vec<u8>,
    object_id: String,
    name: Vec<u8>,
}

struct HistoryScanner<'a> {
    worktree_root: &'a Path,
    object_reader: Option<GitBatchObjectReader>,
    tree_cache: BTreeMap<String, HistoryTreeSummary>,
    blob_pointer_cache: BTreeMap<String, Option<LfsPointer>>,
    attribute_cache: BTreeMap<HistoryAttributeQueryKey, BTreeSet<PathBuf>>,
    commit_cache: BTreeMap<String, Vec<GitLfsHistoryPointerOccurrence>>,
    pointers: Vec<GitLfsHistoryPointer>,
    seen: BTreeSet<(String, PathBuf, LfsObject)>,
    metrics: HistoryScanMetrics,
}

impl<'a> HistoryScanner<'a> {
    fn new(worktree_root: &'a Path) -> MigrationResult<Self> {
        Ok(Self {
            worktree_root,
            object_reader: Some(GitBatchObjectReader::start(worktree_root)?),
            tree_cache: BTreeMap::new(),
            blob_pointer_cache: BTreeMap::new(),
            attribute_cache: BTreeMap::new(),
            commit_cache: BTreeMap::new(),
            pointers: Vec::new(),
            seen: BTreeSet::new(),
            metrics: HistoryScanMetrics {
                cat_file_processes: 1,
                ..HistoryScanMetrics::default()
            },
        })
    }

    fn scan_ref(&mut self, scanned_ref: &GitLfsScannedRef) -> MigrationResult<()> {
        for commit in rev_list_commits(self.worktree_root, &scanned_ref.commit)? {
            if !self.commit_cache.contains_key(&commit.object_id) {
                let occurrences = self.scan_commit(&commit)?;
                self.commit_cache
                    .insert(commit.object_id.clone(), occurrences);
            }

            for occurrence in self
                .commit_cache
                .get(&commit.object_id)
                .expect("history commit cache should contain scanned commit")
            {
                let key = (
                    occurrence.commit.clone(),
                    occurrence.relative_path.clone(),
                    occurrence.object.clone(),
                );
                if self.seen.insert(key) {
                    self.pointers.push(GitLfsHistoryPointer {
                        ref_name: scanned_ref.name.clone(),
                        commit: occurrence.commit.clone(),
                        relative_path: occurrence.relative_path.clone(),
                        object: occurrence.object.clone(),
                    });
                }
            }
        }

        Ok(())
    }

    fn scan_commit(
        &mut self,
        commit: &GitHistoryCommit,
    ) -> MigrationResult<Vec<GitLfsHistoryPointerOccurrence>> {
        let summary = self.tree_summary(&commit.tree_id)?;
        if summary.pointer_blobs.is_empty() {
            return Ok(Vec::new());
        }

        let query_key = HistoryAttributeQueryKey {
            attribute_blobs: summary.attribute_blobs.clone(),
            pointer_paths: summary
                .pointer_blobs
                .iter()
                .map(|blob| blob.relative_path.clone())
                .collect(),
        };
        let lfs_paths = if let Some(paths) = self.attribute_cache.get(&query_key) {
            paths.clone()
        } else {
            let (paths, process_count) = git_check_attr_lfs_paths_for_tree_blobs(
                self.worktree_root,
                &summary.pointer_blobs,
                &commit.object_id,
            )?;
            self.metrics.attribute_processes += process_count;
            self.attribute_cache.insert(query_key, paths.clone());
            paths
        };

        let mut occurrences = Vec::new();
        for blob in summary
            .pointer_blobs
            .into_iter()
            .filter(|blob| lfs_paths.contains(&blob.relative_path))
        {
            let pointer = self
                .blob_pointer_cache
                .get(&blob.object_id)
                .and_then(Clone::clone)
                .expect("tree summaries contain only parsed pointer blobs");
            occurrences.push(GitLfsHistoryPointerOccurrence {
                commit: commit.object_id.clone(),
                relative_path: blob.relative_path,
                object: pointer.object,
            });
        }

        Ok(occurrences)
    }

    fn tree_summary(&mut self, tree_id: &str) -> MigrationResult<HistoryTreeSummary> {
        if let Some(summary) = self.tree_cache.get(tree_id) {
            return Ok(summary.clone());
        }

        let command_name = format!("git cat-file --batch-command tree {tree_id}");
        let contents = self
            .object_reader
            .as_mut()
            .expect("history scanner object reader should be available")
            .contents(
                tree_id,
                "tree",
                MAX_HISTORY_TREE_OUTPUT_BYTES,
                &command_name,
            )?;
        let entries = parse_raw_git_tree(&contents, tree_id, &command_name)?;
        self.metrics.tree_entries_inspected += entries.len();
        let mut summary = HistoryTreeSummary::default();

        for entry in entries {
            let relative_path = safe_git_relative_path(&entry.name, &command_name)?;
            if entry.mode == b"40000" {
                let child = self.tree_summary(&entry.object_id)?;
                append_prefixed_tree_summary(&mut summary, &relative_path, &entry.name, child);
                continue;
            }
            if entry.mode == b"160000" {
                continue;
            }
            if !matches!(entry.mode.as_slice(), b"100644" | b"100755" | b"120000") {
                return Err(MigrationError::ExternalCommandOutput {
                    command: command_name,
                    message: SanitizedMessage::new("git returned an unsupported tree mode"),
                });
            }

            if entry.name == b".gitattributes" {
                summary
                    .attribute_blobs
                    .push((relative_path.clone(), entry.object_id.clone()));
            }

            if !self.blob_pointer_cache.contains_key(&entry.object_id) {
                self.metrics.blobs_inspected += 1;
                let pointer = self.read_pointer_blob_candidate(&entry.object_id)?;
                self.blob_pointer_cache
                    .insert(entry.object_id.clone(), pointer);
            }
            if self
                .blob_pointer_cache
                .get(&entry.object_id)
                .is_some_and(Option::is_some)
            {
                summary.pointer_blobs.push(GitTreeBlob {
                    object_id: entry.object_id,
                    relative_path,
                    relative_path_bytes: entry.name,
                });
            }
        }

        summary
            .pointer_blobs
            .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        summary.attribute_blobs.sort();
        self.tree_cache.insert(tree_id.to_owned(), summary.clone());
        Ok(summary)
    }

    fn read_pointer_blob_candidate(
        &mut self,
        object_id: &str,
    ) -> MigrationResult<Option<LfsPointer>> {
        let command_name = format!("git cat-file --batch-command info {object_id}");
        let info = self
            .object_reader
            .as_mut()
            .expect("history scanner object reader should be available")
            .info(object_id, &command_name)?;
        if info.object_type != "blob" {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name,
                message: SanitizedMessage::new("git tree entry did not resolve to a blob"),
            });
        }
        if info.size >= LFS_POINTER_SIZE_CUTOFF {
            return Ok(None);
        }

        let blob_command = format!("git cat-file --batch-command contents {object_id}");
        let contents = self
            .object_reader
            .as_mut()
            .expect("history scanner object reader should be available")
            .contents(
                object_id,
                "blob",
                LFS_POINTER_SIZE_CUTOFF as usize,
                &blob_command,
            )?;
        let Ok(contents) = std::str::from_utf8(&contents) else {
            return Ok(None);
        };

        Ok(LfsPointer::parse(contents)
            .ok()
            .filter(|pointer| !pointer.is_empty()))
    }

    fn finish(mut self) -> MigrationResult<(Vec<GitLfsHistoryPointer>, HistoryScanMetrics)> {
        self.object_reader
            .take()
            .expect("history scanner object reader should be available")
            .finish()?;
        Ok((std::mem::take(&mut self.pointers), self.metrics.clone()))
    }
}

fn scan_resolved_history_refs(
    worktree_root: PathBuf,
    scanned_refs: Vec<GitLfsScannedRef>,
) -> MigrationResult<(GitLfsHistoryPointers, HistoryScanMetrics)> {
    if scanned_refs.is_empty() {
        return Ok((
            GitLfsHistoryPointers {
                worktree_root,
                refs: scanned_refs,
                pointers: Vec::new(),
            },
            HistoryScanMetrics::default(),
        ));
    }

    let mut scanner = HistoryScanner::new(&worktree_root)?;
    for scanned_ref in &scanned_refs {
        scanner.scan_ref(scanned_ref)?;
    }
    let (pointers, metrics) = scanner.finish()?;
    Ok((
        GitLfsHistoryPointers {
            worktree_root,
            refs: scanned_refs,
            pointers,
        },
        metrics,
    ))
}

fn append_prefixed_tree_summary(
    target: &mut HistoryTreeSummary,
    prefix: &Path,
    prefix_bytes: &[u8],
    child: HistoryTreeSummary,
) {
    target
        .pointer_blobs
        .extend(child.pointer_blobs.into_iter().map(|blob| {
            let mut relative_path_bytes =
                Vec::with_capacity(prefix_bytes.len() + 1 + blob.relative_path_bytes.len());
            relative_path_bytes.extend_from_slice(prefix_bytes);
            relative_path_bytes.push(b'/');
            relative_path_bytes.extend_from_slice(&blob.relative_path_bytes);
            GitTreeBlob {
                object_id: blob.object_id,
                relative_path: prefix.join(blob.relative_path),
                relative_path_bytes,
            }
        }));
    target.attribute_blobs.extend(
        child
            .attribute_blobs
            .into_iter()
            .map(|(path, object_id)| (prefix.join(path), object_id)),
    );
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

fn all_fetched_ref_names(
    worktree_root: &Path,
    source_remote: &str,
) -> MigrationResult<Vec<String>> {
    let command_name = "git for-each-ref --format=%(refname)%00%(symref) refs/heads refs/remotes/<source> refs/tags";
    let remote_refs = format!("refs/remotes/{source_remote}");
    let output = run_git_os_vec_with_limit(
        worktree_root,
        vec![
            OsString::from("for-each-ref"),
            OsString::from("--format=%(refname)%00%(symref)"),
            OsString::from("refs/heads"),
            OsString::from(remote_refs),
            OsString::from("refs/tags"),
        ],
        command_name,
        MAX_HISTORY_REF_LIST_BYTES,
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

fn rev_list_commits(
    worktree_root: &Path,
    root_commit: &str,
) -> MigrationResult<Vec<GitHistoryCommit>> {
    let command_name =
        format!("git rev-list --topo-order --format=%H%x20%T --no-commit-header {root_commit}");
    let output = run_git_os_vec_with_limit(
        worktree_root,
        vec![
            OsString::from("rev-list"),
            OsString::from("--topo-order"),
            OsString::from("--format=%H %T"),
            OsString::from("--no-commit-header"),
            OsString::from(root_commit),
        ],
        &command_name,
        MAX_HISTORY_COMMIT_LIST_BYTES,
    )?;
    let stdout =
        required_success_stdout_with_limit(output, &command_name, MAX_HISTORY_COMMIT_LIST_BYTES)?;
    let mut commits = Vec::new();

    for line in stdout.lines().filter(|line| !line.is_empty()) {
        let Some((object_id, tree_id)) = line.split_once(' ') else {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.clone(),
                message: SanitizedMessage::new("git returned malformed commit and tree output"),
            });
        };
        if !is_git_object_id(object_id) || !is_git_object_id(tree_id) {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.clone(),
                message: SanitizedMessage::new("git returned an invalid commit or tree object ID"),
            });
        }
        commits.push(GitHistoryCommit {
            object_id: object_id.to_owned(),
            tree_id: tree_id.to_owned(),
        });
    }

    Ok(commits)
}

fn parse_raw_git_tree(
    contents: &[u8],
    tree_id: &str,
    command_name: &str,
) -> MigrationResult<Vec<RawGitTreeEntry>> {
    let object_id_bytes = tree_id.len() / 2;
    if !matches!(object_id_bytes, 20 | 32) {
        return Err(MigrationError::ExternalCommandOutput {
            command: command_name.to_owned(),
            message: SanitizedMessage::new("git returned an invalid tree object ID"),
        });
    }

    let mut entries = Vec::new();
    let mut cursor = 0;
    while cursor < contents.len() {
        let Some(mode_end_offset) = contents[cursor..].iter().position(|byte| *byte == b' ') else {
            return Err(raw_git_tree_parse_error(command_name));
        };
        let mode_end = cursor + mode_end_offset;
        let mode = contents[cursor..mode_end].to_vec();
        cursor = mode_end + 1;

        let Some(name_end_offset) = contents[cursor..].iter().position(|byte| *byte == b'\0')
        else {
            return Err(raw_git_tree_parse_error(command_name));
        };
        let name_end = cursor + name_end_offset;
        let name = contents[cursor..name_end].to_vec();
        cursor = name_end + 1;

        let object_end = cursor
            .checked_add(object_id_bytes)
            .ok_or_else(|| raw_git_tree_parse_error(command_name))?;
        let object_bytes = contents
            .get(cursor..object_end)
            .ok_or_else(|| raw_git_tree_parse_error(command_name))?;
        cursor = object_end;

        if mode.is_empty() || name.is_empty() {
            return Err(raw_git_tree_parse_error(command_name));
        }
        let object_id = object_bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        entries.push(RawGitTreeEntry {
            mode,
            object_id,
            name,
        });
    }

    Ok(entries)
}

fn raw_git_tree_parse_error(command_name: &str) -> MigrationError {
    MigrationError::ExternalCommandOutput {
        command: command_name.to_owned(),
        message: SanitizedMessage::new("git returned malformed tree object data"),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitBatchObjectInfo {
    object_type: String,
    size: u64,
}

struct GitBatchObjectReader {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr_reader: Option<thread::JoinHandle<io::Result<PipeReadResult>>>,
    finished: bool,
}

impl GitBatchObjectReader {
    fn start(worktree_root: &Path) -> MigrationResult<Self> {
        const COMMAND: &str = "git cat-file --batch-command";
        let mut child = read_only_git_command()
            .args(["cat-file", "--batch-command"])
            .current_dir(worktree_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| MigrationError::Io {
                context: format!("failed to start {COMMAND}"),
                source,
            })?;
        let stdin = child.stdin.take().ok_or_else(|| MigrationError::Io {
            context: format!("{COMMAND} stdin was not piped"),
            source: io::Error::other("git cat-file stdin was not piped"),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| MigrationError::Io {
            context: format!("{COMMAND} stdout was not piped"),
            source: io::Error::other("git cat-file stdout was not piped"),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| MigrationError::Io {
            context: format!("{COMMAND} stderr was not piped"),
            source: io::Error::other("git cat-file stderr was not piped"),
        })?;
        let stderr_reader =
            thread::spawn(move || read_pipe_with_limit(stderr, MAX_MIGRATION_GIT_OUTPUT_BYTES + 1));

        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            stderr_reader: Some(stderr_reader),
            finished: false,
        })
    }

    fn info(&mut self, object_id: &str, command_name: &str) -> MigrationResult<GitBatchObjectInfo> {
        self.write_request("info", object_id, command_name)?;
        self.read_header(object_id, command_name)
    }

    fn contents(
        &mut self,
        object_id: &str,
        expected_type: &str,
        max_size: usize,
        command_name: &str,
    ) -> MigrationResult<Vec<u8>> {
        self.write_request("contents", object_id, command_name)?;
        let info = self.read_header(object_id, command_name)?;
        if info.object_type != expected_type {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned an unexpected object type"),
            });
        }
        let size =
            usize::try_from(info.size).map_err(|_| MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned an oversized object"),
            })?;
        if size > max_size {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned too much object data"),
            });
        }

        let mut contents = vec![0; size];
        self.stdout
            .read_exact(&mut contents)
            .map_err(|source| MigrationError::Io {
                context: format!("failed to read {command_name} object data"),
                source,
            })?;
        let mut delimiter = [0];
        self.stdout
            .read_exact(&mut delimiter)
            .map_err(|source| MigrationError::Io {
                context: format!("failed to read {command_name} object delimiter"),
                source,
            })?;
        if delimiter != [b'\n'] {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned malformed batch object data"),
            });
        }

        Ok(contents)
    }

    fn write_request(
        &mut self,
        operation: &str,
        object_id: &str,
        command_name: &str,
    ) -> MigrationResult<()> {
        if !is_git_object_id(object_id) {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git object request contained an invalid ID"),
            });
        }
        let stdin = self
            .stdin
            .as_mut()
            .expect("unfinished git cat-file reader should retain stdin");
        writeln!(stdin, "{operation} {object_id}")
            .and_then(|()| stdin.flush())
            .map_err(|source| MigrationError::Io {
                context: format!("failed to write {command_name} request"),
                source,
            })
    }

    fn read_header(
        &mut self,
        requested_object_id: &str,
        command_name: &str,
    ) -> MigrationResult<GitBatchObjectInfo> {
        let mut header = Vec::new();
        let bytes_read = self
            .stdout
            .read_until(b'\n', &mut header)
            .map_err(|source| MigrationError::Io {
                context: format!("failed to read {command_name} response"),
                source,
            })?;
        if bytes_read == 0 || !header.ends_with(b"\n") {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned a truncated batch object header"),
            });
        }
        header.pop();
        if header.ends_with(b" missing") {
            return Err(MigrationError::GitObjectUnavailable {
                object_id: requested_object_id.to_owned(),
            });
        }
        let header =
            std::str::from_utf8(&header).map_err(|_| MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned non-UTF-8 batch object metadata"),
            })?;
        let fields = header.split(' ').collect::<Vec<_>>();
        let [object_id, object_type, size] = fields.as_slice() else {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned malformed batch object metadata"),
            });
        };
        if *object_id != requested_object_id || !is_git_object_id(object_id) {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned a mismatched batch object ID"),
            });
        }
        let size = size
            .parse::<u64>()
            .map_err(|_| MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned an invalid batch object size"),
            })?;
        Ok(GitBatchObjectInfo {
            object_type: (*object_type).to_owned(),
            size,
        })
    }

    fn finish(mut self) -> MigrationResult<()> {
        const COMMAND: &str = "git cat-file --batch-command";
        self.stdin.take();
        let status = self
            .child
            .as_mut()
            .expect("unfinished git cat-file reader should retain its child")
            .wait()
            .map_err(|source| MigrationError::Io {
                context: format!("failed to wait for {COMMAND}"),
                source,
            })?;
        let stderr = self.join_stderr_reader(COMMAND)?;
        self.finished = true;
        if !status.success() {
            return Err(command_error(COMMAND, status, &stderr.bytes));
        }
        if stderr.exceeded_limit {
            return Err(MigrationError::ExternalCommandOutput {
                command: COMMAND.to_owned(),
                message: SanitizedMessage::new("git returned too much batch diagnostic output"),
            });
        }
        Ok(())
    }

    fn join_stderr_reader(&mut self, command_name: &str) -> MigrationResult<PipeReadResult> {
        self.stderr_reader
            .take()
            .expect("unfinished git cat-file reader should retain stderr reader")
            .join()
            .map_err(|_| MigrationError::Io {
                context: format!("{command_name} stderr reader panicked"),
                source: io::Error::other("git cat-file stderr reader panicked"),
            })?
            .map_err(|source| MigrationError::Io {
                context: format!("failed to read {command_name} stderr"),
                source,
            })
    }
}

impl Drop for GitBatchObjectReader {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.stdin.take();
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(stderr_reader) = self.stderr_reader.take() {
            let _ = stderr_reader.join();
        }
    }
}

#[cfg(test)]
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
) -> MigrationResult<(BTreeSet<PathBuf>, usize)> {
    let mut lfs_paths = BTreeSet::new();
    let mut path_input = Vec::new();
    let mut process_count = 0;

    for blob in blobs {
        let path_entry_len = blob.relative_path_bytes.len() + 1;
        if path_entry_len > MAX_HISTORY_CHECK_ATTR_INPUT_BYTES {
            return Err(MigrationError::ExternalCommandOutput {
                command: git_check_attr_filter_command_name(Some(commit)),
                message: SanitizedMessage::new(
                    "historical pointer path is too large for attribute lookup",
                ),
            });
        }

        if !path_input.is_empty()
            && path_input.len() + path_entry_len > MAX_HISTORY_CHECK_ATTR_INPUT_BYTES
        {
            append_git_check_attr_lfs_paths(worktree_root, commit, path_input, &mut lfs_paths)?;
            process_count += 1;
            path_input = Vec::new();
        }

        path_input.extend_from_slice(&blob.relative_path_bytes);
        path_input.push(b'\0');
    }

    if !path_input.is_empty() {
        append_git_check_attr_lfs_paths(worktree_root, commit, path_input, &mut lfs_paths)?;
        process_count += 1;
    }

    Ok((lfs_paths, process_count))
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
    if !size_output.status.success() {
        return Err(MigrationError::GitObjectUnavailable {
            object_id: object_id.to_owned(),
        });
    }
    let size_stdout = output_stdout(size_output, &size_command)?;
    let size = size_stdout
        .trim_end_matches(['\n', '\r'])
        .parse::<u64>()
        .map_err(|_| MigrationError::ExternalCommandOutput {
            command: size_command.clone(),
            message: SanitizedMessage::new("git returned an invalid blob size"),
        })?;
    if size >= LFS_POINTER_SIZE_CUTOFF {
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
    if blob_output.stdout.len() >= LFS_POINTER_SIZE_CUTOFF as usize {
        return Ok(None);
    }
    let Ok(contents) = std::str::from_utf8(&blob_output.stdout) else {
        return Ok(None);
    };

    Ok(LfsPointer::parse(contents)
        .ok()
        .filter(|pointer| !pointer.is_empty()))
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
    let command_name = format!("git {}", args.join(" "));
    let mut command = read_only_git_command();
    command.args(args).current_dir(current_dir);
    run_bounded_command_output(&mut command, &command_name, MAX_MIGRATION_GIT_OUTPUT_BYTES)
}

fn run_git_os<const N: usize>(
    current_dir: &Path,
    args: [&OsStr; N],
    command_name: &str,
) -> MigrationResult<Output> {
    run_git_os_with_limit(
        current_dir,
        args,
        command_name,
        MAX_MIGRATION_GIT_OUTPUT_BYTES,
    )
}

fn run_git_os_with_limit<const N: usize>(
    current_dir: &Path,
    args: [&OsStr; N],
    command_name: &str,
    stdout_limit: usize,
) -> MigrationResult<Output> {
    let mut command = read_only_git_command();
    command.args(args).current_dir(current_dir);
    run_bounded_command_output(&mut command, command_name, stdout_limit)
}

fn run_git_os_vec(
    current_dir: &Path,
    args: Vec<OsString>,
    command_name: &str,
) -> MigrationResult<Output> {
    run_git_os_vec_with_limit(
        current_dir,
        args,
        command_name,
        MAX_MIGRATION_GIT_OUTPUT_BYTES,
    )
}

fn run_git_os_vec_with_limit(
    current_dir: &Path,
    args: Vec<OsString>,
    command_name: &str,
    stdout_limit: usize,
) -> MigrationResult<Output> {
    let mut command = read_only_git_command();
    command.args(args).current_dir(current_dir);
    run_bounded_command_output(&mut command, command_name, stdout_limit)
}

fn read_only_git_command() -> Command {
    let mut command = Command::new("git");
    // Promisor repositories may fetch missing objects from their remote during
    // otherwise read-only commands. Migration discovery must never transfer
    // data, especially when it is building a dry-run report.
    command.env(GIT_NO_LAZY_FETCH_ENV, "1");
    command
}

enum BoundedGitPipeEvent {
    Stdout(io::Result<PipeReadResult>),
    Stderr(io::Result<PipeReadResult>),
}

/// Runs a migration Git command without allowing either captured pipe to grow
/// beyond its declared boundary.
///
/// stdout and stderr are drained concurrently so a noisy diagnostic stream
/// cannot deadlock a command whose primary output is still being consumed. A
/// reader reports overflow as soon as it sees the first excess byte; the parent
/// then stops the whole owned process tree before returning the bounded prefix.
fn run_bounded_command_output(
    command: &mut Command,
    command_name: &str,
    stdout_limit: usize,
) -> MigrationResult<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_bounded_git_process_tree(command);

    let mut child = command.spawn().map_err(|source| MigrationError::Io {
        context: format!("failed to start {command_name}"),
        source,
    })?;
    let stdout = child.stdout.take().ok_or_else(|| MigrationError::Io {
        context: format!("failed to capture stdout for {command_name}"),
        source: io::Error::other("git stdout was not piped"),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| MigrationError::Io {
        context: format!("failed to capture stderr for {command_name}"),
        source: io::Error::other("git stderr was not piped"),
    })?;

    let (sender, receiver) = mpsc::channel();
    let stdout_sender = sender.clone();
    let stdout_reader = thread::spawn(move || {
        let result = read_pipe_with_hard_limit(stdout, stdout_limit);
        let _ = stdout_sender.send(BoundedGitPipeEvent::Stdout(result));
    });
    let stderr_reader = thread::spawn(move || {
        let result = read_pipe_with_hard_limit(stderr, MAX_MIGRATION_GIT_OUTPUT_BYTES);
        let _ = sender.send(BoundedGitPipeEvent::Stderr(result));
    });

    let mut status = None;
    let mut drain_deadline = None;
    let mut stdout = None;
    let mut stderr = None;

    loop {
        while let Ok(event) = receiver.try_recv() {
            let (stream_name, stream_limit, result, destination) = match event {
                BoundedGitPipeEvent::Stdout(result) => {
                    ("stdout", stdout_limit, result, &mut stdout)
                }
                BoundedGitPipeEvent::Stderr(result) => (
                    "stderr",
                    MAX_MIGRATION_GIT_OUTPUT_BYTES,
                    result,
                    &mut stderr,
                ),
            };
            let output = match result {
                Ok(output) => output,
                Err(source) => {
                    terminate_bounded_git_child(&mut child, command_name)?;
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(MigrationError::Io {
                        context: format!("failed to read {stream_name} from {command_name}"),
                        source,
                    });
                }
            };
            if output.exceeded_limit {
                terminate_bounded_git_child(&mut child, command_name)?;
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(MigrationError::ExternalCommandOutput {
                    command: command_name.to_owned(),
                    message: SanitizedMessage::new(format!(
                        "git {stream_name} exceeded the {stream_limit}-byte limit"
                    )),
                });
            }
            *destination = Some(output.bytes);
        }

        if status.is_none() {
            status = child.try_wait().map_err(|source| MigrationError::Io {
                context: format!("failed to wait for {command_name}"),
                source,
            })?;
            if status.is_some() {
                drain_deadline = Some(Instant::now() + MIGRATION_GIT_OUTPUT_DRAIN_GRACE);
            }
        }

        if let Some(status) = status.filter(|_| stdout.is_some() && stderr.is_some()) {
            stdout_reader.join().map_err(|_| MigrationError::Io {
                context: format!("stdout reader thread panicked for {command_name}"),
                source: io::Error::other("git stdout reader thread panicked"),
            })?;
            stderr_reader.join().map_err(|_| MigrationError::Io {
                context: format!("stderr reader thread panicked for {command_name}"),
                source: io::Error::other("git stderr reader thread panicked"),
            })?;
            return Ok(Output {
                status,
                stdout: stdout.take().expect("stdout was checked above"),
                stderr: stderr.take().expect("stderr was checked above"),
            });
        }

        if status.is_some_and(|_| drain_deadline.is_some_and(|deadline| Instant::now() >= deadline))
        {
            // A descendant inherited a pipe after the direct Git process exited.
            // Stop the process group before waiting for EOF so discovery cannot
            // hang on a helper that outlives Git itself.
            stop_bounded_git_process_tree(&child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(MigrationError::Io {
                context: format!("timed out draining output from {command_name}"),
                source: io::Error::new(
                    io::ErrorKind::TimedOut,
                    "git output pipes remained open after process exit",
                ),
            });
        }

        thread::sleep(MIGRATION_GIT_OUTPUT_POLL_INTERVAL);
    }
}

fn read_pipe_with_hard_limit(mut reader: impl Read, limit: usize) -> io::Result<PipeReadResult> {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0; 8192];

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(PipeReadResult {
                bytes,
                exceeded_limit: false,
            });
        }

        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
        if read > remaining {
            return Ok(PipeReadResult {
                bytes,
                exceeded_limit: true,
            });
        }
    }
}

#[cfg(unix)]
fn configure_bounded_git_process_tree(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_bounded_git_process_tree(_command: &mut Command) {}

fn terminate_bounded_git_child(child: &mut Child, command_name: &str) -> MigrationResult<()> {
    stop_bounded_git_process_tree(child);
    if child
        .try_wait()
        .map_err(|source| MigrationError::Io {
            context: format!("failed to wait for stopped {command_name}"),
            source,
        })?
        .is_none()
    {
        child.kill().map_err(|source| MigrationError::Io {
            context: format!("failed to stop {command_name}"),
            source,
        })?;
        child.wait().map_err(|source| MigrationError::Io {
            context: format!("failed to reap stopped {command_name}"),
            source,
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn stop_bounded_git_process_tree(child: &Child) {
    signal_bounded_git_process_group("TERM", child.id());
    thread::sleep(Duration::from_millis(50));
    signal_bounded_git_process_group("KILL", child.id());
}

#[cfg(unix)]
fn signal_bounded_git_process_group(signal: &str, process_group_id: u32) {
    let _ = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(format!("-{process_group_id}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(windows)]
fn stop_bounded_git_process_tree(child: &Child) {
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(any(unix, windows)))]
fn stop_bounded_git_process_tree(_child: &Child) {}

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
        process::{Command, Stdio},
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use tempfile::TempDir;

    use sha2::{Digest, Sha256};

    use crate::{
        LfsObject, LfsObjectSize, LfsOid, LfsPointer, LocalCacheLayout, ProviderFuture,
        StorageDeleteOutcome, StorageError, StorageProvider, StorageResult, StoredObject,
    };

    use super::{
        GitLfsSourceEndpointSource, LocalMigrationObject, LocalMigrationObjectLocation,
        LocalMigrationObjectLocationKind, LocalMigrationObjectLocationStatus,
        MAX_GIT_ATTRIBUTES_BYTES, MAX_MIGRATION_GIT_OUTPUT_BYTES, MigrationError,
        MigrationFetchMode, MigrationObjectUploadStatus, MigrationStorageUploadOptions,
        check_local_migration_objects, default_lfs_endpoint_for_remote_url,
        discover_git_lfs_migration, discover_git_lfs_migration_from_remote, display_git_command,
        enumerate_all_fetched_ref_lfs_pointers, enumerate_current_checkout_lfs_pointers,
        enumerate_fetched_ref_lfs_pointers_for_remote, enumerate_selected_ref_lfs_pointers,
        enumerate_selected_ref_lfs_pointers_with_metrics, fetch_missing_migration_objects,
        fetch_missing_migration_objects_with_runner, git_lfs_object_path,
        hash_migration_object_file, migration_source_fetch_command,
        parse_git_check_attr_filter_stdout, parse_lfs_patterns_from_attributes,
        parse_ls_tree_blob_output, repo_relative_path_from_git_output, split_gitattributes_line,
        upload_migration_objects_to_storage, upload_migration_objects_to_storage_with_options,
        validate_historical_scan_git_version, validate_history_ref_name,
        verified_migration_upload_source_path, wait_for_git_lfs_fetch_command,
    };

    #[cfg(unix)]
    #[test]
    fn bounded_git_output_stops_a_runaway_process_tree_on_overflow() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("(while :; do printf '0123456789abcdef'; done) & child=$!; wait \"$child\"");
        let started_at = Instant::now();

        let error =
            super::run_bounded_command_output(&mut command, "git runaway-output-test", 4 * 1024)
                .expect_err("runaway output should cross the hard limit");

        assert!(
            started_at.elapsed() < Duration::from_secs(5),
            "overflow cleanup should stop the command process tree promptly"
        );
        assert!(matches!(
            error,
            MigrationError::ExternalCommandOutput { command, message }
                if command == "git runaway-output-test"
                    && message.as_str().contains("stdout")
                    && message.as_str().contains("4096-byte limit")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_git_output_drains_stdout_and_stderr_concurrently() {
        let mut command = Command::new("sh");
        command.arg("-c").arg(
            "i=0; while [ \"$i\" -lt 8192 ]; do printf '0123456789abcdef' >&2; i=$((i + 1)); done; i=0; while [ \"$i\" -lt 8192 ]; do printf 'fedcba9876543210'; i=$((i + 1)); done",
        );

        let output = super::run_bounded_command_output(
            &mut command,
            "git concurrent-output-test",
            MAX_MIGRATION_GIT_OUTPUT_BYTES,
        )
        .expect("bounded runner should drain both pipes without deadlocking");

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 128 * 1024);
        assert_eq!(output.stderr.len(), 128 * 1024);
    }

    struct FakeMigrationStorageProvider {
        provider_id: String,
        existing: Mutex<BTreeSet<LfsObject>>,
        uploaded: Mutex<Vec<LfsObject>>,
        returned_object_override: Mutex<Option<LfsObject>>,
        returned_provider_id_override: Mutex<Option<String>>,
        returned_backend_id_override: Mutex<Option<String>>,
        upload_failures: Mutex<BTreeSet<LfsObject>>,
        upload_attempts: Mutex<Vec<LfsObject>>,
        upload_delay: Mutex<Option<Duration>>,
        active_uploads: AtomicUsize,
        max_active_uploads: AtomicUsize,
    }

    impl FakeMigrationStorageProvider {
        fn new(provider_id: impl Into<String>) -> Self {
            Self {
                provider_id: provider_id.into(),
                existing: Mutex::new(BTreeSet::new()),
                uploaded: Mutex::new(Vec::new()),
                returned_object_override: Mutex::new(None),
                returned_provider_id_override: Mutex::new(None),
                returned_backend_id_override: Mutex::new(None),
                upload_failures: Mutex::new(BTreeSet::new()),
                upload_attempts: Mutex::new(Vec::new()),
                upload_delay: Mutex::new(None),
                active_uploads: AtomicUsize::new(0),
                max_active_uploads: AtomicUsize::new(0),
            }
        }

        fn insert_existing(&self, object: LfsObject) {
            self.existing
                .lock()
                .expect("fake storage lock should not poison")
                .insert(object);
        }

        fn uploaded_objects(&self) -> Vec<LfsObject> {
            self.uploaded
                .lock()
                .expect("fake upload lock should not poison")
                .clone()
        }

        fn fail_upload(&self, object: LfsObject) {
            self.upload_failures
                .lock()
                .expect("fake failure lock should not poison")
                .insert(object);
        }

        fn upload_attempts(&self) -> Vec<LfsObject> {
            self.upload_attempts
                .lock()
                .expect("fake attempt lock should not poison")
                .clone()
        }

        fn delay_uploads_by(&self, delay: Duration) {
            *self
                .upload_delay
                .lock()
                .expect("fake delay lock should not poison") = Some(delay);
        }

        fn max_active_uploads(&self) -> usize {
            self.max_active_uploads.load(Ordering::SeqCst)
        }

        fn override_returned_object(&self, object: LfsObject) {
            *self
                .returned_object_override
                .lock()
                .expect("fake override lock should not poison") = Some(object);
        }

        fn override_returned_provider_id(&self, provider_id: impl Into<String>) {
            *self
                .returned_provider_id_override
                .lock()
                .expect("fake provider override lock should not poison") = Some(provider_id.into());
        }

        fn override_returned_backend_id(&self, backend_id: impl Into<String>) {
            *self
                .returned_backend_id_override
                .lock()
                .expect("fake backend override lock should not poison") = Some(backend_id.into());
        }
    }

    impl StorageProvider for FakeMigrationStorageProvider {
        fn provider_id(&self) -> &str {
            &self.provider_id
        }

        fn object_exists<'a>(
            &'a self,
            object: &'a LfsObject,
        ) -> ProviderFuture<'a, StorageResult<bool>> {
            Box::pin(async move {
                Ok(self
                    .existing
                    .lock()
                    .expect("fake storage lock should not poison")
                    .contains(object))
            })
        }

        fn upload_object<'a>(
            &'a self,
            object: &'a LfsObject,
            source: &'a Path,
        ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
            Box::pin(async move {
                self.upload_attempts
                    .lock()
                    .expect("fake attempt lock should not poison")
                    .push(object.clone());
                let active_uploads = self.active_uploads.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_active_uploads
                    .fetch_max(active_uploads, Ordering::SeqCst);
                let delay = *self
                    .upload_delay
                    .lock()
                    .expect("fake delay lock should not poison");
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                self.active_uploads.fetch_sub(1, Ordering::SeqCst);

                if self
                    .upload_failures
                    .lock()
                    .expect("fake failure lock should not poison")
                    .contains(object)
                {
                    return Err(StorageError::Retryable {
                        provider: self.provider_id.clone(),
                        message: "simulated migration upload failure".to_owned(),
                    });
                }

                let (actual_oid, actual_size) =
                    hash_migration_object_file(source).map_err(|source| {
                        StorageError::IntegrityMismatch {
                            expected_oid: object.oid.as_hex().to_owned(),
                            expected_size: object.size.bytes(),
                            actual_oid: format!("migration-source-error:{source}"),
                            actual_size: 0,
                        }
                    })?;

                if actual_oid != object.oid || actual_size != object.size {
                    return Err(StorageError::IntegrityMismatch {
                        expected_oid: object.oid.as_hex().to_owned(),
                        expected_size: object.size.bytes(),
                        actual_oid: actual_oid.as_hex().to_owned(),
                        actual_size: actual_size.bytes(),
                    });
                }

                self.uploaded
                    .lock()
                    .expect("fake upload lock should not poison")
                    .push(object.clone());
                self.existing
                    .lock()
                    .expect("fake storage lock should not poison")
                    .insert(object.clone());

                let returned_object = self
                    .returned_object_override
                    .lock()
                    .expect("fake override lock should not poison")
                    .clone()
                    .unwrap_or_else(|| object.clone());
                let returned_provider_id = self
                    .returned_provider_id_override
                    .lock()
                    .expect("fake provider override lock should not poison")
                    .clone()
                    .unwrap_or_else(|| self.provider_id.clone());
                let returned_backend_id = self
                    .returned_backend_id_override
                    .lock()
                    .expect("fake backend override lock should not poison")
                    .clone()
                    .unwrap_or_else(|| format!("fake-storage-{}", object.oid));

                Ok(StoredObject::new(
                    returned_provider_id,
                    returned_object,
                    returned_backend_id,
                ))
            })
        }

        fn download_object<'a>(
            &'a self,
            object: &'a LfsObject,
            _destination: &'a Path,
        ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
            Box::pin(async move {
                if self.object_exists(object).await? {
                    Ok(StoredObject::new(
                        self.provider_id.clone(),
                        object.clone(),
                        format!("fake-storage-{}", object.oid),
                    ))
                } else {
                    Err(StorageError::ObjectNotFound {
                        provider: self.provider_id.clone(),
                        oid: object.oid.as_hex().to_owned(),
                        size: object.size.bytes(),
                    })
                }
            })
        }

        fn delete_or_mark_object<'a>(
            &'a self,
            object: &'a LfsObject,
        ) -> ProviderFuture<'a, StorageResult<StorageDeleteOutcome>> {
            Box::pin(async move {
                self.existing
                    .lock()
                    .expect("fake storage lock should not poison")
                    .remove(object);
                Ok(StorageDeleteOutcome::Deleted)
            })
        }
    }

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
    fn enumerates_current_checkout_lfs_pointer_files() {
        let repo = TempRepo::new();
        let pointer_object = test_lfs_object('a', 123);
        let non_lfs_pointer_object = test_lfs_object('c', 789);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n*.txt text\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(pointer_object.clone()).to_pointer_file(),
        );
        repo.write_file("asset/empty.bin", "");
        repo.write_file(
            "docs/pointer-example.txt",
            &LfsPointer::new(non_lfs_pointer_object).to_pointer_file(),
        );
        repo.git([
            "add",
            ".gitattributes",
            "asset/empty.bin",
            "asset/model.bin",
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
    fn current_checkout_pointer_scan_reads_missing_tracked_lfs_files_from_index() {
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
            &LfsPointer::new(missing_object.clone()).to_pointer_file(),
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

        assert_eq!(scan.tracked_path_count, 2);
        assert_eq!(scan.pointers.len(), 2);
        assert!(scan.pointers.iter().any(|pointer| {
            pointer.relative_path == Path::new("asset/present.bin")
                && pointer.object == present_object
        }));
        assert!(scan.pointers.iter().any(|pointer| {
            pointer.relative_path == Path::new("asset/missing.bin")
                && pointer.object == missing_object
        }));
    }

    #[test]
    fn current_checkout_pointer_scan_does_not_lazy_fetch_promisor_blobs() {
        let repo = TempRepo::new();
        let object = test_lfs_object('a', 123);
        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object).to_pointer_file(),
        );
        repo.commit_all("add migration pointer");

        let blob_id = repo.git_stdout(["rev-parse", ":asset/model.bin"]);
        let remote_parent = tempfile::tempdir()
            .expect("temporary promisor remote parent directory should be created");
        let remote_path = remote_parent.path().join("remote.git");
        let clone_output = Command::new("git")
            .args(["clone", "--bare"])
            .arg(repo.path())
            .arg(&remote_path)
            .output()
            .expect("promisor remote clone should start");
        assert!(
            clone_output.status.success(),
            "promisor remote clone failed: {}",
            String::from_utf8_lossy(&clone_output.stderr)
        );

        repo.git([
            "remote",
            "add",
            "origin",
            remote_path
                .to_str()
                .expect("temporary remote path should be UTF-8"),
        ]);
        repo.git(["config", "remote.origin.promisor", "true"]);
        repo.git(["config", "remote.origin.partialclonefilter", "blob:none"]);

        let local_blob_path = repo
            .path()
            .join(".git/objects")
            .join(&blob_id[..2])
            .join(&blob_id[2..]);
        fs::remove_file(&local_blob_path).expect("local pointer blob should be removable");

        let error = enumerate_current_checkout_lfs_pointers(repo.path())
            .expect_err("missing promisor blob should remain unavailable during discovery");

        assert!(
            error.to_string().contains("unavailable locally"),
            "unexpected missing-promisor diagnostic: {error}"
        );
        assert!(
            !local_blob_path.exists(),
            "read-only migration discovery must not lazy-fetch the missing blob"
        );
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
    fn historical_scan_rejects_git_older_than_2_40() {
        let error = validate_historical_scan_git_version("git version 2.39.5")
            .expect_err("Git 2.39 should not support historical attribute sources");

        assert!(matches!(
            &error,
            MigrationError::UnsupportedGitVersion {
                installed,
                required: "2.40.0",
                ..
            } if installed == "2.39.5"
        ));
        assert!(error.to_string().contains("upgrade Git"));
        assert!(error.to_string().contains("current-checkout"));
    }

    #[test]
    fn historical_scan_accepts_supported_git_version_variants() {
        for output in [
            "git version 2.40.0\n",
            "git version 2.52.0 (Apple Git-154)\n",
            "git version 2.40.0.windows.1\n",
            "git version 3.0.0\n",
        ] {
            validate_historical_scan_git_version(output)
                .unwrap_or_else(|error| panic!("{output:?} should be supported: {error}"));
        }
    }

    #[test]
    fn historical_scan_rejects_unrecognized_git_version_output() {
        let error = validate_historical_scan_git_version("vendor git build")
            .expect_err("unrecognized output should not bypass the compatibility preflight");

        assert!(matches!(
            &error,
            MigrationError::ExternalCommandOutput { command, .. }
                if command == "git --version"
        ));
        assert!(error.to_string().contains("Git 2.40.0 or newer"));
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
        repo.write_file("asset/empty.bin", "");
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
                .all(|pointer| pointer.relative_path != Path::new("asset/empty.bin"))
        );
        assert!(
            scan.pointers
                .iter()
                .all(|pointer| pointer.ref_name == "main")
        );
    }

    #[test]
    fn selected_ref_pointer_scan_reuses_unchanged_history_work() {
        let repo = TempRepo::new();
        let object = test_lfs_object('4', 444);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object.clone()).to_pointer_file(),
        );
        for index in 0..128 {
            repo.write_file(
                format!("stable/file-{index:03}.txt"),
                &format!("stable fixture {index}\n"),
            );
        }
        repo.write_file("changing/revision.txt", "revision 0\n");
        repo.commit_all("add representative history fixture");

        for revision in 1..=16 {
            repo.write_file("changing/revision.txt", &format!("revision {revision}\n"));
            repo.commit_all(&format!("update revision {revision}"));
        }

        let (scan, metrics) =
            enumerate_selected_ref_lfs_pointers_with_metrics(repo.path(), ["main"])
                .expect("representative selected-ref scan should succeed");

        assert_eq!(scan.pointers.len(), 17);
        assert!(scan.pointers.iter().all(|pointer| pointer.object == object));
        assert_eq!(metrics.cat_file_processes, 1);
        assert_eq!(metrics.attribute_processes, 1);
        assert!(
            metrics.tree_entries_inspected < 256,
            "unchanged subtrees should be decoded once, got {metrics:?}"
        );
        assert!(
            metrics.blobs_inspected < 160,
            "unchanged blobs should be inspected once, got {metrics:?}"
        );
    }

    #[test]
    fn selected_ref_pointer_scan_rechecks_changed_historical_attributes() {
        let repo = TempRepo::new();
        let object = test_lfs_object('5', 555);

        repo.write_file(".gitattributes", "asset/*.bin -filter\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object.clone()).to_pointer_file(),
        );
        repo.commit_all("add pointer-shaped non-lfs blob");
        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.commit_all("track asset with lfs");
        let lfs_commit = repo.git_stdout(["rev-parse", "HEAD"]);
        repo.write_file(".gitattributes", "asset/*.bin -filter\n");
        repo.commit_all("stop tracking asset with lfs");

        let (scan, metrics) =
            enumerate_selected_ref_lfs_pointers_with_metrics(repo.path(), ["main"])
                .expect("historical attribute changes should be evaluated independently");

        assert_eq!(scan.pointers.len(), 1);
        assert_eq!(scan.pointers[0].commit, lfs_commit);
        assert_eq!(scan.pointers[0].object, object);
        assert_eq!(metrics.attribute_processes, 2);
    }

    #[test]
    fn selected_ref_pointer_scan_rejects_shallow_repository_history() {
        let repo = TempRepo::new();
        let object = test_lfs_object('a', 123);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object).to_pointer_file(),
        );
        repo.commit_all("add pointer at shallow boundary");
        repo.mark_head_as_shallow_boundary();

        let error = enumerate_selected_ref_lfs_pointers(repo.path(), ["main"])
            .expect_err("selected-ref history must reject a shallow repository");

        assert!(matches!(error, MigrationError::ShallowRepository { .. }));
        assert!(error.to_string().contains("git fetch --unshallow"));
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
    fn all_fetched_ref_pointer_scan_rejects_shallow_repository_history() {
        let repo = TempRepo::new();
        let object = test_lfs_object('f', 333);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object).to_pointer_file(),
        );
        repo.commit_all("add pointer at shallow boundary");
        repo.mark_head_as_shallow_boundary();

        let error = enumerate_all_fetched_ref_lfs_pointers(repo.path())
            .expect_err("all-ref history must reject a shallow repository");

        assert!(matches!(error, MigrationError::ShallowRepository { .. }));
        assert!(error.to_string().contains("git fetch --unshallow"));
    }

    #[test]
    fn current_checkout_pointer_scan_accepts_shallow_repository() {
        let repo = TempRepo::new();
        let object = test_lfs_object('c', 321);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object.clone()).to_pointer_file(),
        );
        repo.commit_all("add current pointer at shallow boundary");
        repo.mark_head_as_shallow_boundary();

        let scan = enumerate_current_checkout_lfs_pointers(repo.path())
            .expect("current-checkout inventory does not require repository history");

        assert_eq!(scan.pointers.len(), 1);
        assert_eq!(scan.pointers[0].object, object);
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
    fn all_fetched_ref_pointer_scan_excludes_other_remote_tracking_refs() {
        let repo = TempRepo::new();
        let origin_object = test_lfs_object('5', 555);
        let upstream_object = test_lfs_object('6', 666);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/origin.bin",
            &LfsPointer::new(origin_object.clone()).to_pointer_file(),
        );
        repo.commit_all("add origin pointer");
        repo.git(["update-ref", "refs/remotes/origin/main", "HEAD"]);
        repo.git(["checkout", "-b", "upstream-history"]);
        repo.write_file(
            "asset/upstream.bin",
            &LfsPointer::new(upstream_object.clone()).to_pointer_file(),
        );
        repo.commit_all("add upstream pointer");
        repo.git(["update-ref", "refs/remotes/upstream/main", "HEAD"]);
        repo.git(["checkout", "main"]);
        repo.git(["branch", "-D", "upstream-history"]);

        let scan = enumerate_fetched_ref_lfs_pointers_for_remote(repo.path(), "origin")
            .expect("source-scoped all-ref scan should succeed");
        let ref_names = scan
            .refs
            .iter()
            .map(|scanned_ref| scanned_ref.name.as_str())
            .collect::<BTreeSet<_>>();
        let objects = history_scan_objects(&scan.pointers);

        assert!(ref_names.contains("refs/remotes/origin/main"));
        assert!(!ref_names.contains("refs/remotes/upstream/main"));
        assert!(objects.contains(&origin_object));
        assert!(!objects.contains(&upstream_object));
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
    fn local_object_check_skips_shared_cache_after_verified_git_lfs_media() {
        let repo = TempRepo::new();
        let cache_root = tempfile::tempdir().expect("temporary cache root should be created");
        let layout = LocalCacheLayout::new(cache_root.path());
        let object = test_lfs_object_from_bytes(b"preferred media bytes");
        write_git_lfs_source_object(&repo, &object, b"preferred media bytes");
        write_file(&layout.object_path(&object), b"preferred media bytes");

        let availability = check_local_migration_objects(repo.path(), [&object], Some(&layout))
            .expect("verified media should satisfy local availability");

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
    fn source_fetch_skips_git_lfs_when_all_objects_are_available() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"already available source bytes");
        write_git_lfs_source_object(&repo, &object, b"already available source bytes");
        let mut fetch_attempted = false;

        let report = fetch_missing_migration_objects_with_runner(
            repo.path(),
            [&object],
            None,
            "origin",
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
            "origin",
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
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentalways=false"),
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentrefsdays=0"),
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentremoterefs=false"),
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentcommitsdays=0"),
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
            Some(
                "git -c lfs.fetchrecentalways=false -c lfs.fetchrecentrefsdays=0 -c lfs.fetchrecentremoterefs=false -c lfs.fetchrecentcommitsdays=0 lfs fetch --include= --exclude= origin main"
            )
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
            "origin",
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
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentalways=false"),
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentrefsdays=0"),
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentremoterefs=false"),
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentcommitsdays=0"),
                OsString::from("lfs"),
                OsString::from("fetch"),
                OsString::from("--all"),
                OsString::from("origin"),
            ]
        );
        assert_eq!(
            report.command.as_deref(),
            Some(
                "git -c lfs.fetchrecentalways=false -c lfs.fetchrecentrefsdays=0 -c lfs.fetchrecentremoterefs=false -c lfs.fetchrecentcommitsdays=0 lfs fetch --all origin"
            )
        );
        assert_eq!(report.fetched_objects, vec![object]);
        assert!(report.unavailable_objects.is_empty());
    }

    #[tokio::test]
    async fn upload_migration_objects_skips_existing_and_uploads_verified_sources() {
        let repo = TempRepo::new();
        let already_present = test_lfs_object_from_bytes(b"already stored migration bytes");
        let missing = test_lfs_object_from_bytes(b"new migration upload bytes");
        write_git_lfs_source_object(&repo, &already_present, b"already stored migration bytes");
        write_git_lfs_source_object(&repo, &missing, b"new migration upload bytes");
        let availability =
            check_local_migration_objects(repo.path(), [&already_present, &missing], None)
                .expect("local migration objects should be available");
        let storage = FakeMigrationStorageProvider::new("drive-user-a");
        storage.insert_existing(already_present.clone());

        let report = upload_migration_objects_to_storage(&availability, &storage)
            .await
            .expect("available migration objects should upload");

        assert_eq!(report.storage_provider_id, "drive-user-a");
        assert_eq!(
            report.already_present_objects,
            vec![already_present.clone()]
        );
        assert_eq!(report.uploaded_objects.len(), 1);
        assert_eq!(report.uploaded_objects[0].object, missing);
        assert_eq!(storage.uploaded_objects(), vec![missing]);
        assert!(
            storage
                .object_exists(&already_present)
                .await
                .expect("exists check should succeed")
        );
    }

    #[tokio::test]
    async fn migration_uploads_use_bounded_concurrency() {
        let repo = TempRepo::new();
        let objects = [
            test_lfs_object_from_bytes(b"parallel migration object one"),
            test_lfs_object_from_bytes(b"parallel migration object two"),
            test_lfs_object_from_bytes(b"parallel migration object three"),
        ];
        for (object, bytes) in objects.iter().zip([
            b"parallel migration object one".as_slice(),
            b"parallel migration object two".as_slice(),
            b"parallel migration object three".as_slice(),
        ]) {
            write_git_lfs_source_object(&repo, object, bytes);
        }
        let availability = check_local_migration_objects(repo.path(), &objects, None)
            .expect("migration objects should be available");
        let storage = FakeMigrationStorageProvider::new("drive-user-a");
        storage.delay_uploads_by(Duration::from_millis(50));
        let options = MigrationStorageUploadOptions::new(repo.path().join("checkpoint.jsonl"))
            .with_max_concurrent_uploads(2);

        let report =
            upload_migration_objects_to_storage_with_options(&availability, &storage, &options)
                .await
                .expect("bounded migration uploads should complete");

        assert!(report.failed_objects.is_empty());
        assert_eq!(report.uploaded_objects.len(), 3);
        assert_eq!(storage.max_active_uploads(), 2);
    }

    #[tokio::test]
    async fn migration_uploads_checkpoint_successes_and_retry_failures() {
        let repo = TempRepo::new();
        let completed = test_lfs_object_from_bytes(b"durably completed migration object");
        let failed = test_lfs_object_from_bytes(b"retryable migration object");
        write_git_lfs_source_object(&repo, &completed, b"durably completed migration object");
        write_git_lfs_source_object(&repo, &failed, b"retryable migration object");
        let availability = check_local_migration_objects(repo.path(), [&completed, &failed], None)
            .expect("migration objects should be available");
        let checkpoint_path = repo.path().join("checkpoint.jsonl");
        let options =
            MigrationStorageUploadOptions::new(&checkpoint_path).with_max_concurrent_uploads(2);
        let first_storage = FakeMigrationStorageProvider::new("drive-user-a");
        first_storage.fail_upload(failed.clone());

        let first_report = upload_migration_objects_to_storage_with_options(
            &availability,
            &first_storage,
            &options,
        )
        .await
        .expect("one object failure should still return accumulated outcomes");

        assert_eq!(first_report.uploaded_objects.len(), 1);
        assert_eq!(first_report.failed_objects.len(), 1);
        assert_eq!(first_report.failed_objects[0].object, failed);
        assert!(checkpoint_path.is_file());

        let resumed_storage = FakeMigrationStorageProvider::new("drive-user-a");
        let resumed_report = upload_migration_objects_to_storage_with_options(
            &availability,
            &resumed_storage,
            &options,
        )
        .await
        .expect("a resumed upload should reuse durable completions");

        assert!(resumed_report.failed_objects.is_empty());
        assert_eq!(resumed_storage.upload_attempts(), vec![failed.clone()]);
        assert!(matches!(
            resumed_report.outcomes[0].status,
            MigrationObjectUploadStatus::Uploaded { resumed: true, .. }
        ));
        assert!(matches!(
            resumed_report.outcomes[1].status,
            MigrationObjectUploadStatus::Uploaded { resumed: false, .. }
        ));
    }

    #[tokio::test]
    async fn upload_migration_objects_rechecks_source_bytes_before_upload() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"stable source bytes");
        write_git_lfs_source_object(&repo, &object, b"stable source bytes");
        let availability = check_local_migration_objects(repo.path(), [&object], None)
            .expect("local migration object should be available before mutation");
        write_git_lfs_source_object(&repo, &object, b"corrupt source bytes");
        let storage = FakeMigrationStorageProvider::new("drive-user-a");

        let report = upload_migration_objects_to_storage(&availability, &storage)
            .await
            .expect("per-object failures should return a retry report");

        assert_eq!(report.failed_objects.len(), 1);
        assert!(
            report.failed_objects[0]
                .message
                .as_str()
                .contains("no longer matches")
        );
        assert!(storage.uploaded_objects().is_empty());
    }

    #[tokio::test]
    async fn upload_migration_objects_rejects_returned_object_mismatch() {
        let repo = TempRepo::new();
        let requested = test_lfs_object_from_bytes(b"requested upload bytes");
        let returned = test_lfs_object_from_bytes(b"different returned object");
        write_git_lfs_source_object(&repo, &requested, b"requested upload bytes");
        let availability = check_local_migration_objects(repo.path(), [&requested], None)
            .expect("local migration object should be available");
        let storage = FakeMigrationStorageProvider::new("drive-user-a");
        storage.override_returned_object(returned);

        let report = upload_migration_objects_to_storage(&availability, &storage)
            .await
            .expect("per-object failures should return a retry report");

        assert_eq!(report.failed_objects.len(), 1);
        assert!(
            report.failed_objects[0]
                .message
                .as_str()
                .contains("returned object")
        );
    }

    #[tokio::test]
    async fn upload_migration_objects_rejects_provider_id_mismatch() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"provider mismatch bytes");
        write_git_lfs_source_object(&repo, &object, b"provider mismatch bytes");
        let availability = check_local_migration_objects(repo.path(), [&object], None)
            .expect("local migration object should be available");
        let storage = FakeMigrationStorageProvider::new("drive-user-a");
        storage.override_returned_provider_id("drive-user-b");

        let report = upload_migration_objects_to_storage(&availability, &storage)
            .await
            .expect("per-object failures should return a retry report");

        assert_eq!(report.failed_objects.len(), 1);
        assert!(
            report.failed_objects[0]
                .message
                .as_str()
                .contains("returned provider ID drive-user-b")
        );
    }

    #[tokio::test]
    async fn upload_migration_objects_rejects_empty_backend_id() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"empty backend id bytes");
        write_git_lfs_source_object(&repo, &object, b"empty backend id bytes");
        let availability = check_local_migration_objects(repo.path(), [&object], None)
            .expect("local migration object should be available");
        let storage = FakeMigrationStorageProvider::new("drive-user-a");
        storage.override_returned_backend_id(" ");

        let report = upload_migration_objects_to_storage(&availability, &storage)
            .await
            .expect("per-object failures should return a retry report");

        assert_eq!(report.failed_objects.len(), 1);
        assert!(
            report.failed_objects[0]
                .message
                .as_str()
                .contains("empty backend object ID")
        );
    }

    #[test]
    fn migration_upload_source_prefers_git_lfs_media_over_shared_cache() {
        let temp = tempfile::tempdir().expect("temporary object paths should be created");
        let object = test_lfs_object_from_bytes(b"source preference bytes");
        let shared_cache_path = temp.path().join("shared-cache-object");
        let git_lfs_media_path = temp.path().join("git-lfs-media-object");
        write_file(&shared_cache_path, b"source preference bytes");
        write_file(&git_lfs_media_path, b"source preference bytes");
        let local_object = LocalMigrationObject {
            object,
            locations: vec![
                LocalMigrationObjectLocation {
                    kind: LocalMigrationObjectLocationKind::SharedCache,
                    path: shared_cache_path,
                    status: LocalMigrationObjectLocationStatus::Available,
                },
                LocalMigrationObjectLocation {
                    kind: LocalMigrationObjectLocationKind::GitLfsMedia,
                    path: git_lfs_media_path.clone(),
                    status: LocalMigrationObjectLocationStatus::Available,
                },
            ],
        };

        let selected = verified_migration_upload_source_path(&local_object)
            .expect("available migration source should be selected");

        assert_eq!(selected, git_lfs_media_path);
    }

    #[test]
    fn source_fetch_reports_objects_still_unavailable_after_fetch() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"still missing source bytes");

        let report = fetch_missing_migration_objects_with_runner(
            repo.path(),
            [&object],
            None,
            "origin",
            MigrationFetchMode::CurrentCheckout,
            |_, _| Ok(()),
        )
        .expect("source fetch report should include objects still missing afterward");

        assert_eq!(
            report.command.as_deref(),
            Some(
                "git -c lfs.fetchrecentalways=false -c lfs.fetchrecentrefsdays=0 -c lfs.fetchrecentremoterefs=false -c lfs.fetchrecentcommitsdays=0 lfs fetch --include= --exclude= origin"
            )
        );
        assert!(report.fetched_objects.is_empty());
        assert_eq!(report.unavailable_objects, vec![object]);
    }

    #[test]
    fn source_fetch_commands_match_migration_scope() {
        let current =
            migration_source_fetch_command("upstream", &MigrationFetchMode::CurrentCheckout)
                .expect("current checkout fetch command should be built");
        assert_eq!(
            current.display,
            "git -c lfs.fetchrecentalways=false -c lfs.fetchrecentrefsdays=0 -c lfs.fetchrecentremoterefs=false -c lfs.fetchrecentcommitsdays=0 lfs fetch --include= --exclude= upstream"
        );

        let selected = migration_source_fetch_command(
            "upstream",
            &MigrationFetchMode::selected_refs(["main", "refs/tags/v1"]),
        )
        .expect("selected-ref fetch command should be built");
        assert_eq!(
            selected.display,
            "git -c lfs.fetchrecentalways=false -c lfs.fetchrecentrefsdays=0 -c lfs.fetchrecentremoterefs=false -c lfs.fetchrecentcommitsdays=0 lfs fetch --include= --exclude= upstream main refs/tags/v1"
        );

        let all_refs =
            migration_source_fetch_command("upstream", &MigrationFetchMode::AllFetchedRefs)
                .expect("all-ref fetch command should be built");
        assert_eq!(
            all_refs.display,
            "git -c lfs.fetchrecentalways=false -c lfs.fetchrecentrefsdays=0 -c lfs.fetchrecentremoterefs=false -c lfs.fetchrecentcommitsdays=0 lfs fetch --all upstream"
        );
    }

    #[test]
    fn source_fetch_commands_disable_recent_fetch_configuration() {
        for mode in [
            MigrationFetchMode::CurrentCheckout,
            MigrationFetchMode::selected_refs(["main"]),
            MigrationFetchMode::AllFetchedRefs,
        ] {
            let command = migration_source_fetch_command("origin", &mode)
                .expect("migration source fetch command should be built");

            assert_eq!(
                &command.args[..8],
                [
                    OsString::from("-c"),
                    OsString::from("lfs.fetchrecentalways=false"),
                    OsString::from("-c"),
                    OsString::from("lfs.fetchrecentrefsdays=0"),
                    OsString::from("-c"),
                    OsString::from("lfs.fetchrecentremoterefs=false"),
                    OsString::from("-c"),
                    OsString::from("lfs.fetchrecentcommitsdays=0"),
                ]
            );
        }
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
        let mode = MigrationFetchMode::selected_refs(["main", "refs/tags/v1"]);
        assert_eq!(
            mode.selected_ref_names(),
            Some(&["main".to_owned(), "refs/tags/v1".to_owned()][..])
        );

        assert!(matches!(
            migration_source_fetch_command(
                "origin",
                &MigrationFetchMode::SelectedRefs { refs: Vec::new() }
            ),
            Err(MigrationError::InvalidInput { .. })
        ));
        assert!(matches!(
            migration_source_fetch_command(
                "origin",
                &MigrationFetchMode::selected_refs(["main..feature"])
            ),
            Err(MigrationError::InvalidInput { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn source_fetch_timeout_stops_stderr_holding_descendants() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 60 & wait")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("test shell should start");

        let started_at = Instant::now();
        let error = wait_for_git_lfs_fetch_command(
            &mut child,
            "git lfs fetch test",
            Duration::from_millis(50),
        )
        .expect_err("timed-out command should fail");

        assert!(
            started_at.elapsed() < Duration::from_secs(5),
            "timeout cleanup should not block on descendant stderr handles"
        );
        assert!(matches!(
            error,
            MigrationError::ExternalCommand { status, .. }
                if status == "timed out after 0 seconds"
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
        source.git(["switch", "-c", "recent-extra"]);
        source.write_bytes("asset/recent-extra.bin", b"out-of-scope recent lfs bytes");
        source.commit_all("add recent out-of-scope lfs object");
        source.git(["switch", "main"]);
        let out_of_scope_object = test_lfs_object_from_bytes(b"out-of-scope recent lfs bytes");

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
        for (key, value) in [
            ("lfs.fetchrecentalways", "true"),
            ("lfs.fetchrecentrefsdays", "36500"),
            ("lfs.fetchrecentremoterefs", "true"),
            ("lfs.fetchrecentcommitsdays", "36500"),
        ] {
            let output = Command::new("git")
                .args(["config", "--local", key, value])
                .current_dir(&clone_path)
                .output()
                .expect("hostile recent-fetch configuration should start");
            assert!(
                output.status.success(),
                "hostile recent-fetch configuration failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
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
        assert!(
            !git_lfs_object_path(
                &clone_path.join(".git/lfs/objects"),
                &out_of_scope_object.oid
            )
            .expect("out-of-scope Git LFS object path should be valid")
            .exists(),
            "hostile recent-fetch configuration must not expand migration scope"
        );
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

        fn git_stdout<const N: usize>(&self, args: [&str; N]) -> String {
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

            String::from_utf8(output.stdout)
                .expect("git stdout should be UTF-8")
                .trim()
                .to_owned()
        }

        fn mark_head_as_shallow_boundary(&self) {
            let head = self.git_stdout(["rev-parse", "HEAD"]);
            fs::write(self.root.path().join(".git/shallow"), format!("{head}\n"))
                .expect("shallow boundary should be written");
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
