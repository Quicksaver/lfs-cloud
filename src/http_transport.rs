//! Shared transport policy for URLs that carry authentication or object data.
//!
//! HTTPS is required by default. Plain HTTP is accepted only for literal
//! loopback IP addresses, avoiding DNS-based names whose resolution can change.

use thiserror::Error;
use url::{Host, Url};

/// Reads at most `limit` response bytes and decodes them lossily.
///
/// HTTP error responses are diagnostic only. Bounding the shared reader keeps
/// an upstream from making a caller retain an unbounded error body. The
/// retained prefix is decoded lossily and may end in a replacement character
/// when the byte limit splits a UTF-8 code point.
pub(crate) async fn read_bounded_lossy_response_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<String, reqwest::Error> {
    let mut body = Vec::new();
    while body.len() < limit {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        let remaining = limit - body.len();
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            break;
        }
        body.extend_from_slice(&chunk);
    }

    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Context-specific exceptions to the shared HTTP route-base policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HttpUrlPolicy {
    allow_insecure_http: bool,
}

impl HttpUrlPolicy {
    /// Builds the policy for a URL used as an HTTP route base.
    ///
    /// Route bases never allow credentials, queries, fragments, trailing
    /// slashes, dot segments, or characters that URL parsers normalize
    /// differently. Plaintext non-loopback HTTP is the only caller-selected
    /// exception and must be guarded by an explicit development opt-in.
    pub(crate) const fn route_base(allow_insecure_http: bool) -> Self {
        Self {
            allow_insecure_http,
        }
    }
}

/// Reason an HTTP route-base URL violates the shared safety policy.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum HttpUrlValidationError {
    /// The URL was empty.
    #[error("must not be blank")]
    Blank,
    /// The raw URL contained characters that URL parsers can discard or reinterpret.
    #[error("must not include whitespace, control characters, or backslashes")]
    UnsafeCharacters,
    /// The URL ended with a slash, making later segment appends ambiguous.
    #[error("must not end with a trailing slash")]
    TrailingSlash,
    /// The raw URL path contained a current- or parent-directory segment.
    #[error("path must not include dot segments")]
    DotSegments,
    /// The URL was not an absolute HTTP(S) URL with a host.
    #[error("must be a valid http or https URL")]
    Invalid,
    /// The URL embedded user credentials.
    #[error("must not include credentials")]
    Credentials,
    /// The URL included a query or fragment that is not part of a route base.
    #[error("must not include a query string or fragment")]
    QueryOrFragment,
    /// Plaintext HTTP targeted a non-loopback host without an explicit opt-in.
    #[error("must use HTTPS unless it targets an exact loopback IP")]
    InsecureTransport,
}

/// Parses and validates an HTTP(S) route base under the shared URL policy.
pub(crate) fn validate_http_url(
    value: &str,
    policy: HttpUrlPolicy,
) -> Result<Url, HttpUrlValidationError> {
    if value.is_empty() {
        return Err(HttpUrlValidationError::Blank);
    }
    if value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control() || character == '\\')
    {
        return Err(HttpUrlValidationError::UnsafeCharacters);
    }
    if value.ends_with('/') {
        return Err(HttpUrlValidationError::TrailingSlash);
    }
    if raw_path_has_dot_segments(value) {
        return Err(HttpUrlValidationError::DotSegments);
    }

    let parsed = Url::parse(value).map_err(|_| HttpUrlValidationError::Invalid)?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(HttpUrlValidationError::Invalid);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(HttpUrlValidationError::Credentials);
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(HttpUrlValidationError::QueryOrFragment);
    }
    if !policy.allow_insecure_http && !uses_protected_http_transport(&parsed) {
        return Err(HttpUrlValidationError::InsecureTransport);
    }

    Ok(parsed)
}

/// Returns whether `url` uses HTTPS or literal-IP loopback HTTP.
pub(crate) fn uses_protected_http_transport(url: &Url) -> bool {
    url.scheme() == "https" || (url.scheme() == "http" && has_exact_loopback_host(url))
}

/// Returns whether `url` uses an exact IPv4 or IPv6 loopback address.
pub(crate) fn has_exact_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(_)) | None => false,
    }
}

