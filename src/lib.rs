//! RustBrowser core pipeline: fetch → extract → convert → measure.
//!
//! Deliberately free of any CLI/MCP specifics so the same pipeline can back a
//! CLI today and an MCP server tomorrow. The whole point is to turn a heavy
//! HTML page into the minimal token footprint an LLM actually needs.

pub mod cache;
pub mod convert;
pub mod extract;
pub mod fetch;
pub mod render;
#[cfg(feature = "robots")]
pub mod robots;
pub mod structured;
pub mod tokens;

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures::stream::{self, StreamExt};
use scraper::{Html, Selector};
use serde::Serialize;

use crate::cache::{CachedFetch, CachedRender};
use crate::fetch::{FetchOptions, FetchResult, Fetcher};
use crate::structured::{Link, Table};
use crate::tokens::TokenStats;

/// When (if ever) to fall back to headless rendering of JavaScript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JsMode {
    /// Never render with a headless browser.
    Off,
    /// Render only when the page looks like an unrendered JS app. (default)
    #[default]
    Auto,
    /// Always render with a headless browser.
    Always,
}

/// Options controlling a single distillation.
#[derive(Debug, Clone)]
pub struct DistillOptions {
    pub timeout: Duration,
    pub user_agent: Option<String>,
    /// If set, extract elements matching this CSS selector instead of running
    /// readability.
    pub selector: Option<String>,
    /// Whether to compute token-savings statistics (adds a little work).
    pub measure_tokens: bool,
    /// Use the on-disk fetch cache (read fresh entries, write new ones).
    pub use_cache: bool,
    /// Cache freshness window in seconds; older entries are re-fetched.
    pub cache_ttl: u64,
    /// Also extract all links as structured data into `Distilled.links`.
    pub extract_links: bool,
    /// Also extract all tables as structured data into `Distilled.tables`.
    pub extract_tables: bool,
    /// When extracting links, pull from the whole page (incl. nav/footer)
    /// rather than just the distilled main content. Useful for crawling.
    pub links_full: bool,
    /// Headless JS-rendering fallback policy.
    pub js_mode: JsMode,
    /// Override the headless wait / virtual-time budget, in milliseconds.
    pub js_wait: Option<u64>,
    /// Wait until this CSS selector appears before capturing (drives CDP).
    pub js_wait_for: Option<String>,
    /// Hard cap on decoded response bytes retained from each HTTP response.
    pub max_bytes: usize,
    /// Permit loopback/localhost targets (off by default). Frees only loopback;
    /// private LAN, link-local, and cloud-metadata addresses stay blocked.
    pub allow_local: bool,
    /// Retry transient failures (connect/timeout, 429, 5xx) up to this many
    /// times with exponential backoff + jitter. 0 disables retrying.
    pub max_retries: usize,
    /// Cap simultaneous in-flight requests to any single host. 0 = unlimited.
    pub per_host_concurrency: usize,
    /// Minimum spacing between request starts to the same host (rate limit).
    /// Zero disables it.
    pub min_request_interval: Duration,
    /// Consult each host's robots.txt and refuse disallowed paths (needs the
    /// `robots` feature to enforce; off by default).
    pub respect_robots: bool,
}

impl Default for DistillOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(20),
            user_agent: None,
            selector: None,
            measure_tokens: false,
            use_cache: true,
            cache_ttl: 3600,
            extract_links: false,
            extract_tables: false,
            links_full: false,
            js_mode: JsMode::Auto,
            js_wait: None,
            js_wait_for: None,
            max_bytes: 8 * 1024 * 1024,
            allow_local: false,
            max_retries: 2,
            per_host_concurrency: 4,
            min_request_interval: Duration::ZERO,
            respect_robots: false,
        }
    }
}

/// Token-savings summary.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Stats {
    pub raw_bytes: usize,
    pub raw_tokens: usize,
    pub output_tokens: usize,
    pub saved_tokens: usize,
    pub saved_ratio: f64,
}

/// Distilled output of a fetch.
#[derive(Debug, Clone, Serialize)]
pub struct Distilled {
    pub final_url: String,
    pub status: u16,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    pub markdown: String,
    /// Plain-text rendering (not serialised to JSON to avoid duplication).
    #[serde(skip)]
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<Stats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<Link>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tables: Option<Vec<Table>>,
}

