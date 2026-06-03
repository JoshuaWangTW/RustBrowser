//! Lightweight HTTP fetching — no browser engine, no JS execution.
//!
//! Pulls raw bytes over HTTP(S) with automatic gzip/brotli/deflate
//! decompression and charset-aware decoding, then hands the HTML off to
//! the extraction stage. This is the cheap path that avoids spinning up a
//! full rendering engine for the common case.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::CONTENT_TYPE;

/// Default User-Agent. A real browser-ish UA reduces the chance of being
/// served a degraded/blocked page, while still being honest about origin.
const DEFAULT_UA: &str =
    "Mozilla/5.0 (compatible; RustBrowser/0.1; +https://github.com/rustbrowser)";

/// Tunables for a fetch.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub user_agent: String,
    pub timeout: Duration,
    /// Hard cap on the response body we will decode, to bound memory/tokens.
    pub max_bytes: usize,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            user_agent: DEFAULT_UA.to_string(),
            timeout: Duration::from_secs(20),
            max_bytes: 8 * 1024 * 1024, // 8 MiB is plenty for a document page
        }
    }
}

/// Outcome of a successful fetch.
#[derive(Debug, Clone)]
pub struct FetchResult {
    /// URL after following redirects — the document's real identity.
    pub final_url: String,
    pub status: u16,
    pub content_type: Option<String>,
    /// Decoded response body (HTML in the common case).
    pub html: String,
    /// Number of bytes received before decoding (for stats).
    pub raw_bytes: usize,
}

/// Fetch a URL and return its decoded body.
pub async fn fetch(url: &str, opts: &FetchOptions) -> Result<FetchResult> {
    let client = reqwest::Client::builder()
        .user_agent(&opts.user_agent)
        .timeout(opts.timeout)
        .gzip(true)
        .brotli(true)
        .deflate(true)
        // Follow a sane number of redirects; most pages need 0-2.
        .redirect(reqwest::redirect::Policy::limited(8))
        .build()
        .context("building HTTP client")?;

    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;

    let status = resp.status();
    let final_url = resp.url().to_string();
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // charset-aware decode (honours Content-Type charset, falls back to UTF-8).
    let bytes = resp.bytes().await.context("reading response body")?;
    let raw_bytes = bytes.len();
    let slice = if raw_bytes > opts.max_bytes {
        &bytes[..opts.max_bytes]
    } else {
        &bytes[..]
    };
    let html = String::from_utf8_lossy(slice).into_owned();

    Ok(FetchResult {
        final_url,
        status: status.as_u16(),
        content_type,
        html,
        raw_bytes,
    })
}
