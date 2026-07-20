//! Reusable integration-test fixtures for LFS Cloud.
//!
//! Each integration test crate can import these helpers with `mod support;`.

#![allow(
    dead_code,
    reason = "integration test crates import this shared module independently and use different helper subsets"
)]

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::Mutex,
};

use lfscloud::{
    LfsObject, LfsObjectSize, LfsOid, LfsPointer, ProviderFuture, RepositoryAuthentication,
    RepositoryAuthorization, RepositoryIdentity, RepositoryPermission, RepositoryProvider,
    RepositoryProviderError, ServerError, ServerResult, StorageDeleteOutcome, StorageError,
    StorageProvider, StorageResult, StoredObject,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// Stable SHA-256 test object ID using only the `a` nibble.
pub const TEST_OID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Stable SHA-256 test object ID using only the `b` nibble.
pub const TEST_OID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// Temporary Git repository for integration tests.
pub struct TempGitRepo {
    root: TempDir,
}

impl TempGitRepo {
    /// Creates an empty temporary repository on the `main` branch.
    pub fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary repository directory should be created");
        let repo = Self { root };

        let init = repo.try_git(["init", "--initial-branch", "main"]);
        if !init.status.success() {
            if String::from_utf8_lossy(&init.stderr).contains("unknown option") {
                // Git 2.28+ supports --initial-branch. Older versions need a
                // rename after repository creation so tests can assert `main`.
                repo.git(["init"]);
                repo.git(["branch", "-M", "main"]);
            } else {
                assert_git_success(&init);
            }
        }

        repo.git(["config", "user.email", "lfscloud@example.invalid"]);
        repo.git(["config", "user.name", "LFS Cloud Test"]);

        repo
    }

    /// Returns the repository worktree path.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.root.path()
    }

    /// Writes a UTF-8 file relative to the repository root.
    pub fn write_file(&self, relative_path: impl AsRef<Path>, contents: &str) -> PathBuf {
        let path = self.root.path().join(relative_path);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("test file parent directory should be created");
        }

        fs::write(&path, contents).expect("test file should be written");
        path
    }

    /// Reads a UTF-8 file relative to the repository root.
    #[must_use]
    pub fn read_file(&self, relative_path: impl AsRef<Path>) -> String {
        fs::read_to_string(self.root.path().join(relative_path))
            .expect("test file should be readable")
    }

    /// Runs a Git command in the repository and asserts it succeeds.
    pub fn git<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.try_git(args);
        assert_git_success(&output);
        output
    }

    fn try_git<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new("git")
            .args(args)
            .current_dir(self.root.path())
            .output()
            .expect("git command should start")
    }

    /// Stages all changes and creates a commit.
    pub fn commit_all(&self, message: &str) {
        self.git(["add", "."]);
        self.git(["commit", "-m", message]);
    }
}

/// Creates an LFS object identity from a test SHA-256 hex string and byte size.
#[must_use]
pub fn lfs_object(oid_hex: &str, size: u64) -> LfsObject {
    LfsObject::new(
        LfsOid::new(oid_hex).expect("test object oid should be valid"),
        LfsObjectSize::new(size),
    )
}

/// Creates an LFS object identity for exact byte contents.
#[must_use]
pub fn lfs_object_for_bytes(bytes: &[u8]) -> LfsObject {
    lfs_object(
        &sha256_hex(bytes),
        u64::try_from(bytes.len()).expect("usize object length should always fit in u64"),
    )
}

/// Renders a canonical Git LFS pointer file for tests.
#[must_use]
pub fn lfs_pointer_file(oid_hex: &str, size: u64) -> String {
    LfsPointer::new(lfs_object(oid_hex, size)).to_pointer_file()
}

/// Writes a canonical Git LFS pointer file into a temporary repository.
pub fn write_lfs_pointer(
    repo: &TempGitRepo,
    relative_path: impl AsRef<Path>,
    oid_hex: &str,
    size: u64,
) -> PathBuf {
    repo.write_file(relative_path, &lfs_pointer_file(oid_hex, size))
}

