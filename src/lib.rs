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
pub mod structured;
pub mod tokens;

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures::stream::{self, StreamExt};
use scraper::{Html, Selector};
use serde::Serialize;

use crate::cache::CachedFetch;
use crate::fetch::{FetchOptions, FetchResult};
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
    /// Headless JS-rendering fallback policy.
    pub js_mode: JsMode,
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
            js_mode: JsMode::Auto,
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
    let mut fetched = fetch_maybe_cached(url, opts).await?;
    let mut content = extract_content(&fetched, opts)?;

    // Headless fallback: re-fetch via a real browser when the HTTP HTML looks
    // like an unrendered JS app (or when always/never is forced). Failures here
    // are non-fatal — we keep the HTTP result.
    let needs_js = match opts.js_mode {
        JsMode::Off => false,
        JsMode::Always => true,
        JsMode::Auto => {
            // Count visible (non-whitespace) characters: readability's text is
            // padded with lots of layout whitespace that would otherwise mask
            // an empty, JS-rendered page as if it had real content.
            let visible: usize = content.text.split_whitespace().map(str::len).sum();
            opts.selector.is_none()
                && visible < render::JS_TEXT_THRESHOLD
                && render::looks_like_js_app(&fetched.html)
        }
    };
    if needs_js && let Ok(rendered) = render::render_html(&fetched.final_url, opts.timeout).await {
        fetched.raw_bytes = rendered.len();
        fetched.html = rendered;
        content = extract_content(&fetched, opts)?;
    }

    let markdown = convert::to_markdown(&content.content_html)?;

    // Structured extraction runs against the distilled content, so links and
    // tables reflect the main body rather than page-wide navigation chrome.
    let links = opts
        .extract_links
        .then(|| structured::extract_links(&content.content_html, &fetched.final_url));
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
    stream::iter(urls.iter().cloned())
        .map(|url| async move {
            let result = distill(&url, opts).await;
            (url, result)
        })
        .buffered(concurrency.max(1))
        .collect()
        .await
}

/// Fetch a URL, consulting and populating the on-disk cache when enabled.
/// Cache writes are best-effort: a failure to cache is not a failure to fetch.
async fn fetch_maybe_cached(url: &str, opts: &DistillOptions) -> Result<FetchResult> {
    let cached = if opts.use_cache {
        cache::get(url, opts.cache_ttl)
    } else {
        None
    };
    if let Some(c) = cached {
        return Ok(FetchResult {
            final_url: c.final_url,
            status: c.status,
            content_type: c.content_type,
            html: c.html,
            raw_bytes: c.raw_bytes,
        });
    }

    let mut fopts = FetchOptions {
        timeout: opts.timeout,
        ..Default::default()
    };
    if let Some(ua) = &opts.user_agent {
        fopts.user_agent = ua.clone();
    }
    let fetched = fetch::fetch(url, &fopts).await?;

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
