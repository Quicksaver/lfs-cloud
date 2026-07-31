//! Migration discovery helpers for existing Git LFS repositories.
//!
//! Migration planning starts by inspecting the current repository without
//! writing to Git config, the worktree, the local cache, or any storage
//! provider. This module owns that read-only boundary so later migration steps
//! can build dry-run and transfer plans from one consistent snapshot.

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
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
    thread,
    time::Duration,
};

use fs4::FileExt;
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::child_process::{
    ChildProcessError, ChildProcessOptions, PipeCapture, configure_process_tree,
    terminate_process_tree, wait_for_child,
};
use crate::git_output::{
    GitPathOutputError, parse_lfs_filter_attribute_paths,
    safe_git_relative_path as parse_safe_git_relative_path,
};
use crate::process_output::{command_status_text, truncated_lossy_message};
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
const MINIMUM_HISTORICAL_SCAN_GIT_VERSION: GitVersion = GitVersion::new(2, 40, 0);
const MINIMUM_HISTORICAL_SCAN_GIT_VERSION_TEXT: &str = "2.40.0";
/// Default number of migration objects uploaded concurrently.
pub const DEFAULT_MIGRATION_UPLOAD_CONCURRENCY: usize = 4;
const MIGRATION_UPLOAD_CHECKPOINT_VERSION: u32 = 2;

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

include!("attributes.rs");
include!("checkout.rs");
include!("discovery.rs");
include!("fetch.rs");
include!("history.rs");
include!("local_objects.rs");
include!("process.rs");
include!("upload.rs");

#[cfg(test)]
mod test_support;