fn raw_path_has_dot_segments(value: &str) -> bool {
    let Some((_, after_scheme)) = value.split_once("://") else {
        return false;
    };
    let Some(path_start) = after_scheme.find('/') else {
        return false;
    };
    let path = after_scheme[path_start + 1..]
        .split(['?', '#'])
        .next()
        .unwrap_or_default();

    path.split('/').any(is_dot_segment)
}

fn is_dot_segment(segment: &str) -> bool {
    matches!(
        segment.to_ascii_lowercase().as_str(),
        "." | ".." | "%2e" | ".%2e" | "%2e." | "%2e%2e"
    )
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{
        HttpUrlPolicy, HttpUrlValidationError, has_exact_loopback_host,
        uses_protected_http_transport, validate_http_url,
    };

    const SAFE_ROUTE_BASE_URLS: &[&str] = &[
        "https://lfs.example.com",
        "https://lfs.example.com/custom/base",
        "http://127.0.0.1:8080",
        "http://127.9.8.7:8080",
        "http://[::1]:8080",
    ];

    const UNSAFE_ROUTE_BASE_URLS: &[(&str, HttpUrlValidationError)] = &[
        ("", HttpUrlValidationError::Blank),
        (
            " https://lfs.example.com",
            HttpUrlValidationError::UnsafeCharacters,
        ),
        (
            "https://lfs.example.com/base path",
            HttpUrlValidationError::UnsafeCharacters,
        ),
        (
            "https://lfs.example.com/base\npath",
            HttpUrlValidationError::UnsafeCharacters,
        ),
        (
            "https://lfs.example.com\\base",
            HttpUrlValidationError::UnsafeCharacters,
        ),
        (
            "https://lfs.example.com/",
            HttpUrlValidationError::TrailingSlash,
        ),
        (
            "https://lfs.example.com/../base",
            HttpUrlValidationError::DotSegments,
        ),
        (
            "https://lfs.example.com/./base",
            HttpUrlValidationError::DotSegments,
        ),
        (
            "https://lfs.example.com/%2e%2e/base",
            HttpUrlValidationError::DotSegments,
        ),
        ("ftp://lfs.example.com", HttpUrlValidationError::Invalid),
        (
            "https://user:secret@lfs.example.com",
            HttpUrlValidationError::Credentials,
        ),
        (
            "https://lfs.example.com?token=secret",
            HttpUrlValidationError::QueryOrFragment,
        ),
        (
            "https://lfs.example.com#fragment",
            HttpUrlValidationError::QueryOrFragment,
        ),
        (
            "http://lfs.example.com",
            HttpUrlValidationError::InsecureTransport,
        ),
    ];

    #[test]
    fn shared_route_base_policy_uses_one_safety_matrix() {
        let policy = HttpUrlPolicy::route_base(false);

        for accepted in SAFE_ROUTE_BASE_URLS {
            validate_http_url(accepted, policy).expect("safe route base should be accepted");
        }

        for (rejected, expected) in UNSAFE_ROUTE_BASE_URLS {
            assert_eq!(
                validate_http_url(rejected, policy),
                Err(*expected),
                "{rejected:?}"
            );
        }
    }

    #[test]
    fn route_base_policy_limits_exceptions_to_explicit_insecure_http() {
        let policy = HttpUrlPolicy::route_base(true);

        validate_http_url("http://192.168.1.25:8080/custom/base", policy)
            .expect("explicit opt-in should allow trusted LAN HTTP");
        assert_eq!(
            validate_http_url("http://192.168.1.25:8080/../base", policy),
            Err(HttpUrlValidationError::DotSegments)
        );
    }

    #[test]
    fn protected_transport_accepts_https_and_literal_loopback_http() {
        for accepted in [
            "https://lfs.example.com",
            "http://127.0.0.1:8080",
            "http://127.9.8.7:8080",
            "http://[::1]:8080",
        ] {
            let url = Url::parse(accepted).expect("fixture URL should parse");
            assert!(uses_protected_http_transport(&url), "{accepted}");
        }
    }

    #[test]
    fn protected_transport_rejects_named_and_non_loopback_http() {
        for rejected in [
            "http://localhost:8080",
            "http://192.168.1.25:8080",
            "http://10.0.0.5:8080",
            "http://example.com",
        ] {
            let url = Url::parse(rejected).expect("fixture URL should parse");
            assert!(!uses_protected_http_transport(&url), "{rejected}");
            assert!(!has_exact_loopback_host(&url), "{rejected}");
        }
    }
}
