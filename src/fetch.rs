//! Lightweight HTTP fetching — no browser engine, no JS execution.
//!
//! Pulls raw bytes over HTTP(S) with automatic gzip/brotli/deflate
//! decompression and charset-aware decoding, then hands the HTML off to
//! the extraction stage. This is the cheap path that avoids spinning up a
//! full rendering engine for the common case.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use encoding_rs::{Encoding, UTF_8};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{CONTENT_TYPE, LOCATION, RETRY_AFTER};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::sleep;
use url::{Host, Url};

/// Default User-Agent. A real browser-ish UA reduces the chance of being
/// served a degraded/blocked page, while still being honest about origin. The
/// version tracks the crate version automatically.
const DEFAULT_UA: &str = concat!(
    "Mozilla/5.0 (compatible; RustBrowser/",
    env!("CARGO_PKG_VERSION"),
    "; +https://github.com/JoshuaWangTW/RustBrowser)"
);

/// Tunables for a fetch.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub user_agent: String,
    pub timeout: Duration,
    /// Hard cap on the response body we will decode, to bound memory/tokens.
    pub max_bytes: usize,
    /// Follow up to this many HTTP redirects. Each hop is safety-checked.
    pub max_redirects: usize,
    /// Permit loopback/localhost targets (off by default). Opt-in for hitting
    /// local dev servers — and what the integration tests use to reach a mock
    /// server on 127.0.0.1. Only loopback is freed; private LAN, link-local,
    /// and cloud-metadata addresses stay blocked even when this is set.
    pub allow_local: bool,
    /// Retry transient failures (connect/timeout errors, 429, 5xx) up to this
    /// many times with exponential backoff + jitter. 0 disables retrying.
    pub max_retries: usize,
    /// Cap on simultaneous in-flight requests to any single host. 0 = unlimited.
    pub per_host_concurrency: usize,
    /// Minimum spacing between request starts to the same host (rate limit).
    /// Zero disables rate limiting.
    pub min_request_interval: Duration,
    /// Consult each host's robots.txt and refuse paths it disallows for our
    /// User-Agent (off by default; requires the `robots` feature to enforce).
    pub respect_robots: bool,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            user_agent: DEFAULT_UA.to_string(),
            timeout: Duration::from_secs(20),
            max_bytes: 8 * 1024 * 1024, // 8 MiB is plenty for a document page
            max_redirects: 8,
            allow_local: false,
            max_retries: 2,
            per_host_concurrency: 4,
            min_request_interval: Duration::ZERO,
            respect_robots: false,
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

/// One attempt's outcome, carrying the optional `Retry-After` so the retry
/// loop can honour a server's requested delay.
struct Attempt {
    res: FetchResult,
    retry_after: Option<Duration>,
}

/// Reusable HTTP client for many fetches. Sharing it preserves connection
/// pooling across batch requests, and the per-host gate/robots state is shared
/// (behind `Arc`) so politeness limits apply across a whole batch.
#[derive(Clone)]
pub struct Fetcher {
    client: reqwest::Client,
    opts: FetchOptions,
    gate: Arc<HostGate>,
    #[cfg(feature = "robots")]
    robots: Arc<crate::robots::RobotsCache>,
}

impl Fetcher {
    pub fn new(opts: FetchOptions) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(&opts.user_agent)
            .timeout(opts.timeout)
            .gzip(true)
            .brotli(true)
            .deflate(true)
            .redirect(reqwest::redirect::Policy::none())
            // Resolve + screen IPs at the connection layer so reqwest dials the
            // exact addresses we validated — no separate pre-flight lookup that a
            // rebinding/low-TTL DNS could diverge from. Set once on the shared
            // client, so it applies to every request without breaking pooling.
            .dns_resolver(Arc::new(SafeResolver {
                allow_local: opts.allow_local,
            }))
            .build()
            .context("building HTTP client")?;

        let gate = Arc::new(HostGate::new(
            opts.per_host_concurrency,
            opts.min_request_interval,
        ));

