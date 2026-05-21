//! Web search + readable-content fetch.
//!
//! Improvements over the original minimal stub:
//!   * Better readability extraction — strips nav/header/footer/script/style,
//!     prefers `<article>` / `<main>` / `[role=main]` / `#content`, falls back
//!     to body scoring (paragraph density wins).
//!   * Extracts `<title>` and `<meta name="description">` to bookend the
//!     readable text.
//!   * Multi-engine search with fallback: DuckDuckGo HTML → Brave Search (no
//!     key) → SearXNG (if `ITSY_SEARX_URL` is set).
//!   * URL canonicalization (strips trackers like `utm_*`, lowercase host,
//!     drops default ports, sorts remaining query params).
//!   * Optional `robots.txt` respect via `ITSY_WEB_RESPECT_ROBOTS=1` and a
//!     conservative SSRF guard (no loopback / RFC1918 / link-local /
//!     metadata host unless `ITSY_ALLOW_PUBLIC_ENDPOINTS=1`).

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use url::Url;

#[derive(Debug, Clone, Serialize)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WebPage {
    pub url: String,
    pub title: String,
    pub description: String,
    pub text: String,
}

const USER_AGENT: &str = "Mozilla/5.0 (compatible; itsy/0.9)";

// ── SSRF + robots ────────────────────────────────────────────────────────────

fn assert_url_safe(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).map_err(|e| anyhow!("invalid URL: {e}"))?;
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(anyhow!("refused: only http/https are allowed, got `{scheme}`"));
    }
    let host = url.host_str().unwrap_or("").to_lowercase();
    // Cloud metadata + link-local: always refuse.
    if host == "169.254.169.254" || host == "metadata.google.internal" {
        return Err(anyhow!("refused: cloud-metadata host"));
    }
    if std::env::var("ITSY_ALLOW_PUBLIC_ENDPOINTS").ok().as_deref() == Some("1") {
        return Ok(url);
    }
    if host == "localhost" || host == "::1" || host.starts_with("127.") {
        return Err(anyhow!("refused: loopback URL"));
    }
    static RFC1918: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"^(10\.|192\.168\.|172\.(1[6-9]|2[0-9]|3[01])\.)").unwrap()
    });
    if RFC1918.is_match(&host) {
        return Err(anyhow!("refused: RFC1918 URL"));
    }
    Ok(url)
}

static ROBOTS_CACHE: Lazy<Mutex<std::collections::HashMap<String, String>>> =
    Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

async fn fetch_robots(client: &reqwest::Client, url: &Url) -> String {
    let key = format!("{}://{}", url.scheme(), url.authority());
    if let Some(v) = ROBOTS_CACHE.lock().unwrap().get(&key).cloned() {
        return v;
    }
    let robots_url = format!("{key}/robots.txt");
    let body = match client.get(&robots_url).send().await {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => String::new(),
    };
    ROBOTS_CACHE.lock().unwrap().insert(key, body.clone());
    body
}

