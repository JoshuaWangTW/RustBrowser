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

const ACTIONABLE: &str = include_str!("fixtures/actionable.html");
const ACTIONABLE_BASE: &str = "https://lib.example.com/library";

fn observe_opts() -> DistillOptions {
    DistillOptions {
        extract_actions: true,
        diagnostics: true,
        ..Default::default()
    }
}

#[test]
fn observe_extracts_links_forms_buttons_downloads() {
    let d = distill_html(ACTIONABLE, ACTIONABLE_BASE, &observe_opts()).unwrap();
    let a = d.actions.expect("actions requested");

    // Downloads are split out from ordinary links.
    assert_eq!(a.downloads.len(), 2, "expected pdf + csv downloads");
    assert!(
        a.downloads
            .iter()
            .any(|x| x.filename.as_deref() == Some("q1-report.pdf"))
    );
    assert!(
        a.downloads
            .iter()
            .any(|x| x.filename.as_deref() == Some("library.csv"))
    );

    // Nav/pagination/footer links survive; the file links do not appear here.
    assert!(a.links.iter().any(|l| l.href.ends_with("/account")));
    assert!(a.links.iter().any(|l| l.text == "Next"));
    assert!(
        !a.links.iter().any(|l| l.href.contains(".pdf")),
        "download leaked into links"
    );
    assert_eq!(a.links[0].action_id, "link_0");

    // Two submittable forms with correct method, action, fields, submit id.
    assert_eq!(a.forms.len(), 2);
    let search = &a.forms[0];
    assert_eq!(search.method, "GET");
    assert!(search.action.ends_with("/search"));
    assert_eq!(search.submit_id, "form_0.submit");
    assert!(search.fields.iter().any(|f| f.name == "q"));
    assert!(
        search.fields.iter().any(|f| {
            f.kind == "select"
                && f.options
                    .iter()
                    .map(|o| o.value.as_str())
                    .collect::<Vec<_>>()
                    == vec!["all", "reports", "guides"]
                && f.options
                    .iter()
                    .map(|o| o.label.as_str())
                    .collect::<Vec<_>>()
                    == vec!["All", "Reports", "Guides"]
        }),
        "select options not captured"
    );

    let login = &a.forms[1];
    assert_eq!(login.method, "POST");
    assert!(login.action.ends_with("/login"));
    assert_eq!(login.submit_id, "form_1.submit");
    assert!(
        login
            .fields
            .iter()
            .any(|f| f.name == "csrf_token" && f.value.as_deref() == Some("tok_9f8e")),
        "hidden csrf value not captured"
    );
    assert!(
        login
            .fields
            .iter()
            .any(|f| f.name == "password" && f.kind == "password")
    );

    // Only the standalone button — form submit buttons belong to their forms.
    assert_eq!(a.buttons.len(), 1);
    assert_eq!(a.buttons[0].text, "Load more results");

    // Diagnostics action_count matches the tree total.
    assert_eq!(d.diagnostics.unwrap().action_count, a.len());
}

#[test]
fn action_ids_are_stable_across_runs() {
    let first = distill_html(ACTIONABLE, ACTIONABLE_BASE, &observe_opts())
        .unwrap()
        .actions
        .unwrap();
    let second = distill_html(ACTIONABLE, ACTIONABLE_BASE, &observe_opts())
        .unwrap()
        .actions
        .unwrap();
    let ids = |t: &rustbrowser::actions::ActionTree| -> Vec<String> {
        t.links.iter().map(|l| l.action_id.clone()).collect()
    };
    assert_eq!(ids(&first), ids(&second));
    assert_eq!(first.forms[0].submit_id, "form_0.submit");
}

#[test]
fn max_actions_caps_the_tree() {
    let mut o = observe_opts();
    o.max_actions = Some(2);
    let a = distill_html(ACTIONABLE, ACTIONABLE_BASE, &o)
        .unwrap()
        .actions
        .unwrap();
    assert!(a.links.len() <= 2, "links not capped: {}", a.links.len());
    assert!(a.forms.len() <= 2);
    assert!(a.downloads.len() <= 2);
}