#[derive(Clone, Debug)]
struct FakeRepositoryRecord {
    stable_id: Option<String>,
    permissions_by_login: BTreeMap<String, RepositoryPermission>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FakeRepositoryKey {
    provider_id: String,
    host: String,
    owner: String,
    name: String,
}

impl FakeRepositoryKey {
    fn from_parts(
        provider_id: impl Into<String>,
        host: impl Into<String>,
        owner: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            host: host.into(),
            owner: owner.into(),
            name: name.into(),
        }
    }

    fn from_identity(repository: &RepositoryIdentity) -> Self {
        Self::from_parts(
            repository.provider_id.clone(),
            repository.host.clone(),
            repository.owner.clone(),
            repository.name.clone(),
        )
    }
}

/// Configurable repository provider fake for integration tests.
pub struct FakeRepositoryProvider {
    provider_id: String,
    repositories: Mutex<BTreeMap<FakeRepositoryKey, FakeRepositoryRecord>>,
}

impl FakeRepositoryProvider {
    /// Creates a fake provider with no configured repositories.
    #[must_use]
    pub fn new(provider_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            repositories: Mutex::new(BTreeMap::new()),
        }
    }

    /// Adds a repository identity that the fake can resolve.
    pub fn add_repository(
        &self,
        host: impl Into<String>,
        owner: impl Into<String>,
        name: impl Into<String>,
        stable_id: Option<String>,
    ) {
        self.repositories
            .lock()
            .expect("fake repository lock should not poison")
            .insert(
                FakeRepositoryKey::from_parts(self.provider_id.clone(), host, owner, name),
                FakeRepositoryRecord {
                    stable_id,
                    permissions_by_login: BTreeMap::new(),
                },
            );
    }

    /// Grants a repository permission to a fake provider user.
    pub fn grant_permission(
        &self,
        host: impl Into<String>,
        owner: impl Into<String>,
        name: impl Into<String>,
        login: impl Into<String>,
        permission: RepositoryPermission,
    ) {
        let key = FakeRepositoryKey::from_parts(self.provider_id.clone(), host, owner, name);
        let mut repositories = self
            .repositories
            .lock()
            .expect("fake repository lock should not poison");

        let repository = repositories
            .get_mut(&key)
            .expect("fake repository should exist before granting permissions");
        repository
            .permissions_by_login
            .insert(login.into(), permission);
    }
}

impl RepositoryProvider for FakeRepositoryProvider {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn check_permission<'a>(
        &'a self,
        repository: &'a RepositoryIdentity,
        authentication: &'a RepositoryAuthentication,
        required: RepositoryPermission,
    ) -> ProviderFuture<'a, ServerResult<RepositoryAuthorization>> {
        Box::pin(async move {
            let user = authentication.user();
            if authentication.access_token().is_empty() {
                return Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::AuthenticationRequired {
                        provider: self.provider_id.clone(),
                    },
                });
            }
            if user.provider_id != self.provider_id {
                return Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::PermissionDenied {
                        provider: self.provider_id.clone(),
                        owner: repository.owner.clone(),
                        repo: repository.name.clone(),
                        required,
                    },
                });
            }

            let repositories = self
                .repositories
                .lock()
                .expect("fake repository lock should not poison");
            let Some(record) = repositories.get(&FakeRepositoryKey::from_identity(repository))
            else {
                return Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::RepositoryNotFound {
                        provider: self.provider_id.clone(),
                        owner: repository.owner.clone(),
                        repo: repository.name.clone(),
                    },
                });
            };
            if record.stable_id != repository.stable_id {
                return Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::RepositoryNotFound {
                        provider: self.provider_id.clone(),
                        owner: repository.owner.clone(),
                        repo: repository.name.clone(),
                    },
                });
            }

            let granted = record
                .permissions_by_login
                .get(&user.login)
                .copied()
                .ok_or_else(|| ServerError::RepositoryProvider {
                    source: RepositoryProviderError::PermissionDenied {
                        provider: self.provider_id.clone(),
                        owner: repository.owner.clone(),
                        repo: repository.name.clone(),
                        required,
                    },
                })?;

            if permission_allows(granted, required) {
                Ok(RepositoryAuthorization {
                    user: user.clone(),
                    repository: repository.clone(),
                    required,
                    granted,
                })
            } else {
                Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::PermissionDenied {
                        provider: self.provider_id.clone(),
                        owner: repository.owner.clone(),
                        repo: repository.name.clone(),
                        required,
                    },
                })
            }
        })
    }
}