fn robots_allows(robots: &str, path: &str) -> bool {
    // Minimal parser: only honors the `User-agent: *` block. Returns true
    // unless the path is `Disallow:`-ed.
    if robots.is_empty() {
        return true;
    }
    let mut in_block = false;
    for raw in robots.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (k, v) = match line.split_once(':') {
            Some((k, v)) => (k.trim().to_lowercase(), v.trim()),
            None => continue,
        };
        match k.as_str() {
            "user-agent" => in_block = v == "*",
            "disallow" if in_block => {
                if !v.is_empty() && path.starts_with(v) {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

// ── URL canonicalization ─────────────────────────────────────────────────────

const TRACKER_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "gclid",
    "fbclid",
    "mc_cid",
    "mc_eid",
];

pub fn canonicalize_url(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else { return raw.to_string() };
    if let Some(host) = url.host_str().map(|h| h.to_lowercase()) {
        let _ = url.set_host(Some(&host));
    }
    // Drop default ports.
    if let Some(port) = url.port() {
        let default = match url.scheme() {
            "http" => Some(80),
            "https" => Some(443),
            _ => None,
        };
        if Some(port) == default {
            let _ = url.set_port(None);
        }
    }
    // Strip tracker params, then sort the rest for stable identity.
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .filter(|(k, _)| !TRACKER_PARAMS.contains(&k.as_str()))
        .collect();
    let mut sorted = kept;
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    if sorted.is_empty() {
        url.set_query(None);
    } else {
        let mut q = url.query_pairs_mut();
        q.clear();
        for (k, v) in &sorted {
            q.append_pair(k, v);
        }
        drop(q);
    }
    // Drop fragment — never meaningful for content identity.
    url.set_fragment(None);
    url.to_string()
}

// ── Web search ───────────────────────────────────────────────────────────────

pub async fn web_search(query: &str, limit: usize) -> Result<Vec<WebSearchResult>> {
    let client = http_client()?;
    // Try DDG first.
    if let Ok(mut r) = search_ddg(&client, query, limit).await {
        if !r.is_empty() {
            dedupe_by_url(&mut r);
            return Ok(r);
        }
    }
    // SearXNG fallback (operator-configured).
    if let Ok(url) = std::env::var("ITSY_SEARX_URL") {
        if let Ok(mut r) = search_searxng(&client, &url, query, limit).await {
            if !r.is_empty() {
                dedupe_by_url(&mut r);
                return Ok(r);
            }
        }
    }
    // Brave HTML fallback.
    if let Ok(mut r) = search_brave(&client, query, limit).await {
        dedupe_by_url(&mut r);
        return Ok(r);
    }
    Ok(Vec::new())
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()?)
}

async fn search_ddg(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
) -> Result<Vec<WebSearchResult>> {
    let url = format!("https://html.duckduckgo.com/html/?q={}", url_encode(query));
    let html = client.get(&url).send().await?.text().await?;
    let title_re = Regex::new(
        r#"(?is)<a[^>]+class="result__a"[^>]+href="([^"]+)"[^>]*>(.*?)</a>"#,
    )?;
    let snip_re = Regex::new(r#"(?is)<a[^>]+class="result__snippet"[^>]*>(.*?)</a>"#)?;
    let titles: Vec<(String, String)> = title_re
        .captures_iter(&html)
        .map(|c| (decode_ddg_url(&c[1]), strip_tags(&c[2])))
        .collect();
    let snippets: Vec<String> = snip_re.captures_iter(&html).map(|c| strip_tags(&c[1])).collect();
    let mut results = Vec::new();
    for (i, (link, title)) in titles.into_iter().enumerate().take(limit) {
        results.push(WebSearchResult {
            title: html_unescape(&title),
            url: canonicalize_url(&link),
            snippet: html_unescape(&snippets.get(i).cloned().unwrap_or_default()),
        });
    }
    Ok(results)
}

async fn search_brave(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
) -> Result<Vec<WebSearchResult>> {
    let url = format!("https://search.brave.com/search?q={}&source=web", url_encode(query));
    let html = client.get(&url).send().await?.text().await?;
    // Brave's HTML structure shifts; match a common result anchor + snippet pair.
    let re = Regex::new(
        r#"(?is)<a[^>]+class="[^"]*result-header[^"]*"[^>]+href="([^"]+)"[^>]*>.*?<span[^>]*>(.*?)</span>.*?<p[^>]*class="[^"]*snippet[^"]*"[^>]*>(.*?)</p>"#,
    )?;
    let mut out = Vec::new();
    for c in re.captures_iter(&html).take(limit) {
        out.push(WebSearchResult {
            url: canonicalize_url(&c[1]),
            title: html_unescape(&strip_tags(&c[2])),
            snippet: html_unescape(&strip_tags(&c[3])),
        });
    }
    Ok(out)
}

async fn search_searxng(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<WebSearchResult>> {
    let url = format!(
        "{}/search?q={}&format=json",
        base.trim_end_matches('/'),
        url_encode(query)
    );
    let body: serde_json::Value = client.get(&url).send().await?.json().await?;
    let mut out = Vec::new();
    if let Some(arr) = body.get("results").and_then(|v| v.as_array()) {
        for item in arr.iter().take(limit) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let snippet =
                item.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
            out.push(WebSearchResult { title, url: canonicalize_url(&url), snippet });
        }
    }
    Ok(out)
}

fn dedupe_by_url(results: &mut Vec<WebSearchResult>) {
    let mut seen: HashSet<String> = HashSet::new();
    results.retain(|r| seen.insert(r.url.clone()));
}

/// DDG html search wraps outbound links in `/l/?uddg=<encoded>`.
fn decode_ddg_url(raw: &str) -> String {
    if let Some(idx) = raw.find("uddg=") {
        let tail = &raw[idx + 5..];
        let enc = tail.split('&').next().unwrap_or(tail);
        if let Some(dec) = percent_decode(enc) {
            return dec;
        }
    }
    raw.to_string()
}

// ── Web fetch + readability ──────────────────────────────────────────────────

pub async fn web_fetch(url: &str, timeout_secs: u64) -> Result<String> {
    let page = web_fetch_page(url, timeout_secs).await?;
    let mut out = String::new();
    if !page.title.is_empty() {
        out.push_str(&page.title);
        out.push_str("\n\n");
    }
    if !page.description.is_empty() {
        out.push_str(&page.description);
        out.push_str("\n\n");
    }
    out.push_str(&page.text);
    Ok(out)
}

pub async fn web_fetch_page(url: &str, timeout_secs: u64) -> Result<WebPage> {
    let safe = assert_url_safe(url)?;
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()?;

    if std::env::var("ITSY_WEB_RESPECT_ROBOTS").ok().as_deref() == Some("1") {
        let robots = fetch_robots(&client, &safe).await;
        if !robots_allows(&robots, safe.path()) {
            return Err(anyhow!("refused: robots.txt disallows `{}`", safe.path()));
        }
    }

    let resp = client.get(safe.as_str()).send().await?;
    let final_url = resp.url().to_string();
    let html = resp.text().await?;

    let title = extract_title(&html);
    let description = extract_meta_description(&html);
    let text = extract_readable(&html);
    Ok(WebPage { url: canonicalize_url(&final_url), title, description, text })
}

fn extract_title(html: &str) -> String {
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap());
    RE.captures(html)
        .map(|c| html_unescape(&strip_tags(&c[1])).trim().to_string())
        .unwrap_or_default()
}

fn extract_meta_description(html: &str) -> String {
    static RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r#"(?is)<meta[^>]+name=["']description["'][^>]+content=["']([^"']*)["']"#,
        )
        .unwrap()
    });
    static RE_OG: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r#"(?is)<meta[^>]+property=["']og:description["'][^>]+content=["']([^"']*)["']"#,
        )
        .unwrap()
    });
    if let Some(c) = RE.captures(html) {
        return html_unescape(&c[1]).trim().to_string();
    }
    if let Some(c) = RE_OG.captures(html) {
        return html_unescape(&c[1]).trim().to_string();
    }
    String::new()
}

