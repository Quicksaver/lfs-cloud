//! Reusable integration-test fixtures for LFS Cloud.
//!
//! Each integration test crate can import these helpers with `mod support;`.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::Mutex,
};

use lfs_cloud::{
    LfsObject, LfsObjectSize, LfsOid, LfsPointer, ProviderFuture, RepositoryAuthorization,
    RepositoryHandle, RepositoryIdentity, RepositoryPermission, RepositoryProvider,
    RepositoryProviderError, RepositoryProviderResult, RepositoryUser, StorageDeleteOutcome,
    StorageError, StorageProvider, StorageResult, StoredObject,
};
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

        repo.git(["init", "--initial-branch", "main"]);
        repo.git(["config", "user.email", "lfs-cloud@example.invalid"]);
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
        let output = Command::new("git")
            .args(args)
            .current_dir(self.root.path())
            .output()
            .expect("git command should start");

        assert!(
            output.status.success(),
            "git command failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        output
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

/// Configurable repository provider fake for integration tests.
pub struct FakeRepositoryProvider {
    provider_id: String,
    repositories: Mutex<BTreeMap<(String, String), FakeRepositoryRecord>>,
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
        owner: impl Into<String>,
        name: impl Into<String>,
        stable_id: Option<String>,
    ) {
        self.repositories
            .lock()
            .expect("fake repository lock should not poison")
            .insert(
                (owner.into(), name.into()),
                FakeRepositoryRecord {
                    stable_id,
                    permissions_by_login: BTreeMap::new(),
                },
            );
    }

    /// Grants a repository permission to a fake provider user.
    pub fn grant_permission(
        &self,
        owner: impl Into<String>,
        name: impl Into<String>,
        login: impl Into<String>,
        permission: RepositoryPermission,
    ) {
        let key = (owner.into(), name.into());
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

    fn repository_identity<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
    ) -> ProviderFuture<'a, RepositoryProviderResult<RepositoryIdentity>> {
        Box::pin(async move {
            let repositories = self
                .repositories
                .lock()
                .expect("fake repository lock should not poison");
            let Some(record) =
                repositories.get(&(repository.owner.clone(), repository.name.clone()))
            else {
                return Err(RepositoryProviderError::RepositoryNotFound {
                    provider: self.provider_id.clone(),
                    owner: repository.owner.clone(),
                    repo: repository.name.clone(),
                });
            };

            Ok(RepositoryIdentity::from_handle(
                repository,
                record.stable_id.clone(),
            ))
        })
    }

    fn check_permission<'a>(
        &'a self,
        repository: &'a RepositoryIdentity,
        user: &'a RepositoryUser,
        required: RepositoryPermission,
    ) -> ProviderFuture<'a, RepositoryProviderResult<RepositoryAuthorization>> {
        Box::pin(async move {
            let repositories = self
                .repositories
                .lock()
                .expect("fake repository lock should not poison");
            let Some(record) =
                repositories.get(&(repository.owner.clone(), repository.name.clone()))
            else {
                return Err(RepositoryProviderError::RepositoryNotFound {
                    provider: self.provider_id.clone(),
                    owner: repository.owner.clone(),
                    repo: repository.name.clone(),
                });
            };

            let granted = record
                .permissions_by_login
                .get(&user.login)
                .copied()
                .ok_or_else(|| RepositoryProviderError::PermissionDenied {
                    provider: self.provider_id.clone(),
                    owner: repository.owner.clone(),
                    repo: repository.name.clone(),
                    required,
                })?;

            if permission_allows(granted, required) {
                Ok(RepositoryAuthorization {
                    user: user.clone(),
                    repository: repository.clone(),
                    required,
                    granted,
                })
            } else {
                Err(RepositoryProviderError::PermissionDenied {
                    provider: self.provider_id.clone(),
                    owner: repository.owner.clone(),
                    repo: repository.name.clone(),
                    required,
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

#[derive(Clone, Debug)]
struct FakeStoredBytes {
    object: LfsObject,
    bytes: Vec<u8>,
    backend_id: String,
}

/// In-memory storage provider fake for integration tests.
pub struct FakeStorageProvider {
    provider_id: String,
    objects: Mutex<BTreeMap<LfsObject, FakeStoredBytes>>,
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
    pub fn insert_object(&self, object: LfsObject, bytes: impl Into<Vec<u8>>) -> StoredObject {
        let stored = stored_object(&self.provider_id, &object);

        self.objects
            .lock()
            .expect("fake storage lock should not poison")
            .insert(
                object.clone(),
                FakeStoredBytes {
                    object,
                    bytes: bytes.into(),
                    backend_id: stored.backend_id.clone(),
                },
            );

        stored
    }

    /// Returns a copy of the fake backend bytes for assertions.
    #[must_use]
    pub fn object_bytes(&self, object: &LfsObject) -> Option<Vec<u8>> {
        self.objects
            .lock()
            .expect("fake storage lock should not poison")
            .get(object)
            .map(|stored| stored.bytes.clone())
    }
}

impl StorageProvider for FakeStorageProvider {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn object_exists<'a>(
        &'a self,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<bool>> {
        Box::pin(async move {
            Ok(self
                .objects
                .lock()
                .expect("fake storage lock should not poison")
                .contains_key(object))
        })
    }

    fn upload_object<'a>(
        &'a self,
        object: &'a LfsObject,
        source: &'a Path,
    ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
        Box::pin(async move {
            let bytes =
                fs::read(source).map_err(|source| io_storage_error(&self.provider_id, source))?;

            self.insert_object(object.clone(), bytes);
            Ok(stored_object(&self.provider_id, object))
        })
    }

    fn download_object<'a>(
        &'a self,
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
                    .get(object)
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
                stored.object,
                stored.backend_id,
            ))
        })
    }

    fn delete_or_mark_object<'a>(
        &'a self,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<StorageDeleteOutcome>> {
        Box::pin(async move {
            self.objects
                .lock()
                .expect("fake storage lock should not poison")
                .remove(object);

            Ok(StorageDeleteOutcome::Deleted)
        })
    }
}

fn stored_object(provider_id: &str, object: &LfsObject) -> StoredObject {
    StoredObject::new(
        provider_id.to_owned(),
        object.clone(),
        format!("fake://{provider_id}/objects/{}", object.oid),
    )
}

fn io_storage_error(provider_id: &str, source: io::Error) -> StorageError {
    StorageError::Retryable {
        provider: provider_id.to_owned(),
        message: source.to_string(),
    }
}
