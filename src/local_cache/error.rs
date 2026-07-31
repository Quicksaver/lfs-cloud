//! Errors returned by local cache operations.

use super::*;

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

    /// A worktree path opened for pointer parsing was not a regular file.
    #[error("Git LFS pointer path is not a regular file: {}", path.display())]
    PointerPathNotRegularFile {
        /// Pointer path whose opened filesystem object was not a regular file.
        path: PathBuf,
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
