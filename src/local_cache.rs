//! Local content-addressed cache path layout.
//!
//! The local cache is client-side state, separate from the server metadata
//! database and storage-provider object mapping. Paths are derived only from a
//! validated Git LFS SHA-256 object identifier so identical content can be
//! shared across repositories and worktrees before later hydration and garbage
//! collection logic reasons about reachability.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

#[cfg(unix)]
use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
};

use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD as BASE64_STANDARD_NO_PAD};
use fs4::FileExt;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

#[cfg(windows)]
use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
};

use crate::{
    LFS_POINTER_SIZE_CUTOFF, LfsObject, LfsObjectError, LfsObjectSize, LfsOid, LfsPointer,
};

/// Default directory name used below a user's home directory for local state.
pub const DEFAULT_LOCAL_CACHE_HOME_DIR: &str = ".lfscloud";
/// Directory below the local cache root that stores immutable object bytes.
pub const LOCAL_CACHE_OBJECTS_DIR: &str = "objects";
/// JSON registry file below the local cache root that tracks known worktrees.
pub const LOCAL_CACHE_WORKTREES_FILE: &str = "worktrees.json";

const OBJECT_SHARD_WIDTH: usize = 2;
const OBJECT_SHARD_LEVELS: usize = 2;
const OBJECT_SHARD_PREFIX_LENGTH: usize = OBJECT_SHARD_WIDTH * OBJECT_SHARD_LEVELS;
const CACHE_OPERATION_LOCK_FILE: &str = "objects.lock";
const WORKTREE_PATH_LOCKS_DIR: &str = "worktree-path-locks";
const WORKTREE_REGISTRY_LOCK_FILE: &str = "worktrees.json.lock";
const LEGACY_WORKTREE_REGISTRY_VERSION: u32 = 1;
const WORKTREE_REGISTRY_VERSION: u32 = 2;
#[cfg(unix)]
const DEFAULT_MATERIALIZED_FILE_MODE: u32 = 0o600;
#[cfg(not(unix))]
const DEFAULT_MATERIALIZED_FILE_MODE: () = ();

/// Result type for local cache operations.
pub type LocalCacheResult<T> = Result<T, LocalCacheError>;

/// Error returned when local cache ingest or verification fails.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum LocalCacheError {
    /// The shared cache path for an expected object does not exist.
    #[error("local cache object missing: sha256:{oid} ({size} bytes) at {}", path.display())]
    MissingCacheObject {
        /// Expected hex SHA-256 object identifier without the `sha256:` prefix.
        oid: LfsOid,
        /// Expected object size.
        size: LfsObjectSize,
        /// Cache path that was expected to contain the object bytes.
        path: PathBuf,
    },

    /// The source Git LFS cache path for an expected object does not exist.
    #[error("Git LFS source object missing: sha256:{oid} ({size} bytes) at {}", path.display())]
    MissingSourceObject {
        /// Expected hex SHA-256 object identifier without the `sha256:` prefix.
        oid: LfsOid,
        /// Expected object size.
        size: LfsObjectSize,
        /// Git LFS source path that was expected to contain the object bytes.
        path: PathBuf,
    },

    /// A cache or source file did not match the requested Git LFS object.
    #[error(
        "local cache integrity mismatch at {}: expected sha256:{expected_oid} ({expected_size} bytes), got sha256:{actual_oid} ({actual_size} bytes)",
        path.display()
    )]
    IntegrityMismatch {
        /// Path whose bytes were verified.
        path: PathBuf,
        /// Expected hex SHA-256 object identifier without the `sha256:` prefix.
        expected_oid: LfsOid,
        /// Expected object size.
        expected_size: LfsObjectSize,
        /// Actual hex SHA-256 object identifier calculated from the file.
        actual_oid: LfsOid,
        /// Actual file size calculated while hashing.
        actual_size: LfsObjectSize,
    },

    /// A filesystem operation failed while reading or writing local cache state.
    #[error("{context} at {}: {source}", path.display())]
    Io {
        /// Operation being attempted when I/O failed.
        context: &'static str,
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: io::Error,
    },

    /// A worktree registration was missing a stable identity or absolute path.
    #[error("invalid local cache worktree registration for {field}: {message}")]
    InvalidWorktreeRegistration {
        /// Registration field that failed validation.
        field: &'static str,
        /// Human-readable validation message.
        message: String,
    },

    /// The local worktree registry could not be decoded or encoded.
    #[error("{context} at {}: {source}", path.display())]
    WorktreeRegistryJson {
        /// Operation being attempted when JSON handling failed.
        context: &'static str,
        /// Registry path involved in the failed operation.
        path: PathBuf,
        /// Underlying JSON failure.
        #[source]
        source: serde_json::Error,
    },

    /// Git could not enumerate or classify tracked paths for garbage collection.
    #[error(
        "{command} failed for registered worktree {} with status {status}",
        worktree_root.display()
    )]
    GitCommandFailed {
        /// Stable command description that does not include repository data.
        command: &'static str,
        /// Registered worktree whose tracked paths were being inspected.
        worktree_root: PathBuf,
        /// Platform process status reported by Git.
        status: String,
    },

    /// Git returned malformed path or attribute output during garbage collection.
    #[error(
        "{command} returned malformed output for registered worktree {}: {message}",
        worktree_root.display()
    )]
    GitCommandOutput {
        /// Stable command description that does not include repository data.
        command: &'static str,
        /// Registered worktree whose tracked paths were being inspected.
        worktree_root: PathBuf,
        /// Fixed diagnostic describing the malformed output.
        message: &'static str,
    },

    /// The local worktree registry uses an unsupported schema version.
    #[error(
        "unsupported local cache worktree registry version {version} at {}; supported version is {supported_version}",
        path.display()
    )]
    UnsupportedWorktreeRegistryVersion {
        /// Registry file whose version was unsupported.
        path: PathBuf,
        /// Version found in the registry file.
        version: u32,
        /// Latest version this binary can read.
        supported_version: u32,
    },

    /// A worktree path could not be materialized without overwriting content
    /// that was neither the expected cached object nor a matching pointer.
    #[error(
        "refusing to materialize sha256:{oid} ({size} bytes) over non-matching worktree file at {}",
        path.display()
    )]
    MaterializationTargetExists {
        /// Object that the caller attempted to materialize.
        oid: LfsOid,
        /// Expected object size.
        size: LfsObjectSize,
        /// Existing destination path.
        path: PathBuf,
    },

    /// A worktree cache operation was asked to follow a symbolic link.
    #[error("refusing to follow symbolic link at worktree path {}", path.display())]
    WorktreePathSymlink {
        /// Symbolic link that was rejected before reading or replacement.
        path: PathBuf,
    },

    /// A worktree path could not be parsed as a Git LFS pointer.
    #[error("failed to parse Git LFS pointer at {}: {source}", path.display())]
    PointerParse {
        /// Pointer file path.
        path: PathBuf,
        /// Underlying pointer parse failure.
        #[source]
        source: LfsObjectError,
    },

    /// A worktree path was too large to safely parse as a Git LFS pointer.
    #[error(
        "Git LFS pointer at {} is too large to hydrate safely: {size} bytes must be smaller than {size_cutoff} bytes",
        path.display()
    )]
    PointerFileTooLarge {
        /// Pointer file path.
        path: PathBuf,
        /// Actual file size in bytes.
        size: u64,
        /// Exclusive Git LFS pointer size cutoff.
        size_cutoff: u64,
    },

    /// A worktree path was small enough to be a pointer but was not UTF-8 text.
    #[error("Git LFS pointer at {} is not valid UTF-8: {source}", path.display())]
    PointerFileInvalidUtf8 {
        /// Pointer file path.
        path: PathBuf,
        /// Underlying UTF-8 validation failure.
        #[source]
        source: std::str::Utf8Error,
    },

    /// A dehydrated worktree path already contained a pointer for another object.
    #[error(
        "Git LFS pointer at {} points to sha256:{actual_oid} ({actual_size} bytes), expected sha256:{expected_oid} ({expected_size} bytes)",
        path.display()
    )]
    PointerObjectMismatch {
        /// Pointer file path.
        path: PathBuf,
        /// Expected hex SHA-256 object identifier without the `sha256:` prefix.
        expected_oid: LfsOid,
        /// Expected object size.
        expected_size: LfsObjectSize,
        /// Actual pointer hex SHA-256 object identifier without the `sha256:` prefix.
        actual_oid: LfsOid,
        /// Actual pointer object size.
        actual_size: LfsObjectSize,
    },

    /// A failed rollback left displaced worktree content at a recovery path.
    #[error(
        "failed to restore worktree content at {}; displaced bytes remain at {}: {source}",
        path.display(),
        recovery_path.display()
    )]
    WorktreeReplacementRollback {
        /// Worktree path whose replacement could not be rolled back.
        path: PathBuf,
        /// Temporary path retaining the displaced worktree bytes.
        recovery_path: PathBuf,
        /// Underlying atomic-exchange failure.
        #[source]
        source: io::Error,
    },
}

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

/// Repository worktree registered as a consumer of the shared local cache.
///
/// `lfscloud gc` uses this kind of record to know which worktrees must be
/// inspected before deleting cached objects. Paths are required to be absolute
/// so the registry does not depend on a future process's current directory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalCacheWorktreeRegistration {
    /// Stable repository mapping ID or provider-derived repository identity.
    pub repository_id: String,
    /// Absolute worktree root path.
    #[serde(
        serialize_with = "serialize_worktree_registry_path",
        deserialize_with = "deserialize_worktree_registry_path"
    )]
    pub worktree_root: PathBuf,
    /// Absolute Git directory path for the worktree.
    #[serde(
        serialize_with = "serialize_worktree_registry_path",
        deserialize_with = "deserialize_worktree_registry_path"
    )]
    pub git_dir: PathBuf,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SerializedWorktreeRegistryPath {
    Legacy(PathBuf),
    Encoded { encoding: String, value: String },
}

#[derive(Serialize)]
struct EncodedWorktreeRegistryPath {
    encoding: &'static str,
    value: String,
}

fn serialize_worktree_registry_path<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    #[cfg(unix)]
    let encoded = EncodedWorktreeRegistryPath {
        encoding: "unix_bytes_base64",
        value: BASE64_STANDARD_NO_PAD.encode(path.as_os_str().as_bytes()),
    };

    #[cfg(windows)]
    let encoded = {
        let wide_bytes = path
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        EncodedWorktreeRegistryPath {
            encoding: "windows_wide_base64",
            value: BASE64_STANDARD_NO_PAD.encode(wide_bytes),
        }
    };

    #[cfg(not(any(unix, windows)))]
    let encoded = EncodedWorktreeRegistryPath {
        encoding: "utf8",
        value: path
            .to_str()
            .ok_or_else(|| {
                serde::ser::Error::custom(
                    "worktree registry path cannot be represented as UTF-8 on this platform",
                )
            })?
            .to_owned(),
    };

    encoded.serialize(serializer)
}

fn deserialize_worktree_registry_path<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
where
    D: Deserializer<'de>,
{
    match SerializedWorktreeRegistryPath::deserialize(deserializer)? {
        SerializedWorktreeRegistryPath::Legacy(path) => Ok(path),
        SerializedWorktreeRegistryPath::Encoded { encoding, value } => {
            decode_worktree_registry_path::<D::Error>(&encoding, &value)
        }
    }
}

fn decode_worktree_registry_path<E>(encoding: &str, value: &str) -> Result<PathBuf, E>
where
    E: serde::de::Error,
{
    #[cfg(unix)]
    if encoding == "unix_bytes_base64" {
        let bytes = BASE64_STANDARD_NO_PAD
            .decode(value)
            .map_err(|_| E::custom("invalid base64 in Unix worktree registry path"))?;
        return Ok(PathBuf::from(OsString::from_vec(bytes)));
    }

    #[cfg(windows)]
    if encoding == "windows_wide_base64" {
        let bytes = BASE64_STANDARD_NO_PAD
            .decode(value)
            .map_err(|_| E::custom("invalid base64 in Windows worktree registry path"))?;
        let mut chunks = bytes.chunks_exact(2);
        let wide = chunks
            .by_ref()
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        if !chunks.remainder().is_empty() {
            return Err(E::custom(
                "Windows worktree registry path has an incomplete wide unit",
            ));
        }
        return Ok(PathBuf::from(OsString::from_wide(&wide)));
    }

    if encoding == "utf8" {
        return Ok(PathBuf::from(value));
    }

    Err(E::custom(format!(
        "unsupported worktree registry path encoding {encoding:?} on this platform"
    )))
}

impl LocalCacheWorktreeRegistration {
    /// Creates a validated worktree registration.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when the repository identity is blank or
    /// either path is relative. Callers should resolve symlinks or Git-specific
    /// path forms before registration when that distinction matters.
    pub fn new(
        repository_id: impl Into<String>,
        worktree_root: impl Into<PathBuf>,
        git_dir: impl Into<PathBuf>,
    ) -> LocalCacheResult<Self> {
        let registration = Self {
            repository_id: repository_id.into(),
            worktree_root: worktree_root.into(),
            git_dir: git_dir.into(),
        };

        registration.validate()?;

        Ok(registration)
    }

    fn validate(&self) -> LocalCacheResult<()> {
        let trimmed_repository_id = self.repository_id.trim();
        if trimmed_repository_id.is_empty() {
            return Err(LocalCacheError::InvalidWorktreeRegistration {
                field: "repository_id",
                message: "must not be blank".to_owned(),
            });
        }
        if trimmed_repository_id != self.repository_id {
            return Err(LocalCacheError::InvalidWorktreeRegistration {
                field: "repository_id",
                message: "must not be padded".to_owned(),
            });
        }
        validate_absolute_path("worktree_root", &self.worktree_root)?;
        validate_absolute_path("git_dir", &self.git_dir)?;

        Ok(())
    }
}

/// In-memory view of registered local cache worktrees.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalCacheWorktreeRegistry {
    version: u32,
    worktrees: Vec<LocalCacheWorktreeRegistration>,
}

impl LocalCacheWorktreeRegistry {
    /// Creates an empty registry using the current schema version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: WORKTREE_REGISTRY_VERSION,
            worktrees: Vec::new(),
        }
    }

    /// Returns the registered worktrees in stable worktree-path order.
    #[must_use]
    pub fn worktrees(&self) -> &[LocalCacheWorktreeRegistration] {
        &self.worktrees
    }

    /// Returns whether the registry has no worktree registrations.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.worktrees.is_empty()
    }

    fn validate_for_path(&self, path: &Path) -> LocalCacheResult<()> {
        if !(LEGACY_WORKTREE_REGISTRY_VERSION..=WORKTREE_REGISTRY_VERSION).contains(&self.version) {
            return Err(LocalCacheError::UnsupportedWorktreeRegistryVersion {
                path: path.to_path_buf(),
                version: self.version,
                supported_version: WORKTREE_REGISTRY_VERSION,
            });
        }

        let mut worktree_roots = BTreeSet::new();
        for registration in &self.worktrees {
            registration.validate()?;
            let key = normalized_path_key(&registration.worktree_root);
            if !worktree_roots.insert(key.clone()) {
                return Err(LocalCacheError::InvalidWorktreeRegistration {
                    field: "worktree_root",
                    message: format!(
                        "duplicate worktree root in registry: {}",
                        registration.worktree_root.display()
                    ),
                });
            }
        }

        Ok(())
    }

    fn upsert(
        &mut self,
        registration: LocalCacheWorktreeRegistration,
    ) -> LocalCacheWorktreeRegistrationStatus {
        let key = normalized_path_key(&registration.worktree_root);
        if let Some(existing) = self
            .worktrees
            .iter_mut()
            .find(|existing| normalized_path_key(&existing.worktree_root) == key)
        {
            if *existing == registration {
                return LocalCacheWorktreeRegistrationStatus::Unchanged;
            }

            *existing = registration;
            self.sort();
            return LocalCacheWorktreeRegistrationStatus::Updated;
        }

        self.worktrees.push(registration);
        self.sort();
        LocalCacheWorktreeRegistrationStatus::Added
    }

    fn remove(&mut self, worktree_root: &Path) -> Option<LocalCacheWorktreeRegistration> {
        let key = normalized_path_key(worktree_root);
        let index = self
            .worktrees
            .iter()
            .position(|registration| normalized_path_key(&registration.worktree_root) == key)?;

        Some(self.worktrees.remove(index))
    }

    fn sort(&mut self) {
        self.worktrees
            .sort_by_cached_key(|registration| normalized_path_key(&registration.worktree_root));
    }
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

/// Deterministic filesystem layout for local Git LFS object cache paths.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
///
/// use lfscloud::{LocalCacheLayout, LfsOid};
///
/// let layout = LocalCacheLayout::new("/home/alice/.lfscloud");
/// let oid = LfsOid::new(
///     "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
/// )
/// .expect("example OID should be valid");
///
/// assert_eq!(
///     layout.object_path_for_oid(&oid),
///     PathBuf::from(
///         "/home/alice/.lfscloud/objects/01/23/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
///     )
/// );
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCacheLayout {
    root: PathBuf,
}

