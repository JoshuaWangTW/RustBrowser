//! Main-content extraction (Readability-style) via dom_smoothie.
//!
//! Scores the DOM, finds the main article container, and strips nav bars,
//! ads, footers, and scripts — returning just the meaningful content. This
//! is where the bulk of the token reduction happens before we even convert
//! to Markdown.

use anyhow::{Context, Result};
use dom_smoothie::{Article, Config, Readability};
use scraper::{Html, Selector};

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

/// Extract the main article from a full HTML document (Readability).
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

/// Extract the whole `<body>` with `<script>`/`<style>`/`<noscript>`/`<template>`
/// removed, but no Readability filtering. Use this when Readability over-strips
/// — common on reference docs, landing pages, and structured layouts.
///
/// `text` is intentionally left empty; the Markdown rendering serves as the
/// plain-text fallback (so we don't surface script source as "text").
pub fn extract_whole_body(html: &str) -> Result<Extracted> {
    let doc = Html::parse_document(html);
    let title = doc
        .select(&title_selector())
        .next()
        .map(|t| collapse_ws(&t.text().collect::<String>()))
        .unwrap_or_default();
    let body_html = doc
        .select(&body_selector())
        .next()
        .map(|b| b.inner_html())
        .unwrap_or_else(|| html.to_string());

    Ok(Extracted {
        title,
        byline: None,
        excerpt: None,
        content_html: strip_noise(&body_html),
        text: String::new(),
    })
}

fn title_selector() -> Selector {
    Selector::parse("title").expect("static selector")
}

fn body_selector() -> Selector {
    Selector::parse("body").expect("static selector")
}

/// Collapse all runs of whitespace into single spaces and trim.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Noise elements whose contents are token waste with no readable value.
const NOISE_TAGS: [&str; 4] = ["script", "style", "noscript", "template"];

/// Remove noise elements (and their contents) from an HTML fragment. A simple
/// tag-aware scan — robust enough for `script`/`style`/`noscript`/`template`,
/// which never legitimately nest the same tag.
fn strip_noise(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        let next = NOISE_TAGS
            .iter()
            .filter_map(|tag| find_open(&lower, i, tag).map(|pos| (pos, *tag)))
            .min_by_key(|(pos, _)| *pos);

        match next {
            Some((pos, tag)) => {
                out.push_str(&html[i..pos]);
                let close = format!("</{tag}");
                match lower[pos..].find(&close) {
                    Some(crel) => {
                        let after_close = pos + crel;
                        i = match lower[after_close..].find('>') {
                            Some(gt) => after_close + gt + 1,
                            None => html.len(),
                        };
                    }
                    // Unterminated noise element → drop the remainder.
                    None => break,
                }
            }
            None => {
                out.push_str(&html[i..]);
                break;
            }
        }
    }
    out
}

/// Find the next real `<tag` opening at or after `from` (followed by whitespace,
/// `>`, or `/` so `<tablet` doesn't match `<table`).
fn find_open(lower: &str, from: usize, tag: &str) -> Option<usize> {
    let needle = format!("<{tag}");
    let mut at = from;
    while let Some(rel) = lower[at..].find(&needle) {
        let pos = at + rel;
        let after = pos + needle.len();
        match lower[after..].chars().next() {
            Some(c) if c.is_whitespace() || c == '>' || c == '/' => return Some(pos),
            None => return Some(pos),
            _ => at = after,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_noise_removes_scripts_and_styles() {
        let html =
            r#"<p>keep</p><script>var x = 1;</script><style>.a{color:red}</style><p>also</p>"#;
        let out = strip_noise(html);
        assert!(out.contains("keep") && out.contains("also"));
        assert!(!out.contains("var x"));
        assert!(!out.contains("color:red"));
    }

    #[test]
    fn strip_noise_handles_unterminated_element() {
        let out = strip_noise("<p>keep</p><script>oops no close tag");
        assert!(out.contains("keep"));
        assert!(!out.contains("oops"));
    }

    #[test]
    fn strip_noise_leaves_similar_tags_intact() {
        // Only exact noise tags are removed; tables/sections survive untouched.
        let html = "<table><tr><td>cell</td></tr></table>";
        assert_eq!(strip_noise(html), html);
    }

    #[test]
    fn whole_body_keeps_content_and_drops_scripts() {
        let html = "<html><head><title>  Doc  Title </title></head>\
                    <body><h1>Head</h1><p>Body text here.</p>\
                    <script>track()</script></body></html>";
        let ex = extract_whole_body(html).unwrap();
        assert_eq!(ex.title, "Doc Title"); // whitespace collapsed
        assert!(ex.content_html.contains("Head"));
        assert!(ex.content_html.contains("Body text here."));
        assert!(!ex.content_html.contains("track()"));
    }
}
