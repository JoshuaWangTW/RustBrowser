//! Simple on-disk cache, keyed by a hash of the request identity.
//!
//! We cache raw HTTP fetches separately from rendered DOMs. A cached page can
//! still be re-extracted with a different selector or format later, while
//! JS-heavy pages avoid repeatedly launching Chrome for the same render inputs.

use std::path::{Path, PathBuf};
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

/// The cache kinds we manage on disk.
const KINDS: [&str; 2] = ["fetch", "render"];

/// What a cache-maintenance operation removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CachePurge {
    /// Number of cache files removed.
    pub removed: usize,
    /// Total bytes freed.
    pub bytes: u64,
}

/// Entry counts and on-disk sizes for each cache kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheReport {
    pub fetch_entries: usize,
    pub fetch_bytes: u64,
    pub render_entries: usize,
    pub render_bytes: u64,
}

impl CacheReport {
    pub fn total_entries(&self) -> usize {
        self.fetch_entries + self.render_entries
    }
    pub fn total_bytes(&self) -> u64 {
        self.fetch_bytes + self.render_bytes
    }
}

/// Summarise both cache kinds: how many entries and how many bytes each holds.
pub fn report() -> CacheReport {
    let (fetch_entries, fetch_bytes) = cache_dir("fetch").map(|d| dir_report(&d)).unwrap_or((0, 0));
    let (render_entries, render_bytes) = cache_dir("render")
        .map(|d| dir_report(&d))
        .unwrap_or((0, 0));
    CacheReport {
        fetch_entries,
        fetch_bytes,
        render_entries,
        render_bytes,
    }
}

/// Remove every cached entry across both kinds. Returns what was freed.
pub fn clear() -> Result<CachePurge> {
    purge_kinds(|_| true)
}

/// Remove entries older than `ttl_secs` — the same staleness rule `get` applies,
/// but enacted on disk. Corrupt/unparseable entries are dropped too.
pub fn prune(ttl_secs: u64) -> Result<CachePurge> {
    let cutoff = now_secs().saturating_sub(ttl_secs);
    purge_kinds(move |path| entry_is_expired(path, cutoff))
}

/// Apply `should_remove` to every kind's directory and total the results.
fn purge_kinds(should_remove: impl Fn(&Path) -> bool) -> Result<CachePurge> {
    let mut total = CachePurge::default();
    for kind in KINDS {
        if let Some(dir) = cache_dir(kind) {
            let p = purge_dir(&dir, &should_remove)?;
            total.removed += p.removed;
            total.bytes += p.bytes;
        }
    }
    Ok(total)
}

/// Count `*.json` cache files in `dir` and sum their sizes. Missing dir → zero.
fn dir_report(dir: &Path) -> (usize, u64) {
    let mut count = 0;
    let mut bytes = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            if !is_cache_file(&entry.path()) {
                continue;
            }
            if let Ok(meta) = entry.metadata()
                && meta.is_file()
            {
                count += 1;
                bytes += meta.len();
            }
        }
    }
    (count, bytes)
}

/// Remove every `*.json` file in `dir` for which `should_remove` is true. Only
/// our own cache files are ever touched; a missing dir is a no-op.
fn purge_dir(dir: &Path, should_remove: impl Fn(&Path) -> bool) -> Result<CachePurge> {
    let mut purge = CachePurge::default();
    if !dir.exists() {
        return Ok(purge);
    }
    let rd =
        std::fs::read_dir(dir).with_context(|| format!("reading cache dir {}", dir.display()))?;
    for entry in rd {
        let entry = entry.context("reading cache entry")?;
        let path = entry.path();
        if !is_cache_file(&path) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        if should_remove(&path) {
            let len = meta.len();
            if std::fs::remove_file(&path).is_ok() {
                purge.removed += 1;
                purge.bytes += len;
            }
        }
    }
    Ok(purge)
}

/// Cache files are the `*.json` entries we write; never touch anything else.
fn is_cache_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("json")
}

/// Minimal view of a cache file: just the timestamp, valid for both kinds since
/// `fetched_at` is the first field of each.
#[derive(Deserialize)]
struct Stamp {
    fetched_at: u64,
}

/// Whether the entry at `path` is older than `cutoff` (and so should be pruned).
/// Unreadable JSON counts as expired; a transient read error leaves it in place.
fn entry_is_expired(path: &Path, cutoff: u64) -> bool {
    match std::fs::read(path) {
        Ok(data) => match serde_json::from_slice::<Stamp>(&data) {
            Ok(s) => s.fetched_at < cutoff,
            Err(_) => true,
        },
        Err(_) => false,
    }
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

    /// A unique scratch directory under the OS temp dir, isolated per test so
    /// the suite never touches the user's real cache and stays parallel-safe.
    fn temp_subdir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rustbrowser-test-{}-{}-{tag}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn dir_report_counts_only_json_files() {
        let dir = temp_subdir("report");
        std::fs::write(dir.join("a.json"), b"{}").unwrap();
        std::fs::write(dir.join("b.json"), b"hello").unwrap();
        std::fs::write(dir.join("note.txt"), b"ignore me").unwrap();
        let (count, bytes) = dir_report(&dir);
        assert_eq!(count, 2);
        assert_eq!(bytes, 2 + 5); // only the two json files
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn purge_dir_clear_removes_only_json() {
        let dir = temp_subdir("clear");
        std::fs::write(dir.join("a.json"), b"{}").unwrap();
        std::fs::write(dir.join("b.json"), b"{}").unwrap();
        std::fs::write(dir.join("keep.txt"), b"x").unwrap();
        let purge = purge_dir(&dir, |_| true).unwrap();
        assert_eq!(purge.removed, 2);
        assert!(!dir.join("a.json").exists());
        assert!(!dir.join("b.json").exists());
        // Foreign files are never deleted.
        assert!(dir.join("keep.txt").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn purge_dir_prune_removes_only_expired() {
        let dir = temp_subdir("prune");
        let fresh = serde_json::json!({ "fetched_at": now_secs() }).to_string();
        let stale = serde_json::json!({ "fetched_at": 1000u64 }).to_string();
        std::fs::write(dir.join("fresh.json"), fresh).unwrap();
        std::fs::write(dir.join("stale.json"), stale).unwrap();
        // Cutoff = "older than one hour".
        let cutoff = now_secs().saturating_sub(3600);
        let purge = purge_dir(&dir, |p| entry_is_expired(p, cutoff)).unwrap();
        assert_eq!(purge.removed, 1);
        assert!(dir.join("fresh.json").exists());
        assert!(!dir.join("stale.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn entry_is_expired_treats_corrupt_as_expired() {
        let dir = temp_subdir("corrupt");
        let path = dir.join("bad.json");
        std::fs::write(&path, b"not valid json").unwrap();
        // Corrupt entry is always prunable, regardless of cutoff.
        assert!(entry_is_expired(&path, now_secs()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn purge_dir_missing_directory_is_noop() {
        let dir = temp_subdir("missing");
        std::fs::remove_dir_all(&dir).ok(); // ensure it does not exist
        let purge = purge_dir(&dir, |_| true).unwrap();
        assert_eq!(purge, CachePurge::default());
    }
}