impl LocalCacheLayout {
    /// Creates a local cache layout rooted at a concrete cache directory.
    ///
    /// `root` is the directory that should contain the `objects` subdirectory,
    /// normally `~/.lfscloud`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Creates the default local cache layout below a home directory.
    ///
    /// This helper is deterministic and does not inspect process environment,
    /// which keeps call sites explicit about which home directory they trust.
    #[must_use]
    pub fn from_home_dir(home_dir: impl AsRef<Path>) -> Self {
        Self::new(home_dir.as_ref().join(DEFAULT_LOCAL_CACHE_HOME_DIR))
    }

    /// Returns the cache root directory, normally `~/.lfscloud`.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the directory that contains sharded cached object files.
    #[must_use]
    pub fn objects_dir(&self) -> PathBuf {
        self.root.join(LOCAL_CACHE_OBJECTS_DIR)
    }

    /// Returns the JSON registry path for worktrees using this local cache.
    #[must_use]
    pub fn worktree_registry_path(&self) -> PathBuf {
        self.root.join(LOCAL_CACHE_WORKTREES_FILE)
    }

    /// Returns the local cache path for a Git LFS object.
    ///
    /// The object's size is intentionally not part of the path. The cache is
    /// addressed by SHA-256 content identity; size checks belong to ingest and
    /// verification logic.
    #[must_use]
    pub fn object_path(&self, object: &LfsObject) -> PathBuf {
        self.object_path_for_oid(&object.oid)
    }

    /// Returns the local cache path for a validated SHA-256 object identifier.
    #[must_use]
    pub fn object_path_for_oid(&self, oid: &LfsOid) -> PathBuf {
        let hex = oid.as_hex();
        let [first_shard, second_shard] = object_shards(hex);

        self.objects_dir()
            .join(first_shard)
            .join(second_shard)
            .join(hex)
    }

    /// Verifies that the shared local cache contains the expected object bytes.
    ///
    /// This checks both SHA-256 and byte size because cache paths intentionally
    /// use only object identity. Callers that know the expected pointer size
    /// should use this before materializing files into a worktree.
    pub fn verify_object(&self, object: &LfsObject) -> LocalCacheResult<VerifiedLocalCacheObject> {
        let path = self.object_path(object);

        ensure_cache_object_file(&path, object)?;
        verify_file_object(&path, object)
    }

    /// Materializes a verified cache object at a worktree destination.
    ///
    /// Existing destination files are accepted only when they already contain
    /// the exact requested object bytes. This keeps the lower-level helper from
    /// overwriting dirty worktree contents; pointer-file replacement is handled
    /// by [`Self::hydrate_pointer_file`], which proves the pointer identity
    /// before replacing it. On Unix, a destination created from scratch starts
    /// owner-readable and owner-writable only. Hydrating an existing pointer
    /// instead preserves that worktree file's mode, including its executable
    /// bit.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when the cache object is missing or corrupt,
    /// the destination contains different bytes, or the destination cannot be
    /// written and verified.
    pub fn materialize_object(
        &self,
        object: &LfsObject,
        destination_path: impl AsRef<Path>,
    ) -> LocalCacheResult<LocalCacheMaterialization> {
        let _operation_lock = self.lock_cache_operation_shared()?;
        let destination_path = destination_path.as_ref();
        let _path_lock = self.lock_worktree_path(destination_path)?;
        let verified = self.verify_object(object)?;

        materialize_verified_object(
            &verified,
            destination_path,
            MaterializationMode::NoReplace,
            || {},
        )
    }

    /// Replaces a Git LFS pointer file with verified cache object bytes.
    ///
    /// The existing worktree file must parse as a Git LFS pointer, and the
    /// pointed object must exist in the shared cache with matching SHA-256 and
    /// byte size. Non-pointer content is treated as a dirty or unsupported
    /// worktree state and is left untouched.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when the pointer cannot be parsed, the cache
    /// object is missing or corrupt, or materialization cannot be completed and
    /// verified.
    pub fn hydrate_pointer_file(
        &self,
        pointer_path: impl AsRef<Path>,
    ) -> LocalCacheResult<LocalCacheMaterialization> {
        self.hydrate_pointer_file_with_before_publish(pointer_path, || {})
    }

    fn hydrate_pointer_file_with_before_publish<F>(
        &self,
        pointer_path: impl AsRef<Path>,
        before_publish: F,
    ) -> LocalCacheResult<LocalCacheMaterialization>
    where
        F: FnOnce(),
    {
        let _operation_lock = self.lock_cache_operation_shared()?;
        let pointer_path = pointer_path.as_ref();
        let _path_lock = self.lock_worktree_path(pointer_path)?;
        let pointer = read_lfs_pointer_file(pointer_path)?;
        if pointer.is_empty() {
            return Ok(LocalCacheMaterialization {
                cache_path: self.object_path(&pointer.object),
                destination_path: pointer_path.to_path_buf(),
                object: pointer.object,
                status: LocalCacheMaterializationStatus::AlreadyMaterialized,
            });
        }
        let verified = self.verify_object(&pointer.object)?;

        materialize_verified_object(
            &verified,
            pointer_path,
            MaterializationMode::ReplaceMatchingPointer,
            before_publish,
        )
    }

    /// Replaces a clean hydrated worktree file with a Git LFS pointer.
    ///
    /// The worktree file must contain the exact expected object bytes, unless
    /// it is already a matching pointer. If the shared cache does not already
    /// contain a verified copy of those bytes, the bytes are copied into cache
    /// before the worktree path is replaced. Dirty or unrelated worktree
    /// content is rejected and left untouched.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when the worktree path is missing, the
    /// hydrated bytes do not match `object`, the cache object is corrupt, or
    /// the pointer cannot be written and verified.
    pub fn dehydrate_file(
        &self,
        object: &LfsObject,
        worktree_path: impl AsRef<Path>,
    ) -> LocalCacheResult<LocalCacheDehydration> {
        self.dehydrate_file_with_before_pointer_publish(object, worktree_path, || {})
    }

    fn dehydrate_file_with_before_pointer_publish<F>(
        &self,
        object: &LfsObject,
        worktree_path: impl AsRef<Path>,
        before_pointer_publish: F,
    ) -> LocalCacheResult<LocalCacheDehydration>
    where
        F: FnOnce(),
    {
        self.dehydrate_file_with_read_observer(object, worktree_path, before_pointer_publish, || {})
    }

    fn dehydrate_file_with_read_observer<F, R>(
        &self,
        object: &LfsObject,
        worktree_path: impl AsRef<Path>,
        before_pointer_publish: F,
        mut before_full_worktree_read: R,
    ) -> LocalCacheResult<LocalCacheDehydration>
    where
        F: FnOnce(),
        R: FnMut(),
    {
        // Hold a shared operation lock through pointer publication. GC takes
        // the exclusive side, so it can never observe the cache object after
        // publication but before the worktree reference becomes visible.
        let _operation_lock = self.lock_cache_operation_shared()?;
        let worktree_path = worktree_path.as_ref();
        let _path_lock = self.lock_worktree_path(worktree_path)?;
        let cache_path = self.object_path(object);

        let existing_pointer = read_existing_lfs_pointer_file(worktree_path)?;
        if let Some(pointer) = existing_pointer.as_ref()
            && pointer.object == *object
        {
            return Ok(LocalCacheDehydration {
                object: object.clone(),
                cache_path,
                pointer_path: worktree_path.to_path_buf(),
                status: LocalCacheDehydrationStatus::AlreadyDehydrated,
            });
        }
        if let Some(pointer) = existing_pointer {
            // A valid pointer can also be the literal contents of another LFS
            // object. This bounded read distinguishes that case while keeping
            // ordinary large hydrated files on the staged-copy fast path.
            before_full_worktree_read();
            if verify_worktree_file_object(worktree_path, object).is_err() {
                return Err(LocalCacheError::PointerObjectMismatch {
                    path: worktree_path.to_path_buf(),
                    expected_oid: object.oid.clone(),
                    expected_size: object.size,
                    actual_oid: pointer.object.oid,
                    actual_size: pointer.object.size,
                });
            }
        }
        let status = if cache_object_path_exists(&cache_path)? {
            self.verify_object(object)?;
            sync_verified_cache_object(&cache_path)?;
            LocalCacheDehydrationStatus::ReplacedWithPointer
        } else {
            // Hash the source while staging its cache copy. Atomic exchange
            // later retains and verifies the exact displaced bytes, so a
            // separate pre-copy and pre-exchange hash would add I/O without
            // strengthening the concurrent-edit guarantee.
            before_full_worktree_read();
            copy_verified_worktree_object_to_cache(worktree_path, &cache_path, object)?;
            LocalCacheDehydrationStatus::CachedAndReplacedWithPointer
        };

        publish_pointer_file(
            worktree_path,
            object,
            before_pointer_publish,
            &mut before_full_worktree_read,
        )?;

        Ok(LocalCacheDehydration {
            object: object.clone(),
            cache_path,
            pointer_path: worktree_path.to_path_buf(),
            status,
        })
    }

    /// Ingests an object from an existing repository `.git/lfs/objects` cache.
    ///
    /// The source object is verified before publication, then copied into the
    /// shared cache using a temporary file in the target shard directory. If a
    /// valid shared-cache object is already present, it is reused without
    /// requiring the repository-local source file to exist.
    pub fn ingest_git_lfs_object(
        &self,
        git_lfs_objects_dir: impl AsRef<Path>,
        object: &LfsObject,
    ) -> LocalCacheResult<LocalCacheIngest> {
        let _operation_lock = self.lock_cache_operation_shared()?;
        let git_lfs_objects_dir = git_lfs_objects_dir.as_ref();
        let source_path = git_lfs_object_path(git_lfs_objects_dir, &object.oid);
        let cache_path = self.object_path(object);

        if cache_object_path_exists(&cache_path)? {
            self.verify_object(object)?;

            return Ok(LocalCacheIngest {
                object: object.clone(),
                source_path,
                cache_path,
                status: LocalCacheIngestStatus::AlreadyCached,
            });
        }

        ensure_source_object_file(&source_path, object)?;
        let status = copy_verified_object_to_cache(&source_path, &cache_path, object)?;

        Ok(LocalCacheIngest {
            object: object.clone(),
            source_path,
            cache_path,
            status,
        })
    }

    /// Loads registered worktrees from the local cache registry.
    ///
    /// A missing registry file is treated as an empty registry because older
    /// cache roots and fresh installs will not have one yet.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when the registry cannot be read, decoded,
    /// or validated against the current schema.
    pub fn load_worktree_registry(&self) -> LocalCacheResult<LocalCacheWorktreeRegistry> {
        let path = self.worktree_registry_path();

        match File::open(&path) {
            Ok(file) => {
                let mut registry: LocalCacheWorktreeRegistry = serde_json::from_reader(file)
                    .map_err(|source| LocalCacheError::WorktreeRegistryJson {
                        context: "failed to decode local cache worktree registry",
                        path: path.clone(),
                        source,
                    })?;
                registry.validate_for_path(&path)?;
                // In-memory registries always use the latest schema so the
                // next mutation upgrades a legacy v1 file atomically.
                registry.version = WORKTREE_REGISTRY_VERSION;

                Ok(registry)
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                Ok(LocalCacheWorktreeRegistry::new())
            }
            Err(source) => Err(LocalCacheError::Io {
                context: "failed to open local cache worktree registry",
                path,
                source,
            }),
        }
    }

    /// Registers or refreshes one worktree as a local cache consumer.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when the registration is invalid or the
    /// registry cannot be read or written.
    pub fn register_worktree(
        &self,
        registration: LocalCacheWorktreeRegistration,
    ) -> LocalCacheResult<LocalCacheWorktreeRegistrationChange> {
        registration.validate()?;

        let _lock = self.lock_worktree_registry()?;
        let mut registry = self.load_worktree_registry()?;
        let status = registry.upsert(registration.clone());

        if status != LocalCacheWorktreeRegistrationStatus::Unchanged {
            self.save_worktree_registry(&registry)?;
        }

        Ok(LocalCacheWorktreeRegistrationChange {
            registration,
            status,
        })
    }

    /// Removes one worktree from the local cache registry.
    ///
    /// This is intended for future explicit cleanup and for pruning worktrees
    /// that no longer exist before local cache garbage collection.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when `worktree_root` is relative or the
    /// registry cannot be read or written.
    pub fn remove_worktree_registration(
        &self,
        worktree_root: impl AsRef<Path>,
    ) -> LocalCacheResult<Option<LocalCacheWorktreeRegistration>> {
        let worktree_root = worktree_root.as_ref();
        validate_absolute_path("worktree_root", worktree_root)?;

        let _lock = self.lock_worktree_registry()?;
        let mut registry = self.load_worktree_registry()?;
        let removed = registry.remove(worktree_root);

        if removed.is_some() {
            self.save_worktree_registry(&registry)?;
        }

        Ok(removed)
    }

    /// Removes cached objects not referenced by any registered worktree.
    ///
    /// Reachability is intentionally conservative and local: the collector asks
    /// Git for NUL-delimited tracked paths in each registered worktree, filters
    /// those paths by the effective `filter=lfs` index attribute, and keeps
    /// every cached OID that a matching worktree pointer references. When any
    /// registered root is unavailable, objects not referenced by the remaining
    /// roots are
    /// protected unless `prune_unavailable_worktrees` is true. Explicit pruning
    /// treats unavailable roots as permanently abandoned. Cache paths that do
    /// not match the expected sharded SHA-256 layout are reported but never
    /// deleted.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when the worktree registry cannot be read or
    /// written, a registered worktree cannot be scanned, or a cache object
    /// cannot be removed.
    pub fn garbage_collect(
        &self,
        dry_run: bool,
        prune_unavailable_worktrees: bool,
    ) -> LocalCacheResult<LocalCacheGarbageCollection> {
        // Mutations and materializations take the shared side of this lock.
        // Taking it exclusively gives GC a stable cache/worktree snapshot and
        // keeps it out of multi-step publication windows.
        let _operation_lock = self.lock_cache_operation_exclusive()?;
        // Keep registry roots stable while reachability is computed and cache
        // objects are deleted; otherwise a concurrent worktree registration
        // could lose cache bytes before its pointers are considered.
        let _lock = self.lock_worktree_registry()?;
        let mut registry = self.load_worktree_registry()?;
        let (active_worktrees, unavailable_worktrees) =
            partition_existing_worktrees(registry.worktrees())?;
        let referenced_oids = referenced_worktree_oids(&active_worktrees)?;
        let (mut cache_objects, mut skipped_cache_paths) = self.cache_object_files()?;
        let mut retained_objects = Vec::new();
        let mut protected_objects = Vec::new();
        let mut unreferenced_objects = Vec::new();
        let mut deleted_objects = Vec::new();
        let pruned_worktrees = if prune_unavailable_worktrees {
            unavailable_worktrees.clone()
        } else {
            Vec::new()
        };

        cache_objects.sort_by(|left, right| left.path.cmp(&right.path));
        skipped_cache_paths.sort();

        if !dry_run && prune_unavailable_worktrees && !pruned_worktrees.is_empty() {
            for registration in &pruned_worktrees {
                registry.remove(&registration.worktree_root);
            }
            self.save_worktree_registry(&registry)?;
        }

        for object in cache_objects {
            if referenced_oids.contains(&object.oid) {
                retained_objects.push(object);
            } else if !unavailable_worktrees.is_empty() && !prune_unavailable_worktrees {
                // An unavailable worktree may contain the only pointer keeping
                // this object reachable, so absence from the scanned roots is
                // not enough evidence for destructive collection.
                protected_objects.push(object);
            } else {
                if !dry_run {
                    self.delete_cache_object(&object)?;
                    deleted_objects.push(object.clone());
                }
                unreferenced_objects.push(object);
            }
        }

        Ok(LocalCacheGarbageCollection {
            dry_run,
            active_worktree_count: active_worktrees.len(),
            unavailable_worktrees,
            pruned_worktrees,
            retained_objects,
            protected_objects,
            unreferenced_objects,
            deleted_objects,
            skipped_cache_paths,
        })
    }

    fn cache_object_files(
        &self,
    ) -> LocalCacheResult<(Vec<LocalCacheGarbageCollectionObject>, Vec<PathBuf>)> {
        collect_cache_object_files(&self.objects_dir())
    }

    fn delete_cache_object(
        &self,
        object: &LocalCacheGarbageCollectionObject,
    ) -> LocalCacheResult<()> {
        fs::remove_file(&object.path).map_err(|source| LocalCacheError::Io {
            context: "failed to remove unreferenced local cache object",
            path: object.path.clone(),
            source,
        })?;
        remove_empty_cache_shard_dirs(&object.path, &self.objects_dir())
    }

