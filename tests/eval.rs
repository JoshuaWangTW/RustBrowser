//! Fixed extraction-quality eval set.
//!
//! Runs the distillation pipeline over a handful of hand-built fixtures (no
//! network — via `distill_html`) and asserts quality properties: the right main
//! content survives, chrome is stripped, profiles behave distinctly, and token
//! savings clear a floor. This is the regression net for extraction quality.

use rustbrowser::{DistillOptions, Profile, distill_html, tokens};

const ARTICLE: &str = include_str!("fixtures/article.html");
const DOCS: &str = include_str!("fixtures/docs.html");
const NOISY: &str = include_str!("fixtures/noisy.html");

fn opts(profile: Profile) -> DistillOptions {
    DistillOptions {
        profile,
        measure_tokens: true,
        diagnostics: true,
        ..Default::default()
    }
}

#[test]
fn article_keeps_main_content_and_strips_chrome() {
    let d = distill_html(
        ARTICLE,
        "https://example.com/blog/sourdough",
        &opts(Profile::Article),
    )
    .unwrap();

    assert!(
        d.title.to_lowercase().contains("sourdough"),
        "title: {}",
        d.title
    );
    // Body survives…
    assert!(d.markdown.contains("wild yeast"));
    assert!(d.markdown.contains("open crumb"));
    // …chrome is gone.
    assert!(!d.markdown.contains("SUBSCRIBE NOW"), "ad leaked");
    assert!(!d.markdown.contains("Sign in"), "nav leaked");
    assert!(!d.markdown.contains("All rights reserved"), "footer leaked");
    assert!(!d.markdown.contains("analytics beacon"), "script leaked");

    let s = d.stats.expect("stats requested");
    assert!(s.saved_ratio > 0.25, "weak savings: {:.3}", s.saved_ratio);
}

#[test]
fn full_profile_keeps_sidebar_that_article_drops() {
    let base = "https://example.com/docs/config";
    let article = distill_html(DOCS, base, &opts(Profile::Article)).unwrap();
    let full = distill_html(DOCS, base, &opts(Profile::Full)).unwrap();

    // The page title is recovered (Readability lifts the <h1> into the title).
    assert!(
        article.title.contains("Configuration Reference"),
        "title: {}",
        article.title
    );
    // Real body content survives in both profiles.
    assert!(article.markdown.contains("Environment overrides"));
    assert!(article.markdown.contains("workers"));
    assert!(full.markdown.contains("Environment overrides"));
    // The code block content is preserved.
    assert!(full.markdown.contains("8080"));

    // The <nav> sidebar is chrome to Readability but kept by the full profile.
    assert!(
        !article.markdown.contains("Sidebar link"),
        "article should drop the nav sidebar"
    );
    assert!(
        full.markdown.contains("Sidebar link"),
        "full profile should keep the whole body"
    );
}

#[test]
fn metadata_profile_is_a_short_summary() {
    let base = "https://example.com/blog/sourdough";
    let meta = distill_html(ARTICLE, base, &opts(Profile::Metadata)).unwrap();
    let article = distill_html(ARTICLE, base, &opts(Profile::Article)).unwrap();

    assert!(meta.title.to_lowercase().contains("sourdough"));
    assert!(meta.markdown.to_lowercase().contains("yeast"));
    // A summary is much smaller than the full article extraction.
    assert!(
        meta.markdown.len() < article.markdown.len(),
        "metadata ({}) should be shorter than article ({})",
        meta.markdown.len(),
        article.markdown.len()
    );
}

#[test]
fn noisy_page_strips_boilerplate_with_high_savings() {
    let d = distill_html(
        NOISY,
        "https://news.example.com/library",
        &opts(Profile::Article),
    )
    .unwrap();

    assert!(d.title.to_lowercase().contains("library"));
    // The full multi-paragraph article is extracted (first and last paragraphs).
    assert!(d.markdown.contains("weekend"));
    assert!(d.markdown.contains("shift workers"));
    // The high-link-density chrome — footer nav + trending aside — is dropped.
    assert!(!d.markdown.contains("News Wire Holdings"), "footer leaked");
    assert!(!d.markdown.contains("Trending"), "trending aside leaked");

    // Even if a stray promo line survives, the bulk of the page is stripped.
    let s = d.stats.unwrap();
    assert!(
        s.saved_ratio > 0.5,
        "noisy page should save a lot: {:.3}",
        s.saved_ratio
    );
}

#[test]
fn token_budget_truncates_and_flags_diagnostics() {
    let mut o = opts(Profile::Article);
    o.max_output_tokens = Some(20);
    let d = distill_html(ARTICLE, "https://example.com/blog/sourdough", &o).unwrap();

    assert!(
        d.markdown.contains("truncated"),
        "missing truncation marker"
    );
    assert!(
        tokens::count(&d.markdown) <= 20,
        "markdown exceeded token budget"
    );
    assert!(tokens::count(&d.text) <= 20, "text exceeded token budget");
    let diag = d.diagnostics.expect("diagnostics requested");
    assert!(diag.truncated, "diagnostics should flag truncation");
    assert_eq!(diag.profile, "article");
}

#[test]
fn every_fixture_yields_a_title_and_content() {
    for (name, html, base) in [
        ("article", ARTICLE, "https://example.com/a"),
        ("docs", DOCS, "https://example.com/d"),
        ("noisy", NOISY, "https://example.com/n"),
    ] {
        let d = distill_html(html, base, &opts(Profile::Article)).unwrap();
        assert!(!d.title.trim().is_empty(), "{name}: empty title");
        assert!(d.markdown.trim().len() > 80, "{name}: too little content");
        let diag = d.diagnostics.unwrap();
        assert!(
            !diag.low_content,
            "{name}: flagged low_content unexpectedly"
        );
    }
}
