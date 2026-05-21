//! Web search via DuckDuckGo HTML
//! and plain HTTP fetch with simple readable-text extraction.

use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub async fn web_search(query: &str, limit: usize) -> Result<Vec<WebSearchResult>> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (itsy)")
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let url = format!("https://duckduckgo.com/html/?q={}", urlencoding(query));
    let html = client.get(&url).send().await?.text().await?;
    let re = regex::Regex::new(r#"<a[^>]+class="result__a"[^>]+href="([^"]+)"[^>]*>(.*?)</a>"#).ok();
    let snip = regex::Regex::new(r#"<a[^>]+class="result__snippet"[^>]*>(.*?)</a>"#).ok();
    let mut results = Vec::new();
    let titles: Vec<(String, String)> = re
        .as_ref()
        .map(|r| r.captures_iter(&html).map(|c| (c[1].to_string(), strip_tags(&c[2]))).collect())
        .unwrap_or_default();
    let snippets: Vec<String> = snip
        .as_ref()
        .map(|r| r.captures_iter(&html).map(|c| strip_tags(&c[1])).collect())
        .unwrap_or_default();
    for (i, (url, title)) in titles.into_iter().enumerate().take(limit) {
        results.push(WebSearchResult {
            title,
            url,
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        });
    }
    Ok(results)
}

pub async fn web_fetch(url: &str, timeout_secs: u64) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (itsy)")
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()?;
    let html = client.get(url).send().await?.text().await?;
    Ok(extract_readable(&html))
}

fn extract_readable(html: &str) -> String {
    let no_scripts = regex::Regex::new(r"(?is)<script[^>]*>.*?</script>")
        .map(|r| r.replace_all(html, "").into_owned())
        .unwrap_or_else(|_| html.to_string());
    let no_styles = regex::Regex::new(r"(?is)<style[^>]*>.*?</style>")
        .map(|r| r.replace_all(&no_scripts, "").into_owned())
        .unwrap_or(no_scripts);
    let text = strip_tags(&no_styles);
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_tags(s: &str) -> String {
    regex::Regex::new(r"<[^>]+>")
        .map(|r| r.replace_all(s, "").into_owned())
        .unwrap_or_else(|_| s.to_string())
}

fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
