//! Shared cache directory layout and object verification.

use super::*;

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
    pub(super) root: PathBuf,
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
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use crate::local_cache::test_support::*;
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
}
