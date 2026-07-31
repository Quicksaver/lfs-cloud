//! YAML-preserving-value edits for the private server configuration.
//!
//! The server's runtime model resolves environment references and defaults,
//! which makes it unsuitable for writing config back to disk. This module
//! edits the YAML value tree instead so values such as `${HOME}` remain
//! references. Saving revalidates the complete runtime configuration before
//! atomically replacing the original file.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use tempfile::NamedTempFile;
use yaml_rust2::{Yaml, YamlEmitter, YamlLoader, yaml::Hash};

use crate::{CliError, CliResult, ServerConfig};

const REPOSITORY_PROVIDERS_KEY: &str = "repository_providers";
const STORAGE_PROVIDERS_KEY: &str = "storage_providers";
const REPOSITORIES_KEY: &str = "repositories";

/// Whether an upsert materially changed the configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EditOutcome {
    /// A new entry was added.
    Added,
    /// An existing entry was updated.
    Updated,
    /// The requested values already matched the entry.
    Unchanged,
}

/// Whether a remove operation found its target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemoveOutcome {
    /// An existing entry was removed.
    Removed,
    /// No entry with the requested ID existed.
    NotFound,
}

/// Editable repository-provider values before environment interpolation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RepositoryProviderValues {
    pub(crate) id: String,
    pub(crate) provider_type: Option<String>,
    pub(crate) api_url: Option<String>,
    pub(crate) personal_access_token: Option<String>,
}

/// Editable storage-provider values before environment interpolation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StorageProviderValues {
    pub(crate) id: String,
    pub(crate) provider_type: Option<String>,
    pub(crate) credentials_type: Option<String>,
    pub(crate) config_dir: Option<String>,
    pub(crate) executable: Option<String>,
    pub(crate) root_folder_id: Option<String>,
    pub(crate) display_name: Option<String>,
}

/// Editable served-repository mapping values.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RepositoryValues {
    pub(crate) id: String,
    pub(crate) repo_provider: Option<String>,
    pub(crate) host: Option<String>,
    pub(crate) owner: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) provider_repository_id: Option<String>,
    pub(crate) storage_provider: Option<String>,
}

/// An editable private server configuration.
#[derive(Debug)]
pub(crate) struct EditableServerConfig {
    path: PathBuf,
    document: Yaml,
}

impl EditableServerConfig {
    /// Loads one YAML document from the requested config path.
    pub(crate) fn load(path: impl AsRef<Path>) -> CliResult<Self> {
        let path = path.as_ref().to_path_buf();
        let contents = fs::read_to_string(&path).map_err(|source| CliError::Io {
            context: format!("failed to read server config {}", path.display()),
            source,
        })?;
        let mut documents =
            YamlLoader::load_from_str(&contents).map_err(|error| CliError::InvalidArguments {
                message: format!("failed to parse server config {}: {error}", path.display()),
            })?;
        if documents.len() != 1 {
            return Err(CliError::InvalidArguments {
                message: format!(
                    "server config {} must contain exactly one YAML document",
                    path.display()
                ),
            });
        }
        let document = documents.remove(0);
        if !matches!(document, Yaml::Hash(_)) {
            return Err(CliError::InvalidArguments {
                message: format!(
                    "server config {} must contain a YAML mapping at its root",
                    path.display()
                ),
            });
        }

        Ok(Self { path, document })
    }

    /// Returns the config file path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Returns one repository-provider entry by ID.
    pub(crate) fn repository_provider(
        &self,
        id: &str,
    ) -> CliResult<Option<RepositoryProviderValues>> {
        let Some(entry) = self.map_entry(REPOSITORY_PROVIDERS_KEY, id)? else {
            return Ok(None);
        };

        Ok(Some(RepositoryProviderValues {
            id: id.to_owned(),
            provider_type: string_value(entry, "type"),
            api_url: string_value(entry, "api_url"),
            personal_access_token: string_value(entry, "personal_access_token"),
        }))
    }