fn permission_allows(granted: RepositoryPermission, required: RepositoryPermission) -> bool {
    matches!(
        (granted, required),
        (RepositoryPermission::Admin, _)
            | (RepositoryPermission::Write, RepositoryPermission::Write)
            | (RepositoryPermission::Write, RepositoryPermission::Read)
            | (RepositoryPermission::Read, RepositoryPermission::Read)
    )
}

/// Asserts the permission lattice required of every repository-provider adapter.
///
/// The same contract is run against integration fakes and production adapters,
/// preventing a test double from inventing different read/write/admin semantics.
pub async fn assert_repository_permission_contract(
    provider: &dyn RepositoryProvider,
    repository: &RepositoryIdentity,
    authentication: &RepositoryAuthentication,
    granted: RepositoryPermission,
) {
    assert_eq!(provider.provider_id(), repository.provider_id);

    for required in [
        RepositoryPermission::Read,
        RepositoryPermission::Write,
        RepositoryPermission::Admin,
    ] {
        let result = provider
            .check_permission(repository, authentication, required)
            .await;

        if permission_allows(granted, required) {
            let authorization = result.expect("provider grant should satisfy required permission");
            assert_eq!(authorization.user, *authentication.user());
            assert_eq!(authorization.repository, *repository);
            assert_eq!(authorization.required, required);
            assert_eq!(authorization.granted, granted);
        } else {
            assert!(
                matches!(
                    result,
                    Err(ServerError::RepositoryProvider {
                        source: RepositoryProviderError::PermissionDenied {
                            required: denied_required,
                            ..
                        }
                    }) if denied_required == required
                ),
                "provider should deny {required:?} when it grants only {granted:?}"
            );
        }
    }
}

/// Asserts that a provider grant cannot cross a stable repository boundary.
pub async fn assert_repository_isolation_contract(
    provider: &dyn RepositoryProvider,
    repository: &RepositoryIdentity,
    isolated_repository: &RepositoryIdentity,
    authentication: &RepositoryAuthentication,
) {
    provider
        .check_permission(repository, authentication, RepositoryPermission::Read)
        .await
        .expect("configured repository should authorize reads");

    let result = provider
        .check_permission(
            isolated_repository,
            authentication,
            RepositoryPermission::Read,
        )
        .await;

    assert!(
        matches!(
            result,
            Err(ServerError::RepositoryProvider {
                source: RepositoryProviderError::RepositoryNotFound { .. }
                    | RepositoryProviderError::PermissionDenied { .. }
            })
        ),
        "provider grant must not authorize a different stable repository identity"
    );
}

#[derive(Clone, Debug)]
struct FakeStoredBytes {
    bytes: Vec<u8>,
    backend_id: String,
}

/// In-memory storage provider fake for integration tests.
pub struct FakeStorageProvider {
    provider_id: String,
    // These fixtures expose synchronous setup/assertion helpers, and no lock is
    // held across an await point in the async provider methods.
    objects: Mutex<BTreeMap<(String, LfsObject), FakeStoredBytes>>,
}

