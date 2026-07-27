//! Runtime provider construction from validated server configuration.
//!
//! Configuration parsing names concrete provider variants, while the rest of
//! the application consumes repository and storage traits. This module is the
//! registration boundary between those two layers. The only intentionally
//! concrete production exception remains the Google Drive transfer store in
//! `server`, pending the separately scoped generic transfer-store design.

use std::{path::Path, sync::Arc};

use crate::{
    GitHubProviderConfig, GitHubRepositoryProvider, GoogleDriveGcloudTokenProvider,
    GoogleDriveObjectStore, GoogleDriveRootValidator, GoogleDriveStorageConfig, LfsObject,
    MetadataDatabase, ProviderFuture, RepositoryMapping, RepositoryProvider,
    RepositoryProviderConfig, ServerConfig, ServerError, ServerResult, StorageDeleteOutcome,
    StorageError, StorageProvider, StorageProviderConfig, StorageResult, StoredObject,
    google_drive::{GoogleDriveAccessTokenCache, GoogleDriveAccessTokenSource},
};

/// Provider-neutral behavior registered by each repository config variant.
///
/// Implementations keep config parsing exhaustive while preventing callers
/// from matching concrete providers merely to construct adapters or validate
/// repository mappings.
trait RepositoryProviderRegistration {
    /// Returns the provider ID embedded by validated configuration loading.
    fn id(&self) -> &str;
    /// Returns the stable provider type stored in metadata.
    fn provider_type(&self) -> &'static str;
    /// Builds the runtime repository-provider adapter.
    fn build_provider(&self) -> Arc<dyn RepositoryProvider + Send + Sync>;
    /// Rejects mapping fields that violate provider-specific identity rules.
    fn validate_mapping(&self, repository: &RepositoryMapping, path: &str) -> ServerResult<()>;
    /// Reports whether route uniqueness and mapping lookup ignore identity case.
    fn route_identity_is_case_insensitive(&self) -> bool;
}

impl RepositoryProviderRegistration for GitHubProviderConfig {
    fn id(&self) -> &str {
        &self.id
    }

    fn provider_type(&self) -> &'static str {
        "github"
    }

    fn build_provider(&self) -> Arc<dyn RepositoryProvider + Send + Sync> {
        Arc::new(GitHubRepositoryProvider::new(self.clone()))
    }

    fn validate_mapping(&self, repository: &RepositoryMapping, path: &str) -> ServerResult<()> {
        if repository
            .provider_repository_id
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0)
            .is_some()
        {
            return Ok(());
        }
        Err(ServerError::InvalidConfiguration {
            message: format!(
                "{path}.provider_repository_id must be a positive GitHub numeric repository ID"
            ),
        })
    }

    fn route_identity_is_case_insensitive(&self) -> bool {
        true
    }
}

impl RepositoryProviderConfig {
    fn registration(&self) -> &dyn RepositoryProviderRegistration {
        match self {
            Self::GitHub(config) => config,
        }
    }

    /// Returns the configured provider ID.
    #[must_use]
    pub fn id(&self) -> &str {
        self.registration().id()
    }

    /// Returns the configured provider type.
    #[must_use]
    pub fn provider_type(&self) -> &'static str {
        self.registration().provider_type()
    }

    /// Builds the runtime repository-provider adapter.
    pub(crate) fn build_provider(&self) -> Arc<dyn RepositoryProvider + Send + Sync> {
        self.registration().build_provider()
    }

    /// Validates provider-specific repository mapping fields.
    pub(crate) fn validate_mapping(
        &self,
        repository: &RepositoryMapping,
        path: &str,
    ) -> ServerResult<()> {
        self.registration().validate_mapping(repository, path)
    }

    /// Reports whether repository route identity is case-insensitive.
    ///
    /// Callers use this for both route uniqueness and mapping lookup so the
    /// two operations cannot disagree about provider identity.
    pub(crate) fn route_identity_is_case_insensitive(&self) -> bool {
        self.registration().route_identity_is_case_insensitive()
    }
}

impl ServerConfig {
    /// Selects the only GitHub provider supported by one PAT-auth consumer.
    ///
    /// `consumer` keeps the startup diagnostic specific to the subsystem that
    /// cannot yet compose multiple configured GitHub accounts.
    pub(crate) fn single_github_pat_provider(
        &self,
        consumer: &str,
    ) -> ServerResult<Option<&GitHubProviderConfig>> {
        let mut providers = self
            .repository_providers
            .values()
            .map(|provider| match provider {
                RepositoryProviderConfig::GitHub(provider) => provider,
            });
        let provider = providers.next();
        if providers.next().is_some() {
            return Err(ServerError::InvalidConfiguration {
                message: format!(
                    "multiple GitHub repository providers are not yet supported by {consumer}"
                ),
            });
        }
        Ok(provider)
    }
}