    /// Lists repository-provider entries in file order.
    pub(crate) fn repository_providers(&self) -> CliResult<Vec<RepositoryProviderValues>> {
        self.map_entries(REPOSITORY_PROVIDERS_KEY)?
            .into_iter()
            .map(|(id, entry)| {
                Ok(RepositoryProviderValues {
                    id,
                    provider_type: string_value(entry, "type"),
                    api_url: string_value(entry, "api_url"),
                    personal_access_token: string_value(entry, "personal_access_token"),
                })
            })
            .collect()
    }

    /// Adds or updates one repository-provider entry.
    pub(crate) fn upsert_repository_provider(
        &mut self,
        values: RepositoryProviderValues,
    ) -> CliResult<EditOutcome> {
        require_nonempty(&values.id, "--id")?;
        let existed = self
            .map_entry(REPOSITORY_PROVIDERS_KEY, &values.id)?
            .is_some();
        if !existed {
            require_some(&values.provider_type, "--type")?;
            require_some(&values.api_url, "--api-url")?;
            require_some(&values.personal_access_token, "--personal-access-token")?;
        }

        let before = self.document.clone();
        let entry = self.map_entry_mut(REPOSITORY_PROVIDERS_KEY, &values.id)?;
        set_optional_string(entry, "type", values.provider_type);
        set_optional_string(entry, "api_url", values.api_url);
        set_optional_string(entry, "personal_access_token", values.personal_access_token);

        Ok(edit_outcome(existed, before != self.document))
    }

    /// Removes one repository provider.
    pub(crate) fn remove_repository_provider(&mut self, id: &str) -> CliResult<RemoveOutcome> {
        remove_map_entry(self.root_mut()?, REPOSITORY_PROVIDERS_KEY, id)
    }

    /// Returns one storage-provider entry by ID.
    pub(crate) fn storage_provider(&self, id: &str) -> CliResult<Option<StorageProviderValues>> {
        let Some(entry) = self.map_entry(STORAGE_PROVIDERS_KEY, id)? else {
            return Ok(None);
        };
        let credentials = optional_hash(entry, "credentials");

        Ok(Some(StorageProviderValues {
            id: id.to_owned(),
            provider_type: string_value(entry, "type"),
            credentials_type: credentials.and_then(|value| string_value(value, "type")),
            config_dir: credentials.and_then(|value| string_value(value, "config_dir")),
            executable: credentials.and_then(|value| string_value(value, "executable")),
            root_folder_id: string_value(entry, "root_folder_id"),
            display_name: string_value(entry, "display_name"),
        }))
    }

    /// Lists storage-provider entries in file order.
    pub(crate) fn storage_providers(&self) -> CliResult<Vec<StorageProviderValues>> {
        self.map_entries(STORAGE_PROVIDERS_KEY)?
            .into_iter()
            .map(|(id, entry)| {
                let credentials = optional_hash(entry, "credentials");
                Ok(StorageProviderValues {
                    id,
                    provider_type: string_value(entry, "type"),
                    credentials_type: credentials.and_then(|value| string_value(value, "type")),
                    config_dir: credentials.and_then(|value| string_value(value, "config_dir")),
                    executable: credentials.and_then(|value| string_value(value, "executable")),
                    root_folder_id: string_value(entry, "root_folder_id"),
                    display_name: string_value(entry, "display_name"),
                })
            })
            .collect()
    }

    /// Adds or updates one storage-provider entry.
    pub(crate) fn upsert_storage_provider(
        &mut self,
        values: StorageProviderValues,
    ) -> CliResult<EditOutcome> {
        require_nonempty(&values.id, "--id")?;
        let existed = self.map_entry(STORAGE_PROVIDERS_KEY, &values.id)?.is_some();
        if !existed {
            require_some(&values.provider_type, "--type")?;
            require_some(&values.credentials_type, "--credentials-type")?;
            require_some(&values.config_dir, "--config-dir")?;
            require_some(&values.root_folder_id, "--root-folder-id")?;
        }

        let before = self.document.clone();
        let entry = self.map_entry_mut(STORAGE_PROVIDERS_KEY, &values.id)?;
        set_optional_string(entry, "type", values.provider_type);
        if values.credentials_type.is_some()
            || values.config_dir.is_some()
            || values.executable.is_some()
        {
            let credentials = nested_hash_mut(entry, "credentials")?;
            set_optional_string(credentials, "type", values.credentials_type);
            set_optional_string(credentials, "config_dir", values.config_dir);
            set_optional_string(credentials, "executable", values.executable);
        }
        set_optional_string(entry, "root_folder_id", values.root_folder_id);
        set_optional_string(entry, "display_name", values.display_name);

        Ok(edit_outcome(existed, before != self.document))
    }

