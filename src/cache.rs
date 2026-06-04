//! Simple on-disk cache, keyed by a hash of the request identity.
//!
//! We cache raw HTTP fetches separately from rendered DOMs. A cached page can
//! still be re-extracted with a different selector or format later, while
//! JS-heavy pages avoid repeatedly launching Chrome for the same render inputs.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A cached fetch result plus the time it was stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFetch {
    /// Unix timestamp (seconds) when this entry was written.
    pub fetched_at: u64,
    pub final_url: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub html: String,
    pub raw_bytes: usize,
}

/// A cached headless render result plus the time it was stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedRender {
    /// Unix timestamp (seconds) when this entry was written.
    pub fetched_at: u64,
    pub final_url: String,
    pub html: String,
    pub raw_bytes: usize,
}

/// Where cache files live: `<system cache dir>/rustbrowser/<kind>/`.
fn cache_dir(kind: &str) -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("rustbrowser").join(kind))
}

/// Hex SHA-256 of the cache identity — stable and filesystem-safe.
fn key(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Look up a fresh cache entry for `url`. Returns `None` if it is missing,
/// unreadable, or older than `ttl_secs`.
pub fn get(url: &str, ttl_secs: u64) -> Option<CachedFetch> {
    let path = cache_dir("fetch")?.join(format!("{}.json", key(url)));
    let data = std::fs::read(&path).ok()?;
    let cached: CachedFetch = serde_json::from_slice(&data).ok()?;
    if now_secs().saturating_sub(cached.fetched_at) > ttl_secs {
        return None; // expired
    }
    Some(cached)
}

/// Store a fetch result for `url`. Best-effort — callers treat failure as a
/// cache miss rather than a hard error.
pub fn put(url: &str, entry: &CachedFetch) -> Result<()> {
    let dir = cache_dir("fetch").context("no system cache directory available")?;
    std::fs::create_dir_all(&dir).context("creating cache directory")?;
    let path = dir.join(format!("{}.json", key(url)));
    let data = serde_json::to_vec(entry).context("serialising cache entry")?;
    std::fs::write(&path, data).context("writing cache file")?;
    Ok(())
}

/// Build the render-cache identity. Rendering depends on URL and wait policy,
/// not on selector/format, because extraction happens after this stage.
pub fn render_identity(url: &str, js_mode: &str, wait_ms: u128, wait_for: Option<&str>) -> String {
    format!(
        "url={url}\njs_mode={js_mode}\nwait_ms={wait_ms}\nwait_for={}",
        wait_for.unwrap_or("")
    )
}

/// Look up a fresh rendered DOM cache entry.
pub fn get_render(identity: &str, ttl_secs: u64) -> Option<CachedRender> {
    let path = cache_dir("render")?.join(format!("{}.json", key(identity)));
    let data = std::fs::read(&path).ok()?;
    let cached: CachedRender = serde_json::from_slice(&data).ok()?;
    if now_secs().saturating_sub(cached.fetched_at) > ttl_secs {
        return None;
    }
    Some(cached)
}

/// Store a rendered DOM cache entry.
pub fn put_render(identity: &str, entry: &CachedRender) -> Result<()> {
    let dir = cache_dir("render").context("no system cache directory available")?;
    std::fs::create_dir_all(&dir).context("creating render cache directory")?;
    let path = dir.join(format!("{}.json", key(identity)));
    let data = serde_json::to_vec(entry).context("serialising render cache entry")?;
    std::fs::write(&path, data).context("writing render cache file")?;
    Ok(())
}

/// Convenience constructor for the current time.
pub fn now() -> u64 {
    now_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_stable_unique_and_hex() {
        // Same URL → same key.
        assert_eq!(key("https://example.com"), key("https://example.com"));
        // Different URLs → different keys.
        assert_ne!(key("https://example.com"), key("https://example.org"));
        // SHA-256 hex is always 64 chars, all hex digits.
        let k = key("https://example.com/some/path?q=1");
        assert_eq!(k.len(), 64);
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn render_identity_includes_render_controls() {
        let a = render_identity("https://example.com", "auto", 1000, None);
        let b = render_identity("https://example.com", "always", 1000, None);
        let c = render_identity("https://example.com", "auto", 2000, Some(".ready"));
        assert_ne!(a, b);
        assert_ne!(a, c);
    }
}