fn extract_readable(html: &str) -> String {
    let cleaned = strip_noise(html);
    // Prefer high-signal containers in priority order.
    let candidates: [(&str, fn(&str) -> Option<String>); 5] = [
        ("article", |h| extract_tag(h, "article")),
        ("main", |h| extract_tag(h, "main")),
        ("role-main", extract_role_main),
        ("content-class", |h| extract_by_attr(h, "class", "content")),
        ("content-id", |h| extract_by_attr(h, "id", "content")),
    ];
    let mut best_text: Option<String> = None;
    let mut best_score = 0usize;
    for (_, f) in candidates {
        if let Some(block) = f(&cleaned) {
            let plain = html_to_text(&block);
            let score = score_text(&plain);
            if score > best_score {
                best_score = score;
                best_text = Some(plain);
            }
        }
    }
    if let Some(t) = best_text {
        if best_score >= 30 {
            return t;
        }
    }
    // Fallback: pick the highest-scoring <div>/<section>; else strip the body.
    if let Some(body) = extract_tag(&cleaned, "body") {
        let block_re = Regex::new(r"(?is)<(div|section)[^>]*>(.*?)</\1>").ok();
        let mut best = (0usize, String::new());
        if let Some(re) = block_re {
            for c in re.captures_iter(&body) {
                let block = &c[2];
                let plain = html_to_text(block);
                let s = score_text(&plain);
                if s > best.0 {
                    best = (s, plain);
                }
            }
        }
        if best.0 > 0 {
            return best.1;
        }
        return html_to_text(&body);
    }
    html_to_text(&cleaned)
}

fn strip_noise(html: &str) -> String {
    static SCRIPT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<script[^>]*>.*?</script>").unwrap());
    static STYLE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<style[^>]*>.*?</style>").unwrap());
    static NAV: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<nav[^>]*>.*?</nav>").unwrap());
    static HEADER: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<header[^>]*>.*?</header>").unwrap());
    static FOOTER: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<footer[^>]*>.*?</footer>").unwrap());
    static ASIDE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<aside[^>]*>.*?</aside>").unwrap());
    static NOSCRIPT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<noscript[^>]*>.*?</noscript>").unwrap());
    static COMMENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<!--.*?-->").unwrap());
    let mut s = html.to_string();
    for re in [&*SCRIPT, &*STYLE, &*NAV, &*HEADER, &*FOOTER, &*ASIDE, &*NOSCRIPT, &*COMMENT] {
        s = re.replace_all(&s, "").into_owned();
    }
    s
}