    /// Removes one storage provider.
    pub(crate) fn remove_storage_provider(&mut self, id: &str) -> CliResult<RemoveOutcome> {
        remove_map_entry(self.root_mut()?, STORAGE_PROVIDERS_KEY, id)
    }

    /// Returns one served-repository mapping by ID.
    pub(crate) fn repository(&self, id: &str) -> CliResult<Option<RepositoryValues>> {
        Ok(self
            .repository_entries()?
            .into_iter()
            .find(|repository| repository.id == id))
    }

    /// Lists served-repository mappings in file order.
    pub(crate) fn repositories(&self) -> CliResult<Vec<RepositoryValues>> {
        self.repository_entries()
    }

    /// Adds or updates one served-repository mapping.
    pub(crate) fn upsert_repository(&mut self, values: RepositoryValues) -> CliResult<EditOutcome> {
        require_nonempty(&values.id, "--id")?;
        let existing_index = self
            .repository_entries()?
            .iter()
            .position(|repository| repository.id == values.id);
        if existing_index.is_none() {
            require_some(&values.repo_provider, "--repo-provider")?;
            require_some(&values.host, "--host")?;
            require_some(&values.owner, "--owner")?;
            require_some(&values.name, "--name")?;
            require_some(&values.provider_repository_id, "--provider-repository-id")?;
            require_some(&values.storage_provider, "--storage-provider")?;
        }

        let before = self.document.clone();
        let repositories = self.repositories_mut()?;
        let entry = if let Some(index) = existing_index {
            yaml_hash_mut(&mut repositories[index])
                .ok_or_else(|| invalid_section(REPOSITORIES_KEY, "a sequence of mappings"))?
        } else {
            repositories.push(Yaml::Hash(Hash::new()));
            repositories
                .last_mut()
                .and_then(yaml_hash_mut)
                .expect("new repository entry is a mapping")
        };
        set_string(entry, "id", values.id);
        set_optional_string(entry, "repo_provider", values.repo_provider);
        set_optional_string(entry, "host", values.host);
        set_optional_string(entry, "owner", values.owner);
        set_optional_string(entry, "name", values.name);
        set_optional_string(
            entry,
            "provider_repository_id",
            values.provider_repository_id,
        );
        set_optional_string(entry, "storage_provider", values.storage_provider);

        Ok(edit_outcome(
            existing_index.is_some(),
            before != self.document,
        ))
    }

    /// Removes one served-repository mapping.
    pub(crate) fn remove_repository(&mut self, id: &str) -> CliResult<RemoveOutcome> {
        let repositories = self.repositories_mut()?;
        let Some(index) = repositories.iter().position(|entry| {
            entry
                .as_hash()
                .and_then(|entry| string_value(entry, "id"))
                .is_some_and(|entry_id| entry_id == id)
        }) else {
            return Ok(RemoveOutcome::NotFound);
        };
        repositories.remove(index);
        Ok(RemoveOutcome::Removed)
    }