/// Provider-neutral behavior registered by each storage config variant.
///
/// Runtime transfer concerns that require concrete dependencies stay with the
/// concrete transfer path instead of leaking those types through this trait.
trait StorageProviderRegistration {
    /// Returns the provider ID embedded by validated configuration loading.
    fn id(&self) -> &str;
    /// Returns the stable provider type stored in metadata.
    fn provider_type(&self) -> &'static str;
    /// Returns the stable backend root recorded for config reconciliation.
    fn backend_root_id(&self) -> &str;
    /// Returns the optional operator-facing provider label.
    fn display_name(&self) -> Option<&str>;
    /// Checks local prerequisites and returns a safe operator-facing failure.
    fn validate_local_readiness(&self) -> Result<(), String>;
    /// Builds a readiness-checked repository-scoped storage adapter.
    fn build_provider(
        &self,
        repository_namespace: String,
        metadata: Arc<MetadataDatabase>,
    ) -> ProviderFuture<'static, StorageResult<Arc<dyn StorageProvider + Send + Sync>>>;
}

impl StorageProviderRegistration for GoogleDriveStorageConfig {
    fn id(&self) -> &str {
        &self.id
    }

    fn provider_type(&self) -> &'static str {
        "google_drive"
    }

    fn backend_root_id(&self) -> &str {
        &self.root_folder_id
    }

    fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    fn validate_local_readiness(&self) -> Result<(), String> {
        GoogleDriveGcloudTokenProvider::new().validate_local_readiness(&self.id, &self.credentials)
            .map_err(|_| {
                format!(
                    "Google Drive credential for {} is not usable; check the configured gcloud ADC credentials directory",
                    self.id
                )
            })
    }

    fn build_provider(
        &self,
        repository_namespace: String,
        metadata: Arc<MetadataDatabase>,
    ) -> ProviderFuture<'static, StorageResult<Arc<dyn StorageProvider + Send + Sync>>> {
        let storage = self.clone();
        Box::pin(async move {
            let token_source: Arc<dyn GoogleDriveAccessTokenSource> =
                Arc::new(GoogleDriveGcloudTokenProvider::new());
            let token_cache = GoogleDriveAccessTokenCache::default();
            let token = token_cache
                .get_or_refresh(&storage, token_source.as_ref())
                .await?;
            GoogleDriveRootValidator::new()?
                .validate_root_folder(&storage, &token)
                .await?;

            Ok(Arc::new(GoogleDriveStorageProvider::with_dependencies(
                storage,
                repository_namespace,
                token_source,
                token_cache,
                metadata,
            )) as Arc<dyn StorageProvider + Send + Sync>)
        })
    }
}

impl StorageProviderConfig {
    fn registration(&self) -> &dyn StorageProviderRegistration {
        match self {
            Self::GoogleDrive(config) => config,
        }
    }

    /// Returns the configured storage provider ID.
    #[must_use]
    pub fn id(&self) -> &str {
        self.registration().id()
    }

    /// Returns the configured storage provider type.
    #[must_use]
    pub fn provider_type(&self) -> &'static str {
        self.registration().provider_type()
    }

    /// Returns the stable backend root recorded in metadata.
    pub(crate) fn backend_root_id(&self) -> &str {
        self.registration().backend_root_id()
    }

    /// Returns the optional operator-facing provider label.
    pub(crate) fn display_name(&self) -> Option<&str> {
        self.registration().display_name()
    }

    /// Checks local prerequisites and returns a safe operator-facing failure.
    pub(crate) fn validate_local_readiness(&self) -> Result<(), String> {
        self.registration().validate_local_readiness()
    }

    /// Builds a repository-scoped provider after credential and root checks.
    pub(crate) async fn build_provider(
        &self,
        repository_namespace: String,
        metadata: Arc<MetadataDatabase>,
    ) -> StorageResult<Arc<dyn StorageProvider + Send + Sync>> {
        self.registration()
            .build_provider(repository_namespace, metadata)
            .await
    }
}

