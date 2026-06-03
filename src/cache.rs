//! Simple on-disk fetch cache, keyed by a hash of the URL.
//!
//! We cache the *fetch* stage (raw HTML + metadata), not the distilled output,
//! so a cached page can still be re-extracted with a different selector or
//! format later. The expensive part we're skipping is the network round-trip.

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

/// Where cache files live: `<system cache dir>/rustbrowser/fetch/`.
fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("rustbrowser").join("fetch"))
}

/// Hex SHA-256 of the URL — a stable, filesystem-safe cache key.
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
    let path = cache_dir()?.join(format!("{}.json", key(url)));
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
    let dir = cache_dir().context("no system cache directory available")?;
    std::fs::create_dir_all(&dir).context("creating cache directory")?;
    let path = dir.join(format!("{}.json", key(url)));
    let data = serde_json::to_vec(entry).context("serialising cache entry")?;
    std::fs::write(&path, data).context("writing cache file")?;
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
}