    /// Validates and atomically writes the current document.
    pub(crate) fn save(&self) -> CliResult<()> {
        let rendered = self.render()?;
        ServerConfig::load_from_str(&rendered).map_err(|error| CliError::InvalidArguments {
            message: format!("updated server configuration is invalid: {error}"),
        })?;

        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut temporary = NamedTempFile::new_in(parent).map_err(|source| CliError::Io {
            context: format!(
                "failed to create temporary server config beside {}",
                self.path.display()
            ),
            source,
        })?;
        let permissions = fs::metadata(&self.path)
            .map_err(|source| CliError::Io {
                context: format!(
                    "failed to inspect server config permissions {}",
                    self.path.display()
                ),
                source,
            })?
            .permissions();
        temporary
            .as_file()
            .set_permissions(permissions)
            .map_err(|source| CliError::Io {
                context: format!(
                    "failed to preserve server config permissions {}",
                    self.path.display()
                ),
                source,
            })?;
        temporary
            .write_all(rendered.as_bytes())
            .and_then(|()| temporary.flush())
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|source| CliError::Io {
                context: format!(
                    "failed to write temporary server config for {}",
                    self.path.display()
                ),
                source,
            })?;
        temporary
            .persist(&self.path)
            .map_err(|error| CliError::Io {
                context: format!("failed to replace server config {}", self.path.display()),
                source: error.error,
            })?;
        Ok(())
    }

    fn render(&self) -> CliResult<String> {
        let mut rendered = String::new();
        let mut emitter = YamlEmitter::new(&mut rendered);
        emitter.compact(false);
        emitter
            .dump(&self.document)
            .map_err(|error| CliError::InvalidArguments {
                message: format!("failed to render server config: {error}"),
            })?;
        rendered.push('\n');
        Ok(rendered)
    }

    fn root(&self) -> CliResult<&Hash> {
        self.document
            .as_hash()
            .ok_or_else(|| invalid_section("<root>", "a mapping"))
    }

    fn root_mut(&mut self) -> CliResult<&mut Hash> {
        yaml_hash_mut(&mut self.document).ok_or_else(|| invalid_section("<root>", "a mapping"))
    }

    fn map_entry(&self, section: &str, id: &str) -> CliResult<Option<&Hash>> {
        let Some(section_value) = self.root()?.get(&key(section)) else {
            return Ok(None);
        };
        let section_values = section_value
            .as_hash()
            .ok_or_else(|| invalid_section(section, "a mapping"))?;
        section_values
            .get(&key(id))
            .map(|entry| {
                entry
                    .as_hash()
                    .ok_or_else(|| invalid_section(id, "a mapping"))
            })
            .transpose()
    }

    fn map_entries(&self, section: &str) -> CliResult<Vec<(String, &Hash)>> {
        let Some(values) = self.root()?.get(&key(section)) else {
            return Ok(Vec::new());
        };
        let values = values
            .as_hash()
            .ok_or_else(|| invalid_section(section, "a mapping"))?;
        values
            .iter()
            .map(|(id, entry)| {
                let id = id
                    .as_str()
                    .ok_or_else(|| invalid_section(section, "a mapping with string entry IDs"))?;
                let entry = entry
                    .as_hash()
                    .ok_or_else(|| invalid_section(section, "a mapping of mappings"))?;
                Ok((id.to_owned(), entry))
            })
            .collect()
    }

    fn map_entry_mut(&mut self, section: &str, id: &str) -> CliResult<&mut Hash> {
        let root = self.root_mut()?;
        let section_value = root
            .entry(key(section))
            .or_insert_with(|| Yaml::Hash(Hash::new()));
        let section_values =
            yaml_hash_mut(section_value).ok_or_else(|| invalid_section(section, "a mapping"))?;
        let entry = section_values
            .entry(key(id))
            .or_insert_with(|| Yaml::Hash(Hash::new()));
        yaml_hash_mut(entry).ok_or_else(|| invalid_section(id, "a mapping"))
    }

    fn repository_entries(&self) -> CliResult<Vec<RepositoryValues>> {
        let Some(repositories) = self.root()?.get(&key(REPOSITORIES_KEY)) else {
            return Ok(Vec::new());
        };
        repositories
            .as_vec()
            .ok_or_else(|| invalid_section(REPOSITORIES_KEY, "a sequence of mappings"))?
            .iter()
            .map(|entry| {
                let entry = entry
                    .as_hash()
                    .ok_or_else(|| invalid_section(REPOSITORIES_KEY, "a sequence of mappings"))?;
                Ok(RepositoryValues {
                    id: string_value(entry, "id").unwrap_or_default(),
                    repo_provider: string_value(entry, "repo_provider"),
                    host: string_value(entry, "host"),
                    owner: string_value(entry, "owner"),
                    name: string_value(entry, "name"),
                    provider_repository_id: string_value(entry, "provider_repository_id"),
                    storage_provider: string_value(entry, "storage_provider"),
                })
            })
            .collect()
    }

    fn repositories_mut(&mut self) -> CliResult<&mut Vec<Yaml>> {
        let repositories = self
            .root_mut()?
            .entry(key(REPOSITORIES_KEY))
            .or_insert_with(|| Yaml::Array(Vec::new()));
        yaml_vec_mut(repositories)
            .ok_or_else(|| invalid_section(REPOSITORIES_KEY, "a sequence of mappings"))
    }
}

