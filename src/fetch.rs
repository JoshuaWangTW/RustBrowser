//! Lightweight HTTP fetching — no browser engine, no JS execution.
//!
//! Pulls raw bytes over HTTP(S) with automatic gzip/brotli/deflate
//! decompression and charset-aware decoding, then hands the HTML off to
//! the extraction stage. This is the cheap path that avoids spinning up a
//! full rendering engine for the common case.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use encoding_rs::{Encoding, UTF_8};
use reqwest::header::{CONTENT_TYPE, LOCATION};
use url::{Host, Url};

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
    /// Follow up to this many HTTP redirects. Each hop is safety-checked.
    pub max_redirects: usize,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            user_agent: DEFAULT_UA.to_string(),
            timeout: Duration::from_secs(20),
            max_bytes: 8 * 1024 * 1024, // 8 MiB is plenty for a document page
            max_redirects: 8,
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

/// Reusable HTTP client for many fetches. Sharing it preserves connection
/// pooling across batch requests.
#[derive(Clone)]
pub struct Fetcher {
    client: reqwest::Client,
    opts: FetchOptions,
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
            .build()
            .context("building HTTP client")?;

        Ok(Self { client, opts })
    }

    /// Fetch a URL and return its decoded body.
    pub async fn fetch(&self, url: &str) -> Result<FetchResult> {
        let mut current = parse_and_validate_url_basics(url)?;

        for redirect_count in 0..=self.opts.max_redirects {
            validate_url_network_boundary(&current).await?;

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
                if let Some(next) = redirect_target(&current, &resp)? {
                    current = next;
                    continue;
                }
            }

            let final_url = resp.url().to_string();
            validate_cached_url(&final_url)?;

            let content_type = resp
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            let bytes = read_limited_body(&mut resp, self.opts.max_bytes).await?;
            let raw_bytes = bytes.len();
            let html = decode_body(&bytes, content_type.as_deref());

            return Ok(FetchResult {
                final_url,
                status: status.as_u16(),
                content_type,
                html,
                raw_bytes,
            });
        }

        unreachable!("redirect loop exits by returning or bailing")
    }
}

/// Convenience fetch for callers that do not need client reuse.
pub async fn fetch(url: &str, opts: &FetchOptions) -> Result<FetchResult> {
    Fetcher::new(opts.clone())?.fetch(url).await
}

/// Validate URL syntax and obvious local targets without DNS. This is used
/// before serving cached entries so unsafe local URLs are rejected even when a
/// previous version wrote them to disk.
pub fn validate_cached_url(url: &str) -> Result<()> {
    let parsed = parse_and_validate_url_basics(url)?;
    validate_host_basics(&parsed)
}

fn parse_and_validate_url_basics(url: &str) -> Result<Url> {
    let parsed = Url::parse(url).with_context(|| format!("invalid URL: {url}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => bail!("unsupported URL scheme '{scheme}'; only http and https are allowed"),
    }
    validate_host_basics(&parsed)?;
    Ok(parsed)
}

fn validate_host_basics(url: &Url) -> Result<()> {
    let host = url
        .host()
        .ok_or_else(|| anyhow!("URL must include a host"))?;

    match host {
        Host::Domain(domain) => {
            let normalized = domain.trim_end_matches('.').to_ascii_lowercase();
            if normalized == "localhost" || normalized.ends_with(".localhost") {
                bail!("refusing to fetch localhost URL");
            }
        }
        Host::Ipv4(ip) => validate_ip(IpAddr::V4(ip))?,
        Host::Ipv6(ip) => validate_ip(IpAddr::V6(ip))?,
    }

    Ok(())
}

async fn validate_url_network_boundary(url: &Url) -> Result<()> {
    validate_host_basics(url)?;

    if let Some(host) = url.host_str()
        && url.host().is_some_and(|h| matches!(h, Host::Domain(_)))
    {
        let port = url
            .port_or_known_default()
            .ok_or_else(|| anyhow!("URL has no known default port"))?;
        let addrs = tokio::net::lookup_host((host, port))
            .await
            .with_context(|| format!("resolving {host}"))?;
        for addr in addrs {
            validate_ip(addr.ip())?;
        }
    }

    Ok(())
}

fn validate_ip(ip: IpAddr) -> Result<()> {
    if is_blocked_ip(ip) {
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

fn redirect_target(current: &Url, resp: &reqwest::Response) -> Result<Option<Url>> {
    let Some(location) = resp.headers().get(LOCATION) else {
        return Ok(None);
    };
    let location = location
        .to_str()
        .context("redirect Location header is not valid UTF-8")?;
    let next = current
        .join(location)
        .with_context(|| format!("invalid redirect Location: {location}"))?;
    parse_and_validate_url_basics(next.as_str())?;
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
        assert!(validate_cached_url("file:///etc/passwd").is_err());
        assert!(validate_cached_url("http://localhost:3000").is_err());
        assert!(validate_cached_url("http://127.0.0.1:8080").is_err());
        assert!(validate_cached_url("http://[::1]/").is_err());
        assert!(validate_cached_url("http://169.254.169.254/latest/meta-data/").is_err());
    }

    #[test]
    fn accepts_public_http_urls() {
        assert!(validate_cached_url("https://example.com/article").is_ok());
    }

    #[test]
    fn rejects_cgnat_zero_and_nat64() {
        // CGNAT 100.64.0.0/10
        assert!(validate_cached_url("http://100.64.0.1/").is_err());
        assert!(validate_cached_url("http://100.127.255.254/").is_err());
        // 0.0.0.0/8 beyond 0.0.0.0 itself
        assert!(validate_cached_url("http://0.0.0.1/").is_err());
        // NAT64 64:ff9b::/96 embedding 127.0.0.1
        assert!(validate_cached_url("http://[64:ff9b::7f00:1]/").is_err());
        // A normal public IP in the 100.x but outside CGNAT stays allowed.
        assert!(validate_cached_url("http://100.0.0.1/").is_ok());
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
}
