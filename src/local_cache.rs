//! Local content-addressed cache path layout.
//!
//! The local cache is client-side state, separate from the server metadata
//! database and storage-provider object mapping. Paths are derived only from a
//! validated Git LFS SHA-256 object identifier so identical content can be
//! shared across repositories and worktrees before later hydration and garbage
//! collection logic reasons about reachability.

use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::{LfsObject, LfsObjectSize, LfsOid};

/// Default directory name used below a user's home directory for local state.
pub const DEFAULT_LOCAL_CACHE_HOME_DIR: &str = ".lfs-cloud";
/// Directory below the local cache root that stores immutable object bytes.
pub const LOCAL_CACHE_OBJECTS_DIR: &str = "objects";

const OBJECT_SHARD_WIDTH: usize = 2;
const OBJECT_SHARD_LEVELS: usize = 2;
const OBJECT_SHARD_PREFIX_LENGTH: usize = OBJECT_SHARD_WIDTH * OBJECT_SHARD_LEVELS;

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

        if !path.is_file() {
            return Err(LocalCacheError::MissingCacheObject {
                oid: object.oid.clone(),
                size: object.size,
                path,
            });
        }

        verify_file_object(&path, object)
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

        if cache_path.exists() {
            self.verify_object(object)?;

            return Ok(LocalCacheIngest {
                object: object.clone(),
                source_path,
                cache_path,
                status: LocalCacheIngestStatus::AlreadyCached,
            });
        }

        if !source_path.is_file() {
            return Err(LocalCacheError::MissingSourceObject {
                oid: object.oid.clone(),
                size: object.size,
                path: source_path,
            });
        }

        verify_file_object(&source_path, object)?;
        let status = copy_verified_object_to_cache(&source_path, &cache_path, object)?;
        self.verify_object(object)?;

        Ok(LocalCacheIngest {
            object: object.clone(),
            source_path,
            cache_path,
            status,
        })
    }
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
    io::copy(&mut source, &mut temp).map_err(|source| LocalCacheError::Io {
        context: "failed to copy object into temporary cache file",
        path: cache_path.to_path_buf(),
        source,
    })?;
    temp.as_file_mut()
        .flush()
        .map_err(|source| LocalCacheError::Io {
            context: "failed to flush temporary cache object",
            path: cache_path.to_path_buf(),
            source,
        })?;
    verify_file_object(temp.path(), object)?;

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
        LfsOid::new(hex_digest(&hasher.finalize())).expect("SHA-256 hex should be valid"),
        LfsObjectSize::new(total_size),
    ))
}

#[cfg(test)]
fn sha256_hex(bytes: &[u8]) -> String {
    hex_digest(&Sha256::digest(bytes))
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);

    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }

    output
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        str::FromStr,
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
}
