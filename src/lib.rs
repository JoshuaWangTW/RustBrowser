//! RustBrowser core pipeline: fetch → extract → convert → measure.
//!
//! Deliberately free of any CLI/MCP specifics so the same pipeline can back a
//! CLI today and an MCP server tomorrow. The whole point is to turn a heavy
//! HTML page into the minimal token footprint an LLM actually needs.

pub mod actions;
pub mod budget;
pub mod cache;
pub mod convert;
pub mod extract;
pub mod fetch;
pub mod planner;
pub mod render;
#[cfg(feature = "robots")]
pub mod robots;
pub mod session;
pub mod structured;
pub mod tokens;

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures::stream::{self, StreamExt};
use scraper::{Html, Selector};
use serde::Serialize;

use crate::actions::{ActionLimits, ActionTree};
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

/// How to select content from the fetched page. A `selector` in `DistillOptions`
/// overrides the profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// Readability main-content extraction — nav/ads/footer stripped. (default)
    #[default]
    Article,
    /// The whole `<body>` (scripts/styles removed) with no Readability
    /// filtering — for pages Readability over-strips (docs, reference, layouts).
    Full,
    /// Title + a short summary only — the cheapest "what is this page about".
    Metadata,
}

impl Profile {
    /// Stable lowercase label for diagnostics/CLI.
    pub fn label(self) -> &'static str {
        match self {
            Profile::Article => "article",
            Profile::Full => "full",
            Profile::Metadata => "metadata",
        }
    }
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
    /// Content-selection profile (ignored when `selector` is set).
    pub profile: Profile,
    /// If set, truncate the Markdown/text output to fit this many tokens (at a
    /// paragraph boundary, with a marker). `None` = no limit.
    pub max_output_tokens: Option<usize>,
    /// Compute and attach extraction-quality `Diagnostics` to the result.
    pub diagnostics: bool,
    /// Extract the operable action tree (links/forms/buttons/downloads).
    pub extract_actions: bool,
    /// Cap each action category at this many entries (None = sensible defaults),
    /// keeping the action tree from exploding on huge pages.
    pub max_actions: Option<usize>,
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
            profile: Profile::Article,
            max_output_tokens: None,
            diagnostics: false,
            extract_actions: false,
            max_actions: None,
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

