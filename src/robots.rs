//! robots.txt fetching, parsing, and enforcement (opt-in, `robots` feature).
//!
//! `texting_robots` does the parsing; we fetch robots.txt ourselves through the
//! shared, SSRF-safe reqwest client and cache the parsed rules per origin so
//! each host's robots.txt is fetched at most once per process.
//!
//! Policy for a *directed* fetcher (not a mass crawler): a 2xx robots.txt is
//! enforced; 4xx/404 means "no rules, allow"; network errors, 5xx, or an
//! unparseable body fail OPEN (allow) so a flaky robots endpoint never blocks a
//! user's explicit fetch.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use texting_robots::Robot;
use url::Url;

use crate::fetch::{FetchOptions, validate_cached_url};

/// robots.txt files should be small; cap what we read defensively.
const MAX_ROBOTS_BYTES: usize = 512 * 1024;

/// Per-origin cache of robots decisions, shared across a batch behind an `Arc`.
pub struct RobotsCache {
    origins: Mutex<HashMap<String, Arc<Decision>>>,
}

/// A parsed robots decision for one origin, already bound to our User-Agent.
enum Decision {
    /// No restrictions (missing / erroring / unparseable robots.txt).
    AllowAll,
    /// Enforce these parsed rules.
    Rules(Box<Robot>),
}

impl Decision {
    fn allows(&self, url: &str) -> bool {
        match self {
            Decision::AllowAll => true,
            Decision::Rules(robot) => robot.allowed(url),
        }
    }
}

impl RobotsCache {
    pub fn new() -> Self {
        Self {
            origins: Mutex::new(HashMap::new()),
        }
    }

    /// Whether `url` may be fetched under its origin's robots.txt, fetching and
    /// caching that robots.txt on first sight of the origin.
    pub async fn allowed(
        &self,
        client: &reqwest::Client,
        url: &Url,
        opts: &FetchOptions,
    ) -> Result<bool> {
        let origin = url.origin().ascii_serialization();

        if let Some(decision) = self.cached(&origin) {
            return Ok(decision.allows(url.as_str()));
        }

        let decision = Arc::new(self.resolve(client, url, opts).await);
        self.store(origin, decision.clone());
        Ok(decision.allows(url.as_str()))
    }

    fn cached(&self, origin: &str) -> Option<Arc<Decision>> {
        self.origins
            .lock()
            .expect("robots cache mutex poisoned")
            .get(origin)
            .cloned()
    }

    fn store(&self, origin: String, decision: Arc<Decision>) {
        self.origins
            .lock()
            .expect("robots cache mutex poisoned")
            .insert(origin, decision);
    }

    /// Fetch + parse the robots.txt for `url`'s origin. Always yields *some*
    /// decision — every error path fails open to `AllowAll`.
    async fn resolve(&self, client: &reqwest::Client, url: &Url, opts: &FetchOptions) -> Decision {
        let Some(robots_url) = robots_url_for(url) else {
            return Decision::AllowAll;
        };
        // The robots URL shares the target's already-validated host, but
        // re-screen defensively (and honour allow_local).
        if validate_cached_url(robots_url.as_str(), opts.allow_local).is_err() {
            return Decision::AllowAll;
        }

        match fetch_robots_body(client, &robots_url, opts).await {
            Ok(Some(bytes)) => match Robot::new(&opts.user_agent, &bytes) {
                Ok(robot) => Decision::Rules(Box::new(robot)),
                Err(_) => Decision::AllowAll, // unparseable → fail open
            },
            Ok(None) => Decision::AllowAll, // 4xx/404 → no rules
            Err(_) => Decision::AllowAll,   // network / 5xx → fail open
        }
    }
}

impl Default for RobotsCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Build `scheme://host[:port]/robots.txt` for the target URL's origin.
fn robots_url_for(url: &Url) -> Option<Url> {
    url.host()?; // opaque-origin URLs have nowhere to fetch robots from
    let mut r = url.clone();
    r.set_path("/robots.txt");
    r.set_query(None);
    r.set_fragment(None);
    Some(r)
}

/// Fetch a robots.txt body. `Ok(Some(bytes))` for 2xx, `Ok(None)` for 4xx (no
/// rules), `Err` for everything else (caller fails open).
async fn fetch_robots_body(
    client: &reqwest::Client,
    robots_url: &Url,
    opts: &FetchOptions,
) -> Result<Option<Vec<u8>>> {
    let resp = client
        .get(robots_url.clone())
        .timeout(opts.timeout)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        Ok(Some(read_capped_bytes(resp, MAX_ROBOTS_BYTES).await?))
    } else if status.is_client_error() {
        Ok(None)
    } else {
        bail!("robots.txt returned status {}", status.as_u16())
    }
}

/// Read a response body up to `max` bytes, stopping early once the cap is hit.
async fn read_capped_bytes(mut resp: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if out.len() >= max {
            break;
        }
        let take = (max - out.len()).min(chunk.len());
        out.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(txt: &str) -> Decision {
        Decision::Rules(Box::new(Robot::new("RustBrowser", txt.as_bytes()).unwrap()))
    }

    #[test]
    fn disallow_blocks_matching_path() {
        let d = rules("User-agent: *\nDisallow: /private");
        assert!(!d.allows("https://example.com/private/page"));
        assert!(d.allows("https://example.com/public/page"));
    }

    #[test]
    fn allow_all_permits_everything() {
        assert!(Decision::AllowAll.allows("https://example.com/anything"));
    }

    #[test]
    fn empty_robots_allows_all() {
        let d = rules(""); // no rules → unrestricted
        assert!(d.allows("https://example.com/whatever"));
    }

    #[test]
    fn robots_url_is_origin_root() {
        let u = Url::parse("https://example.com:8443/deep/path?q=1#frag").unwrap();
        let r = robots_url_for(&u).unwrap();
        assert_eq!(r.as_str(), "https://example.com:8443/robots.txt");
    }
}
