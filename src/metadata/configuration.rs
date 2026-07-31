//! Synchronization of validated server configuration into metadata parent rows.

use std::{collections::HashMap, path::Path};

use rusqlite::{Connection, params};

use crate::{RepositoryMapping, ServerConfig, ServerError, ServerResult, StorageProviderConfig};

use super::MetadataDatabase;

impl MetadataDatabase {
    /// Synchronizes validated server configuration into metadata parent rows.
    ///
    /// Object metadata references repository and storage-provider rows through
    /// foreign keys. The server calls this during startup before transfer
    /// handlers can record verified uploads. Removed repository mappings remain
    /// as inactive history, but their route keys are released for the current
    /// configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when SQLite cannot upsert the current server
    /// configuration.
    pub fn sync_config(&self, config: &ServerConfig) -> ServerResult<()> {
        let mut connection = self.lock_connection()?;
        let transaction = connection
            .transaction()
            .map_err(|source| self.operation_error(source))?;

        release_inactive_repository_routes(&transaction, config, &self.path)?;

        for storage in config.storage_providers.values() {
            upsert_storage_provider(&transaction, storage, &self.path)?;
        }
        for repository in &config.repositories {
            upsert_repository_mapping(&transaction, repository, &self.path)?;
        }

        transaction
            .commit()
            .map_err(|source| self.operation_error(source))
    }
}

fn upsert_storage_provider(
    connection: &Connection,
    storage: &StorageProviderConfig,
    path: &Path,
) -> ServerResult<()> {
    connection
        .execute(
            "INSERT INTO storage_providers(
                id,
                provider_type,
                backend_root_id,
                display_name
            ) VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(id)
            DO UPDATE SET
                provider_type = excluded.provider_type,
                backend_root_id = excluded.backend_root_id,
                display_name = excluded.display_name,
                updated_at_unix_seconds = unixepoch()",
            params![
                storage.id(),
                storage.provider_type(),
                storage.backend_root_id(),
                storage.display_name()
            ],
        )
        .map(|_| ())
        .map_err(|source| ServerError::MetadataOperation {
            path: path.to_path_buf(),
            source,
        })
}

fn upsert_repository_mapping(
    connection: &Connection,
    repository: &RepositoryMapping,
    path: &Path,
) -> ServerResult<()> {
    connection
        .execute(
            "INSERT INTO repository_mappings(
                id,
                repo_provider_id,
                host,
                owner,
                name,
                storage_provider_id,
                route_path,
                is_active
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)
            ON CONFLICT(id)
            DO UPDATE SET
                repo_provider_id = excluded.repo_provider_id,
                host = excluded.host,
                owner = excluded.owner,
                name = excluded.name,
                storage_provider_id = excluded.storage_provider_id,
                route_path = excluded.route_path,
                is_active = 1,
                updated_at_unix_seconds = unixepoch()",
            params![
                repository.id.as_str(),
                repository.repo_provider.as_str(),
                repository.host.as_str(),
                repository.owner.as_str(),
                repository.name.as_str(),
                repository.storage_provider.as_str(),
                repository.route_path(),
            ],
        )
        .map(|_| ())
        .map_err(|source| ServerError::MetadataOperation {
            path: path.to_path_buf(),
            source,
        })
}

fn release_inactive_repository_routes(
    connection: &Connection,
    config: &ServerConfig,
    path: &Path,
) -> ServerResult<()> {
    let configured_routes = config
        .repositories
        .iter()
        .map(|repository| (repository.id.as_str(), repository.route_path()))
        .collect::<HashMap<_, _>>();
    let persisted_mappings = {
        let mut statement = connection
            .prepare("SELECT id, route_path FROM repository_mappings")
            .map_err(|source| ServerError::MetadataOperation {
                path: path.to_path_buf(),
                source,
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|source| ServerError::MetadataOperation {
                path: path.to_path_buf(),
                source,
            })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|source| ServerError::MetadataOperation {
                path: path.to_path_buf(),
                source,
            })?
    };

    for (id, persisted_route) in persisted_mappings {
        let retains_route = configured_routes
            .get(id.as_str())
            .is_some_and(|configured_route| *configured_route == persisted_route);
        if retains_route {
            continue;
        }

        connection
            .execute(
                "UPDATE repository_mappings
                 SET route_path = 'inactive:' || id,
                     is_active = 0,
                     updated_at_unix_seconds = unixepoch()
                 WHERE id = ?1
                   AND (route_path != 'inactive:' || id OR is_active != 0)",
                [&id],
            )
            .map_err(|source| ServerError::MetadataOperation {
                path: path.to_path_buf(),
                source,
            })?;
    }

    Ok(())
}

