//! Repository and storage provider abstraction traits.
//!
//! The MVP starts with GitHub as the repository provider and Google Drive as
//! the storage provider, but the LFS protocol layer should depend on these
//! traits rather than concrete provider implementations.

use std::{future::Future, path::Path};

use crate::{LfsObject, RepositoryPermission, RepositoryProviderResult, StorageResult};

/// Configured repository address resolved from an LFS route or CLI context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryHandle {
    /// Repository provider ID from server configuration.
    pub provider_id: String,
    /// Repository host, such as `github.com`.
    pub host: String,
    /// Repository owner or namespace.
    pub owner: String,
    /// Repository name.
    pub name: String,
}

impl RepositoryHandle {
    /// Creates a configured repository handle.
    #[must_use]
    pub fn new(
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
}

/// Stable repository identity returned by a repository provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryIdentity {
    /// Configured repository provider ID.
    pub provider_id: String,
    /// Provider-specific stable repository ID, when available.
    pub stable_id: Option<String>,
    /// Repository host, such as `github.com`.
    pub host: String,
    /// Repository owner or namespace.
    pub owner: String,
    /// Repository name.
    pub name: String,
}

impl RepositoryIdentity {
    /// Creates repository identity metadata from a configured handle.
    #[must_use]
    pub fn from_handle(handle: &RepositoryHandle, stable_id: Option<String>) -> Self {
        Self {
            provider_id: handle.provider_id.clone(),
            stable_id,
            host: handle.host.clone(),
            owner: handle.owner.clone(),
            name: handle.name.clone(),
        }
    }
}

/// Authenticated repository-provider user.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryUser {
    /// Configured repository provider ID that authenticated this user.
    pub provider_id: String,
    /// Human-readable provider login, such as a GitHub username.
    pub login: String,
    /// Provider-specific stable user ID, when available.
    pub stable_id: Option<String>,
}

impl RepositoryUser {
    /// Creates authenticated user identity metadata.
    #[must_use]
    pub fn new(
        provider_id: impl Into<String>,
        login: impl Into<String>,
        stable_id: Option<String>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            login: login.into(),
            stable_id,
        }
    }
}

/// Successful repository authorization decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryAuthorization {
    /// Authenticated user that was authorized.
    pub user: RepositoryUser,
    /// Repository identity that was checked.
    pub repository: RepositoryIdentity,
    /// Permission required by the LFS operation.
    pub required: RepositoryPermission,
    /// Provider permission that satisfied the request.
    pub granted: RepositoryPermission,
}

/// Repository-provider operations required by the LFS server.
pub trait RepositoryProvider {
    /// Returns this provider's configured ID.
    fn provider_id(&self) -> &str;

    /// Resolves a configured repository handle to a provider-stable identity.
    fn repository_identity<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
    ) -> impl Future<Output = RepositoryProviderResult<RepositoryIdentity>> + Send + 'a;

    /// Checks whether a user has the permission required by an LFS operation.
    fn check_permission<'a>(
        &'a self,
        repository: &'a RepositoryIdentity,
        user: &'a RepositoryUser,
        required: RepositoryPermission,
    ) -> impl Future<Output = RepositoryProviderResult<RepositoryAuthorization>> + Send + 'a;
}

/// Storage metadata for an object that exists in a backend provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredObject {
    /// Configured storage provider ID.
    pub provider_id: String,
    /// Provider-independent LFS object identity.
    pub object: LfsObject,
    /// Backend object ID, file ID, or storage key.
    pub backend_id: String,
}

impl StoredObject {
    /// Creates stored-object metadata after backend verification.
    #[must_use]
    pub fn new(
        provider_id: impl Into<String>,
        object: LfsObject,
        backend_id: impl Into<String>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            object,
            backend_id: backend_id.into(),
        }
    }
}

