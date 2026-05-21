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
