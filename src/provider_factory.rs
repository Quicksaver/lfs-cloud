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

trait RepositoryProviderRegistration {
    fn id(&self) -> &str;
    fn provider_type(&self) -> &'static str;
    fn build_provider(&self) -> Arc<dyn RepositoryProvider + Send + Sync>;
    fn github_pat_provider(&self) -> Option<&GitHubProviderConfig>;
    fn validate_mapping(&self, repository: &RepositoryMapping, path: &str) -> ServerResult<()>;
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

    fn github_pat_provider(&self) -> Option<&GitHubProviderConfig> {
        Some(self)
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

    pub(crate) fn build_provider(&self) -> Arc<dyn RepositoryProvider + Send + Sync> {
        self.registration().build_provider()
    }

    fn github_pat_provider(&self) -> Option<&GitHubProviderConfig> {
        self.registration().github_pat_provider()
    }

    pub(crate) fn validate_mapping(
        &self,
        repository: &RepositoryMapping,
        path: &str,
    ) -> ServerResult<()> {
        self.registration().validate_mapping(repository, path)
    }

    pub(crate) fn route_identity_is_case_insensitive(&self) -> bool {
        self.registration().route_identity_is_case_insensitive()
    }
}

impl ServerConfig {
    pub(crate) fn single_github_pat_provider(&self) -> ServerResult<Option<&GitHubProviderConfig>> {
        let mut providers = self
            .repository_providers
            .values()
            .filter_map(RepositoryProviderConfig::github_pat_provider);
        let provider = providers.next();
        if providers.next().is_some() {
            return Err(ServerError::InvalidConfiguration {
                message: "multiple GitHub repository providers are not yet supported by single-account PAT authentication".to_owned(),
            });
        }
        Ok(provider)
    }
}

trait StorageProviderRegistration {
    fn id(&self) -> &str;
    fn provider_type(&self) -> &'static str;
    fn backend_root_id(&self) -> &str;
    fn display_name(&self) -> Option<&str>;
    fn validate_local_readiness(&self) -> StorageResult<()>;
    fn local_readiness_error_message(&self) -> String;
    fn validate_runtime<'a>(
        &'a self,
        token_cache: &'a GoogleDriveAccessTokenCache,
        token_source: &'a dyn GoogleDriveAccessTokenSource,
        root_validator: &'a GoogleDriveRootValidator,
    ) -> ProviderFuture<'a, ServerResult<()>>;
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

    fn validate_local_readiness(&self) -> StorageResult<()> {
        GoogleDriveGcloudTokenProvider::new().validate_local_readiness(&self.id, &self.credentials)
    }

    fn local_readiness_error_message(&self) -> String {
        format!(
            "Google Drive credential for {} is not usable; check the configured gcloud ADC credentials directory",
            self.id
        )
    }

    fn validate_runtime<'a>(
        &'a self,
        token_cache: &'a GoogleDriveAccessTokenCache,
        token_source: &'a dyn GoogleDriveAccessTokenSource,
        root_validator: &'a GoogleDriveRootValidator,
    ) -> ProviderFuture<'a, ServerResult<()>> {
        Box::pin(async move {
            let token = token_cache.get_or_refresh(self, token_source).await?;
            root_validator.validate_root_folder(self, &token).await?;
            Ok(())
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
                None,
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

    pub(crate) fn backend_root_id(&self) -> &str {
        self.registration().backend_root_id()
    }

    pub(crate) fn display_name(&self) -> Option<&str> {
        self.registration().display_name()
    }

    pub(crate) fn validate_local_readiness(&self) -> StorageResult<()> {
        self.registration().validate_local_readiness()
    }

    pub(crate) fn local_readiness_error_message(&self) -> String {
        self.registration().local_readiness_error_message()
    }

    pub(crate) async fn validate_runtime(
        &self,
        token_cache: &GoogleDriveAccessTokenCache,
        token_source: &dyn GoogleDriveAccessTokenSource,
        root_validator: &GoogleDriveRootValidator,
    ) -> ServerResult<()> {
        self.registration()
            .validate_runtime(token_cache, token_source, root_validator)
            .await
    }

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
    fn with_dependencies(
        storage: GoogleDriveStorageConfig,
        repository_namespace: String,
        token_source: Arc<dyn GoogleDriveAccessTokenSource>,
        token_cache: GoogleDriveAccessTokenCache,
        metadata: Arc<MetadataDatabase>,
        #[cfg(test)] api_base_url: Option<String>,
        #[cfg(not(test))] _api_base_url: Option<String>,
    ) -> Self {
        Self {
            storage,
            repository_namespace,
            token_source,
            token_cache,
            metadata,
            #[cfg(test)]
            api_base_url,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_dependencies(
        storage: GoogleDriveStorageConfig,
        repository_namespace: impl Into<String>,
        token_source: Arc<dyn GoogleDriveAccessTokenSource>,
        token_cache: GoogleDriveAccessTokenCache,
        metadata: Arc<MetadataDatabase>,
        api_base_url: Option<String>,
    ) -> Self {
        Self::with_dependencies(
            storage,
            repository_namespace.into(),
            token_source,
            token_cache,
            metadata,
            api_base_url,
        )
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

            // Verify before waiting, but mint a fresh token after acquiring the
            // cross-process lock so long lock waits cannot age the credential.
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
