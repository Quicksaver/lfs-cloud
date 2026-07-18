//! Shared transport policy for URLs that carry authentication or object data.
//!
//! HTTPS is required by default. Plain HTTP is accepted only for literal
//! loopback IP addresses, avoiding DNS-based names whose resolution can change.

use url::{Host, Url};

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

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{has_exact_loopback_host, uses_protected_http_transport};

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