/// Repository-scoped Google Drive adapter with idempotent upload locking.
///
/// The wrapper pins every operation to one repository namespace, coordinates
/// lookup/upload through metadata-backed cross-process locks, and acquires
/// Drive tokens only after lock admission so long waits cannot age them.
pub(crate) struct GoogleDriveStorageProvider {
    storage: GoogleDriveStorageConfig,
    repository_namespace: String,
    token_source: Arc<dyn GoogleDriveAccessTokenSource>,
    token_cache: GoogleDriveAccessTokenCache,
    metadata: Arc<MetadataDatabase>,
    #[cfg(test)]
    api_base_url: Option<String>,
}

impl GoogleDriveStorageProvider {
    /// Builds the production adapter from provider-specific dependencies.
    fn with_dependencies(
        storage: GoogleDriveStorageConfig,
        repository_namespace: String,
        token_source: Arc<dyn GoogleDriveAccessTokenSource>,
        token_cache: GoogleDriveAccessTokenCache,
        metadata: Arc<MetadataDatabase>,
    ) -> Self {
        Self {
            storage,
            repository_namespace,
            token_source,
            token_cache,
            metadata,
            #[cfg(test)]
            api_base_url: None,
        }
    }

    /// Builds a deterministic adapter with injected Drive dependencies.
    #[cfg(test)]
    pub(crate) fn with_test_dependencies(
        storage: GoogleDriveStorageConfig,
        repository_namespace: impl Into<String>,
        token_source: Arc<dyn GoogleDriveAccessTokenSource>,
        token_cache: GoogleDriveAccessTokenCache,
        metadata: Arc<MetadataDatabase>,
        api_base_url: Option<String>,
    ) -> Self {
        let mut provider = Self::with_dependencies(
            storage,
            repository_namespace.into(),
            token_source,
            token_cache,
            metadata,
        );
        provider.api_base_url = api_base_url;
        provider
    }

    async fn object_store(&self) -> StorageResult<GoogleDriveObjectStore> {
        let token = self
            .token_cache
            .get_or_refresh(&self.storage, self.token_source.as_ref())
            .await?;
        #[cfg(test)]
        if let Some(api_base_url) = &self.api_base_url {
            return GoogleDriveObjectStore::with_api_base_url(
                self.storage.clone(),
                &self.repository_namespace,
                token,
                api_base_url,
            );
        }
        GoogleDriveObjectStore::new(self.storage.clone(), &self.repository_namespace, token)
    }

    fn validate_repository_namespace(&self, repository_namespace: &str) -> StorageResult<()> {
        if repository_namespace == self.repository_namespace {
            Ok(())
        } else {
            Err(StorageError::RepositoryNamespaceMismatch {
                provider: self.storage.id.clone(),
            })
        }
    }
}

impl StorageProvider for GoogleDriveStorageProvider {
    fn provider_id(&self) -> &str {
        &self.storage.id
    }

    fn object_exists<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<bool>> {
        Box::pin(async move {
            self.validate_repository_namespace(repository_namespace)?;
            Ok(self
                .object_store()
                .await?
                .lookup_object(object)
                .await?
                .is_some())
        })
    }

    fn upload_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        source: &'a Path,
    ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
        Box::pin(async move {
            self.validate_repository_namespace(repository_namespace)?;
            let verified_file = GoogleDriveObjectStore::open_verified_staged_upload_file(
                &self.storage,
                object,
                source,
            )
            .await?;
            let _upload_lock = self
                .metadata
                .acquire_object_upload_lock(
                    repository_namespace.to_owned(),
                    self.storage.id.clone(),
                    object.clone(),
                )
                .await
                .map_err(|error| StorageError::Retryable {
                    provider: self.storage.id.clone(),
                    message: format!("provider upload lock failed: {error}"),
                })?;

            // Verify before waiting so cache-hit retries do not serialize
            // large-file reads. Keep lookup and possible upload under the
            // cross-process lock so a live server cannot create a duplicate
            // Drive file, and mint the token after admission so long waits
            // cannot age the credential.
            self.object_store()
                .await?
                .upload_verified_object_idempotent(object, source, verified_file)
                .await
        })
    }

    fn download_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        destination: &'a Path,
    ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
        Box::pin(async move {
            self.validate_repository_namespace(repository_namespace)?;
            StorageProvider::download_object(
                &self.object_store().await?,
                repository_namespace,
                object,
                destination,
            )
            .await
        })
    }

    fn delete_or_mark_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<StorageDeleteOutcome>> {
        Box::pin(async move {
            self.validate_repository_namespace(repository_namespace)?;
            StorageProvider::delete_or_mark_object(
                &self.object_store().await?,
                repository_namespace,
                object,
            )
            .await
        })
    }
}

#[cfg(test)]
mod tests;
