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
    process::{Command, Stdio},
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

use crate::git_output::{GitPathOutputError, parse_lfs_filter_attribute_paths};
use crate::{
    LFS_POINTER_SIZE_CUTOFF, LfsObject, LfsObjectError, LfsObjectSize, LfsOid, LfsPointer,
    child_process::{
        ChildProcessError, ChildProcessOptions, PipeCapture, configure_process_tree, wait_for_child,
    },
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
const DEFAULT_MATERIALIZED_FILE_MODE: u32 = 0;

#[cfg(test)]
mod test_support;

mod dehydration;
mod error;
mod garbage_collection;
mod ingest;
mod layout;
mod locking;
mod materialization;
mod object_io;
mod registry;
mod types;

pub use error::{LocalCacheError, LocalCacheResult};
pub use layout::LocalCacheLayout;
pub use registry::{LocalCacheWorktreeRegistration, LocalCacheWorktreeRegistry};
pub use types::{
    LocalCacheDehydration, LocalCacheDehydrationStatus, LocalCacheGarbageCollection,
    LocalCacheGarbageCollectionObject, LocalCacheIngest, LocalCacheIngestStatus,
    LocalCacheMaterialization, LocalCacheMaterializationStatus,
    LocalCacheWorktreeRegistrationChange, LocalCacheWorktreeRegistrationStatus,
    VerifiedLocalCacheObject,
};

use garbage_collection::*;
use materialization::*;
use object_io::*;
