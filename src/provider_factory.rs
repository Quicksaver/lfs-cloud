//! Runtime provider construction from validated server configuration.
//!
//! Configuration parsing names concrete provider variants, while the rest of
//! the application consumes repository and storage traits. This module is the
//! registration boundary between those two layers, including production server
//! storage composition and optional provider capabilities.

use std::{collections::BTreeMap, path::Path, sync::Arc};

use crate::{
    BackendIdLookup, GitHubProviderConfig, GitHubRepositoryProvider,
    GoogleDriveGcloudTokenProvider, GoogleDriveObjectStore, GoogleDriveRootValidator,
    GoogleDriveStorageConfig, LfsObject, MetadataDatabase, ProviderFuture, RepositoryMapping,
    RepositoryProvider, RepositoryProviderConfig, ServerConfig, ServerError, ServerResult,
    StorageDeleteOutcome, StorageDownloadResponse, StorageError, StorageProvider,
    StorageProviderConfig, StorageResult, StoredObject, StreamingStorageProvider,
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
    /// Selects the only GitHub provider supported by one authentication consumer.
    ///
    /// `consumer` keeps the startup diagnostic specific to the subsystem that
    /// cannot yet compose multiple configured GitHub accounts.
    pub(crate) fn single_github_provider(
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

/// One configured storage provider plus production transfer capabilities.
#[derive(Clone)]
pub(crate) struct ServerStorageProvider {
    provider: Arc<dyn StorageProvider + Send + Sync>,
    backend_id_lookup: Option<Arc<dyn BackendIdLookup>>,
    streaming_download: Option<Arc<dyn StreamingStorageProvider>>,
}

impl ServerStorageProvider {
    /// Builds the default server adapter for a provider without optional capabilities.
    pub(crate) fn from_provider(provider: Arc<dyn StorageProvider + Send + Sync>) -> Self {
        Self {
            provider,
            backend_id_lookup: None,
            streaming_download: None,
        }
    }

    /// Attaches indexed backend-ID lookup when the provider supports it.
    fn with_backend_id_lookup(mut self, backend_id_lookup: Arc<dyn BackendIdLookup>) -> Self {
        self.backend_id_lookup = Some(backend_id_lookup);
        self
    }

    /// Attaches direct streaming when the provider supports it.
    fn with_streaming_download(
        mut self,
        streaming_download: Arc<dyn StreamingStorageProvider>,
    ) -> Self {
        self.streaming_download = Some(streaming_download);
        self
    }

    /// Returns the provider's generic storage contract.
    pub(crate) fn provider(&self) -> &(dyn StorageProvider + Send + Sync) {
        self.provider.as_ref()
    }

    /// Returns indexed backend-ID lookup when the provider supports it.
    pub(crate) fn backend_id_lookup(&self) -> Option<&dyn BackendIdLookup> {
        self.backend_id_lookup.as_deref()
    }

    /// Returns direct streaming when the provider supports it.
    pub(crate) fn streaming_download(&self) -> Option<&dyn StreamingStorageProvider> {
        self.streaming_download.as_deref()
    }
}

/// Repository-scoped storage providers composed through config registration.
#[derive(Clone)]
pub(crate) struct ConfiguredStorageProviders {
    by_repository_id: BTreeMap<String, ServerStorageProvider>,
}

impl ConfiguredStorageProviders {
    /// Builds a registry around one injected provider for local end-to-end tests.
    pub(crate) fn from_provider(
        config: &ServerConfig,
        provider: Arc<dyn StorageProvider + Send + Sync>,
    ) -> ServerResult<Self> {
        let mut by_repository_id = BTreeMap::new();
        for repository in &config.repositories {
            if repository.storage_provider != provider.provider_id() {
                return Err(ServerError::InvalidConfiguration {
                    message: format!(
                        "repository {} references storage provider {}, but injected provider is {}",
                        repository.id,
                        repository.storage_provider,
                        provider.provider_id()
                    ),
                });
            }
            by_repository_id.insert(
                repository.id.clone(),
                ServerStorageProvider::from_provider(provider.clone()),
            );
        }
        Ok(Self { by_repository_id })
    }

    /// Resolves the provider registered for one validated repository mapping.
    pub(crate) fn provider_for(
        &self,
        repository: &RepositoryMapping,
    ) -> ServerResult<&ServerStorageProvider> {
        let provider = self.by_repository_id.get(&repository.id).ok_or_else(|| {
            ServerError::InvalidConfiguration {
                message: format!(
                    "repository {} has no configured server storage provider",
                    repository.id
                ),
            }
        })?;
        if provider.provider().provider_id() != repository.storage_provider {
            return Err(ServerError::InvalidConfiguration {
                message: format!(
                    "repository {} references storage provider {}, but composed provider is {}",
                    repository.id,
                    repository.storage_provider,
                    provider.provider().provider_id()
                ),
            });
        }
        Ok(provider)
    }
}

/// Constructs readiness-checked production storage providers.
///
/// Provider-specific clients remain private to this registration boundary.
/// The server receives only the generic provider contract and optional
/// capabilities that preserve indexed lookup and direct streaming.
#[derive(Clone)]
pub(crate) struct ServerStorageProviderFactory {
    drive_token_source: Arc<dyn GoogleDriveAccessTokenSource>,
    drive_token_cache: GoogleDriveAccessTokenCache,
    drive_root_validator: GoogleDriveRootValidator,
    #[cfg(test)]
    drive_object_api_base_url: Option<String>,
}

impl ServerStorageProviderFactory {
    /// Builds production provider dependencies.
    pub(crate) fn production() -> ServerResult<Self> {
        Ok(Self {
            drive_token_source: Arc::new(GoogleDriveGcloudTokenProvider::new()),
            drive_token_cache: GoogleDriveAccessTokenCache::default(),
            drive_root_validator: GoogleDriveRootValidator::new()?,
            #[cfg(test)]
            drive_object_api_base_url: None,
        })
    }

    /// Builds deterministic Drive dependencies for server composition tests.
    #[cfg(test)]
    pub(crate) fn with_drive_dependencies(
        token_source: Arc<dyn GoogleDriveAccessTokenSource>,
        root_validator: GoogleDriveRootValidator,
    ) -> Self {
        Self {
            drive_token_source: token_source,
            drive_token_cache: GoogleDriveAccessTokenCache::default(),
            drive_root_validator: root_validator,
            drive_object_api_base_url: None,
        }
    }

    /// Overrides the Drive object API base for loopback server tests.
    #[cfg(test)]
    pub(crate) fn with_drive_object_api_base_url(
        mut self,
        api_base_url: impl Into<String>,
    ) -> Self {
        self.drive_object_api_base_url = Some(api_base_url.into());
        self
    }

    /// Validates configured storage roots and builds repository-scoped adapters.
    pub(crate) async fn build(
        &self,
        config: &ServerConfig,
        metadata: Arc<MetadataDatabase>,
    ) -> ServerResult<ConfiguredStorageProviders> {
        for storage in config.storage_providers.values() {
            storage
                .registration()
                .validate_server_readiness(self)
                .await?;
        }

        let mut by_repository_id = BTreeMap::new();
        for repository in &config.repositories {
            let storage = config
                .storage_providers
                .get(&repository.storage_provider)
                .ok_or_else(|| ServerError::InvalidConfiguration {
                    message: format!(
                        "repository {} references unknown storage provider {}",
                        repository.id, repository.storage_provider
                    ),
                })?;
            let provider = storage.registration().build_server_provider(
                repository.id.clone(),
                metadata.clone(),
                self,
            );
            by_repository_id.insert(repository.id.clone(), provider);
        }

        Ok(ConfiguredStorageProviders { by_repository_id })
    }
}

/// Provider-neutral behavior registered by each storage config variant.
///
/// Runtime transfer concerns are composed here so the server does not match
/// concrete provider variants.
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
    /// Validates provider readiness before the production listener binds.
    fn validate_server_readiness<'a>(
        &'a self,
        factory: &'a ServerStorageProviderFactory,
    ) -> ProviderFuture<'a, StorageResult<()>>;
    /// Builds one repository-scoped production server provider.
    fn build_server_provider(
        &self,
        repository_namespace: String,
        metadata: Arc<MetadataDatabase>,
        factory: &ServerStorageProviderFactory,
    ) -> ServerStorageProvider;
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

    fn validate_server_readiness<'a>(
        &'a self,
        factory: &'a ServerStorageProviderFactory,
    ) -> ProviderFuture<'a, StorageResult<()>> {
        Box::pin(async move {
            let token = factory
                .drive_token_cache
                .get_or_refresh(self, factory.drive_token_source.as_ref())
                .await?;
            factory
                .drive_root_validator
                .validate_root_folder(self, &token)
                .await?;
            Ok(())
        })
    }

    fn build_server_provider(
        &self,
        repository_namespace: String,
        metadata: Arc<MetadataDatabase>,
        factory: &ServerStorageProviderFactory,
    ) -> ServerStorageProvider {
        let provider = GoogleDriveStorageProvider::with_dependencies(
            self.clone(),
            repository_namespace,
            factory.drive_token_source.clone(),
            factory.drive_token_cache.clone(),
            metadata,
        )
        .without_provider_upload_lock();
        #[cfg(test)]
        let provider = match &factory.drive_object_api_base_url {
            Some(api_base_url) => provider.with_object_api_base_url(api_base_url.clone()),
            None => provider,
        };
        let provider = Arc::new(provider);
        ServerStorageProvider::from_provider(provider.clone())
            .with_backend_id_lookup(provider.clone())
            .with_streaming_download(provider)
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
    // The server upload handler disables this provider-level lock because it
    // holds the same durable lock across both the final lookup and the upload.
    acquire_upload_lock: bool,
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
            acquire_upload_lock: true,
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

    #[cfg(test)]
    fn with_object_api_base_url(mut self, api_base_url: impl Into<String>) -> Self {
        self.api_base_url = Some(api_base_url.into());
        self
    }

    /// Disables provider-level locking for the server-owned upload path.
    ///
    /// Callers must already hold the metadata upload lock across the final
    /// object lookup and this provider call. Keeping both layers enabled would
    /// attempt to acquire the same process-shared file lock twice.
    fn without_provider_upload_lock(mut self) -> Self {
        self.acquire_upload_lock = false;
        self
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

    fn lookup_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<Option<StoredObject>>> {
        Box::pin(async move {
            self.validate_repository_namespace(repository_namespace)?;
            self.object_store().await?.lookup_object(object).await
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
            let _upload_lock = if self.acquire_upload_lock {
                self.metadata
                    .acquire_object_upload_lock(
                        repository_namespace.to_owned(),
                        self.storage.id.clone(),
                        object.clone(),
                    )
                    .await
                    .map_err(|error| StorageError::Retryable {
                        provider: self.storage.id.clone(),
                        message: format!("provider upload lock failed: {error}"),
                    })?
            } else {
                None
            };

            // Verify before waiting so cache-hit retries do not serialize
            // large-file reads. Migration keeps lookup and upload under the
            // provider-acquired lock; the server caller already holds that
            // lock across its final lookup and this upload. In both paths the
            // token is minted after lock admission so waits cannot age it.
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

impl BackendIdLookup for GoogleDriveStorageProvider {
    fn lookup_object_by_backend_id<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        backend_id: &'a str,
    ) -> ProviderFuture<'a, StorageResult<Option<StoredObject>>> {
        Box::pin(async move {
            self.validate_repository_namespace(repository_namespace)?;
            self.object_store()
                .await?
                .lookup_object_by_backend_id(object, backend_id)
                .await
        })
    }
}

impl StreamingStorageProvider for GoogleDriveStorageProvider {
    fn download_object_response<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        stored_object: StoredObject,
    ) -> ProviderFuture<'a, StorageResult<StorageDownloadResponse>> {
        Box::pin(async move {
            self.validate_repository_namespace(repository_namespace)?;
            self.object_store()
                .await?
                .download_object_response_for_stored_object(object, stored_object)
                .await
        })
    }
}

#[cfg(test)]
mod tests;