/// Result of a storage provider's delete-or-mark operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StorageDeleteOutcome {
    /// The backend object was deleted.
    Deleted,
    /// The backend object was marked for later cleanup instead of deleted now.
    Marked {
        /// Provider-specific marker, tombstone ID, or cleanup note.
        marker: String,
    },
    /// The provider intentionally keeps the object because hard deletion is unsupported.
    Retained {
        /// Human-readable reason the object was retained.
        reason: String,
    },
}

/// Storage-provider operations required by upload, download, and cleanup flows.
pub trait StorageProvider {
    /// Returns this provider's configured ID.
    fn provider_id(&self) -> &str;

    /// Checks whether an object exists in this storage backend.
    fn object_exists<'a>(
        &'a self,
        object: &'a LfsObject,
    ) -> impl Future<Output = StorageResult<bool>> + Send + 'a;

    /// Uploads an already-staged and verified object file to this backend.
    fn upload_object<'a>(
        &'a self,
        object: &'a LfsObject,
        source: &'a Path,
    ) -> impl Future<Output = StorageResult<StoredObject>> + Send + 'a;

    /// Downloads an object from this backend into the provided destination path.
    fn download_object<'a>(
        &'a self,
        object: &'a LfsObject,
        destination: &'a Path,
    ) -> impl Future<Output = StorageResult<StoredObject>> + Send + 'a;

    /// Deletes an object or marks it for later cleanup when deletion is unavailable.
    fn delete_or_mark_object<'a>(
        &'a self,
        object: &'a LfsObject,
    ) -> impl Future<Output = StorageResult<StorageDeleteOutcome>> + Send + 'a;
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::Path, str::FromStr, sync::Mutex};

    use super::{
        RepositoryAuthorization, RepositoryHandle, RepositoryIdentity, RepositoryProvider,
        RepositoryUser, StorageDeleteOutcome, StorageProvider, StoredObject,
    };
    use crate::{
        LfsObject, LfsObjectSize, LfsOid, RepositoryPermission, RepositoryProviderError,
        RepositoryProviderResult, StorageResult,
    };

    const OID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct FakeRepositoryProvider {
        provider_id: String,
        granted: RepositoryPermission,
    }

    impl RepositoryProvider for FakeRepositoryProvider {
        fn provider_id(&self) -> &str {
            &self.provider_id
        }

        fn repository_identity<'a>(
            &'a self,
            repository: &'a RepositoryHandle,
        ) -> impl Future<Output = RepositoryProviderResult<RepositoryIdentity>> + Send + 'a
        {
            async move {
                Ok(RepositoryIdentity::from_handle(
                    repository,
                    Some("repo-123".to_owned()),
                ))
            }
        }

        fn check_permission<'a>(
            &'a self,
            repository: &'a RepositoryIdentity,
            user: &'a RepositoryUser,
            required: RepositoryPermission,
        ) -> impl Future<Output = RepositoryProviderResult<RepositoryAuthorization>> + Send + 'a
        {
            async move {
                if self.granted == required || self.granted == RepositoryPermission::Admin {
                    Ok(RepositoryAuthorization {
                        user: user.clone(),
                        repository: repository.clone(),
                        required,
                        granted: self.granted,
                    })
                } else {
                    Err(RepositoryProviderError::PermissionDenied {
                        provider: self.provider_id.clone(),
                        owner: repository.owner.clone(),
                        repo: repository.name.clone(),
                        required,
                    })
                }
            }
        }
    }

    struct FakeStorageProvider {
        provider_id: String,
        objects: Mutex<BTreeSet<LfsObject>>,
    }

    impl StorageProvider for FakeStorageProvider {
        fn provider_id(&self) -> &str {
            &self.provider_id
        }

        fn object_exists<'a>(
            &'a self,
            object: &'a LfsObject,
        ) -> impl Future<Output = StorageResult<bool>> + Send + 'a {
            async move {
                Ok(self
                    .objects
                    .lock()
                    .expect("fake storage lock should not poison")
                    .contains(object))
            }
        }

        fn upload_object<'a>(
            &'a self,
            object: &'a LfsObject,
            _source: &'a Path,
        ) -> impl Future<Output = StorageResult<StoredObject>> + Send + 'a {
            async move {
                self.objects
                    .lock()
                    .expect("fake storage lock should not poison")
                    .insert(object.clone());

                Ok(StoredObject::new(
                    self.provider_id.clone(),
                    object.clone(),
                    format!("drive-file-{}", object.oid),
                ))
            }
        }

        fn download_object<'a>(
            &'a self,
            object: &'a LfsObject,
            _destination: &'a Path,
        ) -> impl Future<Output = StorageResult<StoredObject>> + Send + 'a {
            async move {
                Ok(StoredObject::new(
                    self.provider_id.clone(),
                    object.clone(),
                    format!("drive-file-{}", object.oid),
                ))
            }
        }

        fn delete_or_mark_object<'a>(
            &'a self,
            object: &'a LfsObject,
        ) -> impl Future<Output = StorageResult<StorageDeleteOutcome>> + Send + 'a {
            async move {
                self.objects
                    .lock()
                    .expect("fake storage lock should not poison")
                    .remove(object);

                Ok(StorageDeleteOutcome::Deleted)
            }
        }
    }

    fn lfs_object() -> LfsObject {
        LfsObject::new(
            LfsOid::from_str(OID).expect("test oid should parse"),
            LfsObjectSize::new(42),
        )
    }

    #[tokio::test]
    async fn repository_provider_trait_resolves_identity_and_authorizes_user() {
        let provider = FakeRepositoryProvider {
            provider_id: "github-main".to_owned(),
            granted: RepositoryPermission::Write,
        };
        let handle = RepositoryHandle::new("github-main", "github.com", "owner", "repo");
        let user = RepositoryUser::new("github-main", "octocat", Some("user-123".to_owned()));

        let identity = provider
            .repository_identity(&handle)
            .await
            .expect("repository should resolve");
        let authorization = provider
            .check_permission(&identity, &user, RepositoryPermission::Write)
            .await
            .expect("write permission should be granted");

        assert_eq!(provider.provider_id(), "github-main");
        assert_eq!(identity.stable_id.as_deref(), Some("repo-123"));
        assert_eq!(authorization.granted, RepositoryPermission::Write);
    }

    #[tokio::test]
    async fn repository_provider_trait_reports_denied_permissions() {
        let provider = FakeRepositoryProvider {
            provider_id: "github-main".to_owned(),
            granted: RepositoryPermission::Read,
        };
        let handle = RepositoryHandle::new("github-main", "github.com", "owner", "repo");
        let user = RepositoryUser::new("github-main", "octocat", None);
        let identity = provider
            .repository_identity(&handle)
            .await
            .expect("repository should resolve");

        let error = provider
            .check_permission(&identity, &user, RepositoryPermission::Write)
            .await
            .expect_err("read permission should not satisfy write");

        assert!(matches!(
            error,
            RepositoryProviderError::PermissionDenied {
                required: RepositoryPermission::Write,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn storage_provider_trait_uploads_checks_downloads_and_deletes_objects() {
        let provider = FakeStorageProvider {
            provider_id: "drive-user-a".to_owned(),
            objects: Mutex::new(BTreeSet::new()),
        };
        let object = lfs_object();
        let path = Path::new("/tmp/lfs-cloud-test-object");

        assert!(
            !provider
                .object_exists(&object)
                .await
                .expect("exists check should work")
        );

        let uploaded = provider
            .upload_object(&object, path)
            .await
            .expect("upload should succeed");
        let downloaded = provider
            .download_object(&object, path)
            .await
            .expect("download should succeed");

        assert!(provider.object_exists(&object).await.unwrap());
        assert_eq!(uploaded.backend_id, format!("drive-file-{OID}"));
        assert_eq!(downloaded.object, object);

        let deletion = provider
            .delete_or_mark_object(&object)
            .await
            .expect("delete should succeed");

        assert_eq!(deletion, StorageDeleteOutcome::Deleted);
        assert!(!provider.object_exists(&object).await.unwrap());
    }
}