/// Reusable core pipeline. Construct one per option set to preserve HTTP
/// connection pooling across a batch.
pub struct BrowserCore {
    fetcher: Fetcher,
}

impl BrowserCore {
    pub fn new(opts: &DistillOptions) -> Result<Self> {
        let mut fopts = FetchOptions {
            timeout: opts.timeout,
            max_bytes: opts.max_bytes,
            allow_local: opts.allow_local,
            max_retries: opts.max_retries,
            per_host_concurrency: opts.per_host_concurrency,
            min_request_interval: opts.min_request_interval,
            respect_robots: opts.respect_robots,
            ..Default::default()
        };
        if let Some(ua) = &opts.user_agent {
            fopts.user_agent = ua.clone();
        }
        Ok(Self {
            fetcher: Fetcher::new(fopts)?,
        })
    }

    /// Run the full pipeline for a single URL.
    pub async fn distill(&self, url: &str, opts: &DistillOptions) -> Result<Distilled> {
        let mut fetched = self.fetch_maybe_cached(url, opts).await?;
        let mut content = extract_content(&fetched, opts)?;

        // Headless fallback: re-fetch via a real browser when the HTTP HTML looks
        // like an unrendered JS app (or when always/never is forced). Failures here
        // are non-fatal — we keep the HTTP result.
        let needs_js = match opts.js_mode {
            JsMode::Off => false,
            JsMode::Always => true,
            JsMode::Auto => {
                // An explicit wait-for selector means "definitely render". Otherwise
                // count visible (non-whitespace) characters: readability's text is
                // padded with layout whitespace that would mask an empty JS page.
                opts.js_wait_for.is_some() || {
                    let visible: usize = content.text.split_whitespace().map(str::len).sum();
                    opts.selector.is_none()
                        && visible < render::JS_TEXT_THRESHOLD
                        && render::looks_like_js_app(&fetched.html)
                }
            }
        };
        if needs_js {
            let budget = opts
                .js_wait
                .map(Duration::from_millis)
                .unwrap_or(opts.timeout);
            let cache_identity = cache::render_identity(
                &fetched.final_url,
                js_mode_label(opts.js_mode),
                budget.as_millis(),
                opts.js_wait_for.as_deref(),
            );

            let rendered_cache = opts
                .use_cache
                .then(|| cache::get_render(&cache_identity, opts.cache_ttl))
                .flatten();

            if let Some(cached) = rendered_cache {
                fetch::validate_cached_url(&cached.final_url, opts.allow_local)?;
                fetched.final_url = cached.final_url;
                fetched.raw_bytes = cached.raw_bytes;
                fetched.html = cached.html;
                content = extract_content(&fetched, opts)?;
            } else {
                let rendered = match &opts.js_wait_for {
                    Some(sel) => render::render_html_cdp(&fetched.final_url, sel, budget).await,
                    None => render::render_html(&fetched.final_url, budget).await,
                };
                if let Ok(html) = rendered {
                    fetched.raw_bytes = html.len();
                    fetched.html = html;
                    content = extract_content(&fetched, opts)?;

                    if opts.use_cache {
                        let _ = cache::put_render(
                            &cache_identity,
                            &CachedRender {
                                fetched_at: cache::now(),
                                final_url: fetched.final_url.clone(),
                                html: fetched.html.clone(),
                                raw_bytes: fetched.raw_bytes,
                            },
                        );
                    }
                }
            }
        }

        let markdown = convert::to_markdown(&content.content_html)?;

        // Structured extraction runs against the distilled content, so links and
        // tables reflect the main body rather than page-wide navigation chrome.
        let links = opts.extract_links.then(|| {
            // Whole-page links (incl. nav) for crawling, or just the main content.
            let scope = if opts.links_full {
                &fetched.html
            } else {
                &content.content_html
            };
            structured::extract_links(scope, &fetched.final_url)
        });
        let tables = opts
            .extract_tables
            .then(|| structured::extract_tables(&content.content_html));

        let text = if content.text.is_empty() {
            markdown.clone()
        } else {
            content.text
        };

        let stats = if opts.measure_tokens {
            let ts = TokenStats::measure(&fetched.html, &markdown);
            Some(Stats {
                raw_bytes: fetched.raw_bytes,
                raw_tokens: ts.raw_tokens,
                output_tokens: ts.output_tokens,
                saved_tokens: ts.saved(),
                saved_ratio: ts.saved_ratio(),
            })
        } else {
            None
        };

        Ok(Distilled {
            final_url: fetched.final_url,
            status: fetched.status,
            title: content.title,
            byline: content.byline,
            excerpt: content.excerpt,
            markdown,
            text,
            stats,
            links,
            tables,
        })
    }

