//! Local content-addressed cache path layout.
//!
//! The local cache is client-side state, separate from the server metadata
//! database and storage-provider object mapping. Paths are derived only from a
//! validated Git LFS SHA-256 object identifier so identical content can be
//! shared across repositories and worktrees before later hydration and garbage
//! collection logic reasons about reachability.

use std::path::{Path, PathBuf};

use crate::{LfsObject, LfsOid};

/// Default directory name used below a user's home directory for local state.
pub const DEFAULT_LOCAL_CACHE_HOME_DIR: &str = ".lfs-cloud";
/// Directory below the local cache root that stores immutable object bytes.
pub const LOCAL_CACHE_OBJECTS_DIR: &str = "objects";

const OBJECT_SHARD_WIDTH: usize = 2;

/// Deterministic filesystem layout for local Git LFS object cache paths.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
/// use std::str::FromStr;
///
/// use lfs_cloud::{LocalCacheLayout, LfsOid};
///
/// let layout = LocalCacheLayout::new("/home/alice/.lfs-cloud");
/// let oid = LfsOid::from_str(
///     "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
/// )?;
///
/// assert_eq!(
///     layout.object_path_for_oid(&oid),
///     PathBuf::from(
///         "/home/alice/.lfs-cloud/objects/01/23/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
///     )
/// );
/// # Ok::<(), lfs_cloud::LfsObjectError>(())
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
        let first_shard = &hex[..OBJECT_SHARD_WIDTH];
        let second_shard = &hex[OBJECT_SHARD_WIDTH..OBJECT_SHARD_WIDTH * 2];

        self.objects_dir()
            .join(first_shard)
            .join(second_shard)
            .join(hex)
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, str::FromStr};

    use crate::{LfsObjectSize, local_cache::DEFAULT_LOCAL_CACHE_HOME_DIR};

    use super::*;

    fn oid(value: &str) -> LfsOid {
        LfsOid::from_str(value).expect("test OID should be valid")
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
}