/// Extraction-quality signals — a quick health check on what the pipeline
/// produced, so callers can spot over-stripping, empty results, or truncation.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostics {
    /// Which content profile produced this output.
    pub profile: &'static str,
    /// Raw response bytes before extraction.
    pub raw_bytes: usize,
    /// Characters of distilled Markdown produced.
    pub output_chars: usize,
    /// Estimated tokens of the distilled output.
    pub output_tokens: usize,
    /// `output_chars` as a fraction of raw bytes — a very low value on a large
    /// page can signal Readability over-stripped (try the `full` profile).
    pub extraction_ratio: f64,
    /// Structured links extracted (0 unless link extraction was requested).
    pub link_count: usize,
    /// Structured tables extracted (0 unless table extraction was requested).
    pub table_count: usize,
    /// Total actions in the action tree (0 unless action extraction requested).
    pub action_count: usize,
    /// Whether headless rendering was used for this page.
    pub used_headless: bool,
    /// Whether the output was truncated to fit the token budget.
    pub truncated: bool,
    /// Warning: the distilled output is suspiciously short — extraction may have
    /// failed or the page may be mostly non-text.
    pub low_content: bool,
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
    /// Operable action tree (links/forms/buttons/downloads) when requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<ActionTree>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Diagnostics>,
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
        let mut used_headless = false;
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
                used_headless = true;
            } else {
                let rendered = match &opts.js_wait_for {
                    Some(sel) => render::render_html_cdp(&fetched.final_url, sel, budget).await,
                    None => render::render_html(&fetched.final_url, budget).await,
                };
                if let Ok(html) = rendered {
                    fetched.raw_bytes = html.len();
                    fetched.html = html;
                    content = extract_content(&fetched, opts)?;
                    used_headless = true;

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

        assemble(fetched, content, used_headless, opts)
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

/// Run the extraction pipeline over HTML you already hold — no network, no
/// headless rendering, no cache. Backs the offline eval suite and is handy for
/// distilling captured HTML. `base_url` resolves relative links.
pub fn distill_html(html: &str, base_url: &str, opts: &DistillOptions) -> Result<Distilled> {
    let fetched = FetchResult {
        final_url: base_url.to_string(),
        status: 200,
        content_type: Some("text/html".to_string()),
        raw_bytes: html.len(),
        html: html.to_string(),
    };
    let content = extract_content(&fetched, opts)?;
    assemble(fetched, content, false, opts)
}

/// Turn extracted content into the final `Distilled`: convert to Markdown, apply
/// the token budget, run structured extraction, and compute stats/diagnostics.
/// Shared by the live `distill` path and the offline `distill_html`.
fn assemble(
    fetched: FetchResult,
    content: Content,
    used_headless: bool,
    opts: &DistillOptions,
) -> Result<Distilled> {
    let markdown = convert::to_markdown(&content.content_html)?;

    // Token budget: truncate rendered output to fit, if a limit was requested.
    let (markdown, markdown_truncated) = match opts.max_output_tokens {
        Some(max) => budget::fit(&markdown, max),
        None => (markdown, false),
    };

    // Structured extraction runs against the distilled content, so links and
    // tables reflect the main body rather than page-wide navigation chrome.
    let links = opts.extract_links.then(|| {
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

    // The action tree is page-wide (nav links, forms, …), so it runs against the
    // full HTML, not the distilled main content.
    let actions = opts.extract_actions.then(|| {
        let limits = opts
            .max_actions
            .map(ActionLimits::uniform)
            .unwrap_or_default();
        actions::extract_actions(&fetched.html, &fetched.final_url, limits)
    });

    let (text, text_truncated) = if content.text.is_empty() {
        (markdown.clone(), false)
    } else {
        match opts.max_output_tokens {
            Some(max) => budget::fit(&content.text, max),
            None => (content.text.clone(), false),
        }
    };
    let truncated = markdown_truncated || text_truncated;

    let stats = opts.measure_tokens.then(|| {
        let ts = TokenStats::measure(&fetched.html, &markdown);
        Stats {
            raw_bytes: fetched.raw_bytes,
            raw_tokens: ts.raw_tokens,
            output_tokens: ts.output_tokens,
            saved_tokens: ts.saved(),
            saved_ratio: ts.saved_ratio(),
        }
    });

    let diagnostics = opts.diagnostics.then(|| {
        let output_chars = markdown.chars().count();
        let raw_chars = fetched.html.chars().count().max(1);
        Diagnostics {
            profile: if opts.selector.is_some() {
                "selector"
            } else {
                opts.profile.label()
            },
            raw_bytes: fetched.raw_bytes,
            output_chars,
            output_tokens: tokens::count(&markdown),
            extraction_ratio: output_chars as f64 / raw_chars as f64,
            link_count: links.as_ref().map_or(0, Vec::len),
            table_count: tables.as_ref().map_or(0, Vec::len),
            action_count: actions.as_ref().map_or(0, ActionTree::len),
            used_headless,
            truncated,
            low_content: markdown.trim().chars().count() < 200,
        }
    });

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
        actions,
        diagnostics,
    })
}

/// Select content from the fetched page: an explicit CSS `selector` wins;
/// otherwise the chosen `profile` decides.
fn extract_content(fetched: &FetchResult, opts: &DistillOptions) -> Result<Content> {
    if let Some(sel) = &opts.selector {
        let html = select_html(&fetched.html, sel)?;
        return Ok(Content {
            title: String::new(),
            byline: None,
            excerpt: None,
            content_html: html,
            text: String::new(),
        });
    }

    match opts.profile {
        Profile::Article => Ok(content_from(extract::extract(
            &fetched.html,
            &fetched.final_url,
        )?)),
        Profile::Full => Ok(content_from(extract::extract_whole_body(&fetched.html)?)),
        Profile::Metadata => {
            let ex = extract::extract(&fetched.html, &fetched.final_url)?;
            let summary = ex
                .excerpt
                .clone()
                .filter(|e| !e.trim().is_empty())
                .unwrap_or_else(|| first_chars(&ex.text, 400));
            Ok(Content {
                title: ex.title,
                byline: ex.byline,
                excerpt: ex.excerpt,
                content_html: format!("<p>{}</p>", escape_text(&summary)),
                text: summary,
            })
        }
    }
}

fn content_from(ex: extract::Extracted) -> Content {
    Content {
        title: ex.title,
        byline: ex.byline,
        excerpt: ex.excerpt,
        content_html: ex.content_html,
        text: ex.text,
    }
}

/// First `n` characters of `s` (trimmed), with an ellipsis if it was longer.
fn first_chars(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() > n {
        let head: String = t.chars().take(n).collect();
        format!("{}…", head.trim_end())
    } else {
        t.to_string()
    }
}

/// Minimal HTML text escaping for content we synthesise ourselves.
fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