fn remove_map_entry(root: &mut Hash, section: &str, id: &str) -> CliResult<RemoveOutcome> {
    let Some(values) = root.get_mut(&key(section)) else {
        return Ok(RemoveOutcome::NotFound);
    };
    let values = yaml_hash_mut(values).ok_or_else(|| invalid_section(section, "a mapping"))?;
    Ok(if values.remove(&key(id)).is_some() {
        RemoveOutcome::Removed
    } else {
        RemoveOutcome::NotFound
    })
}

fn nested_hash_mut<'a>(parent: &'a mut Hash, name: &str) -> CliResult<&'a mut Hash> {
    let value = parent
        .entry(key(name))
        .or_insert_with(|| Yaml::Hash(Hash::new()));
    yaml_hash_mut(value).ok_or_else(|| invalid_section(name, "a mapping"))
}

fn optional_hash<'a>(parent: &'a Hash, name: &str) -> Option<&'a Hash> {
    parent.get(&key(name)).and_then(Yaml::as_hash)
}

fn set_optional_string(mapping: &mut Hash, name: &str, value: Option<String>) {
    if let Some(value) = value {
        set_string(mapping, name, value);
    }
}

fn set_string(mapping: &mut Hash, name: &str, value: String) {
    mapping.insert(key(name), Yaml::String(value));
}

fn string_value(mapping: &Hash, name: &str) -> Option<String> {
    mapping
        .get(&key(name))
        .and_then(Yaml::as_str)
        .map(ToOwned::to_owned)
}

fn key(value: &str) -> Yaml {
    Yaml::String(value.to_owned())
}

fn yaml_hash_mut(yaml: &mut Yaml) -> Option<&mut Hash> {
    match yaml {
        Yaml::Hash(value) => Some(value),
        _ => None,
    }
}

fn yaml_vec_mut(yaml: &mut Yaml) -> Option<&mut Vec<Yaml>> {
    match yaml {
        Yaml::Array(value) => Some(value),
        _ => None,
    }
}

fn require_some<T>(value: &Option<T>, flag: &str) -> CliResult<()> {
    if value.is_none() {
        return Err(CliError::InvalidArguments {
            message: format!("{flag} is required when adding a new entry with flags"),
        });
    }
    Ok(())
}

fn require_nonempty(value: &str, flag: &str) -> CliResult<()> {
    if value.trim().is_empty() {
        return Err(CliError::InvalidArguments {
            message: format!("{flag} must not be empty"),
        });
    }
    Ok(())
}

fn invalid_section(section: &str, expected: &str) -> CliError {
    CliError::InvalidArguments {
        message: format!("server config {section} must be {expected}"),
    }
}