        Ok(Self {
            client,
            opts,
            gate,
            #[cfg(feature = "robots")]
            robots: Arc::new(crate::robots::RobotsCache::new()),
        })
    }

    /// Fetch a URL and return its decoded body. Each actual request hop applies,
    /// in order: robots.txt (opt-in), the per-host concurrency permit, the
    /// per-host rate limit, then the request itself.
    pub async fn fetch(&self, url: &str) -> Result<FetchResult> {
        let allow_local = self.opts.allow_local;
        let start = parse_and_validate_url_basics(url, allow_local)?;
        self.fetch_with_retries(url, start).await
    }

    /// Enforce robots.txt for a URL string under this fetcher's policy. This is
    /// also used before serving cache hits so `respect_robots` cannot be bypassed
    /// by content fetched under a looser previous policy.
    pub async fn enforce_robots_for_url(&self, url: &str) -> Result<()> {
        let parsed = parse_and_validate_url_basics(url, self.opts.allow_local)?;
        self.enforce_robots(&parsed).await
    }

    async fn enforce_robots(&self, url: &Url) -> Result<()> {
        #[cfg(not(feature = "robots"))]
        let _ = url;
        #[cfg(feature = "robots")]
        if self.opts.respect_robots && !self.robots.allowed(&self.client, url, &self.opts).await? {
            bail!("blocked by robots.txt: {}", url.as_str());
        }
        Ok(())
    }

    /// Retry wrapper around `fetch_once`: backs off on transient transport
    /// errors and retryable statuses (429/5xx), honouring `Retry-After`.
    async fn fetch_with_retries(&self, url: &str, start: Url) -> Result<FetchResult> {
        let max = self.opts.max_retries;
        let mut attempt = 0usize;
        loop {
            match self.fetch_once(url, start.clone()).await {
                Ok(a) if attempt < max && is_retryable_status(a.res.status) => {
                    let delay = a.retry_after.unwrap_or_else(|| backoff_delay(attempt));
                    attempt += 1;
                    sleep(delay).await;
                }
                Ok(a) => return Ok(a.res),
                Err(e) if attempt < max && is_transient_error(&e) => {
                    sleep(backoff_delay(attempt)).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// A single fetch attempt: follows redirects (each safety-checked) and reads
    /// the final body. Status is surfaced as-is (no error on 4xx/5xx).
    async fn fetch_once(&self, url: &str, start: Url) -> Result<Attempt> {
        let allow_local = self.opts.allow_local;
        let mut current = start;

        for redirect_count in 0..=self.opts.max_redirects {
            // First line: cheap, DNS-free checks (scheme, literal IPs, localhost).
            // The connection-layer SafeResolver is the authoritative second line
            // that screens the IPs reqwest actually dials.
            validate_host_basics(&current, allow_local)?;

            // robots.txt and host politeness are enforced per actual request URL,
            // including redirect hops, so a redirect cannot bypass either policy.
            self.enforce_robots(&current).await?;
            let host = host_key(&current);
            let _permit = self.gate.enter(&host).await;

            let mut resp = self
                .client
                .get(current.clone())
                .send()
                .await
                .with_context(|| format!("requesting {current}"))?;

            let status = resp.status();
            if status.is_redirection() {
                if redirect_count == self.opts.max_redirects {
                    bail!("too many redirects while requesting {url}");
                }
                if let Some(next) = redirect_target(&current, &resp, allow_local)? {
                    current = next;
                    continue;
                }
            }

            let final_url = resp.url().to_string();
            validate_cached_url(&final_url, allow_local)?;

            let retry_after = parse_retry_after(resp.headers());
            let content_type = resp
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            let bytes = read_limited_body(&mut resp, self.opts.max_bytes).await?;
            let raw_bytes = bytes.len();
            let html = decode_body(&bytes, content_type.as_deref());

            return Ok(Attempt {
                res: FetchResult {
                    final_url,
                    status: status.as_u16(),
                    content_type,
                    html,
                    raw_bytes,
                },
                retry_after,
            });
        }

        unreachable!("redirect loop exits by returning or bailing")
    }
}

/// Convenience fetch for callers that do not need client reuse.
pub async fn fetch(url: &str, opts: &FetchOptions) -> Result<FetchResult> {
    Fetcher::new(opts.clone())?.fetch(url).await
}

/// The gate key for a URL: its host (domain or IP literal). Politeness limits
/// are per host, independent of scheme/port/path.
fn host_key(url: &Url) -> String {
    url.host().map(|h| h.to_string()).unwrap_or_default()
}

/// Per-host politeness coordinator: bounds simultaneous requests to a host and
/// spaces out their start times. State is created lazily per host and shared
/// across the whole batch (the `Fetcher` holds it behind an `Arc`).
struct HostGate {
    per_host_concurrency: usize,
    min_interval: Duration,
    slots: Mutex<HashMap<String, Arc<HostSlot>>>,
}

struct HostSlot {
    /// Permits = per-host concurrency limit.
    sem: Arc<Semaphore>,
    /// Earliest `Instant` the next request to this host may start.
    next_allowed: Mutex<Option<Instant>>,
}

impl HostGate {
    fn new(per_host_concurrency: usize, min_interval: Duration) -> Self {
        Self {
            per_host_concurrency,
            min_interval,
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// Get-or-create the per-host slot.
    fn slot(&self, host: &str) -> Arc<HostSlot> {
        let mut slots = self.slots.lock().expect("host gate mutex poisoned");
        slots
            .entry(host.to_string())
            .or_insert_with(|| {
                // 0 means "no per-host cap" — model it with a very large permit
                // pool rather than a special case in the hot path.
                let permits = if self.per_host_concurrency == 0 {
                    Semaphore::MAX_PERMITS
                } else {
                    self.per_host_concurrency
                };
                Arc::new(HostSlot {
                    sem: Arc::new(Semaphore::new(permits)),
                    next_allowed: Mutex::new(None),
                })
            })
            .clone()
    }

    /// Acquire a concurrency permit for `host`, then wait out any rate-limit
    /// spacing. The returned permit must be held for the duration of the work.
    async fn enter(&self, host: &str) -> OwnedSemaphorePermit {
        let slot = self.slot(host);
        let permit = slot
            .sem
            .clone()
            .acquire_owned()
            .await
            .expect("host semaphore is never closed");
        self.rate_wait(&slot).await;
        permit
    }

    /// Sleep until this host's next start time, reserving the following slot so
    /// concurrent callers queue rather than burst.
    async fn rate_wait(&self, slot: &HostSlot) {
        if self.min_interval.is_zero() {
            return;
        }
        // Reserve under the lock (no await held), then sleep outside it.
        let wait = {
            let mut next = slot.next_allowed.lock().expect("rate mutex poisoned");
            let now = Instant::now();
            let scheduled = next.map(|t| t.max(now)).unwrap_or(now);
            *next = Some(scheduled + self.min_interval);
            scheduled.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            sleep(wait).await;
        }
    }
}

/// Statuses worth retrying: rate-limit and the transient server-side 5xx set.
/// 500 is included because many gateways surface transient faults as a bare 500.
fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// Whether an error is worth retrying: connect/timeout transport errors, but
/// never our own SSRF block (surfaced as a `PermissionDenied` io error) and
/// never the non-network bails (scheme, redirect cap, …).
fn is_transient_error(e: &anyhow::Error) -> bool {
    for cause in e.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::PermissionDenied
        {
            return false; // an SSRF-blocked host will never become reachable
        }
        if let Some(re) = cause.downcast_ref::<reqwest::Error>()
            && (re.is_timeout() || re.is_connect())
        {
            return true;
        }
    }
    false
}

/// Exponential backoff (base 200 ms, doubling, capped at 10 s) plus clock-seeded
/// jitter — no RNG dependency needed for a backoff delay.
fn backoff_delay(attempt: usize) -> Duration {
    let base_ms = 200u64.saturating_mul(1u64 << attempt.min(6)).min(10_000);
    Duration::from_millis(base_ms + jitter_ms(base_ms))
}

/// Up to ~25% jitter, seeded from the wall clock's sub-second nanos.
fn jitter_ms(base_ms: u64) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let span = (base_ms / 4).max(1);
    nanos % span
}

/// Parse a `Retry-After` header's delta-seconds form, capped so a hostile or
/// silly value can't park us for ages. The HTTP-date form is ignored (we fall
/// back to normal backoff for it).
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?;
    let secs = raw.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(secs.min(60)))
}

/// Validate URL syntax and obvious local targets without DNS. This is used
/// before serving cached entries so unsafe local URLs are rejected even when a
/// previous version wrote them to disk.
pub fn validate_cached_url(url: &str, allow_local: bool) -> Result<()> {
    let parsed = parse_and_validate_url_basics(url, allow_local)?;
    validate_host_basics(&parsed, allow_local)
}

fn parse_and_validate_url_basics(url: &str, allow_local: bool) -> Result<Url> {
    let parsed = Url::parse(url).with_context(|| format!("invalid URL: {url}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => bail!("unsupported URL scheme '{scheme}'; only http and https are allowed"),
    }
    validate_host_basics(&parsed, allow_local)?;
    Ok(parsed)
}

fn validate_host_basics(url: &Url, allow_local: bool) -> Result<()> {
    let host = url
        .host()
        .ok_or_else(|| anyhow!("URL must include a host"))?;

    match host {
        Host::Domain(domain) => {
            let normalized = domain.trim_end_matches('.').to_ascii_lowercase();
            let is_localhost = normalized == "localhost" || normalized.ends_with(".localhost");
            if is_localhost && !allow_local {
                bail!("refusing to fetch localhost URL (enable allow_local to permit)");
            }
        }
        Host::Ipv4(ip) => check_ip(IpAddr::V4(ip), allow_local)?,
        Host::Ipv6(ip) => check_ip(IpAddr::V6(ip), allow_local)?,
    }

    Ok(())
}

/// Custom DNS resolver that screens every resolved address against the same
/// block-list used for literal IPs. reqwest connects to exactly the addresses
/// this returns, so the IP that passes validation is the IP that gets dialed.
///
/// This closes the DNS-rebinding gap: previously the safety check resolved DNS
/// once and reqwest resolved again at connect time, and a malicious or low-TTL
/// record could point the second lookup at internal space (`127.0.0.1`,
/// `169.254.169.254`, …). With the check living inside the resolver there is
/// only one resolution and it is the one that is enforced. TLS SNI and
/// certificate validation still use the original domain — reqwest keeps the
/// hostname and only takes the socket addresses from us.
#[derive(Debug)]
struct SafeResolver {
    allow_local: bool,
}

impl Resolve for SafeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(resolve_safely(name, self.allow_local))
    }
}

