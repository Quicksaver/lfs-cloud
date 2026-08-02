//! Command-line parsing and dispatch for LFS Cloud.
//!
//! This module keeps the binary target small while making CLI behavior
//! testable without binding sockets. The process entry point owns global
//! tracing initialization, while parser and dispatch helpers stay side-effect
//! free for focused tests.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    future::Future,
    io::{self, BufRead, IsTerminal, Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::Context;
use clap::{Args, Parser, Subcommand, ValueEnum};
use reqwest::{
    Body as ReqwestBody, Client, StatusCode as HttpStatusCode,
    header::{CONTENT_LENGTH, HeaderMap as ReqwestHeaderMap, HeaderName, HeaderValue},
    redirect::Policy,
};
use tokio_util::io::ReaderStream;
use url::Url;

use crate::child_process::{
    ChildProcessError, ChildProcessOptions, ChildProcessOutput, PipeCapture,
    configure_process_tree, wait_for_child,
};
use crate::config_edit::{
    EditOutcome, EditableServerConfig, RemoveOutcome, RepositoryProviderValues, RepositoryValues,
    StorageProviderValues,
};
use crate::git_output::{GitPathOutputError, parse_lfs_filter_attribute_paths};
use crate::{
    CliError, CliResult, SanitizedMessage,
    git::redacted_url_for_display,
    process_output::{command_status_text, truncate_with_ellipsis},
};
use crate::{
    GITHUB_PERSONAL_ACCESS_TOKEN_LOGIN_PATH, GitCredentialApproval, GitCredentialLookup,
    GitCredentialRejection, GitLfsConfigChange, GitLfsConfigTarget, GitLfsHistoryPointers,
    GitLfsMigrationDiscovery, GitLfsSourceEndpointSource, GitRemote, GitRepository,
    LFS_BASIC_TRANSFER, LFS_POINTER_SIZE_CUTOFF, LFS_SESSION_REVOKE_PATH, LfsBatchAction,
    LfsBatchHashAlgorithm, LfsBatchOperation, LfsBatchRequest, LfsBatchResponse, LfsInitRoute,
    LfsObject, LfsPointer, LfsSessionToken, LocalCacheDehydration, LocalCacheDehydrationStatus,
    LocalCacheGarbageCollection, LocalCacheGarbageCollectionObject, LocalCacheIngest,
    LocalCacheIngestStatus, LocalCacheLayout, LocalCacheMaterialization,
    LocalCacheMaterializationStatus, LocalCacheWorktreeRegistration,
    LocalMigrationObjectAvailability, MigrationError, MigrationFetchMode, MigrationSourceFetch,
    ServeOptions, ServerConfig, StorageProviderConfig, TracingConfig,
    check_local_migration_objects, discover_git_lfs_migration_from_remote,
    enumerate_current_checkout_lfs_pointers, enumerate_fetched_ref_lfs_pointers_for_remote,
    enumerate_selected_ref_lfs_pointers, fetch_migration_git_refs,
    fetch_missing_migration_objects_from_remote,
    fetch_missing_migration_objects_from_remote_at_endpoint, init_tracing,
};

mod authentication;
mod cache;
mod command;
mod configuration;
mod http;
mod init;
mod migration;
mod status;

pub use command::run_from_env;

use authentication::*;
use cache::*;
use command::*;
use configuration::*;
use http::*;
use init::*;
use migration::*;
use status::*;

#[cfg(test)]
pub(super) mod test_support {
    use std::{fs, path::Path, process::Command as ProcessCommand};

    use sha2::{Digest, Sha256};

    use crate::{LfsObject, LfsObjectSize, LfsOid, LfsPointer};

    pub(super) fn status_config(public_url: &str) -> String {
        format!(
            r#"
server:
  host: 127.0.0.1
  port: 8080
  public_url: {public_url}

repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    personal_access_token: github-pat

storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
    root_folder_id: root-folder

repositories:
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    provider_repository_id: "8675309"
    storage_provider: drive-user-a
"#
        )
    }

    pub(super) fn object_for_bytes(bytes: &[u8]) -> LfsObject {
        let oid = LfsOid::new(format!("{:x}", Sha256::digest(bytes)))
            .expect("test SHA-256 object id should parse");

        LfsObject::new(
            oid,
            LfsObjectSize::new(u64::try_from(bytes.len()).expect("test bytes should fit u64")),
        )
    }

    pub(super) fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("file parent should be created");
        }
        fs::write(path, contents).expect("test file should be written");
    }

    pub(super) fn write_git_lfs_source_object(repo: &Path, object: &LfsObject, contents: &[u8]) {
        write_git_lfs_source_object_in(
            &repo.join(".git").join("lfs").join("objects"),
            object,
            contents,
        );
    }

    pub(super) fn write_git_lfs_source_object_in(
        objects_dir: &Path,
        object: &LfsObject,
        contents: &[u8],
    ) {
        let oid = object.oid.as_hex();
        let path = objects_dir.join(&oid[..2]).join(&oid[2..4]).join(oid);
        write_file(&path, contents);
    }

    pub(super) fn init_git_repo_with_origin(repo: &Path) {
        fs::create_dir_all(repo).expect("temporary repository directory should be created");
        run_git(repo, &["init"]);
        run_git(
            repo,
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
    }

    pub(super) fn stage_lfs_pointer(repo: &Path, relative_path: &str, object: &LfsObject) {
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join(relative_path),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        run_git(repo, &["add", ".gitattributes", relative_path]);
    }

    pub(super) fn require_git() {
        let output = ProcessCommand::new("git")
            .arg("--version")
            .output()
            .expect("Git is required to run CLI integration tests");
        assert!(
            output.status.success(),
            "Git is required to run CLI integration tests: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    pub(super) fn require_git_lfs() {
        let output = ProcessCommand::new("git")
            .args(["lfs", "version"])
            .output()
            .expect("Git LFS is required to run CLI integration tests");
        assert!(
            output.status.success(),
            "Git LFS is required to run CLI integration tests: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    pub(super) fn run_git(current_dir: &Path, args: &[&str]) {
        let output = ProcessCommand::new("git")
            .args(args)
            .current_dir(current_dir)
            .output()
            .expect("git command should start");

        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    pub(super) fn read_git_config(current_dir: &Path, args: &[&str]) -> String {
        let output = ProcessCommand::new("git")
            .args(args)
            .current_dir(current_dir)
            .output()
            .expect("git config command should start");

        assert!(
            output.status.success(),
            "git config command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8(output.stdout)
            .expect("git config output should be UTF-8")
            .trim_end()
            .to_owned()
    }
}