fn extract_tag(html: &str, tag: &str) -> Option<String> {
    let re = Regex::new(&format!(r"(?is)<{tag}[^>]*>(.*?)</{tag}>")).ok()?;
    re.captures(html).map(|c| c[1].to_string())
}

fn extract_role_main(html: &str) -> Option<String> {
    let re = Regex::new(r#"(?is)<[a-z0-9]+[^>]+role=["']main["'][^>]*>(.*?)</[a-z0-9]+>"#).ok()?;
    re.captures(html).map(|c| c[1].to_string())
}

fn extract_by_attr(html: &str, attr: &str, value: &str) -> Option<String> {
    let pat = format!(
        r#"(?is)<([a-z0-9]+)[^>]+{attr}=["'][^"']*\b{value}\b[^"']*["'][^>]*>(.*?)</\1>"#
    );
    let re = Regex::new(&pat).ok()?;
    re.captures(html).map(|c| c[2].to_string())
}

fn score_text(s: &str) -> usize {
    // Simple proxy: word count, with bonus for paragraph-like density.
    let words = s.split_whitespace().count();
    let newlines = s.matches('\n').count();
    words + newlines * 2
}

fn html_to_text(html: &str) -> String {
    let s = strip_tags(html);
    let s = html_unescape(&s);
    let mut out = String::with_capacity(s.len());
    let mut last_blank = false;
    for line in s.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !last_blank {
                out.push('\n');
                last_blank = true;
            }
        } else {
            out.push_str(trimmed);
            out.push('\n');
            last_blank = false;
        }
    }
    out.trim().to_string()
}

fn strip_tags(s: &str) -> String {
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<[^>]+>").unwrap());
    // Insert a newline for paragraph-like breaks before stripping.
    let s = s
        .replace("</p>", "\n")
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("</li>", "\n")
        .replace("</h1>", "\n")
        .replace("</h2>", "\n")
        .replace("</h3>", "\n");
    RE.replace_all(&s, "").into_owned()
}

fn html_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(semi) = s[i..].find(';') {
                let entity = &s[i..i + semi + 1];
                let replacement = match entity {
                    "&amp;" => Some("&"),
                    "&lt;" => Some("<"),
                    "&gt;" => Some(">"),
                    "&quot;" => Some("\""),
                    "&#39;" | "&apos;" => Some("'"),
                    "&nbsp;" => Some(" "),
                    _ => None,
                };
                if let Some(r) = replacement {
                    out.push_str(r);
                    i += entity.len();
                    continue;
                }
                if let Some(stripped) = entity.strip_prefix("&#") {
                    let stripped = stripped.trim_end_matches(';');
                    let parsed = if let Some(hex) = stripped.strip_prefix('x').or_else(|| stripped.strip_prefix('X')) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        stripped.parse::<u32>().ok()
                    };
                    if let Some(cp) = parsed.and_then(char::from_u32) {
                        out.push(cp);
                        i += entity.len();
                        continue;
                    }
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn percent_decode(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push(((hi << 4) | lo) as u8);
            i += 3;
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_strips_trackers_and_sorts() {
        let c = canonicalize_url("https://Example.COM:443/foo?b=2&utm_source=x&a=1#frag");
        assert_eq!(c, "https://example.com/foo?a=1&b=2");
    }

    #[test]
    fn ssrf_blocks_loopback() {
        assert!(assert_url_safe("http://127.0.0.1/").is_err());
        assert!(assert_url_safe("http://169.254.169.254/").is_err());
    }

    #[test]
    fn extracts_title_and_description() {
        let html = r#"<html><head><title>Hi &amp; Bye</title><meta name="description" content="Sum"></head><body><article><p>Body text here.</p></article></body></html>"#;
        assert_eq!(extract_title(html), "Hi & Bye");
        assert_eq!(extract_meta_description(html), "Sum");
        let t = extract_readable(html);
        assert!(t.contains("Body text here"));
    }

    #[test]
    fn robots_disallow_path() {
        let txt = "User-agent: *\nDisallow: /private";
        assert!(!robots_allows(txt, "/private/file"));
        assert!(robots_allows(txt, "/public"));
    }
}