async fn resolve_safely(
    name: Name,
    allow_local: bool,
) -> Result<Addrs, Box<dyn std::error::Error + Send + Sync>> {
    let host = name.as_str();
    // Port 0 is a placeholder; reqwest replaces it with the URL's real port.
    let resolved = tokio::net::lookup_host((host, 0)).await?;
    let safe = screen_resolved_addrs(host, resolved, allow_local)?;
    Ok(Box::new(safe.into_iter()) as Addrs)
}

/// Keep only addresses permitted by the policy. Errors if every resolved
/// address is blocked, so a host that resolves solely to internal space is
/// refused at connect time instead of silently dialing nothing.
fn screen_resolved_addrs(
    host: &str,
    addrs: impl Iterator<Item = SocketAddr>,
    allow_local: bool,
) -> std::io::Result<Vec<SocketAddr>> {
    let safe: Vec<SocketAddr> = addrs
        .filter(|addr| ip_allowed(addr.ip(), allow_local))
        .collect();
    if safe.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing to connect to {host}: all resolved addresses are private, local, link-local, or metadata"
            ),
        ));
    }
    Ok(safe)
}

/// Whether an IP may be connected to under the current policy. `allow_local`
/// frees *only* loopback; private LAN, link-local, CGNAT, and metadata stay
/// blocked so the opt-in cannot be abused to reach cloud-internal services.
fn ip_allowed(ip: IpAddr, allow_local: bool) -> bool {
    if allow_local && ip.is_loopback() {
        return true;
    }
    !is_blocked_ip(ip)
}