    fn worktree_registry_lock_path(&self) -> PathBuf {
        self.root.join(WORKTREE_REGISTRY_LOCK_FILE)
    }

    fn cache_operation_lock_path(&self) -> PathBuf {
        self.root.join(CACHE_OPERATION_LOCK_FILE)
    }

    fn worktree_path_lock_path(&self, worktree_path: &Path) -> PathBuf {
        let normalized = normalized_path_key(worktree_path);
        let digest = format!(
            "{:x}",
            Sha256::digest(normalized.as_os_str().to_string_lossy().as_bytes())
        );

        // Fixed stripes bound persistent coordination state. A collision only
        // serializes unrelated paths; every operation for one path still uses
        // the same cross-process lock.
        self.root
            .join(WORKTREE_PATH_LOCKS_DIR)
            .join(format!("{}.lock", &digest[..2]))
    }

    fn lock_worktree_path(&self, worktree_path: &Path) -> LocalCacheResult<File> {
        let path = self.worktree_path_lock_path(worktree_path);
        let parent = path
            .parent()
            .expect("worktree lock path should have a parent");
        fs::create_dir_all(parent).map_err(|source| LocalCacheError::Io {
            context: "failed to create local cache worktree lock directory",
            path: parent.to_path_buf(),
            source,
        })?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| LocalCacheError::Io {
                context: "failed to open local cache worktree path lock",
                path: path.clone(),
                source,
            })?;
        FileExt::lock(&lock).map_err(|source| LocalCacheError::Io {
            context: "failed to lock local cache worktree path",
            path,
            source,
        })?;

        Ok(lock)
    }

    fn open_cache_operation_lock(&self) -> LocalCacheResult<(File, PathBuf)> {
        fs::create_dir_all(&self.root).map_err(|source| LocalCacheError::Io {
            context: "failed to create local cache root",
            path: self.root.clone(),
            source,
        })?;

        let path = self.cache_operation_lock_path();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| LocalCacheError::Io {
                context: "failed to open local cache operation lock",
                path: path.clone(),
                source,
            })?;

        Ok((lock, path))
    }

    fn lock_cache_operation_shared(&self) -> LocalCacheResult<File> {
        let (lock, path) = self.open_cache_operation_lock()?;
        FileExt::lock_shared(&lock).map_err(|source| LocalCacheError::Io {
            context: "failed to lock local cache operation for shared access",
            path,
            source,
        })?;

        Ok(lock)
    }

    fn lock_cache_operation_exclusive(&self) -> LocalCacheResult<File> {
        let (lock, path) = self.open_cache_operation_lock()?;
        FileExt::lock(&lock).map_err(|source| LocalCacheError::Io {
            context: "failed to lock local cache operation for exclusive access",
            path,
            source,
        })?;

        Ok(lock)
    }

    fn lock_worktree_registry(&self) -> LocalCacheResult<File> {
        fs::create_dir_all(&self.root).map_err(|source| LocalCacheError::Io {
            context: "failed to create local cache root",
            path: self.root.clone(),
            source,
        })?;

        let path = self.worktree_registry_lock_path();
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| LocalCacheError::Io {
                context: "failed to open local cache worktree registry lock",
                path: path.clone(),
                source,
            })?;

        // This blocking lock is deliberately non-reentrant. Registry callers
        // must not acquire it again while holding the returned file handle.
        FileExt::lock(&lock).map_err(|source| LocalCacheError::Io {
            context: "failed to lock local cache worktree registry",
            path,
            source,
        })?;

        Ok(lock)
    }

    fn save_worktree_registry(
        &self,
        registry: &LocalCacheWorktreeRegistry,
    ) -> LocalCacheResult<()> {
        let path = self.worktree_registry_path();
        let parent = path.parent().ok_or_else(|| LocalCacheError::Io {
            context: "failed to resolve local cache worktree registry parent",
            path: path.clone(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "worktree registry path has no parent directory",
            ),
        })?;

        fs::create_dir_all(parent).map_err(|source| LocalCacheError::Io {
            context: "failed to create local cache worktree registry directory",
            path: parent.to_path_buf(),
            source,
        })?;

        let mut temp =
            tempfile::NamedTempFile::new_in(parent).map_err(|source| LocalCacheError::Io {
                context: "failed to create temporary local cache worktree registry",
                path: parent.to_path_buf(),
                source,
            })?;

        serde_json::to_writer_pretty(&mut temp, registry).map_err(|source| {
            LocalCacheError::WorktreeRegistryJson {
                context: "failed to encode local cache worktree registry",
                path: path.clone(),
                source,
            }
        })?;
        temp.write_all(b"\n")
            .map_err(|source| LocalCacheError::Io {
                context: "failed to write local cache worktree registry",
                path: path.clone(),
                source,
            })?;
        temp.flush().map_err(|source| LocalCacheError::Io {
            context: "failed to flush local cache worktree registry",
            path: path.clone(),
            source,
        })?;
        temp.as_file_mut()
            .sync_all()
            .map_err(|source| LocalCacheError::Io {
                context: "failed to sync local cache worktree registry",
                path: path.clone(),
                source,
            })?;

        temp.persist(&path).map_err(|error| LocalCacheError::Io {
            context: "failed to publish local cache worktree registry",
            path: path.clone(),
            source: error.error,
        })?;
        sync_directory(parent).map_err(|source| LocalCacheError::Io {
            context: "failed to sync local cache worktree registry directory",
            path: parent.to_path_buf(),
            source,
        })?;

        Ok(())
    }
}

impl Default for LocalCacheWorktreeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn partition_existing_worktrees(
    registrations: &[LocalCacheWorktreeRegistration],
) -> LocalCacheResult<(
    Vec<LocalCacheWorktreeRegistration>,
    Vec<LocalCacheWorktreeRegistration>,
)> {
    let mut active = Vec::new();
    let mut missing = Vec::new();

    for registration in registrations {
        match fs::metadata(&registration.worktree_root) {
            Ok(metadata) if metadata.is_dir() => active.push(registration.clone()),
            Ok(_) => missing.push(registration.clone()),
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                missing.push(registration.clone());
            }
            Err(source) => {
                return Err(LocalCacheError::Io {
                    context: "failed to inspect registered worktree root",
                    path: registration.worktree_root.clone(),
                    source,
                });
            }
        }
    }

    Ok((active, missing))
}

fn referenced_worktree_oids(
    registrations: &[LocalCacheWorktreeRegistration],
) -> LocalCacheResult<BTreeSet<LfsOid>> {
    let mut referenced = BTreeSet::new();

    for registration in registrations {
        collect_tracked_lfs_pointer_oids(registration, &mut referenced)?;
    }

    Ok(referenced)
}

fn collect_tracked_lfs_pointer_oids(
    registration: &LocalCacheWorktreeRegistration,
    referenced: &mut BTreeSet<LfsOid>,
) -> LocalCacheResult<()> {
    const LS_FILES: &str = "git ls-files -z";
    let tracked_paths = registered_git_command(registration)
        .args(["ls-files", "-z"])
        .output()
        .map_err(|source| LocalCacheError::Io {
            context: "failed to start git ls-files -z for local cache garbage collection",
            path: registration.worktree_root.clone(),
            source,
        })?;
    if !tracked_paths.status.success() {
        return Err(git_command_failed(
            LS_FILES,
            registration,
            tracked_paths.status,
        ));
    }
    if tracked_paths.stdout.is_empty() {
        return Ok(());
    }

    let attributes = check_tracked_path_filter_attributes(registration, tracked_paths.stdout)?;
    let mut fields = attributes.split(|byte| *byte == b'\0').collect::<Vec<_>>();
    if fields.last() == Some(&&[][..]) {
        fields.pop();
    }
    let chunks = fields.chunks_exact(3);
    if !chunks.remainder().is_empty() {
        return Err(git_command_output(
            "git check-attr --cached -z --stdin filter",
            registration,
            "expected path, attribute, and value triples",
        ));
    }

    for chunk in chunks {
        let [relative_path, attribute, value] = chunk else {
            unreachable!("chunks_exact yielded a non-triple chunk");
        };
        if *attribute != b"filter" || *value != b"lfs" {
            continue;
        }

        let relative_path = git_relative_path(relative_path, registration)?;
        collect_pointer_oid_from_file(&registration.worktree_root.join(relative_path), referenced)?;
    }

    Ok(())
}

fn registered_git_command(registration: &LocalCacheWorktreeRegistration) -> Command {
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(&registration.git_dir)
        .arg("--work-tree")
        .arg(&registration.worktree_root)
        .current_dir(&registration.worktree_root);
    command
}