fn edit_outcome(existed: bool, changed: bool) -> EditOutcome {
    match (existed, changed) {
        (false, _) => EditOutcome::Added,
        (true, true) => EditOutcome::Updated,
        (true, false) => EditOutcome::Unchanged,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{
        EditOutcome, EditableServerConfig, RemoveOutcome, RepositoryProviderValues,
        RepositoryValues, StorageProviderValues,
    };
    use crate::ServerConfig;

    fn valid_config() -> &'static str {
        r#"
server:
  host: 127.0.0.1
  port: 8080
  public_url: http://127.0.0.1:8080

repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    personal_access_token: test-pat

storage_providers:
  drive-main:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: ./gcloud
    root_folder_id: root-a

repositories:
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    provider_repository_id: "123"
    storage_provider: drive-main
"#
    }

    fn fixture() -> (TempDir, std::path::PathBuf) {
        let temp = TempDir::new().expect("temporary directory should be created");
        let path = temp.path().join("lfscloud.yml");
        fs::write(&path, valid_config()).expect("config fixture should be written");
        (temp, path)
    }

    #[test]
    fn partial_upserts_are_idempotent_and_preserve_other_values() {
        let (_temp, path) = fixture();
        let mut config = EditableServerConfig::load(&path).expect("config should load");

        assert_eq!(
            config
                .upsert_repository_provider(RepositoryProviderValues {
                    id: "github-main".to_owned(),
                    api_url: Some("https://github.example/api/v3".to_owned()),
                    ..RepositoryProviderValues::default()
                })
                .expect("repository provider should update"),
            EditOutcome::Updated
        );
        assert_eq!(
            config
                .upsert_storage_provider(StorageProviderValues {
                    id: "drive-main".to_owned(),
                    display_name: Some("Primary Drive".to_owned()),
                    ..StorageProviderValues::default()
                })
                .expect("storage provider should update"),
            EditOutcome::Updated
        );
        assert_eq!(
            config
                .upsert_repository(RepositoryValues {
                    id: "github-main:owner/repo".to_owned(),
                    name: Some("renamed".to_owned()),
                    ..RepositoryValues::default()
                })
                .expect("repository should update"),
            EditOutcome::Updated
        );
        config.save().expect("updated config should save");

        let loaded = ServerConfig::load_from_path(&path).expect("saved config should be valid");
        assert_eq!(
            loaded.repository_providers["github-main"].id(),
            "github-main"
        );
        assert_eq!(loaded.storage_providers["drive-main"].id(), "drive-main");
        assert_eq!(loaded.repositories[0].name, "renamed");

        let mut config = EditableServerConfig::load(&path).expect("config should reload");
        assert_eq!(
            config
                .upsert_repository(RepositoryValues {
                    id: "github-main:owner/repo".to_owned(),
                    name: Some("renamed".to_owned()),
                    ..RepositoryValues::default()
                })
                .expect("identical repository update should succeed"),
            EditOutcome::Unchanged
        );
    }

    #[test]
    fn new_entries_require_all_required_flag_values() {
        let (_temp, path) = fixture();
        let mut config = EditableServerConfig::load(path).expect("config should load");

        let error = config
            .upsert_repository(RepositoryValues {
                id: "github-main:owner/new".to_owned(),
                repo_provider: Some("github-main".to_owned()),
                ..RepositoryValues::default()
            })
            .expect_err("incomplete new repository must fail");

        assert!(error.to_string().contains("--host"));
    }

    #[test]
    fn remove_is_idempotent_and_referenced_provider_removal_is_not_saved() {
        let (_temp, path) = fixture();
        let mut config = EditableServerConfig::load(&path).expect("config should load");

        assert_eq!(
            config
                .remove_storage_provider("drive-main")
                .expect("storage remove should succeed in memory"),
            RemoveOutcome::Removed
        );
        let error = config
            .save()
            .expect_err("referenced storage removal must fail validation");
        assert!(error.to_string().contains("unknown storage provider"));
        assert!(
            ServerConfig::load_from_path(&path)
                .expect("failed save must leave original config intact")
                .storage_providers
                .contains_key("drive-main")
        );

        let mut config = EditableServerConfig::load(&path).expect("config should reload");
        assert_eq!(
            config
                .remove_repository("github-main:owner/repo")
                .expect("repository remove should succeed"),
            RemoveOutcome::Removed
        );
        assert_eq!(
            config
                .remove_repository("github-main:owner/repo")
                .expect("repeated repository remove should succeed"),
            RemoveOutcome::NotFound
        );
        config.save().expect("unreferenced config should save");

        let mut config = EditableServerConfig::load(&path).expect("config should reload");
        assert_eq!(
            config
                .remove_storage_provider("drive-main")
                .expect("storage remove should succeed"),
            RemoveOutcome::Removed
        );
        config
            .save()
            .expect("unreferenced storage removal should save");
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_existing_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, path) = fixture();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
            .expect("fixture permissions should be set");
        let mut config = EditableServerConfig::load(&path).expect("config should load");
        config
            .upsert_storage_provider(StorageProviderValues {
                id: "drive-main".to_owned(),
                display_name: Some("Permission Test".to_owned()),
                ..StorageProviderValues::default()
            })
            .expect("storage should update");
        config.save().expect("config should save");

        assert_eq!(
            fs::metadata(path)
                .expect("saved config metadata should load")
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }
}
