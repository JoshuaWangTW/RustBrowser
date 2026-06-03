//! Main-content extraction (Readability-style) via dom_smoothie.
//!
//! Scores the DOM, finds the main article container, and strips nav bars,
//! ads, footers, and scripts — returning just the meaningful content. This
//! is where the bulk of the token reduction happens before we even convert
//! to Markdown.

use anyhow::{Context, Result};
use dom_smoothie::{Article, Config, Readability};

/// Cleaned, structured result of extraction.
#[derive(Debug, Clone)]
pub struct Extracted {
    pub title: String,
    pub byline: Option<String>,
    pub excerpt: Option<String>,
    /// Cleaned main-content HTML (ready for Markdown conversion).
    pub content_html: String,
    /// Plain-text rendering of the content.
    pub text: String,
}

/// Extract the main article from a full HTML document.
///
/// `url` is the document's URL, used to resolve relative links while scoring
/// and cleaning the DOM.
pub fn extract(html: &str, url: &str) -> Result<Extracted> {
    let cfg = Config::default();
    let mut readability =
        Readability::new(html, Some(url), Some(cfg)).context("initialising readability parser")?;
    let article: Article = readability.parse().context("extracting main content")?;

    Ok(Extracted {
        title: article.title.to_string(),
        byline: article.byline.map(|b| b.to_string()),
        excerpt: article.excerpt.map(|e| e.to_string()),
        content_html: article.content.to_string(),
        text: article.text_content.to_string(),
    })
}
