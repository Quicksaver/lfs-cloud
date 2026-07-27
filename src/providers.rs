//! Repository and storage provider abstraction traits.
//!
//! The MVP starts with GitHub as the repository provider and Google Drive as
//! the storage provider, but the LFS protocol layer should depend on these
//! traits rather than concrete provider implementations.

use std::{fmt, future::Future, path::Path, pin::Pin};

use axum::response::Response;

use crate::{LfsObject, RepositoryPermission, ServerResult, StorageResult};

/// Boxed asynchronous provider operation.
///
/// Provider traits use this alias so callers can store configured providers as
/// trait objects while implementations can still perform network I/O.
pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Stable repository identity configured for a repository provider.
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

/// Per-session authentication context for repository-provider authorization.
///
/// The access token remains server-side and is never returned to Git LFS. Its
/// debug representation is always redacted so provider adapters can safely be
/// composed without accidentally logging the credential.
#[derive(Clone, Eq, PartialEq)]
pub struct RepositoryAuthentication {
    user: RepositoryUser,
    access_token: String,
}

impl RepositoryAuthentication {
    /// Creates an authenticated provider context from an actor and access token.
    #[must_use]
    pub fn new(user: RepositoryUser, access_token: impl Into<String>) -> Self {
        Self {
            user,
            access_token: access_token.into(),
        }
    }

    /// Returns the authenticated provider user.
    #[must_use]
    pub fn user(&self) -> &RepositoryUser {
        &self.user
    }

    /// Returns the provider access token for the adapter's upstream request.
    ///
    /// Callers must not log, persist, or expose this value outside the
    /// repository-provider boundary.
    #[must_use]
    pub fn access_token(&self) -> &str {
        &self.access_token
    }
}

impl fmt::Debug for RepositoryAuthentication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RepositoryAuthentication")
            .field("user", &self.user)
            .field("access_token", &"<redacted>")
            .finish()
    }
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

    /// Checks whether an authenticated user has the required repository access.
    ///
    /// Implementations must validate both the configured stable repository
    /// identity and the actor identity carried by `authentication` before
    /// returning a successful authorization.
    fn check_permission<'a>(
        &'a self,
        repository: &'a RepositoryIdentity,
        authentication: &'a RepositoryAuthentication,
        required: RepositoryPermission,
    ) -> ProviderFuture<'a, ServerResult<RepositoryAuthorization>>;
}

/// Storage metadata for an object that exists in a backend provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredObject {
    /// Configured storage provider ID.
    pub provider_id: String,
    /// Stable repository namespace that owns this backend object.
    pub repository_namespace: String,
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
        repository_namespace: impl Into<String>,
        object: LfsObject,
        backend_id: impl Into<String>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            repository_namespace: repository_namespace.into(),
            object,
            backend_id: backend_id.into(),
        }
    }
}

/// A verified storage-provider download exposed as an HTTP response body.
///
/// The response body may stream backend bytes while the provider verifies
/// their LFS object hash and size. Provider implementations must not expose
/// backend credentials or private object locations through the response.
///
/// This optional capability is currently coupled to Axum at the HTTP server
/// boundary. If the package is split into provider and server crates, move
/// this wrapper behind the server boundary or replace the body with a
/// framework-neutral verified byte stream.
pub struct StorageDownloadResponse {
    stored_object: StoredObject,
    response: Response,
}

impl StorageDownloadResponse {
    /// Creates a download response from verified object metadata and a body.
    #[must_use]
    pub fn new(stored_object: StoredObject, response: Response) -> Self {
        Self {
            stored_object,
            response,
        }
    }

    /// Returns the verified storage metadata for the downloaded object.
    #[must_use]
    pub fn stored_object(&self) -> &StoredObject {
        &self.stored_object
    }

    /// Consumes this download and returns the HTTP response to send downstream.
    #[must_use]
    pub fn into_response(self) -> Response {
        self.response
    }
}

impl fmt::Debug for StorageDownloadResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StorageDownloadResponse")
            .field("stored_object", &self.stored_object)
            .field("response", &"<streaming body>")
            .finish()
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
///
/// Every object operation includes the stable repository mapping ID as its
/// namespace. Implementations must scope existence, transfer, and cleanup to
/// that namespace even when multiple repositories share one provider account.
pub trait StorageProvider: Send + Sync {
    /// Returns this provider's configured ID.
    fn provider_id(&self) -> &str;

    /// Returns verified backend metadata for an object in one repository namespace.
    ///
    /// Implementations with duplicate backend objects must select one stable
    /// identity so metadata repair and idempotent uploads converge.
    fn lookup_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<Option<StoredObject>>>;

    /// Checks whether an object exists in one repository namespace.
    ///
    /// The default is intentionally a thin wrapper around [`Self::lookup_object`]
    /// so existence and stable backend identity cannot diverge. Providers with
    /// a materially cheaper existence primitive may override this method while
    /// preserving the same namespace and object-validation contract.
    fn object_exists<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<bool>> {
        Box::pin(async move {
            Ok(self
                .lookup_object(repository_namespace, object)
                .await?
                .is_some())
        })
    }

    /// Uploads an already-staged and verified object file to one repository namespace.
    ///
    /// Providers should treat the [`LfsObject`] size as part of the validation
    /// contract: stored bytes must match both the OID and the expected size.
    fn upload_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        source: &'a Path,
    ) -> ProviderFuture<'a, StorageResult<StoredObject>>;

    /// Downloads a namespaced object into the provided destination path.
    ///
    /// Providers should report a missing object or integrity failure when the
    /// stored object does not match the requested OID and size.
    fn download_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        destination: &'a Path,
    ) -> ProviderFuture<'a, StorageResult<StoredObject>>;

    /// Deletes or marks an object only within the supplied repository namespace.
    fn delete_or_mark_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<StorageDeleteOutcome>>;
}

/// Optional indexed lookup capability for storage providers with backend IDs.
///
/// The production transfer path uses this capability to verify a metadata
/// record without scanning the provider namespace. Providers that do not
/// implement it fall back to [`StorageProvider::lookup_object`].
pub trait BackendIdLookup: Send + Sync {
    /// Resolves one backend ID only when it still identifies the exact object.
    fn lookup_object_by_backend_id<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        backend_id: &'a str,
    ) -> ProviderFuture<'a, StorageResult<Option<StoredObject>>>;
}

/// Optional direct-streaming capability for storage providers.
///
/// Providers that do not implement this capability remain usable by the
/// server through a verified temporary-file fallback.
pub trait StreamingStorageProvider: Send + Sync {
    /// Streams an already-discovered object as a verified HTTP response.
    fn download_object_response<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        stored_object: StoredObject,
    ) -> ProviderFuture<'a, StorageResult<StorageDownloadResponse>>;
}

#[cfg(test)]
mod tests {
    use super::{RepositoryAuthentication, RepositoryUser};

    #[test]
    fn repository_authentication_debug_redacts_access_token() {
        let authentication = RepositoryAuthentication::new(
            RepositoryUser::new("github-main", "octocat", Some("user-123".to_owned())),
            "provider-secret-token",
        );
        let debug = format!("{authentication:?}");

        assert!(debug.contains("octocat"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("provider-secret-token"));
    }
}
