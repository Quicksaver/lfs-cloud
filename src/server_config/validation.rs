//! Shared syntactic and HTTP validation for server configuration values.

use crate::{
    ServerError, ServerResult,
    http_transport::{HttpUrlPolicy, HttpUrlValidationError, validate_http_url as validate_url},
};

pub(super) fn validate_key(key: &str, path: impl Into<String>) -> ServerResult<()> {
    let path = path.into();
    if key.trim().is_empty() {
        return invalid_config(path, "must not be empty");
    }
    if key != key.trim() {
        return invalid_config(path, "must not have leading or trailing whitespace");
    }
    if !key
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return invalid_config(
            path,
            "must contain only ASCII letters, digits, '_' or '-' and start with a letter or digit",
        );
    }

    Ok(())
}

pub(super) fn validate_route_host(host: &str, path: impl Into<String>) -> ServerResult<()> {
    let path = path.into();
    validate_no_outer_whitespace(host, &path)?;
    if host.split('.').any(|label| {
        label.is_empty()
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return invalid_config(
            path,
            "must be a route-safe host made of ASCII domain labels",
        );
    }

    Ok(())
}

pub(super) fn validate_route_component(
    component: &str,
    path: impl Into<String>,
) -> ServerResult<()> {
    let path = path.into();
    validate_no_outer_whitespace(component, &path)?;
    if matches!(component, "." | "..")
        || component.contains("..")
        || !component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return invalid_config(
            path,
            "must be a route-safe repository component without separators, percent escapes, or traversal segments",
        );
    }

    Ok(())
}

pub(super) fn validate_config_http_url(
    url: &str,
    path: impl Into<String>,
    allow_insecure_http: bool,
) -> ServerResult<()> {
    let path = path.into();
    validate_url(url, HttpUrlPolicy::route_base(allow_insecure_http))
        .map(|_| ())
        .map_err(|error| {
            let hint = match error {
                HttpUrlValidationError::InsecureTransport => {
                    "; set server.allow_insecure_http to true only for a trusted development network"
                }
                _ => "",
            };
            invalid_config_error(path, format!("{error}{hint}"))
        })
}

pub(super) fn validate_no_outer_whitespace(value: &str, path: &str) -> ServerResult<()> {
    if value != value.trim() {
        return invalid_config(path, "must not have leading or trailing whitespace");
    }

    Ok(())
}

pub(super) fn invalid_config<T>(
    path: impl Into<String>,
    message: impl Into<String>,
) -> ServerResult<T> {
    Err(invalid_config_error(path, message))
}

pub(super) fn invalid_config_error(
    path: impl Into<String>,
    message: impl Into<String>,
) -> ServerError {
    ServerError::InvalidConfiguration {
        message: format!("{} {}", path.into(), message.into()),
    }
}
