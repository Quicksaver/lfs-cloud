//! Environment interpolation and config-relative path resolution.

use std::path::{Path, PathBuf};

use super::{DEFAULT_METADATA_DB_FILE, DEFAULT_METADATA_DIR};
use crate::ServerResult;

use super::validation::{invalid_config, invalid_config_error, validate_no_outer_whitespace};

pub(super) fn resolve_required(
    value: Option<String>,
    path: impl Into<String>,
    env: &mut impl FnMut(&str) -> Option<String>,
) -> ServerResult<String> {
    let path = path.into();
    let value = value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| invalid_config_error(&path, "is required"))?;
    let value = interpolate_env(&value, &path, env)?;
    if value.trim().is_empty() {
        return invalid_config(path, "is required");
    }

    Ok(value)
}

pub(super) fn resolve_metadata_path(
    value: Option<String>,
    metadata_base_dir: &Path,
    env: &mut impl FnMut(&str) -> Option<String>,
) -> ServerResult<PathBuf> {
    let path = match value {
        Some(value) => {
            if value.trim().is_empty() {
                return invalid_config("server.metadata_path", "must not be empty");
            }
            let value = interpolate_env(&value, "server.metadata_path", env)?;
            if value.trim().is_empty() {
                return invalid_config(
                    "server.metadata_path",
                    "must not resolve to an empty value",
                );
            }
            if value.trim() != value {
                return invalid_config(
                    "server.metadata_path",
                    "must not include leading or trailing whitespace",
                );
            }
            if has_trailing_path_separator(&value) {
                return invalid_config(
                    "server.metadata_path",
                    "must include a metadata database file name",
                );
            }
            PathBuf::from(value)
        }
        None => PathBuf::from(DEFAULT_METADATA_DIR).join(DEFAULT_METADATA_DB_FILE),
    };

    if path.as_os_str().is_empty() {
        return invalid_config("server.metadata_path", "must not be empty");
    }

    if path.is_absolute() || metadata_base_dir.as_os_str().is_empty() {
        Ok(path)
    } else {
        Ok(metadata_base_dir.join(path))
    }
}

/// Resolves one environment-backed configuration directory relative to its file.
pub(crate) fn resolve_config_directory(
    value: Option<String>,
    path: impl Into<String>,
    env: &mut impl FnMut(&str) -> Option<String>,
    config_base_dir: &Path,
) -> ServerResult<PathBuf> {
    let path = path.into();
    let value = resolve_required(value, &path, env)?;
    validate_no_outer_whitespace(&value, &path)?;
    let directory = expand_current_user_tilde(&value, &path, env)?;
    if directory.as_os_str().is_empty() {
        return invalid_config(path, "must not be empty");
    }

    if directory.is_absolute() || config_base_dir.as_os_str().is_empty() {
        Ok(directory)
    } else {
        Ok(config_base_dir.join(directory))
    }
}

fn expand_current_user_tilde(
    value: &str,
    path: &str,
    env: &mut impl FnMut(&str) -> Option<String>,
) -> ServerResult<PathBuf> {
    // Paths entered interactively do not pass through a shell, so expand only
    // the conventional current-user component and leave `~other` untouched.
    let directory = PathBuf::from(value);
    let Ok(suffix) = directory.strip_prefix(Path::new("~")) else {
        return Ok(directory);
    };

    #[cfg(windows)]
    const HOME_ENVIRONMENT_VARIABLE: &str = "USERPROFILE";
    #[cfg(not(windows))]
    const HOME_ENVIRONMENT_VARIABLE: &str = "HOME";

    let home = env(HOME_ENVIRONMENT_VARIABLE)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            invalid_config_error(
                path,
                format!("uses ~ but {HOME_ENVIRONMENT_VARIABLE} is unset or empty"),
            )
        })?;
    Ok(PathBuf::from(home).join(suffix))
}

fn has_trailing_path_separator(value: &str) -> bool {
    value.ends_with('/') || value.ends_with('\\')
}

/// Resolves an optional environment-backed configuration string.
pub(crate) fn resolve_optional(
    value: Option<String>,
    path: impl Into<String>,
    env: &mut impl FnMut(&str) -> Option<String>,
) -> ServerResult<Option<String>> {
    let path = path.into();
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    let value = interpolate_env(&value, &path, env)?;
    if value.trim().is_empty() {
        return Ok(None);
    }

    Ok(Some(value))
}

fn interpolate_env(
    value: &str,
    path: &str,
    env: &mut impl FnMut(&str) -> Option<String>,
) -> ServerResult<String> {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            return invalid_config(path, "contains an unterminated environment reference");
        };
        let name = &after_start[..end];
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return invalid_config(
                path,
                format!("contains invalid environment variable reference {name:?}"),
            );
        }
        let resolved = env(name).ok_or_else(|| {
            invalid_config_error(
                path,
                format!("references unset environment variable {name}"),
            )
        })?;
        output.push_str(&resolved);
        rest = &after_start[end + 1..];
    }

    output.push_str(rest);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_directory_expands_current_user_tilde_before_relative_resolution() {
        #[cfg(not(windows))]
        let (home_variable, home, input, expected) = (
            "HOME",
            "/Users/alice",
            "~/.config/lfscloud/gcloud-drive-alice",
            PathBuf::from("/Users/alice/.config/lfscloud/gcloud-drive-alice"),
        );
        #[cfg(windows)]
        let (home_variable, home, input, expected) = (
            "USERPROFILE",
            r"C:\Users\alice",
            r"~\.config\lfscloud\gcloud-drive-alice",
            PathBuf::from(r"C:\Users\alice\.config\lfscloud\gcloud-drive-alice"),
        );
        let mut env = |name: &str| (name == home_variable).then(|| home.to_owned());

        let resolved = resolve_config_directory(
            Some(input.to_owned()),
            "storage_providers.drive.credentials.config_dir",
            &mut env,
            Path::new("/config"),
        )
        .expect("current-user tilde should resolve");

        assert_eq!(resolved, expected);

        let mut no_env = |_name: &str| None;
        let named_user = resolve_config_directory(
            Some("~other/gcloud-drive".to_owned()),
            "storage_providers.drive.credentials.config_dir",
            &mut no_env,
            Path::new("/config"),
        )
        .expect("named-user tilde should remain a relative path");
        assert_eq!(named_user, PathBuf::from("/config/~other/gcloud-drive"));
    }
}
