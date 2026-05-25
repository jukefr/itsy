//! Endpoint allowlist with
//! origin (scheme+host+port) comparison and metadata-service refusal.

use std::env;

use url::Url;

fn is_loopback(host: &str) -> bool {
    let h = host.to_lowercase();
    h == "localhost" || h == "::1" || h == "::" || h.starts_with("127.")
}

fn is_rfc1918(host: &str) -> bool {
    let parts: Vec<u32> = host.split('.').filter_map(|s| s.parse().ok()).collect();
    if parts.len() != 4 {
        return false;
    }
    let (a, b) = (parts[0], parts[1]);
    a == 10
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
}

fn is_always_blocked(host: &str) -> bool {
    let h = host.to_lowercase();
    if matches!(h.as_str(), "169.254.169.254" | "fd00:ec2::254" | "[fd00:ec2::254]" | "metadata.google.internal" | "metadata") {
        return true;
    }
    let parts: Vec<u32> = h.split('.').filter_map(|s| s.parse().ok()).collect();
    if parts.len() == 4 {
        let (a, b) = (parts[0], parts[1]);
        if a == 169 && b == 254 {
            return true;
        }
        if a == 100 && (64..=127).contains(&b) {
            return true;
        }
        if a == 0 {
            return true;
        }
    }
    false
}

fn origin_of(url: &Url) -> String {
    match (url.scheme(), url.host_str(), url.port()) {
        (scheme, Some(host), Some(port)) => format!("{scheme}://{host}:{port}"),
        (scheme, Some(host), None) => format!("{scheme}://{host}"),
        _ => url.as_str().to_string(),
    }
}

pub fn assert_endpoint_allowed(endpoint: &str) -> Result<(), String> {
    let url = Url::parse(endpoint).map_err(|_| format!("Invalid endpoint URL: {endpoint}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(format!("Endpoint must use http(s): {endpoint}"));
    }
    if let Some(host) = url.host_str() {
        if is_always_blocked(host) {
            return Err(format!("Endpoint {endpoint} targets a metadata/link-local address; refusing."));
        }
    }
    if env::var("LLM_ALLOW_PUBLIC_ENDPOINTS").ok().as_deref() == Some("1") {
        return Ok(());
    }
    let allow = env::var("LLM_ENDPOINT_ALLOWLIST").unwrap_or_default();
    let want_origin = origin_of(&url);
    for raw in allow.split(',') {
        let entry = raw.trim();
        if entry.is_empty() {
            continue;
        }
        if let Ok(allow_url) = Url::parse(entry) {
            if origin_of(&allow_url) == want_origin {
                return Ok(());
            }
        }
    }
    let host = url.host_str().unwrap_or("");
    if is_loopback(host) || is_rfc1918(host) {
        return Ok(());
    }
    Err(format!(
        "Endpoint {endpoint} is not in LLM_ENDPOINT_ALLOWLIST and is not a private host. \
         Set LLM_ENDPOINT_ALLOWLIST=<comma-separated origins> or LLM_ALLOW_PUBLIC_ENDPOINTS=1 to permit."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cloud metadata addresses MUST always be blocked, regardless of any
    /// allowlist or public-endpoints env var. This is the SSRF-blocker
    /// contract; anything that lets these through is a critical security bug.
    #[test]
    fn aws_metadata_ip_is_always_blocked() {
        assert!(is_always_blocked("169.254.169.254"));
        // And confirmed via the full assert_endpoint_allowed path.
        let r = assert_endpoint_allowed("http://169.254.169.254/latest/meta-data");
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("metadata"));
    }

    /// GCP metadata.google.internal is blocked.
    #[test]
    fn gcp_metadata_host_is_always_blocked() {
        assert!(is_always_blocked("metadata.google.internal"));
        assert!(is_always_blocked("metadata"));
        let r = assert_endpoint_allowed("http://metadata.google.internal/");
        assert!(r.is_err());
    }

    /// All 169.254.0.0/16 link-local addresses are blocked, not just the
    /// AWS metadata IP. Anti-regression: a future tweak shouldn't narrow
    /// this to only 169.254.169.254.
    #[test]
    fn link_local_range_is_blocked() {
        assert!(is_always_blocked("169.254.1.2"));
        assert!(is_always_blocked("169.254.0.0"));
        assert!(is_always_blocked("169.254.255.255"));
    }

    /// CGNAT (100.64.0.0/10) blocked — used by some cloud-hosted metadata.
    #[test]
    fn cgnat_range_is_blocked() {
        assert!(is_always_blocked("100.64.0.1"));
        assert!(is_always_blocked("100.127.255.255"));
        assert!(!is_always_blocked("100.63.255.255"),
            "100.63.x is outside CGNAT range");
        assert!(!is_always_blocked("100.128.0.1"),
            "100.128.x is outside CGNAT range");
    }

    /// 0.0.0.0/8 is blocked (unspecified addresses).
    #[test]
    fn zero_block_is_blocked() {
        assert!(is_always_blocked("0.0.0.0"));
        assert!(is_always_blocked("0.1.2.3"));
    }

    /// Loopback variants identified correctly.
    #[test]
    fn loopback_recognition() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("127.1.1.1"));
        assert!(is_loopback("localhost"));
        assert!(is_loopback("::1"));
        assert!(!is_loopback("128.0.0.1"));
    }

    /// RFC1918 ranges recognised: 10/8, 172.16-31/12, 192.168/16.
    #[test]
    fn rfc1918_recognition() {
        assert!(is_rfc1918("10.0.0.1"));
        assert!(is_rfc1918("10.255.255.255"));
        assert!(is_rfc1918("172.16.0.1"));
        assert!(is_rfc1918("172.31.255.255"));
        assert!(is_rfc1918("192.168.1.1"));

        assert!(!is_rfc1918("172.15.0.1"), "172.15 is OUTSIDE rfc1918");
        assert!(!is_rfc1918("172.32.0.1"), "172.32 is OUTSIDE rfc1918");
        assert!(!is_rfc1918("11.0.0.1"));
        assert!(!is_rfc1918("8.8.8.8"));
    }

    /// Loopback endpoints are allowed without an allowlist entry — local
    /// development is the default permitted path.
    #[test]
    fn loopback_endpoint_allowed_by_default() {
        // Skip if env contaminates the result.
        if env::var("LLM_ALLOW_PUBLIC_ENDPOINTS").ok().as_deref() == Some("1") { return; }
        let r = assert_endpoint_allowed("http://127.0.0.1:8080/v1");
        assert!(r.is_ok(), "loopback must be allowed by default; got {:?}", r);
    }

    /// Invalid URLs are rejected.
    #[test]
    fn invalid_url_rejected() {
        assert!(assert_endpoint_allowed("not a url").is_err());
        assert!(assert_endpoint_allowed("").is_err());
    }

    /// Non-http schemes rejected.
    #[test]
    fn non_http_scheme_rejected() {
        let r = assert_endpoint_allowed("file:///etc/passwd");
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("http(s)"));
    }

    /// `origin_of` produces stable strings used for allowlist matching.
    #[test]
    fn origin_omits_default_port() {
        let u = Url::parse("https://api.example.com/v1").unwrap();
        // Default https port (443) is None → no `:443` in origin string.
        assert_eq!(origin_of(&u), "https://api.example.com");
        let u2 = Url::parse("https://api.example.com:8443/v1").unwrap();
        assert_eq!(origin_of(&u2), "https://api.example.com:8443");
    }
}