fn check_ip(ip: IpAddr, allow_local: bool) -> Result<()> {
    if !ip_allowed(ip, allow_local) {
        bail!("refusing to fetch private, local, link-local, or metadata IP address");
    }
    Ok(())
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let o = ip.octets();
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local() // 169.254.0.0/16 — also covers the 169.254.169.254 metadata IP
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || o[0] == 0 // 0.0.0.0/8 (e.g. 0.0.0.1 routes to loopback on Linux)
                || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT 100.64.0.0/10
        }
        IpAddr::V6(ip) => {
            if let Some(ipv4) = ip.to_ipv4_mapped() {
                return is_blocked_ip(IpAddr::V4(ipv4));
            }
            // NAT64 well-known prefix 64:ff9b::/96 embeds an IPv4 address that
            // may point at an internal host via a NAT64 gateway.
            let seg = ip.segments();
            if seg[0] == 0x0064
                && seg[1] == 0xff9b
                && seg[2] == 0
                && seg[3] == 0
                && seg[4] == 0
                && seg[5] == 0
            {
                let embedded = Ipv4Addr::new(
                    (seg[6] >> 8) as u8,
                    (seg[6] & 0xff) as u8,
                    (seg[7] >> 8) as u8,
                    (seg[7] & 0xff) as u8,
                );
                return is_blocked_ip(IpAddr::V4(embedded));
            }
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || is_ipv6_unique_local(ip)
                || is_ipv6_unicast_link_local(ip)
        }
    }
}