    /// Fetch a URL, consulting and populating the on-disk cache when enabled.
    /// Cache writes are best-effort: a failure to cache is not a failure to fetch.
    async fn fetch_maybe_cached(&self, url: &str, opts: &DistillOptions) -> Result<FetchResult> {
        fetch::validate_cached_url(url, opts.allow_local)?;

        let cached = if opts.use_cache {
            cache::get(url, opts.cache_ttl)
        } else {
            None
        };
        if let Some(c) = cached {
            fetch::validate_cached_url(&c.final_url, opts.allow_local)?;
            self.fetcher.enforce_robots_for_url(url).await?;
            self.fetcher.enforce_robots_for_url(&c.final_url).await?;
            return Ok(FetchResult {
                final_url: c.final_url,
                status: c.status,
                content_type: c.content_type,
                html: c.html,
                raw_bytes: c.raw_bytes,
            });
        }

        let fetched = self.fetcher.fetch(url).await?;

        if opts.use_cache {
            let _ = cache::put(
                url,
                &CachedFetch {
                    fetched_at: cache::now(),
                    final_url: fetched.final_url.clone(),
                    status: fetched.status,
                    content_type: fetched.content_type.clone(),
                    html: fetched.html.clone(),
                    raw_bytes: fetched.raw_bytes,
                },
            );
        }

        Ok(fetched)
    }
}

/// Intermediate extracted content, before Markdown conversion.
struct Content {
    title: String,
    byline: Option<String>,
    excerpt: Option<String>,
    content_html: String,
    text: String,
}

/// Run the full pipeline for a single URL.
pub async fn distill(url: &str, opts: &DistillOptions) -> Result<Distilled> {
    BrowserCore::new(opts)?.distill(url, opts).await
}

/// Run readability (or a CSS selector) over a fetched page.
fn extract_content(fetched: &FetchResult, opts: &DistillOptions) -> Result<Content> {
    if let Some(sel) = &opts.selector {
        let html = select_html(&fetched.html, sel)?;
        Ok(Content {
            title: String::new(),
            byline: None,
            excerpt: None,
            content_html: html,
            text: String::new(),
        })
    } else {
        let ex = extract::extract(&fetched.html, &fetched.final_url)?;
        Ok(Content {
            title: ex.title,
            byline: ex.byline,
            excerpt: ex.excerpt,
            content_html: ex.content_html,
            text: ex.text,
        })
    }
}

/// Distill many URLs concurrently, preserving input order in the results.
///
/// `concurrency` caps how many requests are in flight at once — polite to
/// servers and avoids opening hundreds of sockets at once.
pub async fn distill_many(
    urls: &[String],
    opts: &DistillOptions,
    concurrency: usize,
) -> Vec<(String, Result<Distilled>)> {
    let core = match BrowserCore::new(opts) {
        Ok(core) => core,
        Err(e) => {
            let msg = e.to_string();
            return urls
                .iter()
                .cloned()
                .map(|url| (url, Err(anyhow!("building HTTP client failed: {msg}"))))
                .collect();
        }
    };
    let core = &core;

    stream::iter(urls.iter().cloned())
        .map(|url| async move {
            let result = core.distill(&url, opts).await;
            (url, result)
        })
        .buffered(concurrency.max(1))
        .collect()
        .await
}

/// Extract the outer HTML of every element matching a CSS selector.
fn select_html(html: &str, sel: &str) -> Result<String> {
    let doc = Html::parse_document(html);
    let selector =
        Selector::parse(sel).map_err(|e| anyhow!("invalid CSS selector '{sel}': {e:?}"))?;
    let mut out = String::new();
    for el in doc.select(&selector) {
        out.push_str(&el.html());
        out.push('\n');
    }
    if out.trim().is_empty() {
        bail!("selector '{sel}' matched no elements");
    }
    Ok(out)
}

fn js_mode_label(mode: JsMode) -> &'static str {
    match mode {
        JsMode::Off => "off",
        JsMode::Auto => "auto",
        JsMode::Always => "always",
    }
}
