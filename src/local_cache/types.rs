//! Result and status types produced by local cache operations.

use super::*;

/// Verified local cache object metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedLocalCacheObject {
    /// Git LFS object identity that was verified.
    pub object: LfsObject,
    /// Filesystem path that contains the verified bytes.
    pub path: PathBuf,
}

/// Result of ingesting one object from a repository-local Git LFS cache.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCacheIngest {
    /// Git LFS object identity that was ingested or found in cache.
    pub object: LfsObject,
    /// Source path under `.git/lfs/objects` for this object.
    pub source_path: PathBuf,
    /// Shared local cache path for this object.
    pub cache_path: PathBuf,
    /// Whether ingest copied bytes or reused an existing verified cache object.
    pub status: LocalCacheIngestStatus,
}

/// Status for a single local cache ingest operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCacheIngestStatus {
    /// The object already existed in the shared cache and verified correctly.
    AlreadyCached,
    /// The object was copied from `.git/lfs/objects` into the shared cache.
    Copied,
}

/// Result of registering a worktree in the local cache registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCacheWorktreeRegistrationChange {
    /// Registration that was added, updated, or already present.
    pub registration: LocalCacheWorktreeRegistration,
    /// Whether the registry changed.
    pub status: LocalCacheWorktreeRegistrationStatus,
}

/// Status for a local cache worktree registration operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCacheWorktreeRegistrationStatus {
    /// The worktree was not present and was added.
    Added,
    /// The worktree was present with different metadata and was updated.
    Updated,
    /// The worktree was already registered with identical metadata.
    Unchanged,
}

/// Result of materializing one cache object into a worktree path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCacheMaterialization {
    /// Git LFS object identity that was materialized.
    pub object: LfsObject,
    /// Shared cache object path used as the source of truth.
    pub cache_path: PathBuf,
    /// Worktree path that now contains verified bytes.
    pub destination_path: PathBuf,
    /// Filesystem strategy used for the materialization.
    pub status: LocalCacheMaterializationStatus,
}

/// Filesystem strategy used to materialize a cache object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCacheMaterializationStatus {
    /// Destination already contained the exact verified object bytes.
    AlreadyMaterialized,
    /// Destination was created using the platform's copy-on-write primitive.
    CopyOnWriteCloned,
    /// Destination was created by copying bytes because CoW was unavailable.
    Copied,
}

/// Result of replacing one clean worktree object with a Git LFS pointer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCacheDehydration {
    /// Git LFS object identity that was dehydrated.
    pub object: LfsObject,
    /// Shared cache object path that preserves the full object bytes.
    pub cache_path: PathBuf,
    /// Worktree path that now contains a canonical Git LFS pointer file.
    pub pointer_path: PathBuf,
    /// Whether the cache was reused, populated, or the path was already a pointer.
    pub status: LocalCacheDehydrationStatus,
}

/// Filesystem outcome for dehydrating a worktree object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCacheDehydrationStatus {
    /// The worktree path already contained a matching Git LFS pointer.
    AlreadyDehydrated,
    /// Existing verified cache bytes were reused before writing the pointer.
    ReplacedWithPointer,
    /// Worktree bytes were copied into cache before writing the pointer.
    CachedAndReplacedWithPointer,
}

/// Summary returned after local cache garbage collection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCacheGarbageCollection {
    /// Whether the operation only reported what would be removed.
    pub dry_run: bool,
    /// Number of registered worktrees that still existed and were scanned.
    pub active_worktree_count: usize,
    /// Worktree registrations whose roots could not be scanned.
    ///
    /// A missing path may be a disconnected volume or a transient rename, so
    /// these registrations remain authoritative unless pruning was explicitly
    /// requested.
    pub unavailable_worktrees: Vec<LocalCacheWorktreeRegistration>,
    /// Unavailable worktree registrations pruned, or that a dry run would prune.
    pub pruned_worktrees: Vec<LocalCacheWorktreeRegistration>,
    /// Valid cached object files that were still referenced by a worktree.
    pub retained_objects: Vec<LocalCacheGarbageCollectionObject>,
    /// Cached objects protected because an unavailable worktree may reference them.
    pub protected_objects: Vec<LocalCacheGarbageCollectionObject>,
    /// Valid cached object files that were not referenced by any worktree.
    pub unreferenced_objects: Vec<LocalCacheGarbageCollectionObject>,
    /// Valid cached object files removed from disk during a real run.
    ///
    /// This is empty for dry runs even when [`Self::unreferenced_objects`] lists
    /// objects that would be removed.
    pub deleted_objects: Vec<LocalCacheGarbageCollectionObject>,
    /// Cache paths ignored because they did not match the sharded object layout.
    pub skipped_cache_paths: Vec<PathBuf>,
    /// Tracked LFS worktree paths skipped because they were not regular files.
    ///
    /// These candidates are not followed for reachability, so their target
    /// bytes may appear in [`Self::unreferenced_objects`].
    pub skipped_worktree_pointer_paths: Vec<PathBuf>,
}

/// Cached object file found during local cache garbage collection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCacheGarbageCollectionObject {
    /// SHA-256 object identifier encoded by the cache path.
    pub oid: LfsOid,
    /// Cache object path.
    pub path: PathBuf,
    /// Current filesystem size of the cache object.
    pub size_bytes: u64,
}