fn is_ipv6_unique_local(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

fn is_ipv6_unicast_link_local(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

fn redirect_target(
    current: &Url,
    resp: &reqwest::Response,
    allow_local: bool,
) -> Result<Option<Url>> {
    let Some(location) = resp.headers().get(LOCATION) else {
        return Ok(None);
    };
    let location = location
        .to_str()
        .context("redirect Location header is not valid UTF-8")?;
    let next = current
        .join(location)
        .with_context(|| format!("invalid redirect Location: {location}"))?;
    parse_and_validate_url_basics(next.as_str(), allow_local)?;
    Ok(Some(next))
}

async fn read_limited_body(resp: &mut reqwest::Response, max_bytes: usize) -> Result<Vec<u8>> {
    if max_bytes == 0 {
        return Ok(Vec::new());
    }

    let mut out = Vec::with_capacity(
        resp.content_length()
            .map(|n| n.min(max_bytes as u64) as usize)
            .unwrap_or(0),
    );

    while let Some(chunk) = resp.chunk().await.context("reading response body")? {
        if out.len() >= max_bytes {
            break;
        }
        let remaining = max_bytes - out.len();
        if chunk.len() > remaining {
            out.extend_from_slice(&chunk[..remaining]);
            break;
        }
        out.extend_from_slice(&chunk);
    }

    Ok(out)
}

fn decode_body(bytes: &[u8], content_type: Option<&str>) -> String {
    let encoding = content_type
        .and_then(charset_from_content_type)
        .or_else(|| charset_from_html_meta(bytes))
        .and_then(|label| Encoding::for_label(label.as_bytes()))
        .unwrap_or(UTF_8);

    let (decoded, _, _) = encoding.decode(bytes);
    decoded.into_owned()
}

fn charset_from_content_type(content_type: &str) -> Option<String> {
    content_type.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        name.trim().eq_ignore_ascii_case("charset").then(|| {
            value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string()
        })
    })
}