#[cfg(test)]
pub(super) fn server_config_with_repository(
    repository_id: &str,
    owner: &str,
    name: &str,
    provider_repository_id: &str,
) -> ServerConfig {
    ServerConfig::load_from_str(&format!(
        r#"
server:
  public_url: http://127.0.0.1:8080
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
    root_folder_id: drive-root
repositories:
  - id: {repository_id}
    repo_provider: github-main
    host: github.com
    owner: {owner}
    name: {name}
    provider_repository_id: "{provider_repository_id}"
    storage_provider: drive-user-a
"#
    ))
    .expect("test config should load")
}

#[cfg(test)]
mod tests {
    use crate::{RepositoryUser, ServerConfig};

    use super::*;
    use crate::metadata::objects::test_support::lfs_object;

    #[test]
    fn sync_config_upserts_storage_and_repository_parent_rows() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");
        let config = ServerConfig::load_from_str(
            r#"
server:
  public_url: http://127.0.0.1:8080
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
    root_folder_id: drive-root
repositories:
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    provider_repository_id: "8675309"
    storage_provider: drive-user-a
"#,
        )
        .expect("test config should load");
        let object = lfs_object('e', 42);
        let user = RepositoryUser::new("github-main", "octocat", Some("user-1".to_owned()));

        database
            .sync_config(&config)
            .expect("metadata config sync should succeed");
        let record = database
            .record_verified_object(
                "github-main:owner/repo",
                "drive-user-a",
                &object,
                "drive-file-verified",
                &user,
            )
            .expect("verified object should record after config sync");

        assert_eq!(record.repo_id, "github-main:owner/repo");
        assert_eq!(record.storage_provider_id, "drive-user-a");
    }

    #[test]
    fn sync_config_releases_removed_routes_without_deleting_object_history() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");
        let original_config =
            server_config_with_repository("github-main:owner/archived", "owner", "repo", "8675309");
        database
            .sync_config(&original_config)
            .expect("original metadata config sync should succeed");
        let object = lfs_object('e', 42);
        database
            .record_verified_object(
                "github-main:owner/archived",
                "drive-user-a",
                &object,
                "drive-file-verified",
                &RepositoryUser::new("github-main", "octocat", Some("user-1".to_owned())),
            )
            .expect("verified object should record for original mapping");

        let replacement_config = server_config_with_repository(
            "github-main:owner/replacement",
            "owner",
            "repo",
            "97531",
        );
        database
            .sync_config(&replacement_config)
            .expect("replacement mapping should claim the released route");

        let connection = database
            .connection
            .lock()
            .expect("metadata connection should lock");
        let original_mapping: (String, bool) = connection
            .query_row(
                "SELECT route_path, is_active
                 FROM repository_mappings
                 WHERE id = ?1",
                ["github-main:owner/archived"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("original mapping should remain as history");
        let replacement_mapping: (String, bool) = connection
            .query_row(
                "SELECT route_path, is_active
                 FROM repository_mappings
                 WHERE id = ?1",
                ["github-main:owner/replacement"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("replacement mapping should be persisted");
        drop(connection);

        assert_eq!(
            original_mapping,
            ("inactive:github-main:owner/archived".to_owned(), false)
        );
        assert_eq!(
            replacement_mapping,
            ("/github.com/owner/repo.git/info/lfs".to_owned(), true)
        );
        assert!(
            database
                .lookup_object("github-main:owner/archived", "drive-user-a", &object)
                .expect("historical object lookup should succeed")
                .is_some()
        );
    }
}