fn check_tracked_path_filter_attributes(
    registration: &LocalCacheWorktreeRegistration,
    tracked_paths: Vec<u8>,
) -> LocalCacheResult<Vec<u8>> {
    const CHECK_ATTR: &str = "git check-attr --cached -z --stdin filter";
    let mut child = registered_git_command(registration)
        .args(["check-attr", "--cached", "-z", "--stdin", "filter"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| LocalCacheError::Io {
            context: "failed to start git check-attr for local cache garbage collection",
            path: registration.worktree_root.clone(),
            source,
        })?;
    let mut stdin = child
        .stdin
        .take()
        .expect("Git attribute stdin should be piped");
    let writer = thread::spawn(move || {
        let result = stdin.write_all(&tracked_paths);
        drop(stdin);
        result
    });
    let output = child
        .wait_with_output()
        .map_err(|source| LocalCacheError::Io {
            context: "failed to wait for git check-attr during local cache garbage collection",
            path: registration.worktree_root.clone(),
            source,
        })?;
    let write_result = writer.join().map_err(|_| LocalCacheError::Io {
        context: "git check-attr input writer panicked during local cache garbage collection",
        path: registration.worktree_root.clone(),
        source: io::Error::other("git check-attr input writer panicked"),
    })?;

    if !output.status.success() {
        return Err(git_command_failed(CHECK_ATTR, registration, output.status));
    }
    write_result.map_err(|source| LocalCacheError::Io {
        context: "failed to write tracked paths to git check-attr",
        path: registration.worktree_root.clone(),
        source,
    })?;

    Ok(output.stdout)
}

fn git_command_failed(
    command: &'static str,
    registration: &LocalCacheWorktreeRegistration,
    status: std::process::ExitStatus,
) -> LocalCacheError {
    LocalCacheError::GitCommandFailed {
        command,
        worktree_root: registration.worktree_root.clone(),
        status: status.to_string(),
    }
}

fn git_command_output(
    command: &'static str,
    registration: &LocalCacheWorktreeRegistration,
    message: &'static str,
) -> LocalCacheError {
    LocalCacheError::GitCommandOutput {
        command,
        worktree_root: registration.worktree_root.clone(),
        message,
    }
}

fn git_relative_path(
    relative_path: &[u8],
    registration: &LocalCacheWorktreeRegistration,
) -> LocalCacheResult<PathBuf> {
    let path = git_path_bytes_to_path_buf(relative_path, registration)?;
    let contained = !path.is_absolute()
        && path.components().next().is_some()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if !contained {
        return Err(git_command_output(
            "git check-attr --cached -z --stdin filter",
            registration,
            "returned a path outside the registered worktree",
        ));
    }

    Ok(path)
}

#[cfg(unix)]
fn git_path_bytes_to_path_buf(
    relative_path: &[u8],
    _registration: &LocalCacheWorktreeRegistration,
) -> LocalCacheResult<PathBuf> {
    Ok(PathBuf::from(OsString::from_vec(relative_path.to_owned())))
}

#[cfg(not(unix))]
fn git_path_bytes_to_path_buf(
    relative_path: &[u8],
    registration: &LocalCacheWorktreeRegistration,
) -> LocalCacheResult<PathBuf> {
    String::from_utf8(relative_path.to_owned())
        .map(PathBuf::from)
        .map_err(|_| {
            git_command_output(
                "git check-attr --cached -z --stdin filter",
                registration,
                "returned a non-UTF-8 path",
            )
        })
}

fn collect_pointer_oid_from_file(
    path: &Path,
    referenced: &mut BTreeSet<LfsOid>,
) -> LocalCacheResult<()> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(LocalCacheError::Io {
                context: "failed to inspect worktree pointer candidate",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if !metadata.is_file() || metadata.len() >= LFS_POINTER_SIZE_CUTOFF {
        return Ok(());
    }

    let file = File::open(path).map_err(|source| LocalCacheError::Io {
        context: "failed to open worktree pointer candidate",
        path: path.to_path_buf(),
        source,
    })?;
    let mut contents = Vec::new();
    file.take(LFS_POINTER_SIZE_CUTOFF)
        .read_to_end(&mut contents)
        .map_err(|source| LocalCacheError::Io {
            context: "failed to read worktree pointer candidate",
            path: path.to_path_buf(),
            source,
        })?;
    if contents.len() as u64 >= LFS_POINTER_SIZE_CUTOFF {
        return Ok(());
    }

    let Ok(contents) = std::str::from_utf8(&contents) else {
        return Ok(());
    };
    if let Ok(pointer) = LfsPointer::parse(contents)
        && !pointer.is_empty()
    {
        referenced.insert(pointer.object.oid);
    }

    Ok(())
}

fn collect_cache_object_files(
    objects_dir: &Path,
) -> LocalCacheResult<(Vec<LocalCacheGarbageCollectionObject>, Vec<PathBuf>)> {
    let mut objects = Vec::new();
    let mut skipped = Vec::new();
    let first_shards = match fs::read_dir(objects_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok((objects, skipped)),
        Err(source) => {
            return Err(LocalCacheError::Io {
                context: "failed to read local cache objects directory",
                path: objects_dir.to_path_buf(),
                source,
            });
        }
    };

    for first_shard in first_shards {
        let first_shard = read_cache_entry(first_shard, objects_dir)?;
        let first_path = first_shard.path();
        if !entry_is_directory(&first_shard, "first-level cache shard")? {
            skipped.push(first_path);
            continue;
        }

        let second_shards = fs::read_dir(&first_path).map_err(|source| LocalCacheError::Io {
            context: "failed to read local cache shard directory",
            path: first_path.clone(),
            source,
        })?;
        for second_shard in second_shards {
            let second_shard = read_cache_entry(second_shard, &first_path)?;
            let second_path = second_shard.path();
            if !entry_is_directory(&second_shard, "second-level cache shard")? {
                skipped.push(second_path);
                continue;
            }

            let object_entries =
                fs::read_dir(&second_path).map_err(|source| LocalCacheError::Io {
                    context: "failed to read local cache shard directory",
                    path: second_path.clone(),
                    source,
                })?;
            for object_entry in object_entries {
                let object_entry = read_cache_entry(object_entry, &second_path)?;
                let path = object_entry.path();
                if !entry_is_file(&object_entry, "cache object path")? {
                    skipped.push(path);
                    continue;
                }

                match cache_object_from_entry(&object_entry) {
                    Some(object) => objects.push(object),
                    None => skipped.push(path),
                }
            }
        }
    }

    Ok((objects, skipped))
}

fn read_cache_entry(
    entry: io::Result<fs::DirEntry>,
    directory: &Path,
) -> LocalCacheResult<fs::DirEntry> {
    entry.map_err(|source| LocalCacheError::Io {
        context: "failed to read local cache directory entry",
        path: directory.to_path_buf(),
        source,
    })
}

fn entry_is_directory(entry: &fs::DirEntry, label: &'static str) -> LocalCacheResult<bool> {
    entry
        .file_type()
        .map(|file_type| file_type.is_dir())
        .map_err(|source| LocalCacheError::Io {
            context: label,
            path: entry.path(),
            source,
        })
}

fn entry_is_file(entry: &fs::DirEntry, label: &'static str) -> LocalCacheResult<bool> {
    entry
        .file_type()
        .map(|file_type| file_type.is_file())
        .map_err(|source| LocalCacheError::Io {
            context: label,
            path: entry.path(),
            source,
        })
}

fn cache_object_from_entry(entry: &fs::DirEntry) -> Option<LocalCacheGarbageCollectionObject> {
    let path = entry.path();
    let oid = LfsOid::new(entry.file_name().to_str()?).ok()?;
    let [first_shard, second_shard] = object_shards(oid.as_hex());
    let second_directory = path.parent()?;
    let first_directory = second_directory.parent()?;

    if first_directory.file_name()?.to_str()? != first_shard
        || second_directory.file_name()?.to_str()? != second_shard
    {
        return None;
    }

    let size_bytes = entry.metadata().ok()?.len();

    Some(LocalCacheGarbageCollectionObject {
        oid,
        path,
        size_bytes,
    })
}

fn remove_empty_cache_shard_dirs(
    cache_object_path: &Path,
    objects_dir: &Path,
) -> LocalCacheResult<()> {
    let Some(second_shard) = cache_object_path.parent() else {
        return Ok(());
    };
    let Some(first_shard) = second_shard.parent() else {
        return Ok(());
    };

    remove_empty_directory(second_shard)?;
    if first_shard != objects_dir {
        remove_empty_directory(first_shard)?;
    }

    Ok(())
}

fn remove_empty_directory(path: &Path) -> LocalCacheResult<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(source)
            if matches!(
                source.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(source) => Err(LocalCacheError::Io {
            context: "failed to remove empty local cache shard directory",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn validate_absolute_path(field: &'static str, path: &Path) -> LocalCacheResult<()> {
    if !path.is_absolute() {
        return Err(LocalCacheError::InvalidWorktreeRegistration {
            field,
            message: format!("path must be absolute: {}", path.display()),
        });
    }

    Ok(())
}

fn normalized_path_key(path: &Path) -> PathBuf {
    // Existing paths compare by canonical identity, while missing paths remain
    // lexical because there is no stable filesystem identity to resolve yet.
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MaterializationMode {
    NoReplace,
    ReplaceMatchingPointer,
}

fn read_lfs_pointer_file(path: &Path) -> LocalCacheResult<LfsPointer> {
    let file =
        open_worktree_file_without_following_symlinks(path, "failed to open Git LFS pointer file")?;
    let metadata = file.metadata().map_err(|source| LocalCacheError::Io {
        context: "failed to inspect Git LFS pointer file",
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() >= LFS_POINTER_SIZE_CUTOFF {
        return Err(LocalCacheError::PointerFileTooLarge {
            path: path.to_path_buf(),
            size: metadata.len(),
            size_cutoff: LFS_POINTER_SIZE_CUTOFF,
        });
    }

    let mut contents = Vec::new();
    file.take(LFS_POINTER_SIZE_CUTOFF)
        .read_to_end(&mut contents)
        .map_err(|source| LocalCacheError::Io {
            context: "failed to read Git LFS pointer file",
            path: path.to_path_buf(),
            source,
        })?;
    let size = u64::try_from(contents.len()).unwrap_or(u64::MAX);
    if size >= LFS_POINTER_SIZE_CUTOFF {
        return Err(LocalCacheError::PointerFileTooLarge {
            path: path.to_path_buf(),
            size,
            size_cutoff: LFS_POINTER_SIZE_CUTOFF,
        });
    }

    let contents = std::str::from_utf8(&contents).map_err(|source| {
        LocalCacheError::PointerFileInvalidUtf8 {
            path: path.to_path_buf(),
            source,
        }
    })?;

    LfsPointer::parse(contents).map_err(|source| LocalCacheError::PointerParse {
        path: path.to_path_buf(),
        source,
    })
}

fn read_existing_lfs_pointer_file(path: &Path) -> LocalCacheResult<Option<LfsPointer>> {
    let file =
        open_worktree_file_without_following_symlinks(path, "failed to open dehydration target")?;
    let metadata = file.metadata().map_err(|source| LocalCacheError::Io {
        context: "failed to inspect dehydration target",
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() >= LFS_POINTER_SIZE_CUTOFF {
        return Ok(None);
    }

    let mut contents = Vec::new();
    file.take(LFS_POINTER_SIZE_CUTOFF)
        .read_to_end(&mut contents)
        .map_err(|source| LocalCacheError::Io {
            context: "failed to read dehydration target",
            path: path.to_path_buf(),
            source,
        })?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) >= LFS_POINTER_SIZE_CUTOFF {
        return Ok(None);
    }

    let Ok(contents) = std::str::from_utf8(&contents) else {
        return Ok(None);
    };

    Ok(LfsPointer::parse(contents).ok())
}

fn publish_pointer_file<F, R>(
    path: &Path,
    object: &LfsObject,
    before_publish: F,
    before_displaced_verification: R,
) -> LocalCacheResult<()>
where
    F: FnOnce(),
    R: FnOnce(),
{
    let parent = path.parent().ok_or_else(|| LocalCacheError::Io {
        context: "failed to resolve dehydration target parent",
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidInput,
            "dehydration target path has no parent directory",
        ),
    })?;
    let mode = existing_file_mode(path)?;
    let mut temp =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| LocalCacheError::Io {
            context: "failed to create temporary Git LFS pointer",
            path: parent.to_path_buf(),
            source,
        })?;
    let pointer = LfsPointer::new(object.clone()).to_pointer_file();

    temp.write_all(pointer.as_bytes())
        .map_err(|source| LocalCacheError::Io {
            context: "failed to write temporary Git LFS pointer",
            path: path.to_path_buf(),
            source,
        })?;
    temp.flush().map_err(|source| LocalCacheError::Io {
        context: "failed to flush temporary Git LFS pointer",
        path: path.to_path_buf(),
        source,
    })?;
    set_temporary_file_mode(temp.path(), path, mode)?;

    before_publish();
    replace_retaining_displaced(temp, path, |displaced_path| {
        before_displaced_verification();
        remap_integrity_path(verify_worktree_file_object(displaced_path, object), path).map(|_| ())
    })?;

    let pointer = read_lfs_pointer_file(path)?;
    if pointer.object != *object {
        return Err(LocalCacheError::PointerParse {
            path: path.to_path_buf(),
            source: LfsObjectError::PointerUnexpectedLine {
                line: "dehydrated pointer did not round-trip to the expected object".to_owned(),
            },
        });
    }

    Ok(())
}

fn materialize_verified_object(
    verified: &VerifiedLocalCacheObject,
    destination_path: &Path,
    mode: MaterializationMode,
    before_publish: impl FnOnce(),
) -> LocalCacheResult<LocalCacheMaterialization> {
    match existing_destination_status(destination_path, &verified.object)? {
        ExistingDestinationStatus::Missing => {}
        ExistingDestinationStatus::AlreadyMaterialized => {
            return Ok(LocalCacheMaterialization {
                object: verified.object.clone(),
                cache_path: verified.path.clone(),
                destination_path: destination_path.to_path_buf(),
                status: LocalCacheMaterializationStatus::AlreadyMaterialized,
            });
        }
        ExistingDestinationStatus::Different => match mode {
            MaterializationMode::NoReplace => {
                return Err(LocalCacheError::MaterializationTargetExists {
                    oid: verified.object.oid.clone(),
                    size: verified.object.size,
                    path: destination_path.to_path_buf(),
                });
            }
            MaterializationMode::ReplaceMatchingPointer => {
                let pointer = read_lfs_pointer_file(destination_path)?;
                if pointer.object != verified.object {
                    return Err(LocalCacheError::MaterializationTargetExists {
                        oid: verified.object.oid.clone(),
                        size: verified.object.size,
                        path: destination_path.to_path_buf(),
                    });
                }
            }
        },
    }

    let destination_parent = destination_path
        .parent()
        .ok_or_else(|| LocalCacheError::Io {
            context: "failed to resolve materialization destination parent",
            path: destination_path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "destination path has no parent directory",
            ),
        })?;
    fs::create_dir_all(destination_parent).map_err(|source| LocalCacheError::Io {
        context: "failed to create materialization destination directory",
        path: destination_parent.to_path_buf(),
        source,
    })?;

    let (temp, status) =
        materialize_to_temporary_file(&verified.path, destination_parent, &verified.object)?;

    publish_materialized_file(
        temp,
        destination_path,
        mode,
        &verified.object,
        before_publish,
    )?;
    // The final verification proves the path currently contains the expected
    // object. If an uncoordinated writer races this local worktree path, the
    // caller may still receive an integrity error after publication.
    let materialized = verify_worktree_file_object(destination_path, &verified.object)?;

    Ok(LocalCacheMaterialization {
        object: materialized.object,
        cache_path: verified.path.clone(),
        destination_path: materialized.path,
        status,
    })
}

enum ExistingDestinationStatus {
    Missing,
    AlreadyMaterialized,
    Different,
}

fn existing_destination_status(
    path: &Path,
    object: &LfsObject,
) -> LocalCacheResult<ExistingDestinationStatus> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(LocalCacheError::WorktreePathSymlink {
                path: path.to_path_buf(),
            })
        }
        Ok(metadata) if metadata.is_file() => {
            if metadata.len() != object.size.bytes() {
                return Ok(ExistingDestinationStatus::Different);
            }

            match verify_worktree_file_object(path, object) {
                Ok(_) => Ok(ExistingDestinationStatus::AlreadyMaterialized),
                Err(LocalCacheError::IntegrityMismatch { .. }) => {
                    Ok(ExistingDestinationStatus::Different)
                }
                Err(error) => Err(error),
            }
        }
        Ok(_) => Err(LocalCacheError::Io {
            context: "materialization destination is not a file",
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidData, "expected a regular file"),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            Ok(ExistingDestinationStatus::Missing)
        }
        Err(source) => Err(LocalCacheError::Io {
            context: "failed to inspect materialization destination",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn materialize_to_temporary_file(
    cache_path: &Path,
    destination_parent: &Path,
    object: &LfsObject,
) -> LocalCacheResult<(tempfile::NamedTempFile, LocalCacheMaterializationStatus)> {
    materialize_to_temporary_file_with_clone(
        cache_path,
        destination_parent,
        object,
        copy_on_write_clone,
    )
}

fn materialize_to_temporary_file_with_clone(
    cache_path: &Path,
    destination_parent: &Path,
    object: &LfsObject,
    try_clone: impl FnOnce(&Path, &Path) -> LocalCacheResult<Option<tempfile::NamedTempFile>>,
) -> LocalCacheResult<(tempfile::NamedTempFile, LocalCacheMaterializationStatus)> {
    if let Some(temp) = try_clone(cache_path, destination_parent)? {
        verify_file_object(temp.path(), object)?;
        return Ok((temp, LocalCacheMaterializationStatus::CopyOnWriteCloned));
    }

    let mut temp = tempfile::NamedTempFile::new_in(destination_parent).map_err(|source| {
        LocalCacheError::Io {
            context: "failed to create temporary materialized object",
            path: destination_parent.to_path_buf(),
            source,
        }
    })?;
    copy_cache_object_to_temporary_file(cache_path, &mut temp, object)?;

    Ok((temp, LocalCacheMaterializationStatus::Copied))
}

fn copy_cache_object_to_temporary_file(
    cache_path: &Path,
    temp: &mut tempfile::NamedTempFile,
    expected: &LfsObject,
) -> LocalCacheResult<()> {
    let mut source = File::open(cache_path).map_err(|source| LocalCacheError::Io {
        context: "failed to open verified cache object",
        path: cache_path.to_path_buf(),
        source,
    })?;
    let temp_path = temp.path().to_path_buf();
    let destination = temp.as_file_mut();
    destination
        .set_len(0)
        .and_then(|()| destination.seek(SeekFrom::Start(0)).map(|_| ()))
        .map_err(|source| LocalCacheError::Io {
            context: "failed to open temporary materialized object",
            path: temp_path.clone(),
            source,
        })?;
    let mut hasher = Sha256::new();
    let mut total_size = 0u64;
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|source| LocalCacheError::Io {
                context: "failed to read verified cache object",
                path: cache_path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }

        destination
            .write_all(&buffer[..read])
            .map_err(|source| LocalCacheError::Io {
                context: "failed to write temporary materialized object",
                path: temp_path.clone(),
                source,
            })?;
        hasher.update(&buffer[..read]);
        total_size = total_size
            .checked_add(read as u64)
            .ok_or_else(|| LocalCacheError::Io {
                context: "object is too large to measure",
                path: cache_path.to_path_buf(),
                source: io::Error::new(io::ErrorKind::InvalidData, "object size overflow"),
            })?;
    }
    destination.flush().map_err(|source| LocalCacheError::Io {
        context: "failed to flush temporary materialized object",
        path: temp_path,
        source,
    })?;

    let actual_oid =
        LfsOid::new(format!("{:x}", hasher.finalize())).expect("SHA-256 hex should be valid");
    let actual_size = LfsObjectSize::new(total_size);
    if actual_oid != expected.oid || actual_size != expected.size {
        return Err(LocalCacheError::IntegrityMismatch {
            path: cache_path.to_path_buf(),
            expected_oid: expected.oid.clone(),
            expected_size: expected.size,
            actual_oid,
            actual_size,
        });
    }

    Ok(())
}

fn publish_materialized_file(
    temp: tempfile::NamedTempFile,
    destination_path: &Path,
    mode: MaterializationMode,
    object: &LfsObject,
    before_publish: impl FnOnce(),
) -> LocalCacheResult<()> {
    match mode {
        MaterializationMode::NoReplace => {
            before_publish();
            set_temporary_file_mode(
                temp.path(),
                destination_path,
                DEFAULT_MATERIALIZED_FILE_MODE,
            )?;
            match temp.persist_noclobber(destination_path) {
                Ok(_) => Ok(()),
                Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                    match verify_worktree_file_object(destination_path, object) {
                        Ok(_) => Ok(()),
                        Err(LocalCacheError::IntegrityMismatch { .. }) => {
                            Err(LocalCacheError::MaterializationTargetExists {
                                oid: object.oid.clone(),
                                size: object.size,
                                path: destination_path.to_path_buf(),
                            })
                        }
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(LocalCacheError::Io {
                    context: "failed to publish materialized object",
                    path: destination_path.to_path_buf(),
                    source: error.error,
                }),
            }
        }
        MaterializationMode::ReplaceMatchingPointer => {
            let pointer = read_lfs_pointer_file(destination_path)?;
            if pointer.object != *object {
                return Err(LocalCacheError::MaterializationTargetExists {
                    oid: object.oid.clone(),
                    size: object.size,
                    path: destination_path.to_path_buf(),
                });
            }
            let replacement_mode = existing_file_mode(destination_path)?;
            set_temporary_file_mode(temp.path(), destination_path, replacement_mode)?;
            before_publish();
            replace_retaining_displaced(temp, destination_path, |displaced_path| {
                let pointer = read_lfs_pointer_file(displaced_path).map_err(|_| {
                    LocalCacheError::MaterializationTargetExists {
                        oid: object.oid.clone(),
                        size: object.size,
                        path: destination_path.to_path_buf(),
                    }
                })?;
                if pointer.object != *object {
                    return Err(LocalCacheError::MaterializationTargetExists {
                        oid: object.oid.clone(),
                        size: object.size,
                        path: destination_path.to_path_buf(),
                    });
                }

                Ok(())
            })
        }
    }
}

fn remap_integrity_path<T>(result: LocalCacheResult<T>, public_path: &Path) -> LocalCacheResult<T> {
    result.map_err(|error| match error {
        LocalCacheError::IntegrityMismatch {
            expected_oid,
            expected_size,
            actual_oid,
            actual_size,
            ..
        } => LocalCacheError::IntegrityMismatch {
            path: public_path.to_path_buf(),
            expected_oid,
            expected_size,
            actual_oid,
            actual_size,
        },
        error => error,
    })
}

fn replace_retaining_displaced<F>(
    temp: tempfile::NamedTempFile,
    destination_path: &Path,
    verify_displaced: F,
) -> LocalCacheResult<()>
where
    F: FnOnce(&Path) -> LocalCacheResult<()>,
{
    #[cfg(any(target_os = "android", target_os = "linux", target_vendor = "apple"))]
    {
        let mut temp = temp;
        exchange_paths(temp.path(), destination_path).map_err(|source| LocalCacheError::Io {
            context: "failed to atomically exchange worktree content",
            path: destination_path.to_path_buf(),
            source,
        })?;

        match verify_displaced(temp.path()) {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Err(source) = exchange_paths(temp.path(), destination_path) {
                    let recovery_path = temp.path().to_path_buf();
                    temp.disable_cleanup(true);
                    return Err(LocalCacheError::WorktreeReplacementRollback {
                        path: destination_path.to_path_buf(),
                        recovery_path,
                        source,
                    });
                }

                Err(error)
            }
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "linux", target_vendor = "apple")))]
    {
        // Platforms without exchange rename retain the final identity check and
        // path lock. The replacement remains atomic, but cannot preserve a
        // displaced file for post-rename verification.
        verify_displaced(destination_path)?;
        temp.persist(destination_path)
            .map(|_| ())
            .map_err(|error| LocalCacheError::Io {
                context: "failed to publish worktree content",
                path: destination_path.to_path_buf(),
                source: error.error,
            })
    }
}

#[cfg(any(target_os = "android", target_os = "linux", target_vendor = "apple"))]
fn exchange_paths(left: &Path, right: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        left,
        rustix::fs::CWD,
        right,
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .map_err(io::Error::from)
}

#[cfg(unix)]
fn existing_file_mode(path: &Path) -> LocalCacheResult<u32> {
    use std::os::unix::fs::PermissionsExt;

    worktree_file_metadata_without_following_symlinks(path)
        .map(|metadata| metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn existing_file_mode(_path: &Path) -> LocalCacheResult<u32> {
    Ok(0)
}

#[cfg(unix)]
fn set_temporary_file_mode(
    temp_path: &Path,
    destination_path: &Path,
    mode: u32,
) -> LocalCacheResult<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(temp_path, fs::Permissions::from_mode(mode)).map_err(|source| {
        LocalCacheError::Io {
            context: "failed to set temporary file permissions",
            path: destination_path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_temporary_file_mode(
    _temp_path: &Path,
    _destination_path: &Path,
    _mode: u32,
) -> LocalCacheResult<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn copy_on_write_clone(
    source_path: &Path,
    destination_parent: &Path,
) -> LocalCacheResult<Option<tempfile::NamedTempFile>> {
    let source = File::open(source_path).map_err(|source| LocalCacheError::Io {
        context: "failed to open cache object for copy-on-write cloning",
        path: source_path.to_path_buf(),
        source,
    })?;
    let destination_directory =
        tempfile::tempdir_in(destination_parent).map_err(|source| LocalCacheError::Io {
            context: "failed to create temporary clone directory",
            path: destination_parent.to_path_buf(),
            source,
        })?;
    let candidate = tempfile::NamedTempFile::new_in(destination_parent).map_err(|source| {
        LocalCacheError::Io {
            context: "failed to reserve temporary clone path",
            path: destination_parent.to_path_buf(),
            source,
        }
    })?;
    let destination_path = candidate.path().to_path_buf();
    let clone_name = "materialized-object";
    let clone_path = destination_directory.path().join(clone_name);
    let clone_directory =
        File::open(destination_directory.path()).map_err(|source| LocalCacheError::Io {
            context: "failed to open temporary clone directory",
            path: destination_directory.path().to_path_buf(),
            source,
        })?;

    if let Err(source) = rustix::fs::fclonefileat(
        &source,
        &clone_directory,
        clone_name,
        rustix::fs::CloneFlags::NOFOLLOW,
    ) {
        tracing::debug!(
            source = %source_path.display(),
            destination = %destination_path.display(),
            error = %source,
            "macOS copy-on-write clone failed; falling back to verified copy"
        );
        return Ok(None);
    }

    let (candidate_file, temporary_path) = candidate.into_parts();
    drop(candidate_file);
    fs::rename(&clone_path, &destination_path).map_err(|source| LocalCacheError::Io {
        context: "failed to move temporary cloned object into place",
        path: destination_path.clone(),
        source,
    })?;
    let cloned_file = rustix::fs::openat(
        rustix::fs::CWD,
        &destination_path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(io::Error::from)
    .map_err(|source| LocalCacheError::Io {
        context: "failed to open temporary cloned object",
        path: destination_path,
        source,
    })?;

    Ok(Some(tempfile::NamedTempFile::from_parts(
        cloned_file,
        temporary_path,
    )))
}

#[cfg(not(target_os = "macos"))]
fn copy_on_write_clone(
    _source_path: &Path,
    _destination_parent: &Path,
) -> LocalCacheResult<Option<tempfile::NamedTempFile>> {
    Ok(None)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn sync_verified_cache_object(cache_path: &Path) -> LocalCacheResult<()> {
    let cache_parent = cache_path.parent().ok_or_else(|| LocalCacheError::Io {
        context: "failed to resolve cache object parent",
        path: cache_path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache object path has no parent directory",
        ),
    })?;
    let file = File::open(cache_path).map_err(|source| LocalCacheError::Io {
        context: "failed to open verified cache object for sync",
        path: cache_path.to_path_buf(),
        source,
    })?;
    file.sync_all().map_err(|source| LocalCacheError::Io {
        context: "failed to sync verified cache object",
        path: cache_path.to_path_buf(),
        source,
    })?;
    sync_directory(cache_parent).map_err(|source| LocalCacheError::Io {
        context: "failed to sync cache object directory",
        path: cache_parent.to_path_buf(),
        source,
    })
}

fn object_shards(hex: &str) -> [&str; OBJECT_SHARD_LEVELS] {
    debug_assert!(
        hex.len() >= OBJECT_SHARD_PREFIX_LENGTH,
        "validated SHA-256 OID should be long enough for cache sharding"
    );

    [
        hex.get(..OBJECT_SHARD_WIDTH)
            .expect("validated SHA-256 OID should contain the first cache shard"),
        hex.get(OBJECT_SHARD_WIDTH..OBJECT_SHARD_PREFIX_LENGTH)
            .expect("validated SHA-256 OID should contain the second cache shard"),
    ]
}

fn git_lfs_object_path(git_lfs_objects_dir: &Path, oid: &LfsOid) -> PathBuf {
    let hex = oid.as_hex();
    let [first_shard, second_shard] = object_shards(hex);

    git_lfs_objects_dir
        .join(first_shard)
        .join(second_shard)
        .join(hex)
}

fn cache_object_path_exists(path: &Path) -> LocalCacheResult<bool> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(LocalCacheError::Io {
            context: "cache object path is not a file",
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidData, "expected a regular file"),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(LocalCacheError::Io {
            context: "failed to inspect cache object",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn ensure_cache_object_file(path: &Path, object: &LfsObject) -> LocalCacheResult<()> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(LocalCacheError::Io {
            context: "cache object path is not a file",
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidData, "expected a regular file"),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            Err(LocalCacheError::MissingCacheObject {
                oid: object.oid.clone(),
                size: object.size,
                path: path.to_path_buf(),
            })
        }
        Err(source) => Err(LocalCacheError::Io {
            context: "failed to inspect cache object",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn ensure_source_object_file(path: &Path, object: &LfsObject) -> LocalCacheResult<()> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(LocalCacheError::Io {
            context: "Git LFS source object path is not a file",
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidData, "expected a regular file"),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            Err(LocalCacheError::MissingSourceObject {
                oid: object.oid.clone(),
                size: object.size,
                path: path.to_path_buf(),
            })
        }
        Err(source) => Err(LocalCacheError::Io {
            context: "failed to inspect Git LFS source object",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn verify_file_object(
    path: &Path,
    expected: &LfsObject,
) -> LocalCacheResult<VerifiedLocalCacheObject> {
    let (actual_oid, actual_size) = hash_file(path)?;

    if actual_oid != expected.oid || actual_size != expected.size {
        return Err(LocalCacheError::IntegrityMismatch {
            path: path.to_path_buf(),
            expected_oid: expected.oid.clone(),
            expected_size: expected.size,
            actual_oid,
            actual_size,
        });
    }

    Ok(VerifiedLocalCacheObject {
        object: expected.clone(),
        path: path.to_path_buf(),
    })
}

fn verify_worktree_file_object(
    path: &Path,
    expected: &LfsObject,
) -> LocalCacheResult<VerifiedLocalCacheObject> {
    let file = open_worktree_file_without_following_symlinks(
        path,
        "failed to open worktree object for hashing",
    )?;
    let (actual_oid, actual_size) = hash_open_file(file, path)?;

    if actual_oid != expected.oid || actual_size != expected.size {
        return Err(LocalCacheError::IntegrityMismatch {
            path: path.to_path_buf(),
            expected_oid: expected.oid.clone(),
            expected_size: expected.size,
            actual_oid,
            actual_size,
        });
    }

    Ok(VerifiedLocalCacheObject {
        object: expected.clone(),
        path: path.to_path_buf(),
    })
}

fn copy_verified_object_to_cache(
    source_path: &Path,
    cache_path: &Path,
    object: &LfsObject,
) -> LocalCacheResult<LocalCacheIngestStatus> {
    let source = File::open(source_path).map_err(|source| LocalCacheError::Io {
        context: "failed to open Git LFS source object",
        path: source_path.to_path_buf(),
        source,
    })?;
    copy_verified_file_to_cache(
        source,
        source_path,
        cache_path,
        object,
        "failed to read Git LFS source object",
        CachePublishDurability::Recoverable,
    )
}

fn copy_verified_worktree_object_to_cache(
    source_path: &Path,
    cache_path: &Path,
    object: &LfsObject,
) -> LocalCacheResult<()> {
    let source = open_worktree_file_without_following_symlinks(
        source_path,
        "failed to open hydrated worktree object",
    )?;
    copy_verified_file_to_cache(
        source,
        source_path,
        cache_path,
        object,
        "failed to read hydrated worktree object",
        CachePublishDurability::Durable,
    )
    .map(|_| ())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CachePublishDurability {
    Recoverable,
    Durable,
}

fn copy_verified_file_to_cache(
    mut source: File,
    source_path: &Path,
    cache_path: &Path,
    object: &LfsObject,
    read_context: &'static str,
    durability: CachePublishDurability,
) -> LocalCacheResult<LocalCacheIngestStatus> {
    let cache_parent = cache_path.parent().ok_or_else(|| LocalCacheError::Io {
        context: "failed to resolve cache object parent",
        path: cache_path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache object path has no parent directory",
        ),
    })?;
    fs::create_dir_all(cache_parent).map_err(|source| LocalCacheError::Io {
        context: "failed to create cache object directory",
        path: cache_parent.to_path_buf(),
        source,
    })?;

    let mut temp =
        tempfile::NamedTempFile::new_in(cache_parent).map_err(|source| LocalCacheError::Io {
            context: "failed to create temporary cache object",
            path: cache_parent.to_path_buf(),
            source,
        })?;
    copy_and_verify_object(
        &mut source,
        source_path,
        &mut temp,
        cache_path,
        object,
        read_context,
    )?;
    match durability {
        CachePublishDurability::Recoverable => {
            // This deliberately stops at `flush()`: ordinary cache ingest is
            // recoverable derived state, and every cache reuse revalidates
            // object identity. Avoiding `sync_all()` keeps large-object ingest
            // from paying a durable write latency cost on the hot path.
            temp.as_file_mut()
                .flush()
                .map_err(|source| LocalCacheError::Io {
                    context: "failed to flush temporary cache object",
                    path: cache_path.to_path_buf(),
                    source,
                })?;
        }
        CachePublishDurability::Durable => {
            temp.as_file_mut()
                .sync_all()
                .map_err(|source| LocalCacheError::Io {
                    context: "failed to sync temporary cache object",
                    path: cache_path.to_path_buf(),
                    source,
                })?;
        }
    }

    match temp.persist_noclobber(cache_path) {
        Ok(published) => {
            if durability == CachePublishDurability::Durable {
                published.sync_all().map_err(|source| LocalCacheError::Io {
                    context: "failed to sync published cache object",
                    path: cache_path.to_path_buf(),
                    source,
                })?;
                sync_directory(cache_parent).map_err(|source| LocalCacheError::Io {
                    context: "failed to sync cache object directory",
                    path: cache_parent.to_path_buf(),
                    source,
                })?;
            }

            Ok(LocalCacheIngestStatus::Copied)
        }
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            verify_file_object(cache_path, object)?;
            if durability == CachePublishDurability::Durable {
                sync_verified_cache_object(cache_path)?;
            }
            Ok(LocalCacheIngestStatus::AlreadyCached)
        }
        Err(error) => Err(LocalCacheError::Io {
            context: "failed to publish cache object",
            path: cache_path.to_path_buf(),
            source: error.error,
        }),
    }
}

fn copy_and_verify_object(
    source: &mut File,
    source_path: &Path,
    destination: &mut tempfile::NamedTempFile,
    destination_path: &Path,
    expected: &LfsObject,
    read_context: &'static str,
) -> LocalCacheResult<()> {
    let mut hasher = Sha256::new();
    let mut total_size = 0u64;
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|source| LocalCacheError::Io {
                context: read_context,
                path: source_path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }

        destination
            .write_all(&buffer[..read])
            .map_err(|source| LocalCacheError::Io {
                context: "failed to write temporary cache object",
                path: destination_path.to_path_buf(),
                source,
            })?;
        hasher.update(&buffer[..read]);
        total_size = total_size
            .checked_add(read as u64)
            .ok_or_else(|| LocalCacheError::Io {
                context: "object is too large to measure",
                path: source_path.to_path_buf(),
                source: io::Error::new(io::ErrorKind::InvalidData, "object size overflow"),
            })?;
    }

    let actual_oid =
        LfsOid::new(format!("{:x}", hasher.finalize())).expect("SHA-256 hex should be valid");
    let actual_size = LfsObjectSize::new(total_size);

    if actual_oid != expected.oid || actual_size != expected.size {
        return Err(LocalCacheError::IntegrityMismatch {
            path: source_path.to_path_buf(),
            expected_oid: expected.oid.clone(),
            expected_size: expected.size,
            actual_oid,
            actual_size,
        });
    }

    Ok(())
}

fn hash_file(path: &Path) -> LocalCacheResult<(LfsOid, LfsObjectSize)> {
    let file = File::open(path).map_err(|source| LocalCacheError::Io {
        context: "failed to open object for hashing",
        path: path.to_path_buf(),
        source,
    })?;

    hash_open_file(file, path)
}

fn hash_open_file(mut file: File, path: &Path) -> LocalCacheResult<(LfsOid, LfsObjectSize)> {
    let mut hasher = Sha256::new();
    let mut total_size = 0u64;
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| LocalCacheError::Io {
                context: "failed to read object for hashing",
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total_size = total_size
            .checked_add(read as u64)
            .ok_or_else(|| LocalCacheError::Io {
                context: "object is too large to measure",
                path: path.to_path_buf(),
                source: io::Error::new(io::ErrorKind::InvalidData, "object size overflow"),
            })?;
    }

    Ok((
        LfsOid::new(format!("{:x}", hasher.finalize())).expect("SHA-256 hex should be valid"),
        LfsObjectSize::new(total_size),
    ))
}

fn worktree_file_metadata_without_following_symlinks(
    path: &Path,
) -> LocalCacheResult<fs::Metadata> {
    let metadata = fs::symlink_metadata(path).map_err(|source| LocalCacheError::Io {
        context: "failed to inspect worktree path",
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(LocalCacheError::WorktreePathSymlink {
            path: path.to_path_buf(),
        });
    }
    if !metadata.is_file() {
        return Err(LocalCacheError::Io {
            context: "worktree path is not a file",
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidData, "expected a regular file"),
        });
    }

    Ok(metadata)
}

fn open_worktree_file_without_following_symlinks(
    path: &Path,
    context: &'static str,
) -> LocalCacheResult<File> {
    worktree_file_metadata_without_following_symlinks(path)?;

    #[cfg(unix)]
    let file = rustix::fs::openat(
        rustix::fs::CWD,
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(io::Error::from);

    #[cfg(not(unix))]
    let file = File::open(path);

    let file = file.map_err(|source| {
        if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            LocalCacheError::WorktreePathSymlink {
                path: path.to_path_buf(),
            }
        } else {
            LocalCacheError::Io {
                context,
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    if !file
        .metadata()
        .map_err(|source| LocalCacheError::Io {
            context: "failed to inspect opened worktree file",
            path: path.to_path_buf(),
            source,
        })?
        .is_file()
    {
        return Err(LocalCacheError::Io {
            context: "opened worktree path is not a file",
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidData, "expected a regular file"),
        });
    }

    Ok(file)
}

#[cfg(test)]
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
        str::FromStr,
        sync::{Arc, Barrier, mpsc},
        thread,
        time::Duration,
    };

    use crate::LfsObjectSize;

    use super::*;

    fn oid(value: &str) -> LfsOid {
        LfsOid::from_str(value).expect("test OID should be valid")
    }

    fn object_for_bytes(bytes: &[u8]) -> LfsObject {
        let digest = sha256_hex(bytes);

        LfsObject::new(oid(&digest), LfsObjectSize::new(bytes.len() as u64))
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("test parent directory should be created");
        }
        fs::write(path, bytes).expect("test file should be written");
    }

    #[cfg(target_os = "macos")]
    fn is_apfs(path: &Path) -> bool {
        let file_system = rustix::fs::statfs(path)
            .expect("test filesystem should be inspectable")
            .f_fstypename;
        let file_system = file_system
            .iter()
            .copied()
            .take_while(|byte| *byte != 0)
            .map(|byte| byte as u8)
            .collect::<Vec<_>>();

        file_system == b"apfs"
    }

    fn initialize_git_worktree(path: &Path) {
        fs::create_dir_all(path).expect("test worktree should be created");
        let output = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(path)
            .output()
            .expect("git init should start");
        assert!(
            output.status.success(),
            "git init should succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_add(path: &Path, relative_paths: &[&Path]) {
        let output = Command::new("git")
            .arg("add")
            .arg("--")
            .args(relative_paths)
            .current_dir(path)
            .output()
            .expect("git add should start");
        assert!(
            output.status.success(),
            "git add should succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn cache_layout_places_objects_under_sharded_objects_directory() {
        let layout = LocalCacheLayout::new("/cache/root");
        let oid = oid("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");

        assert_eq!(
            layout.object_path_for_oid(&oid),
            PathBuf::from(
                "/cache/root/objects/01/23/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            )
        );
    }

    #[test]
    fn cache_layout_normalizes_uppercase_oid_components() {
        let layout = LocalCacheLayout::new("/cache/root");
        let oid = oid("ABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCD");

        assert_eq!(
            layout.object_path_for_oid(&oid),
            PathBuf::from(
                "/cache/root/objects/ab/cd/abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
            )
        );
    }

    #[test]
    fn cache_layout_builds_default_root_from_home_directory() {
        let layout = LocalCacheLayout::from_home_dir("/home/alice");

        assert_eq!(
            layout.root(),
            Path::new("/home/alice").join(DEFAULT_LOCAL_CACHE_HOME_DIR)
        );
        assert_eq!(
            layout.objects_dir(),
            PathBuf::from("/home/alice/.lfscloud/objects")
        );
    }

    #[test]
    fn cache_object_path_uses_content_identity_not_pointer_size() {
        let layout = LocalCacheLayout::new("/cache/root");
        let oid = oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let object = LfsObject::new(oid.clone(), LfsObjectSize::new(42));
        let same_content_different_reported_size = LfsObject::new(oid, LfsObjectSize::new(99));

        assert_eq!(
            layout.object_path(&object),
            layout.object_path(&same_content_different_reported_size)
        );
    }

    #[test]
    fn cache_verification_accepts_exact_object_bytes() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"verified local cache object";
        let object = object_for_bytes(bytes);
        let cache_path = layout.object_path(&object);
        write_file(&cache_path, bytes);

        let verified = layout
            .verify_object(&object)
            .expect("cache object should verify");

        assert_eq!(verified.object, object);
        assert_eq!(verified.path, cache_path);
    }

    #[test]
    fn cache_verification_rejects_size_mismatch() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"short";
        let object = LfsObject::new(
            object_for_bytes(bytes).oid,
            LfsObjectSize::new(bytes.len() as u64 + 1),
        );
        let cache_path = layout.object_path(&object);
        write_file(&cache_path, bytes);

        let error = layout
            .verify_object(&object)
            .expect_err("wrong size should fail verification");

        assert!(matches!(
            error,
            LocalCacheError::IntegrityMismatch {
                actual_size,
                expected_size,
                ..
            } if actual_size == LfsObjectSize::new(bytes.len() as u64)
                && expected_size == object.size
        ));
    }

    #[test]
    fn ingest_copies_valid_git_lfs_object_into_shared_cache() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let git_lfs_objects_dir = temp.path().join(".git/lfs/objects");
        let bytes = b"object already downloaded by git lfs";
        let object = object_for_bytes(bytes);
        let source_path = git_lfs_object_path(&git_lfs_objects_dir, &object.oid);
        write_file(&source_path, bytes);

        let ingest = layout
            .ingest_git_lfs_object(&git_lfs_objects_dir, &object)
            .expect("valid Git LFS object should ingest");

        assert_eq!(ingest.status, LocalCacheIngestStatus::Copied);
        assert_eq!(ingest.source_path, source_path);
        assert_eq!(ingest.cache_path, layout.object_path(&object));
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cache object should exist"),
            bytes
        );
    }

    #[test]
    fn ingest_verifies_existing_cache_before_using_source() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let git_lfs_objects_dir = temp.path().join(".git/lfs/objects");
        let bytes = b"already present in shared cache";
        let object = object_for_bytes(bytes);
        write_file(&layout.object_path(&object), bytes);

        let ingest = layout
            .ingest_git_lfs_object(&git_lfs_objects_dir, &object)
            .expect("verified cache object should be reused without source");

        assert_eq!(ingest.status, LocalCacheIngestStatus::AlreadyCached);
    }

    #[test]
    fn ingest_rejects_corrupt_existing_cache_without_using_source() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let git_lfs_objects_dir = temp.path().join(".git/lfs/objects");
        let bytes = b"already present but corrupt in shared cache";
        let object = object_for_bytes(bytes);
        write_file(&layout.object_path(&object), b"corrupt cache bytes");
        write_file(
            &git_lfs_object_path(&git_lfs_objects_dir, &object.oid),
            bytes,
        );

        let error = layout
            .ingest_git_lfs_object(&git_lfs_objects_dir, &object)
            .expect_err("corrupt existing cache object should fail ingest");

        assert!(matches!(
            error,
            LocalCacheError::IntegrityMismatch { path, .. } if path == layout.object_path(&object)
        ));
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cache object should remain inspectable"),
            b"corrupt cache bytes"
        );
    }

    #[test]
    fn ingest_rejects_missing_git_lfs_source_object() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let git_lfs_objects_dir = temp.path().join(".git/lfs/objects");
        let object = object_for_bytes(b"missing from source cache");

        let error = layout
            .ingest_git_lfs_object(&git_lfs_objects_dir, &object)
            .expect_err("missing source object should fail ingest");

        assert!(matches!(
            error,
            LocalCacheError::MissingSourceObject { oid, size, .. }
                if oid == object.oid && size == object.size
        ));
    }

    #[test]
    fn ingest_rejects_corrupt_git_lfs_source_object() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let git_lfs_objects_dir = temp.path().join(".git/lfs/objects");
        let object = object_for_bytes(b"expected contents");
        write_file(
            &git_lfs_object_path(&git_lfs_objects_dir, &object.oid),
            b"corrupt contents",
        );

        let error = layout
            .ingest_git_lfs_object(&git_lfs_objects_dir, &object)
            .expect_err("corrupt source object should fail ingest");

        assert!(matches!(error, LocalCacheError::IntegrityMismatch { .. }));
        assert!(!layout.object_path(&object).exists());
    }

    #[test]
    fn materialize_object_creates_verified_destination_from_cache() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"cache bytes to materialize";
        let object = object_for_bytes(bytes);
        write_file(&layout.object_path(&object), bytes);
        let destination = temp.path().join("repo/assets/model.bin");

        let materialization = layout
            .materialize_object(&object, &destination)
            .expect("verified cache object should materialize");

        assert_eq!(materialization.object, object);
        assert_eq!(materialization.cache_path, layout.object_path(&object));
        assert_eq!(materialization.destination_path, destination);
        #[cfg(target_os = "macos")]
        let expected_status = if is_apfs(temp.path()) {
            LocalCacheMaterializationStatus::CopyOnWriteCloned
        } else {
            LocalCacheMaterializationStatus::Copied
        };
        #[cfg(not(target_os = "macos"))]
        let expected_status = LocalCacheMaterializationStatus::Copied;
        assert_eq!(materialization.status, expected_status);
        assert_eq!(
            fs::read(&materialization.destination_path)
                .expect("materialized destination should be readable"),
            bytes
        );
    }

    #[test]
    fn materialize_to_temporary_file_copies_when_clone_is_unavailable() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let bytes = b"cache bytes for fallback materialization";
        let object = object_for_bytes(bytes);
        let cache_path = temp.path().join("cache-object");
        write_file(&cache_path, bytes);

        let (materialized, status) = materialize_to_temporary_file_with_clone(
            &cache_path,
            temp.path(),
            &object,
            |_source_path, _destination_parent| Ok(None),
        )
        .expect("unavailable cloning should fall back to a verified copy");

        assert_eq!(status, LocalCacheMaterializationStatus::Copied);
        assert_eq!(
            fs::read(materialized.path()).expect("fallback copy should be readable"),
            bytes
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn materialize_object_uses_copy_on_write_on_apfs() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        assert!(is_apfs(temp.path()), "this test requires an APFS fixture");

        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"cache bytes that must be cloned on APFS";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);

        let materialization = layout
            .materialize_object(&object, &destination)
            .expect("APFS materialization should succeed");

        assert_eq!(
            materialization.status,
            LocalCacheMaterializationStatus::CopyOnWriteCloned
        );
    }

    #[cfg(unix)]
    #[test]
    fn materialize_object_respects_restrictive_process_umask_for_new_files() {
        use std::os::unix::fs::PermissionsExt;

        const UMASK_CHILD_ENV: &str = "LFS_CLOUD_MATERIALIZATION_UMASK_CHILD";
        const TEST_NAME: &str = "local_cache::tests::materialize_object_respects_restrictive_process_umask_for_new_files";

        if std::env::var_os(UMASK_CHILD_ENV).is_none() {
            let current_exe = std::env::current_exe().expect("test executable should resolve");
            let status = Command::new("sh")
                .args([
                    "-c",
                    "umask 077; exec \"$1\" --exact \"$2\" --nocapture",
                    "sh",
                ])
                .arg(current_exe)
                .arg(TEST_NAME)
                .env(UMASK_CHILD_ENV, "1")
                .status()
                .expect("restrictive-umask test subprocess should start");
            assert!(status.success(), "restrictive-umask subprocess should pass");
            return;
        }

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"cache bytes with private worktree permissions";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);

        layout
            .materialize_object(&object, &destination)
            .expect("verified cache object should materialize");

        let mode = fs::metadata(&destination)
            .expect("materialized destination should have metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn materialize_object_reuses_existing_verified_destination() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"already hydrated contents";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(&destination, bytes);

        let materialization = layout
            .materialize_object(&object, &destination)
            .expect("existing verified destination should be reused");

        assert_eq!(
            materialization.status,
            LocalCacheMaterializationStatus::AlreadyMaterialized
        );
    }

    #[test]
    fn materialize_object_refuses_to_overwrite_different_destination() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"safe cache contents";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(&destination, b"dirty worktree contents");

        let error = layout
            .materialize_object(&object, &destination)
            .expect_err("different destination content should not be overwritten");

        assert!(matches!(
            error,
            LocalCacheError::MaterializationTargetExists { path, .. } if path == destination
        ));
        assert_eq!(
            fs::read(&destination).expect("destination should remain untouched"),
            b"dirty worktree contents"
        );
    }

    #[test]
    fn hydrate_pointer_file_replaces_matching_pointer_with_cache_bytes() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"hydrated bytes from shared cache";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &destination,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );

        let materialization = layout
            .hydrate_pointer_file(&destination)
            .expect("matching pointer should hydrate from cache");

        assert_eq!(materialization.object, object);
        #[cfg(target_os = "macos")]
        let expected_status = if is_apfs(temp.path()) {
            LocalCacheMaterializationStatus::CopyOnWriteCloned
        } else {
            LocalCacheMaterializationStatus::Copied
        };
        #[cfg(not(target_os = "macos"))]
        let expected_status = LocalCacheMaterializationStatus::Copied;
        assert_eq!(materialization.status, expected_status);
        assert_eq!(
            fs::read(&destination).expect("hydrated destination should be readable"),
            bytes
        );
    }

    #[test]
    fn hydrate_empty_pointer_is_already_materialized_without_cache_bytes() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let object = object_for_bytes(b"");
        let destination = temp.path().join("repo/assets/empty.bin");
        write_file(&destination, b"");

        let materialization = layout
            .hydrate_pointer_file(&destination)
            .expect("an empty pointer should already be materialized");

        assert_eq!(materialization.object, object);
        assert_eq!(materialization.destination_path, destination);
        assert_eq!(
            materialization.status,
            LocalCacheMaterializationStatus::AlreadyMaterialized
        );
        assert!(!materialization.cache_path.exists());
    }

    #[cfg(any(target_os = "android", target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn hydrate_pointer_file_preserves_edit_after_final_pointer_check() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"hydrated bytes from shared cache";
        let edited_bytes = b"concurrent worktree edit";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &destination,
            LfsPointer::new(object).to_pointer_file().as_bytes(),
        );

        let pointer_checked = Arc::new(Barrier::new(2));
        let allow_publish = Arc::new(Barrier::new(2));
        let hydration_layout = layout.clone();
        let hydration_destination = destination.clone();
        let hydration_pointer_checked = Arc::clone(&pointer_checked);
        let hydration_allow_publish = Arc::clone(&allow_publish);
        let hydration = thread::spawn(move || {
            hydration_layout.hydrate_pointer_file_with_before_publish(
                &hydration_destination,
                || {
                    hydration_pointer_checked.wait();
                    hydration_allow_publish.wait();
                },
            )
        });

        pointer_checked.wait();
        write_file(&destination, edited_bytes);
        allow_publish.wait();

        let error = hydration
            .join()
            .expect("hydration thread should not panic")
            .expect_err("a concurrent edit should abort hydration");
        assert!(matches!(
            error,
            LocalCacheError::MaterializationTargetExists { path, .. } if path == destination
        ));
        assert_eq!(
            fs::read(&destination).expect("concurrent edit should remain readable"),
            edited_bytes
        );
    }

    #[cfg(any(target_os = "android", target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn hydrate_pointer_file_serializes_same_path_operations() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"hydrated bytes from shared cache";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &destination,
            LfsPointer::new(object).to_pointer_file().as_bytes(),
        );

        let pointer_checked = Arc::new(Barrier::new(2));
        let allow_publish = Arc::new(Barrier::new(2));
        let first_layout = layout.clone();
        let first_destination = destination.clone();
        let first_pointer_checked = Arc::clone(&pointer_checked);
        let first_allow_publish = Arc::clone(&allow_publish);
        let first = thread::spawn(move || {
            first_layout.hydrate_pointer_file_with_before_publish(&first_destination, || {
                first_pointer_checked.wait();
                first_allow_publish.wait();
            })
        });

        pointer_checked.wait();
        let second_layout = layout.clone();
        let second_destination = destination.clone();
        let (second_done_tx, second_done_rx) = mpsc::channel();
        let second = thread::spawn(move || {
            second_done_tx
                .send(second_layout.hydrate_pointer_file(&second_destination))
                .expect("test should receive second hydration result");
        });

        assert!(
            second_done_rx
                .recv_timeout(Duration::from_millis(250))
                .is_err(),
            "a same-path hydration should wait for the active publication"
        );
        allow_publish.wait();

        first
            .join()
            .expect("first hydration thread should not panic")
            .expect("first hydration should succeed");
        let second_error = second_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second hydration should finish after publication")
            .expect_err("the completed hydration should no longer be a pointer");
        second.join().expect("second hydration should not panic");

        assert!(matches!(
            second_error,
            LocalCacheError::PointerParse { path, .. } if path == destination
        ));
        assert_eq!(
            fs::read(&destination).expect("hydrated destination should be readable"),
            bytes
        );
    }

    #[cfg(unix)]
    #[test]
    fn hydrate_pointer_file_preserves_existing_worktree_file_mode() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"hydrated executable bytes";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/tool.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &destination,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        let mut permissions = fs::metadata(&destination)
            .expect("pointer should have metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&destination, permissions)
            .expect("pointer mode should be made executable");

        layout
            .hydrate_pointer_file(&destination)
            .expect("matching pointer should hydrate from cache");

        let mode = fs::metadata(&destination)
            .expect("hydrated destination should have metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn hydrate_pointer_file_rejects_non_pointer_worktree_content() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&destination, b"dirty non-pointer contents");

        let error = layout
            .hydrate_pointer_file(&destination)
            .expect_err("non-pointer worktree content should not hydrate");

        assert!(matches!(
            error,
            LocalCacheError::PointerParse { path, .. } if path == destination
        ));
        assert_eq!(
            fs::read(&destination).expect("destination should remain untouched"),
            b"dirty non-pointer contents"
        );
    }

    #[test]
    fn hydrate_pointer_file_rejects_non_utf8_worktree_content() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let destination = temp.path().join("repo/assets/model.bin");
        let bytes = b"version https://git-lfs.github.com/spec/v1\noid sha256:\xff\nsize 1\n";
        write_file(&destination, bytes);

        let error = layout
            .hydrate_pointer_file(&destination)
            .expect_err("non-UTF-8 worktree content should not hydrate");

        assert!(matches!(
            error,
            LocalCacheError::PointerFileInvalidUtf8 { path, .. } if path == destination
        ));
        assert_eq!(
            fs::read(&destination).expect("destination should remain untouched"),
            bytes
        );
    }

    #[test]
    fn hydrate_pointer_file_rejects_oversized_non_pointer_without_unbounded_read() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let destination = temp.path().join("repo/assets/model.bin");
        let bytes = vec![b'x'; LFS_POINTER_SIZE_CUTOFF as usize];
        write_file(&destination, &bytes);

        let error = layout
            .hydrate_pointer_file(&destination)
            .expect_err("oversized non-pointer worktree content should not hydrate");

        assert!(matches!(
            error,
            LocalCacheError::PointerFileTooLarge { path, size, size_cutoff }
                if path == destination
                    && size == LFS_POINTER_SIZE_CUTOFF
                    && size_cutoff == LFS_POINTER_SIZE_CUTOFF
        ));
    }

    #[cfg(unix)]
    #[test]
    fn hydrate_pointer_file_rejects_symlink_without_replacing_it() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"cached bytes for symlink hydration";
        let object = object_for_bytes(bytes);
        let outside_pointer = temp.path().join("outside-pointer");
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &outside_pointer,
            LfsPointer::new(object).to_pointer_file().as_bytes(),
        );
        fs::create_dir_all(
            destination
                .parent()
                .expect("destination should have a parent"),
        )
        .expect("destination parent should be created");
        std::os::unix::fs::symlink(&outside_pointer, &destination)
            .expect("worktree symlink should be created");

        let error = layout
            .hydrate_pointer_file(&destination)
            .expect_err("a symlink must not be hydrated");

        assert!(matches!(
            error,
            LocalCacheError::WorktreePathSymlink { path } if path == destination
        ));
        assert!(
            fs::symlink_metadata(&destination)
                .expect("symlink metadata should remain readable")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(&outside_pointer).expect("outside pointer should remain readable"),
            LfsPointer::new(object_for_bytes(bytes)).to_pointer_file()
        );
    }

    #[test]
    fn dehydrate_file_replaces_clean_hydrated_bytes_with_pointer_and_caches_object() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"worktree bytes to dehydrate";
        let object = object_for_bytes(bytes);
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&worktree_path, bytes);

        let dehydration = layout
            .dehydrate_file(&object, &worktree_path)
            .expect("clean worktree object should dehydrate");

        assert_eq!(dehydration.object, object);
        assert_eq!(dehydration.cache_path, layout.object_path(&object));
        assert_eq!(dehydration.pointer_path, worktree_path);
        assert_eq!(
            dehydration.status,
            LocalCacheDehydrationStatus::CachedAndReplacedWithPointer
        );
        assert_eq!(
            fs::read_to_string(&dehydration.pointer_path).expect("pointer file should be readable"),
            LfsPointer::new(object.clone()).to_pointer_file()
        );
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cache object should be readable"),
            bytes
        );
    }

    #[test]
    fn dehydrate_empty_file_preserves_the_canonical_empty_pointer() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let object = object_for_bytes(b"");
        let worktree_path = temp.path().join("repo/assets/empty.bin");
        write_file(&worktree_path, b"");

        let dehydration = layout
            .dehydrate_file(&object, &worktree_path)
            .expect("an empty file should already be its canonical pointer");

        assert_eq!(dehydration.object, object);
        assert_eq!(
            dehydration.status,
            LocalCacheDehydrationStatus::AlreadyDehydrated
        );
        assert_eq!(
            fs::read(&dehydration.pointer_path).expect("empty pointer should remain readable"),
            b""
        );
        assert!(!dehydration.cache_path.exists());
    }

    #[test]
    fn dehydrate_file_reads_uncached_large_worktree_bytes_only_twice() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = vec![b'x'; (LFS_POINTER_SIZE_CUTOFF + 1) as usize];
        let object = object_for_bytes(&bytes);
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&worktree_path, &bytes);
        let mut full_read_count = 0;

        layout
            .dehydrate_file_with_read_observer(
                &object,
                &worktree_path,
                || {},
                || full_read_count += 1,
            )
            .expect("clean worktree object should dehydrate");

        assert_eq!(full_read_count, 2);
    }

    #[test]
    fn dehydrate_file_reads_cached_large_worktree_bytes_only_once() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = vec![b'x'; (LFS_POINTER_SIZE_CUTOFF + 1) as usize];
        let object = object_for_bytes(&bytes);
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), &bytes);
        write_file(&worktree_path, &bytes);
        let mut full_read_count = 0;

        layout
            .dehydrate_file_with_read_observer(
                &object,
                &worktree_path,
                || {},
                || full_read_count += 1,
            )
            .expect("clean cached worktree object should dehydrate");

        assert_eq!(full_read_count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn dehydrate_file_rejects_symlink_without_replacing_it() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"outside bytes must not be dehydrated";
        let object = object_for_bytes(bytes);
        let outside_file = temp.path().join("outside-object");
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&outside_file, bytes);
        fs::create_dir_all(
            worktree_path
                .parent()
                .expect("worktree path should have a parent"),
        )
        .expect("worktree parent should be created");
        std::os::unix::fs::symlink(&outside_file, &worktree_path)
            .expect("worktree symlink should be created");

        let error = layout
            .dehydrate_file(&object, &worktree_path)
            .expect_err("a symlink must not be dehydrated");

        assert!(matches!(
            error,
            LocalCacheError::WorktreePathSymlink { path } if path == worktree_path
        ));
        assert!(
            fs::symlink_metadata(&worktree_path)
                .expect("symlink metadata should remain readable")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read(&outside_file).expect("outside file should remain readable"),
            bytes
        );
        assert!(!layout.object_path(&object).exists());
    }

    #[cfg(any(target_os = "android", target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn dehydrate_file_preserves_edit_before_pointer_exchange() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"worktree bytes to dehydrate";
        let edited_bytes = b"concurrent worktree edit";
        let object = object_for_bytes(bytes);
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&worktree_path, bytes);

        let pointer_staged = Arc::new(Barrier::new(2));
        let allow_publish = Arc::new(Barrier::new(2));
        let dehydration_layout = layout.clone();
        let dehydration_object = object.clone();
        let dehydration_path = worktree_path.clone();
        let dehydration_pointer_staged = Arc::clone(&pointer_staged);
        let dehydration_allow_publish = Arc::clone(&allow_publish);
        let dehydration = thread::spawn(move || {
            dehydration_layout.dehydrate_file_with_before_pointer_publish(
                &dehydration_object,
                &dehydration_path,
                || {
                    dehydration_pointer_staged.wait();
                    dehydration_allow_publish.wait();
                },
            )
        });

        pointer_staged.wait();
        write_file(&worktree_path, edited_bytes);
        allow_publish.wait();

        let error = dehydration
            .join()
            .expect("dehydration thread should not panic")
            .expect_err("a concurrent edit should abort dehydration");
        assert!(matches!(error, LocalCacheError::IntegrityMismatch { .. }));
        assert_eq!(
            fs::read(&worktree_path).expect("concurrent edit should remain readable"),
            edited_bytes
        );
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("verified cache object should remain"),
            bytes
        );
    }

    #[test]
    fn garbage_collect_waits_until_dehydration_publishes_its_pointer() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let repo = temp.path().join("repo");
        let worktree_path = repo.join("assets/model.bin");
        let bytes = b"only copy must survive concurrent garbage collection";
        let object = object_for_bytes(bytes);
        initialize_git_worktree(&repo);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &worktree_path,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        git_add(
            &repo,
            &[Path::new(".gitattributes"), Path::new("assets/model.bin")],
        );
        write_file(&worktree_path, bytes);
        layout
            .register_worktree(
                LocalCacheWorktreeRegistration::new(
                    "github-main:owner/repo",
                    &repo,
                    repo.join(".git"),
                )
                .expect("registration should validate"),
            )
            .expect("worktree should register");

        let cache_published = Arc::new(Barrier::new(2));
        let allow_pointer_publish = Arc::new(Barrier::new(2));
        let dehydration_layout = layout.clone();
        let dehydration_object = object.clone();
        let dehydration_path = worktree_path.clone();
        let dehydration_cache_published = Arc::clone(&cache_published);
        let dehydration_allow_pointer_publish = Arc::clone(&allow_pointer_publish);
        let dehydration = thread::spawn(move || {
            dehydration_layout.dehydrate_file_with_before_pointer_publish(
                &dehydration_object,
                &dehydration_path,
                || {
                    dehydration_cache_published.wait();
                    dehydration_allow_pointer_publish.wait();
                },
            )
        });

        cache_published.wait();
        let cache_bytes_during_pause = fs::read(layout.object_path(&object));
        let worktree_bytes_during_pause = fs::read(&worktree_path);

        let collection_layout = layout.clone();
        let (collection_started_tx, collection_started_rx) = mpsc::channel();
        let (collection_done_tx, collection_done_rx) = mpsc::channel();
        let collection = thread::spawn(move || {
            collection_started_tx
                .send(())
                .expect("test should receive collector start");
            collection_done_tx
                .send(collection_layout.garbage_collect(false, false))
                .expect("test should receive collection result");
        });
        let collection_started = collection_started_rx.recv();
        let collection_during_pause = collection_done_rx.recv_timeout(Duration::from_millis(250));

        allow_pointer_publish.wait();
        let dehydration = dehydration
            .join()
            .expect("dehydration thread should not panic")
            .expect("dehydration should finish");
        let collection_report = collection_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("garbage collection should finish after dehydration")
            .expect("garbage collection should succeed");
        collection
            .join()
            .expect("collection thread should not panic");

        assert_eq!(
            cache_bytes_during_pause.expect("cache object should be published"),
            bytes
        );
        assert_eq!(
            worktree_bytes_during_pause
                .expect("worktree bytes should remain until pointer publish"),
            bytes
        );
        collection_started.expect("collector should report before locking");
        assert!(
            collection_during_pause.is_err(),
            "garbage collection should wait for pointer publication"
        );
        assert_eq!(
            dehydration.status,
            LocalCacheDehydrationStatus::CachedAndReplacedWithPointer
        );
        assert_eq!(collection_report.retained_objects.len(), 1);
        assert!(collection_report.deleted_objects.is_empty());
        assert!(layout.object_path(&object).exists());
        assert_eq!(
            fs::read_to_string(&worktree_path).expect("pointer should be readable"),
            LfsPointer::new(object).to_pointer_file()
        );
    }

    #[cfg(unix)]
    #[test]
    fn dehydrate_file_preserves_existing_worktree_file_mode_on_pointer() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"executable worktree bytes";
        let object = object_for_bytes(bytes);
        let worktree_path = temp.path().join("repo/assets/tool.bin");
        write_file(&worktree_path, bytes);
        fs::set_permissions(&worktree_path, fs::Permissions::from_mode(0o755))
            .expect("test file mode should be set");

        layout
            .dehydrate_file(&object, &worktree_path)
            .expect("clean executable worktree object should dehydrate");

        let mode = fs::metadata(&worktree_path)
            .expect("pointer file metadata should be readable")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn dehydrate_file_reuses_existing_verified_cache_before_replacing() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"already cached worktree bytes";
        let object = object_for_bytes(bytes);
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(&worktree_path, bytes);

        let dehydration = layout
            .dehydrate_file(&object, &worktree_path)
            .expect("clean cached worktree object should dehydrate");

        assert_eq!(
            dehydration.status,
            LocalCacheDehydrationStatus::ReplacedWithPointer
        );
        assert_eq!(
            fs::read_to_string(&worktree_path).expect("pointer file should be readable"),
            LfsPointer::new(object).to_pointer_file()
        );
    }

    #[test]
    fn dehydrate_file_accepts_matching_pointer_without_cache_object() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let object = object_for_bytes(b"already dehydrated bytes");
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(
            &worktree_path,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );

        let dehydration = layout
            .dehydrate_file(&object, &worktree_path)
            .expect("matching pointer should already be dehydrated");

        assert_eq!(
            dehydration.status,
            LocalCacheDehydrationStatus::AlreadyDehydrated
        );
        assert!(!layout.object_path(&object).exists());
    }

    #[test]
    fn dehydrate_file_accepts_pointer_shaped_object_contents() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let referenced = object_for_bytes(b"referenced object bytes");
        let pointer_shaped_bytes = LfsPointer::new(referenced).to_pointer_file();
        let object = object_for_bytes(pointer_shaped_bytes.as_bytes());
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&worktree_path, pointer_shaped_bytes.as_bytes());

        let dehydration = layout
            .dehydrate_file(&object, &worktree_path)
            .expect("pointer-shaped object bytes should dehydrate");

        assert_eq!(
            dehydration.status,
            LocalCacheDehydrationStatus::CachedAndReplacedWithPointer
        );
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cache object should be readable"),
            pointer_shaped_bytes.as_bytes()
        );
        assert_eq!(
            fs::read_to_string(&worktree_path).expect("pointer file should be readable"),
            LfsPointer::new(object).to_pointer_file()
        );
    }

    #[test]
    fn dehydrate_file_reports_existing_pointer_for_different_object() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let expected = object_for_bytes(b"expected object bytes");
        let actual = object_for_bytes(b"other object bytes");
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(
            &worktree_path,
            LfsPointer::new(actual.clone()).to_pointer_file().as_bytes(),
        );

        let error = layout
            .dehydrate_file(&expected, &worktree_path)
            .expect_err("different pointer should not dehydrate");

        assert!(matches!(
            error,
            LocalCacheError::PointerObjectMismatch {
                path,
                expected_oid,
                expected_size,
                actual_oid,
                actual_size,
            } if path == worktree_path
                && expected_oid == expected.oid
                && expected_size == expected.size
                && actual_oid == actual.oid
                && actual_size == actual.size
        ));
        assert_eq!(
            fs::read_to_string(&worktree_path).expect("existing pointer should remain"),
            LfsPointer::new(actual).to_pointer_file()
        );
    }

    #[test]
    fn dehydrate_file_refuses_to_replace_dirty_worktree_content() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"original hydrated bytes";
        let object = object_for_bytes(bytes);
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(&worktree_path, b"dirty worktree bytes");

        let error = layout
            .dehydrate_file(&object, &worktree_path)
            .expect_err("dirty worktree content should not dehydrate");

        assert!(matches!(
            error,
            LocalCacheError::IntegrityMismatch { path, .. } if path == worktree_path
        ));
        assert_eq!(
            fs::read(&worktree_path).expect("dirty worktree content should remain"),
            b"dirty worktree bytes"
        );
    }

    #[test]
    fn worktree_registry_path_lives_under_cache_root() {
        let layout = LocalCacheLayout::new("/cache/root");

        assert_eq!(
            layout.worktree_registry_path(),
            PathBuf::from("/cache/root").join(LOCAL_CACHE_WORKTREES_FILE)
        );
    }

    #[test]
    fn missing_worktree_registry_loads_as_empty() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));

        let registry = layout
            .load_worktree_registry()
            .expect("missing registry should be empty");

        assert!(registry.is_empty());
        assert_eq!(registry.worktrees(), &[]);
    }

    #[test]
    fn register_worktree_writes_and_loads_stable_registry() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/repo",
            temp.path().join("repo"),
            temp.path().join("repo/.git"),
        )
        .expect("absolute registration should validate");

        let change = layout
            .register_worktree(registration.clone())
            .expect("worktree should register");

        assert_eq!(change.status, LocalCacheWorktreeRegistrationStatus::Added);
        assert_eq!(change.registration, registration);

        let registry = layout
            .load_worktree_registry()
            .expect("registry should reload");

        assert_eq!(registry.worktrees(), &[registration]);
        assert!(
            fs::read_to_string(layout.worktree_registry_path())
                .expect("registry should be readable")
                .contains("\"version\": 2")
        );
    }

    #[cfg(unix)]
    #[test]
    fn worktree_registry_round_trips_non_utf8_unix_paths() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let mut worktree_root = temp.path().as_os_str().as_bytes().to_vec();
        worktree_root.extend_from_slice(b"/repo-\xff");
        let worktree_root = PathBuf::from(OsString::from_vec(worktree_root));
        let registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/repo",
            &worktree_root,
            worktree_root.join(".git"),
        )
        .expect("absolute non-UTF-8 registration should validate");

        layout
            .register_worktree(registration.clone())
            .expect("non-UTF-8 worktree should register");

        assert_eq!(
            layout
                .load_worktree_registry()
                .expect("registry should reload")
                .worktrees(),
            std::slice::from_ref(&registration)
        );
        assert_eq!(
            layout
                .remove_worktree_registration(&worktree_root)
                .expect("non-UTF-8 worktree should remove"),
            Some(registration)
        );
    }

    #[test]
    fn worktree_registry_loads_legacy_utf8_paths_and_upgrades_on_change() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let existing_root = temp.path().join("existing");
        let added_root = temp.path().join("added");
        let legacy_registry = serde_json::json!({
            "version": 1,
            "worktrees": [{
                "repository_id": "github-main:owner/existing",
                "worktree_root": existing_root,
                "git_dir": temp.path().join("existing/.git"),
            }],
        });
        write_file(
            &layout.worktree_registry_path(),
            &serde_json::to_vec_pretty(&legacy_registry)
                .expect("legacy registry fixture should encode"),
        );

        let loaded = layout
            .load_worktree_registry()
            .expect("legacy registry should load");
        assert_eq!(loaded.worktrees()[0].worktree_root, existing_root);

        layout
            .register_worktree(
                LocalCacheWorktreeRegistration::new(
                    "github-main:owner/added",
                    &added_root,
                    added_root.join(".git"),
                )
                .expect("added registration should validate"),
            )
            .expect("registry mutation should upgrade the legacy file");

        let upgraded: serde_json::Value = serde_json::from_slice(
            &fs::read(layout.worktree_registry_path()).expect("registry should be readable"),
        )
        .expect("upgraded registry should decode as JSON");
        assert_eq!(upgraded["version"], 2);
        assert_eq!(
            upgraded["worktrees"][0]["worktree_root"]["encoding"],
            if cfg!(unix) {
                "unix_bytes_base64"
            } else if cfg!(windows) {
                "windows_wide_base64"
            } else {
                "utf8"
            }
        );
    }

    #[test]
    fn register_worktree_updates_existing_path_without_duplicates() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let worktree_root = temp.path().join("repo");
        let first = LocalCacheWorktreeRegistration::new(
            "github-main:owner/repo",
            &worktree_root,
            temp.path().join("repo/.git"),
        )
        .expect("absolute registration should validate");
        let updated = LocalCacheWorktreeRegistration::new(
            "github-main:owner/renamed",
            &worktree_root,
            temp.path().join("repo/.git/worktrees/main"),
        )
        .expect("absolute registration should validate");

        layout
            .register_worktree(first)
            .expect("first worktree should register");
        let change = layout
            .register_worktree(updated.clone())
            .expect("worktree should update");
        let unchanged = layout
            .register_worktree(updated.clone())
            .expect("identical worktree should remain unchanged");

        assert_eq!(change.status, LocalCacheWorktreeRegistrationStatus::Updated);
        assert_eq!(
            unchanged.status,
            LocalCacheWorktreeRegistrationStatus::Unchanged
        );
        assert_eq!(
            layout
                .load_worktree_registry()
                .expect("registry should reload")
                .worktrees(),
            &[updated]
        );
    }

    #[test]
    fn register_worktree_waits_for_registry_lock() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        fs::create_dir_all(layout.root()).expect("cache root should be created");
        let lock_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(layout.worktree_registry_lock_path())
            .expect("registry lock file should open");
        FileExt::lock(&lock_file).expect("registry lock should be acquired by test");
        let contended_lock_file = OpenOptions::new()
            .write(true)
            .open(layout.worktree_registry_lock_path())
            .expect("contended registry lock file should open");
        assert!(
            FileExt::try_lock(&contended_lock_file).is_err(),
            "the platform should report the held registry lock as contended"
        );

        let registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/repo",
            temp.path().join("repo"),
            temp.path().join("repo/.git"),
        )
        .expect("absolute registration should validate");
        let thread_layout = layout.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            started_tx
                .send(())
                .expect("test should receive worker start");
            done_tx
                .send(thread_layout.register_worktree(registration))
                .expect("test should receive registration result");
        });

        started_rx
            .recv()
            .expect("worker should report before attempting registration");
        assert!(
            done_rx.recv_timeout(Duration::from_secs(1)).is_err(),
            "registration should wait while another process holds the registry lock"
        );

        drop(lock_file);
        let change = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("registration should finish after lock release")
            .expect("worktree should register");
        worker.join().expect("worker should not panic");

        assert_eq!(change.status, LocalCacheWorktreeRegistrationStatus::Added);
    }

    #[cfg(unix)]
    #[test]
    fn register_and_remove_worktree_use_canonical_path_keys() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let real_worktree_root = temp.path().join("repo");
        let symlink_worktree_root = temp.path().join("repo-link");
        fs::create_dir_all(real_worktree_root.join(".git"))
            .expect("real worktree should be created");
        std::os::unix::fs::symlink(&real_worktree_root, &symlink_worktree_root)
            .expect("worktree symlink should be created");

        let first = LocalCacheWorktreeRegistration::new(
            "github-main:owner/repo",
            &symlink_worktree_root,
            symlink_worktree_root.join(".git"),
        )
        .expect("symlink registration should validate");
        let updated = LocalCacheWorktreeRegistration::new(
            "github-main:owner/repo",
            &real_worktree_root,
            real_worktree_root.join(".git"),
        )
        .expect("real path registration should validate");

        layout
            .register_worktree(first)
            .expect("first worktree should register");
        let change = layout
            .register_worktree(updated.clone())
            .expect("canonical worktree key should update existing record");

        assert_eq!(change.status, LocalCacheWorktreeRegistrationStatus::Updated);
        assert_eq!(
            layout
                .load_worktree_registry()
                .expect("registry should reload")
                .worktrees(),
            std::slice::from_ref(&updated)
        );
        assert_eq!(
            layout
                .remove_worktree_registration(&symlink_worktree_root)
                .expect("canonical worktree key should remove existing record"),
            Some(updated)
        );
    }

    #[test]
    fn remove_worktree_registration_deletes_matching_absolute_path() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/repo",
            temp.path().join("repo"),
            temp.path().join("repo/.git"),
        )
        .expect("absolute registration should validate");
        layout
            .register_worktree(registration.clone())
            .expect("worktree should register");

        let removed = layout
            .remove_worktree_registration(&registration.worktree_root)
            .expect("worktree should remove");

        assert_eq!(removed, Some(registration));
        assert!(
            layout
                .load_worktree_registry()
                .expect("registry should reload")
                .is_empty()
        );
    }

    #[test]
    fn garbage_collect_preserves_objects_when_a_registered_worktree_is_unavailable() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let repo = temp.path().join("repo");
        let missing_repo = temp.path().join("missing-repo");
        let referenced_bytes = b"referenced cache object";
        let unreferenced_bytes = b"unreferenced cache object";
        let referenced = object_for_bytes(referenced_bytes);
        let unreferenced = object_for_bytes(unreferenced_bytes);
        let active_registration =
            LocalCacheWorktreeRegistration::new("github-main:owner/repo", &repo, repo.join(".git"))
                .expect("active registration should validate");
        let missing_registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/missing",
            &missing_repo,
            missing_repo.join(".git"),
        )
        .expect("missing registration should validate");

        initialize_git_worktree(&repo);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(&layout.object_path(&referenced), referenced_bytes);
        write_file(&layout.object_path(&unreferenced), unreferenced_bytes);
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(referenced.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        git_add(
            &repo,
            &[Path::new(".gitattributes"), Path::new("asset/model.bin")],
        );
        layout
            .register_worktree(active_registration.clone())
            .expect("active worktree should register");
        layout
            .register_worktree(missing_registration.clone())
            .expect("missing worktree should register");

        let report = layout
            .garbage_collect(false, false)
            .expect("garbage collection should finish");

        assert_eq!(report.active_worktree_count, 1);
        assert_eq!(
            report.unavailable_worktrees,
            vec![missing_registration.clone()]
        );
        assert!(report.pruned_worktrees.is_empty());
        assert_eq!(report.retained_objects.len(), 1);
        assert_eq!(report.retained_objects[0].oid, referenced.oid);
        assert_eq!(report.protected_objects.len(), 1);
        assert_eq!(report.protected_objects[0].oid, unreferenced.oid);
        assert!(report.unreferenced_objects.is_empty());
        assert!(report.deleted_objects.is_empty());
        assert!(layout.object_path(&referenced).exists());
        assert!(layout.object_path(&unreferenced).exists());
        assert_eq!(
            layout
                .load_worktree_registry()
                .expect("registry should reload")
                .worktrees(),
            &[active_registration, missing_registration]
        );
    }

    #[test]
    fn garbage_collect_uses_only_tracked_lfs_paths_for_reachability() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let repo = temp.path().join("repo");
        let tracked_lfs = object_for_bytes(b"tracked LFS object");
        let tracked_non_lfs = object_for_bytes(b"tracked non-LFS object");
        let ignored = object_for_bytes(b"ignored generated object");
        let empty = object_for_bytes(b"");
        initialize_git_worktree(&repo);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(&repo.join(".gitignore"), b"generated/\n");
        write_file(
            &repo.join("asset/keep.bin"),
            LfsPointer::new(tracked_lfs.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        write_file(
            &repo.join("docs/pointer.txt"),
            LfsPointer::new(tracked_non_lfs.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        write_file(
            &repo.join("generated/pointer.bin"),
            LfsPointer::new(ignored.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        write_file(&repo.join("asset/empty.bin"), b"");
        git_add(
            &repo,
            &[
                Path::new(".gitattributes"),
                Path::new(".gitignore"),
                Path::new("asset/empty.bin"),
                Path::new("asset/keep.bin"),
                Path::new("docs/pointer.txt"),
            ],
        );
        write_file(&layout.object_path(&tracked_lfs), b"tracked LFS object");
        write_file(
            &layout.object_path(&tracked_non_lfs),
            b"tracked non-LFS object",
        );
        write_file(&layout.object_path(&ignored), b"ignored generated object");
        write_file(&layout.object_path(&empty), b"");
        let registration =
            LocalCacheWorktreeRegistration::new("github-main:owner/repo", &repo, repo.join(".git"))
                .expect("registration should validate");
        layout
            .register_worktree(registration)
            .expect("worktree should register");

        let report = layout
            .garbage_collect(false, false)
            .expect("garbage collection should finish");

        assert_eq!(
            report
                .retained_objects
                .iter()
                .map(|object| object.oid.clone())
                .collect::<Vec<_>>(),
            vec![tracked_lfs.oid.clone()]
        );
        assert_eq!(report.deleted_objects.len(), 3);
        assert!(layout.object_path(&tracked_lfs).exists());
        assert!(!layout.object_path(&tracked_non_lfs).exists());
        assert!(!layout.object_path(&ignored).exists());
        assert!(!layout.object_path(&empty).exists());
    }

    #[test]
    fn garbage_collect_handles_nul_delimited_tracked_lfs_paths() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let repo = temp.path().join("repo");
        let object = object_for_bytes(b"newline path object");
        let relative_path = Path::new("asset/line\nbreak.bin");
        initialize_git_worktree(&repo);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join(relative_path),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        git_add(&repo, &[Path::new(".gitattributes"), relative_path]);
        write_file(&layout.object_path(&object), b"newline path object");
        layout
            .register_worktree(
                LocalCacheWorktreeRegistration::new(
                    "github-main:owner/repo",
                    &repo,
                    repo.join(".git"),
                )
                .expect("registration should validate"),
            )
            .expect("worktree should register");

        let report = layout
            .garbage_collect(false, false)
            .expect("garbage collection should finish");

        assert_eq!(report.retained_objects.len(), 1);
        assert_eq!(report.retained_objects[0].oid, object.oid);
        assert!(layout.object_path(&object).exists());
    }

    #[test]
    fn garbage_collect_prunes_unavailable_worktree_only_when_explicitly_requested() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let replaced_repo = temp.path().join("replaced-repo");
        let bytes = b"file-replaced worktree cache object";
        let object = object_for_bytes(bytes);
        let registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/replaced",
            &replaced_repo,
            temp.path().join("replaced-repo/.git"),
        )
        .expect("registration should validate");

        write_file(&layout.object_path(&object), bytes);
        write_file(&replaced_repo, b"not a directory");
        layout
            .register_worktree(registration.clone())
            .expect("worktree should register");

        let report = layout
            .garbage_collect(false, true)
            .expect("file-replaced worktree should prune");

        assert_eq!(report.active_worktree_count, 0);
        assert_eq!(report.unavailable_worktrees, vec![registration.clone()]);
        assert_eq!(report.pruned_worktrees, vec![registration]);
        assert!(report.protected_objects.is_empty());
        assert_eq!(report.unreferenced_objects.len(), 1);
        assert!(!layout.object_path(&object).exists());
        assert!(
            layout
                .load_worktree_registry()
                .expect("registry should reload")
                .is_empty()
        );
    }

    #[test]
    fn garbage_collect_ignores_untracked_pointer_in_git_metadata() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let repo = temp.path().join("repo");
        let bytes = b"git file referenced cache object";
        let object = object_for_bytes(bytes);
        let registration =
            LocalCacheWorktreeRegistration::new("github-main:owner/repo", &repo, repo.join(".git"))
                .expect("active registration should validate");

        initialize_git_worktree(&repo);
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &repo.join(".git/lfscloud-pointer"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        layout
            .register_worktree(registration)
            .expect("worktree should register");

        let report = layout
            .garbage_collect(false, false)
            .expect("garbage collection should finish");

        assert_eq!(report.active_worktree_count, 1);
        assert_eq!(report.unreferenced_objects.len(), 1);
        assert_eq!(report.unreferenced_objects[0].oid, object.oid);
        assert!(!layout.object_path(&object).exists());
    }

    #[cfg(unix)]
    #[test]
    fn garbage_collect_ignores_symlinked_directory_when_collecting_pointers() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let repo = temp.path().join("repo");
        let outside = temp.path().join("outside");
        let bytes = b"symlinked pointer cache object";
        let object = object_for_bytes(bytes);
        let registration =
            LocalCacheWorktreeRegistration::new("github-main:owner/repo", &repo, repo.join(".git"))
                .expect("active registration should validate");

        initialize_git_worktree(&repo);
        fs::create_dir_all(&outside).expect("outside directory should be created");
        std::os::unix::fs::symlink(&outside, repo.join("linked"))
            .expect("directory symlink should be created");
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &outside.join("model.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        layout
            .register_worktree(registration)
            .expect("worktree should register");

        let report = layout
            .garbage_collect(false, false)
            .expect("garbage collection should finish");

        assert_eq!(report.unreferenced_objects.len(), 1);
        assert_eq!(report.unreferenced_objects[0].oid, object.oid);
        assert!(!layout.object_path(&object).exists());
    }

    #[test]
    fn garbage_collect_dry_run_leaves_cache_objects_and_registry_untouched() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let missing_repo = temp.path().join("missing-repo");
        let bytes = b"dry run unreferenced cache object";
        let object = object_for_bytes(bytes);
        let missing_registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/missing",
            &missing_repo,
            missing_repo.join(".git"),
        )
        .expect("missing registration should validate");

        write_file(&layout.object_path(&object), bytes);
        layout
            .register_worktree(missing_registration.clone())
            .expect("missing worktree should register");

        let report = layout
            .garbage_collect(true, false)
            .expect("dry-run garbage collection should finish");

        assert!(report.dry_run);
        assert_eq!(report.active_worktree_count, 0);
        assert_eq!(
            report.unavailable_worktrees,
            vec![missing_registration.clone()]
        );
        assert!(report.pruned_worktrees.is_empty());
        assert_eq!(report.protected_objects.len(), 1);
        assert!(report.unreferenced_objects.is_empty());
        assert!(report.deleted_objects.is_empty());
        assert!(layout.object_path(&object).exists());
        assert_eq!(
            layout
                .load_worktree_registry()
                .expect("registry should reload")
                .worktrees(),
            &[missing_registration]
        );
    }

    #[test]
    fn garbage_collect_reports_invalid_cache_paths_without_deleting_them() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let invalid_cache_path = layout.objects_dir().join("zz/zz/not-a-sha256");
        write_file(&invalid_cache_path, b"invalid cache payload");

        let report = layout
            .garbage_collect(false, false)
            .expect("garbage collection should skip invalid paths");

        assert!(report.retained_objects.is_empty());
        assert!(report.unreferenced_objects.is_empty());
        assert_eq!(report.skipped_cache_paths, vec![invalid_cache_path.clone()]);
        assert!(invalid_cache_path.exists());
    }

    #[test]
    fn worktree_registration_rejects_relative_paths() {
        let error =
            LocalCacheWorktreeRegistration::new("github-main:owner/repo", "repo", "/repo/.git")
                .expect_err("relative worktree root should fail");

        assert!(matches!(
            error,
            LocalCacheError::InvalidWorktreeRegistration {
                field: "worktree_root",
                ..
            }
        ));
    }

    #[test]
    fn worktree_registry_rejects_future_schema_version() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        write_file(
            &layout.worktree_registry_path(),
            br#"{"version": 999, "worktrees": []}"#,
        );

        let error = layout
            .load_worktree_registry()
            .expect_err("future registry version should fail");

        assert!(matches!(
            error,
            LocalCacheError::UnsupportedWorktreeRegistryVersion { version: 999, .. }
        ));
    }

    #[test]
    fn worktree_registry_rejects_duplicate_registered_roots() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let worktree_root = temp.path().join("repo");
        let first_git_dir = temp.path().join("repo/.git");
        let second_git_dir = temp.path().join("repo/.git/worktrees/duplicate");
        let registry = LocalCacheWorktreeRegistry {
            version: WORKTREE_REGISTRY_VERSION,
            worktrees: vec![
                LocalCacheWorktreeRegistration {
                    repository_id: "github-main:owner/repo".to_owned(),
                    worktree_root: worktree_root.clone(),
                    git_dir: first_git_dir,
                },
                LocalCacheWorktreeRegistration {
                    repository_id: "github-main:owner/repo".to_owned(),
                    worktree_root,
                    git_dir: second_git_dir,
                },
            ],
        };
        write_file(
            &layout.worktree_registry_path(),
            &serde_json::to_vec_pretty(&registry).expect("registry fixture should encode"),
        );

        let error = layout
            .load_worktree_registry()
            .expect_err("duplicate registry roots should fail");

        assert!(matches!(
            error,
            LocalCacheError::InvalidWorktreeRegistration {
                field: "worktree_root",
                ..
            }
        ));
    }
}