fn charset_from_html_meta(bytes: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).to_ascii_lowercase();
    let marker = "charset=";
    let start = head.find(marker)? + marker.len();
    let value = head[start..].trim_start_matches([' ', '"', '\'']);
    let end = value
        .find(|c: char| c == '"' || c == '\'' || c == '>' || c.is_whitespace() || c == ';')
        .unwrap_or(value.len());
    let label = value[..end].trim();
    (!label.is_empty()).then(|| label.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_http_schemes_and_local_hosts() {
        assert!(validate_cached_url("file:///etc/passwd", false).is_err());
        assert!(validate_cached_url("http://localhost:3000", false).is_err());
        assert!(validate_cached_url("http://127.0.0.1:8080", false).is_err());
        assert!(validate_cached_url("http://[::1]/", false).is_err());
        assert!(validate_cached_url("http://169.254.169.254/latest/meta-data/", false).is_err());
    }

    #[test]
    fn accepts_public_http_urls() {
        assert!(validate_cached_url("https://example.com/article", false).is_ok());
    }

    #[test]
    fn rejects_cgnat_zero_and_nat64() {
        // CGNAT 100.64.0.0/10
        assert!(validate_cached_url("http://100.64.0.1/", false).is_err());
        assert!(validate_cached_url("http://100.127.255.254/", false).is_err());
        // 0.0.0.0/8 beyond 0.0.0.0 itself
        assert!(validate_cached_url("http://0.0.0.1/", false).is_err());
        // NAT64 64:ff9b::/96 embedding 127.0.0.1
        assert!(validate_cached_url("http://[64:ff9b::7f00:1]/", false).is_err());
        // A normal public IP in the 100.x but outside CGNAT stays allowed.
        assert!(validate_cached_url("http://100.0.0.1/", false).is_ok());
    }

    #[test]
    fn allow_local_frees_only_loopback() {
        // With allow_local, loopback + localhost become reachable…
        assert!(validate_cached_url("http://127.0.0.1:8080", true).is_ok());
        assert!(validate_cached_url("http://localhost:3000", true).is_ok());
        assert!(validate_cached_url("http://[::1]/", true).is_ok());
        // …but private LAN and cloud metadata stay blocked even then.
        assert!(validate_cached_url("http://10.0.0.1/", true).is_err());
        assert!(validate_cached_url("http://192.168.1.1/", true).is_err());
        assert!(validate_cached_url("http://169.254.169.254/", true).is_err());
    }

    #[test]
    fn screen_resolved_addrs_rejects_all_private() {
        // A host whose addresses are all internal is refused outright.
        let blocked = [
            SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 0)),
            SocketAddr::from((Ipv4Addr::new(169, 254, 169, 254), 0)),
            SocketAddr::from((Ipv4Addr::new(10, 0, 0, 5), 0)),
        ];
        assert!(screen_resolved_addrs("evil.example", blocked.into_iter(), false).is_err());
    }

    #[test]
    fn screen_resolved_addrs_keeps_only_public() {
        // Mixed resolution: internal addresses are dropped, public ones survive.
        let public = SocketAddr::from((Ipv4Addr::new(8, 8, 8, 8), 0));
        let mixed = [SocketAddr::from((Ipv4Addr::new(192, 168, 1, 1), 0)), public];
        let safe = screen_resolved_addrs("mixed.example", mixed.into_iter(), false).unwrap();
        assert_eq!(safe, vec![public]);
    }

    #[test]
    fn screen_resolved_addrs_allow_local_keeps_loopback() {
        let loopback = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 0));
        let safe = screen_resolved_addrs("local.test", [loopback].into_iter(), true).unwrap();
        assert_eq!(safe, vec![loopback]);
        // metadata is still dropped even with allow_local
        let meta = SocketAddr::from((Ipv4Addr::new(169, 254, 169, 254), 0));
        assert!(screen_resolved_addrs("meta.test", [meta].into_iter(), true).is_err());
    }

    #[tokio::test]
    async fn safe_resolver_rejects_private_ip() {
        // A private/metadata IP literal resolves locally (no network) and must be
        // refused — this is the exact path reqwest drives at connect time.
        let loopback: Name = "127.0.0.1".parse().unwrap();
        assert!(
            SafeResolver { allow_local: false }
                .resolve(loopback)
                .await
                .is_err()
        );

        let metadata: Name = "169.254.169.254".parse().unwrap();
        assert!(
            SafeResolver { allow_local: false }
                .resolve(metadata)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn safe_resolver_allows_public_ip() {
        let name: Name = "8.8.8.8".parse().unwrap();
        let addrs: Vec<SocketAddr> = SafeResolver { allow_local: false }
            .resolve(name)
            .await
            .expect("public IP must pass screening")
            .collect();
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].ip(), IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)));
    }

    #[tokio::test]
    async fn safe_resolver_allow_local_permits_loopback() {
        let name: Name = "127.0.0.1".parse().unwrap();
        let addrs: Vec<SocketAddr> = SafeResolver { allow_local: true }
            .resolve(name)
            .await
            .expect("loopback must pass with allow_local")
            .collect();
        assert_eq!(addrs[0].ip(), IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    }

    #[test]
    fn decodes_declared_charset() {
        let html = decode_body(b"\xa4\xa4\xa4\xe5", Some("text/html; charset=big5"));
        assert_eq!(html, "中文");
    }

    #[test]
    fn extracts_charset_from_meta_tag() {
        let bytes = br#"<html><head><meta charset="shift_jis"></head></html>"#;
        assert_eq!(charset_from_html_meta(bytes).as_deref(), Some("shift_jis"));
    }

    #[test]
    fn retryable_statuses_are_429_and_5xx_subset() {
        for s in [429, 500, 502, 503, 504] {
            assert!(is_retryable_status(s), "{s} should retry");
        }
        for s in [200, 301, 400, 403, 404, 418] {
            assert!(!is_retryable_status(s), "{s} should not retry");
        }
    }

    #[test]
    fn retry_after_parses_seconds_and_caps() {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("3"));
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(3)));
        // Absurd values are capped so a server can't park us for ages.
        h.insert(RETRY_AFTER, HeaderValue::from_static("99999"));
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(60)));
        // HTTP-date form is ignored (we fall back to backoff).
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after(&h), None);
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn backoff_grows_and_stays_bounded() {
        let d0 = backoff_delay(0);
        assert!(d0 >= Duration::from_millis(200) && d0 < Duration::from_millis(300));
        let d3 = backoff_delay(3); // base 1600 ms
        assert!(d3 >= Duration::from_millis(1600));
        // Capped at 10 s base + ≤25% jitter regardless of how high the attempt is.
        assert!(backoff_delay(50) <= Duration::from_millis(12_500));
    }

    #[test]
    fn host_key_is_normalised_host() {
        assert_eq!(
            host_key(&Url::parse("https://Example.COM/a/b?x=1").unwrap()),
            "example.com"
        );
        assert_eq!(
            host_key(&Url::parse("http://8.8.8.8:8080/").unwrap()),
            "8.8.8.8"
        );
    }

    #[tokio::test]
    async fn host_gate_serialises_per_host_when_concurrency_one() {
        let gate = HostGate::new(1, Duration::ZERO);
        let held = gate.enter("h").await; // takes the single permit
        // A second acquire on the same host cannot make progress yet.
        let mut blocked = Box::pin(gate.enter("h"));
        assert!(futures::poll!(blocked.as_mut()).is_pending());
        // A different host is independent and proceeds immediately.
        let _other = gate.enter("other").await;
        // Releasing the first permit lets the queued acquire complete.
        drop(held);
        let _second = blocked.await;
    }
}