impl FakeStorageProvider {
    /// Creates an empty fake storage provider.
    #[must_use]
    pub fn new(provider_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            objects: Mutex::new(BTreeMap::new()),
        }
    }

    /// Inserts object bytes directly into the fake backend.
    pub fn insert_object(
        &self,
        repository_namespace: impl Into<String>,
        object: LfsObject,
        bytes: impl Into<Vec<u8>>,
    ) -> StoredObject {
        let repository_namespace = repository_namespace.into();
        let stored = stored_object(&self.provider_id, &repository_namespace, &object);

        self.objects
            .lock()
            .expect("fake storage lock should not poison")
            .insert(
                (repository_namespace, object.clone()),
                FakeStoredBytes {
                    bytes: bytes.into(),
                    backend_id: stored.backend_id.clone(),
                },
            );

        stored
    }

    /// Returns a copy of the fake backend bytes for assertions.
    #[must_use]
    pub fn object_bytes(&self, repository_namespace: &str, object: &LfsObject) -> Option<Vec<u8>> {
        self.objects
            .lock()
            .expect("fake storage lock should not poison")
            .get(&(repository_namespace.to_owned(), object.clone()))
            .map(|stored| stored.bytes.clone())
    }
}

impl StorageProvider for FakeStorageProvider {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn object_exists<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<bool>> {
        Box::pin(async move {
            Ok(self
                .objects
                .lock()
                .expect("fake storage lock should not poison")
                .contains_key(&(repository_namespace.to_owned(), object.clone())))
        })
    }

    fn upload_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        source: &'a Path,
    ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
        Box::pin(async move {
            let bytes =
                fs::read(source).map_err(|source| io_storage_error(&self.provider_id, source))?;
            let actual_size =
                u64::try_from(bytes.len()).expect("usize object length should always fit in u64");
            let actual_oid = sha256_hex(&bytes);

            if actual_size != object.size.bytes() || actual_oid != object.oid.as_hex() {
                return Err(StorageError::IntegrityMismatch {
                    expected_oid: object.oid.as_hex().to_owned(),
                    expected_size: object.size.bytes(),
                    actual_oid,
                    actual_size,
                });
            }

            self.insert_object(repository_namespace, object.clone(), bytes);
            Ok(stored_object(
                &self.provider_id,
                repository_namespace,
                object,
            ))
        })
    }

    fn download_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        destination: &'a Path,
    ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
        Box::pin(async move {
            let stored = {
                let objects = self
                    .objects
                    .lock()
                    .expect("fake storage lock should not poison");
                objects
                    .get(&(repository_namespace.to_owned(), object.clone()))
                    .cloned()
                    .ok_or_else(|| StorageError::ObjectNotFound {
                        provider: self.provider_id.clone(),
                        oid: object.oid.as_hex().to_owned(),
                        size: object.size.bytes(),
                    })?
            };

            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .map_err(|source| io_storage_error(&self.provider_id, source))?;
            }
            fs::write(destination, &stored.bytes)
                .map_err(|source| io_storage_error(&self.provider_id, source))?;

            Ok(StoredObject::new(
                self.provider_id.clone(),
                repository_namespace,
                object.clone(),
                stored.backend_id,
            ))
        })
    }

    fn delete_or_mark_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<StorageDeleteOutcome>> {
        Box::pin(async move {
            let deleted = self
                .objects
                .lock()
                .expect("fake storage lock should not poison")
                .remove(&(repository_namespace.to_owned(), object.clone()))
                .is_some();

            if deleted {
                Ok(StorageDeleteOutcome::Deleted)
            } else {
                Err(StorageError::ObjectNotFound {
                    provider: self.provider_id.clone(),
                    oid: object.oid.as_hex().to_owned(),
                    size: object.size.bytes(),
                })
            }
        })
    }
}

fn stored_object(
    provider_id: &str,
    repository_namespace: &str,
    object: &LfsObject,
) -> StoredObject {
    StoredObject::new(
        provider_id.to_owned(),
        repository_namespace,
        object.clone(),
        format!(
            "fake://{provider_id}/repositories/{repository_namespace}/objects/{}",
            object.oid
        ),
    )
}

fn io_storage_error(provider_id: &str, source: io::Error) -> StorageError {
    StorageError::Retryable {
        provider: provider_id.to_owned(),
        message: source.to_string(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn assert_git_success(output: &Output) {
    assert!(
        output.status.success(),
        "git command failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
