//! Real-website regression tests, run nightly (not in PR CI — see
//! `.github/workflows/nightly-live.yml`). Every test is `#[ignore]`d so
//! `cargo test` stays hermetic; run these explicitly with:
//!
//! ```text
//! cargo test --test live -- --ignored --test-threads=2
//! ```
//!
//! These assert **shape invariants**, not page copy — target sites can reword
//! their content at any time without that being a RustBrowser regression.
//! A failure here means either a real regression or the target site being
//! unreachable/changed; nightly CI opens a `live-regression` issue either way
//! for a human to triage.

use std::time::Duration;

use rustbrowser::session::Session;
use rustbrowser::{DistillOptions, distill};

/// Distill options shared by every live test: no cache (always hit the real
/// site), a polite per-host rate limit, and token stats on so the token-bloat
/// regression test has something to assert against.
fn opts() -> DistillOptions {
    DistillOptions {
        use_cache: false,
        min_request_interval: Duration::from_secs(1),
        measure_tokens: true,
        ..Default::default()
    }
}

/// Baseline canary: if this fails, the network/CI runner is broken, not RB.
#[tokio::test]
#[ignore = "live network; run with --ignored"]
async fn live_baseline_canary() {
    let d = distill("https://example.com/", &opts())
        .await
        .expect("fetch example.com");
    assert!(!d.title.trim().is_empty(), "title should be non-empty");
    assert!(
        !d.markdown.trim().is_empty(),
        "markdown should be non-empty"
    );
}

/// Regression lock for the v1.6 fix that made `text/markdown` / `text/plain`
/// responses pass through unchanged instead of being mangled by the HTML
/// pipeline (which escaped literal `#`/`_` and inflated token counts).
///
/// Target: a tag-pinned raw.githubusercontent.com file — immutable content,
/// served as `text/plain`, so it always exercises the passthrough branch. The
/// original v1.6 issue URL (developers.openai.com/codex/codex-manual.md) now
/// serves `application/octet-stream` and no longer reaches passthrough — see
/// RB_FETCH_ISSUES.md (2026-08-14) — so it is unsuitable as a nightly lock.
#[tokio::test]
#[ignore = "live network; run with --ignored"]
async fn live_markdown_passthrough_is_not_inflated() {
    let start = std::time::Instant::now();
    let mut o = opts();
    o.diagnostics = true;
    let d = distill(
        "https://raw.githubusercontent.com/rust-lang/rust/1.70.0/README.md",
        &o,
    )
    .await
    .expect("fetch pinned rust README.md");

    let diagnostics = d
        .diagnostics
        .as_ref()
        .expect("diagnostics=true should populate diagnostics");
    assert!(
        !diagnostics.used_headless,
        "markdown passthrough must not use headless render"
    );
    assert!(
        !d.markdown.contains("\\#") && !d.markdown.contains("\\_"),
        "passthrough markdown must not double-escape '#' or '_'"
    );

    let stats = d.stats.expect("measure_tokens=true should populate stats");
    assert!(
        stats.output_tokens as f64 <= stats.raw_tokens as f64 * 1.02,
        "output_tokens ({}) should not exceed raw_tokens ({}) * 1.02 — pipeline is inflating passthrough content",
        stats.output_tokens,
        stats.raw_tokens
    );

    assert!(
        start.elapsed() < Duration::from_secs(60),
        "fetch took {:?}, expected under 60s",
        start.elapsed()
    );
}

/// MDN articles should distill down to a lean, substantial extract.
#[tokio::test]
#[ignore = "live network; run with --ignored"]
async fn live_mdn_article_distills_lean() {
    let mut o = opts();
    o.measure_tokens = true;
    let d = distill(
        "https://developer.mozilla.org/en-US/docs/Web/HTTP/Methods",
        &o,
    )
    .await
    .expect("fetch MDN HTTP Methods article");

    assert!(!d.title.trim().is_empty(), "title should be non-empty");
    assert!(
        d.text.len() > 500,
        "distilled text should be substantial, got {} chars",
        d.text.len()
    );

    let stats = d.stats.expect("measure_tokens=true should populate stats");
    assert!(
        stats.saved_ratio > 0.5,
        "saved_ratio ({}) should exceed 0.5 on a real article page",
        stats.saved_ratio
    );
}

/// docs.rs crate pages expose an action tree whose link hrefs are already
/// absolute (no bare relative paths an agent can't resolve).
#[tokio::test]
#[ignore = "live network; run with --ignored"]
async fn live_docs_rs_actions_are_absolute() {
    let mut o = opts();
    o.extract_actions = true;
    let d = distill("https://docs.rs/reqwest/latest/reqwest/", &o)
        .await
        .expect("fetch docs.rs/reqwest");

    let actions = d
        .actions
        .expect("extract_actions=true should populate actions");
    assert!(
        !actions.links.is_empty(),
        "docs.rs page should expose nav links"
    );
    for link in &actions.links {
        assert!(
            link.href.starts_with("http://") || link.href.starts_with("https://"),
            "link href should be absolute, got: {}",
            link.href
        );
    }
}

/// A real session drives observe -> follow across a redirect-free docs.rs
/// navigation without needing the Chrome fallback.
#[tokio::test]
#[ignore = "live network; run with --ignored"]
async fn live_session_follow_on_real_site() {
    let mut s = Session::new(opts()).expect("session construction");

    let snap = s
        .observe("https://docs.rs/reqwest/latest/reqwest/")
        .await
        .expect("observe docs.rs/reqwest");
    let actions = snap
        .actions
        .as_ref()
        .expect("session snapshots always extract actions");
    // Follow the stable "All Items" rustdoc link instead of links[0]: the first
    // link is the site-chrome "Docs.rs" logo pointing at the homepage, whose
    // distilled content varies with the live release feed and can dip under the
    // js_app fallback threshold (flaked nightly 2026-08-19 / 2026-08-21).
    let link_id = actions
        .links
        .iter()
        .find(|l| l.href.ends_with("/all.html"))
        .map(|l| l.action_id.clone())
        .expect("docs.rs rustdoc page should expose an All Items link");

    let before_url = s
        .current_url()
        .expect("current_url set after observe")
        .to_string();

    s.follow(&link_id).await.expect("follow first link");

    let after_url = s.current_url().expect("current_url set after follow");
    assert_ne!(
        before_url, after_url,
        "current_url should change after follow"
    );
    assert_eq!(
        s.redirect_history().len(),
        2,
        "redirect_history should have one entry per completed step (observe + follow)"
    );
    assert!(
        s.last_fallback().is_none(),
        "docs.rs should not need the Chrome fallback, got: {:?}",
        s.last_fallback()
    );
}
