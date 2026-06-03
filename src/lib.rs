//! RustBrowser core pipeline: fetch → extract → convert → measure.
//!
//! Deliberately free of any CLI/MCP specifics so the same pipeline can back a
//! CLI today and an MCP server tomorrow. The whole point is to turn a heavy
//! HTML page into the minimal token footprint an LLM actually needs.

pub mod convert;
pub mod extract;
pub mod fetch;
pub mod tokens;

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use scraper::{Html, Selector};
use serde::Serialize;

use crate::fetch::FetchOptions;
use crate::tokens::TokenStats;

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
}

impl Default for DistillOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(20),
            user_agent: None,
            selector: None,
            measure_tokens: false,
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
}

/// Run the full pipeline for a single URL.
pub async fn distill(url: &str, opts: &DistillOptions) -> Result<Distilled> {
    let mut fopts = FetchOptions {
        timeout: opts.timeout,
        ..Default::default()
    };
    if let Some(ua) = &opts.user_agent {
        fopts.user_agent = ua.clone();
    }

    let fetched = fetch::fetch(url, &fopts).await?;

    // Either pluck a specific CSS selector, or run full readability extraction.
    let (title, byline, excerpt, content_html, mut text) = if let Some(sel) = &opts.selector {
        let html = select_html(&fetched.html, sel)?;
        (String::new(), None, None, html, String::new())
    } else {
        let ex = extract::extract(&fetched.html, &fetched.final_url)?;
        (ex.title, ex.byline, ex.excerpt, ex.content_html, ex.text)
    };

    let markdown = convert::to_markdown(&content_html)?;
    if text.is_empty() {
        text = markdown.clone();
    }

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
        title,
        byline,
        excerpt,
        markdown,
        text,
        stats,
    })
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
