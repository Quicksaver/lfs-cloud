//! Explicit repository route and storage mappings.

use serde::Deserialize;

use super::{
    resolution::resolve_required,
    validation::{invalid_config, validate_route_component, validate_route_host},
};
use crate::ServerResult;

/// Explicit repository-to-storage mapping served by this instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryMapping {
    /// Stable mapping ID used in server config and metadata records.
    pub id: String,
    /// Configured repository-provider ID.
    pub repo_provider: String,
    /// Repository host, such as `github.com`.
    pub host: String,
    /// Repository owner or namespace.
    pub owner: String,
    /// Repository name without the `.git` suffix.
    pub name: String,
    /// Provider-stable repository ID used to detect rename and name reuse.
    pub provider_repository_id: String,
    /// Configured storage-provider ID.
    pub storage_provider: String,
}

impl RepositoryMapping {
    /// Returns the Git LFS route path for this repository mapping.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfscloud::RepositoryMapping;
    ///
    /// let mapping = RepositoryMapping {
    ///     id: "github-main:owner/repo".to_owned(),
    ///     repo_provider: "github-main".to_owned(),
    ///     host: "github.com".to_owned(),
    ///     owner: "owner".to_owned(),
    ///     name: "repo".to_owned(),
    ///     provider_repository_id: "123456789".to_owned(),
    ///     storage_provider: "drive-user-a".to_owned(),
    /// };
    ///
    /// assert_eq!(mapping.route_path(), "/github.com/owner/repo.git/info/lfs");
    /// ```
    #[must_use]
    pub fn route_path(&self) -> String {
        format!("/{}/{}/{}.git/info/lfs", self.host, self.owner, self.name)
    }

    pub(super) fn from_raw(
        index: usize,
        raw: RawRepositoryMapping,
        env: &mut impl FnMut(&str) -> Option<String>,
    ) -> ServerResult<Self> {
        let base_path = format!("repositories[{index}]");
        let id = resolve_required(raw.id, format!("{base_path}.id"), env)?;
        let repo_provider =
            resolve_required(raw.repo_provider, format!("{base_path}.repo_provider"), env)?;
        let host = resolve_required(raw.host, format!("{base_path}.host"), env)?;
        validate_route_host(&host, format!("{base_path}.host"))?;
        let owner = resolve_required(raw.owner, format!("{base_path}.owner"), env)?;
        validate_route_component(&owner, format!("{base_path}.owner"))?;
        let name = resolve_required(raw.name, format!("{base_path}.name"), env)?;
        validate_route_component(&name, format!("{base_path}.name"))?;
        if name.ends_with(".git") {
            return invalid_config(
                format!("{base_path}.name"),
                "must omit the .git suffix because the route adds it",
            );
        }
        let provider_repository_id = resolve_required(
            raw.provider_repository_id,
            format!("{base_path}.provider_repository_id"),
            env,
        )?;
        let storage_provider = resolve_required(
            raw.storage_provider,
            format!("{base_path}.storage_provider"),
            env,
        )?;

        Ok(Self {
            id,
            repo_provider,
            host,
            owner,
            name,
            provider_repository_id,
            storage_provider,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRepositoryMapping {
    #[serde(default)]
    pub(super) id: Option<String>,
    #[serde(default)]
    pub(super) repo_provider: Option<String>,
    #[serde(default)]
    pub(super) host: Option<String>,
    #[serde(default)]
    pub(super) owner: Option<String>,
    #[serde(default)]
    pub(super) name: Option<String>,
    #[serde(default)]
    pub(super) provider_repository_id: Option<String>,
    #[serde(default)]
    pub(super) storage_provider: Option<String>,
}
