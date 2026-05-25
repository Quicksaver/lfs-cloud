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
    path::{Path, PathBuf},
};

use fs4::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{LfsObject, LfsObjectError, LfsObjectSize, LfsOid, LfsPointer};

/// Default directory name used below a user's home directory for local state.
pub const DEFAULT_LOCAL_CACHE_HOME_DIR: &str = ".lfs-cloud";
/// Directory below the local cache root that stores immutable object bytes.
pub const LOCAL_CACHE_OBJECTS_DIR: &str = "objects";
/// JSON registry file below the local cache root that tracks known worktrees.
pub const LOCAL_CACHE_WORKTREES_FILE: &str = "worktrees.json";

const OBJECT_SHARD_WIDTH: usize = 2;
const OBJECT_SHARD_LEVELS: usize = 2;
const OBJECT_SHARD_PREFIX_LENGTH: usize = OBJECT_SHARD_WIDTH * OBJECT_SHARD_LEVELS;
const WORKTREE_REGISTRY_LOCK_FILE: &str = "worktrees.json.lock";
const WORKTREE_REGISTRY_VERSION: u32 = 1;
const MAX_LFS_POINTER_FILE_SIZE: u64 = 64 * 1024;
#[cfg(unix)]
const DEFAULT_MATERIALIZED_FILE_MODE: u32 = 0o644;
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
        "Git LFS pointer at {} is too large to hydrate safely: {size} bytes exceeds {max_size} bytes",
        path.display()
    )]
    PointerFileTooLarge {
        /// Pointer file path.
        path: PathBuf,
        /// Actual file size in bytes.
        size: u64,
        /// Maximum pointer file size accepted by hydration.
        max_size: u64,
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
/// `lfs-cloud gc` uses this kind of record to know which worktrees must be
/// inspected before deleting cached objects. Paths are required to be absolute
/// so the registry does not depend on a future process's current directory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalCacheWorktreeRegistration {
    /// Stable repository mapping ID or provider-derived repository identity.
    pub repository_id: String,
    /// Absolute worktree root path.
    pub worktree_root: PathBuf,
    /// Absolute Git directory path for the worktree.
    pub git_dir: PathBuf,
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
        if self.version != WORKTREE_REGISTRY_VERSION {
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
    /// Destination was created using the platform's copy-on-write path.
    ///
    /// Some platform tools may silently fall back to a normal copy while still
    /// reporting success, so this status records the selected strategy rather
    /// than a guaranteed backend storage layout.
    CopyOnWriteAttempted,
    /// Destination was created by copying bytes because CoW was unavailable.
    Copied,
}

/// Deterministic filesystem layout for local Git LFS object cache paths.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
///
/// use lfs_cloud::{LocalCacheLayout, LfsOid};
///
/// let layout = LocalCacheLayout::new("/home/alice/.lfs-cloud");
/// let oid = LfsOid::new(
///     "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
/// )
/// .expect("example OID should be valid");
///
/// assert_eq!(
///     layout.object_path_for_oid(&oid),
///     PathBuf::from(
///         "/home/alice/.lfs-cloud/objects/01/23/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
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
    /// normally `~/.lfs-cloud`.
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

    /// Returns the cache root directory, normally `~/.lfs-cloud`.
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
    /// before replacing it.
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
        let destination_path = destination_path.as_ref();
        let verified = self.verify_object(object)?;

        materialize_verified_object(&verified, destination_path, MaterializationMode::NoReplace)
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
        let pointer_path = pointer_path.as_ref();
        let pointer = read_lfs_pointer_file(pointer_path)?;
        let verified = self.verify_object(&pointer.object)?;

        materialize_verified_object(
            &verified,
            pointer_path,
            MaterializationMode::ReplaceMatchingPointer,
        )
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
                let registry: LocalCacheWorktreeRegistry =
                    serde_json::from_reader(file).map_err(|source| {
                        LocalCacheError::WorktreeRegistryJson {
                            context: "failed to decode local cache worktree registry",
                            path: path.clone(),
                            source,
                        }
                    })?;
                registry.validate_for_path(&path)?;

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

    fn worktree_registry_lock_path(&self) -> PathBuf {
        self.root.join(WORKTREE_REGISTRY_LOCK_FILE)
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
    let metadata = fs::metadata(path).map_err(|source| LocalCacheError::Io {
        context: "failed to inspect Git LFS pointer file",
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_LFS_POINTER_FILE_SIZE {
        return Err(LocalCacheError::PointerFileTooLarge {
            path: path.to_path_buf(),
            size: metadata.len(),
            max_size: MAX_LFS_POINTER_FILE_SIZE,
        });
    }

    let file = File::open(path).map_err(|source| LocalCacheError::Io {
        context: "failed to read Git LFS pointer file",
        path: path.to_path_buf(),
        source,
    })?;
    let mut contents = Vec::new();
    file.take(MAX_LFS_POINTER_FILE_SIZE + 1)
        .read_to_end(&mut contents)
        .map_err(|source| LocalCacheError::Io {
            context: "failed to read Git LFS pointer file",
            path: path.to_path_buf(),
            source,
        })?;
    let size = u64::try_from(contents.len()).unwrap_or(u64::MAX);
    if size > MAX_LFS_POINTER_FILE_SIZE {
        return Err(LocalCacheError::PointerFileTooLarge {
            path: path.to_path_buf(),
            size,
            max_size: MAX_LFS_POINTER_FILE_SIZE,
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

fn materialize_verified_object(
    verified: &VerifiedLocalCacheObject,
    destination_path: &Path,
    mode: MaterializationMode,
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

    publish_materialized_file(temp, destination_path, mode, &verified.object)?;
    // The final verification proves the path currently contains the expected
    // object. If an uncoordinated writer races this local worktree path, the
    // caller may still receive an integrity error after publication.
    let materialized = verify_file_object(destination_path, &verified.object)?;

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
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            if metadata.len() != object.size.bytes() {
                return Ok(ExistingDestinationStatus::Different);
            }

            match verify_file_object(path, object) {
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
    let mut temp = tempfile::NamedTempFile::new_in(destination_parent).map_err(|source| {
        LocalCacheError::Io {
            context: "failed to create temporary materialized object",
            path: destination_parent.to_path_buf(),
            source,
        }
    })?;

    let status = if copy_on_write_clone(cache_path, temp.path())? {
        verify_file_object(temp.path(), object)?;
        LocalCacheMaterializationStatus::CopyOnWriteAttempted
    } else {
        copy_cache_object_to_temporary_file(cache_path, &mut temp, object)?;
        LocalCacheMaterializationStatus::Copied
    };

    Ok((temp, status))
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
) -> LocalCacheResult<()> {
    match mode {
        MaterializationMode::NoReplace => {
            set_materialized_file_mode(
                temp.path(),
                destination_path,
                DEFAULT_MATERIALIZED_FILE_MODE,
            )?;
            match temp.persist_noclobber(destination_path) {
                Ok(_) => Ok(()),
                Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                    match verify_file_object(destination_path, object) {
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
            // This second read is a best-effort local race check before the
            // atomic replacement. Uncoordinated worktree writers can still
            // change the destination after this point.
            let pointer = read_lfs_pointer_file(destination_path)?;
            if pointer.object != *object {
                return Err(LocalCacheError::MaterializationTargetExists {
                    oid: object.oid.clone(),
                    size: object.size,
                    path: destination_path.to_path_buf(),
                });
            }
            let replacement_mode = existing_file_mode(destination_path)?;
            set_materialized_file_mode(temp.path(), destination_path, replacement_mode)?;
            temp.persist(destination_path)
                .map_err(|error| LocalCacheError::Io {
                    context: "failed to publish materialized object",
                    path: destination_path.to_path_buf(),
                    source: error.error,
                })?;
            Ok(())
        }
    }
}

#[cfg(unix)]
fn existing_file_mode(path: &Path) -> LocalCacheResult<u32> {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o777)
        .map_err(|source| LocalCacheError::Io {
            context: "failed to inspect materialization destination permissions",
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(not(unix))]
fn existing_file_mode(_path: &Path) -> LocalCacheResult<()> {
    Ok(())
}

#[cfg(unix)]
fn set_materialized_file_mode(
    temp_path: &Path,
    destination_path: &Path,
    mode: u32,
) -> LocalCacheResult<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(temp_path, fs::Permissions::from_mode(mode)).map_err(|source| {
        LocalCacheError::Io {
            context: "failed to set temporary materialized object permissions",
            path: destination_path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_materialized_file_mode(
    _temp_path: &Path,
    _destination_path: &Path,
    _mode: (),
) -> LocalCacheResult<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn copy_on_write_clone(source_path: &Path, destination_path: &Path) -> LocalCacheResult<bool> {
    // `NamedTempFile` keeps an open descriptor for cleanup, but this macOS-only
    // path intentionally operates by pathname and verifies that path before
    // publication. Fallback copying writes through the open handle.
    let output = std::process::Command::new("/bin/cp")
        .arg("-c")
        .arg(source_path)
        .arg(destination_path)
        .output()
        .map_err(|source| LocalCacheError::Io {
            context: "failed to invoke macOS copy-on-write clone primitive",
            path: destination_path.to_path_buf(),
            source,
        })?;

    if output.status.success() {
        return Ok(true);
    }

    tracing::debug!(
        source = %source_path.display(),
        destination = %destination_path.display(),
        status = %output.status,
        stderr = %String::from_utf8_lossy(&output.stderr),
        "macOS copy-on-write clone command failed; falling back to verified copy"
    );

    Ok(false)
}

#[cfg(not(target_os = "macos"))]
fn copy_on_write_clone(_source_path: &Path, _destination_path: &Path) -> LocalCacheResult<bool> {
    Ok(false)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
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

fn copy_verified_object_to_cache(
    source_path: &Path,
    cache_path: &Path,
    object: &LfsObject,
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

    let mut source = File::open(source_path).map_err(|source| LocalCacheError::Io {
        context: "failed to open Git LFS source object",
        path: source_path.to_path_buf(),
        source,
    })?;
    let mut temp =
        tempfile::NamedTempFile::new_in(cache_parent).map_err(|source| LocalCacheError::Io {
            context: "failed to create temporary cache object",
            path: cache_parent.to_path_buf(),
            source,
        })?;
    copy_and_verify_object(&mut source, source_path, &mut temp, cache_path, object)?;
    // This deliberately stops at `flush()`: the local cache is recoverable
    // derived state, and every cache reuse revalidates object identity.
    // Avoiding `sync_all()` keeps large-object ingest from paying a durable
    // write latency cost on the hot path.
    temp.as_file_mut()
        .flush()
        .map_err(|source| LocalCacheError::Io {
            context: "failed to flush temporary cache object",
            path: cache_path.to_path_buf(),
            source,
        })?;

    match temp.persist_noclobber(cache_path) {
        Ok(_) => Ok(LocalCacheIngestStatus::Copied),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            verify_file_object(cache_path, object)?;
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
) -> LocalCacheResult<()> {
    let mut hasher = Sha256::new();
    let mut total_size = 0u64;
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|source| LocalCacheError::Io {
                context: "failed to read Git LFS source object",
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
    let mut file = File::open(path).map_err(|source| LocalCacheError::Io {
        context: "failed to open object for hashing",
        path: path.to_path_buf(),
        source,
    })?;
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

#[cfg(test)]
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        str::FromStr,
        sync::mpsc,
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
            PathBuf::from("/home/alice/.lfs-cloud/objects")
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
        assert!(matches!(
            materialization.status,
            LocalCacheMaterializationStatus::Copied
                | LocalCacheMaterializationStatus::CopyOnWriteAttempted
        ));
        assert_eq!(
            fs::read(&materialization.destination_path)
                .expect("materialized destination should be readable"),
            bytes
        );
    }

    #[cfg(unix)]
    #[test]
    fn materialize_object_uses_worktree_file_mode_for_new_files() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"cache bytes with normal worktree permissions";
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
        assert_eq!(mode, DEFAULT_MATERIALIZED_FILE_MODE);
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
        assert!(matches!(
            materialization.status,
            LocalCacheMaterializationStatus::Copied
                | LocalCacheMaterializationStatus::CopyOnWriteAttempted
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
        let bytes = vec![b'x'; (MAX_LFS_POINTER_FILE_SIZE + 1) as usize];
        write_file(&destination, &bytes);

        let error = layout
            .hydrate_pointer_file(&destination)
            .expect_err("oversized non-pointer worktree content should not hydrate");

        assert!(matches!(
            error,
            LocalCacheError::PointerFileTooLarge { path, size, max_size }
                if path == destination
                    && size == MAX_LFS_POINTER_FILE_SIZE + 1
                    && max_size == MAX_LFS_POINTER_FILE_SIZE
        ));
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
                .contains("\"version\": 1")
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
