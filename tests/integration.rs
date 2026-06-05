//! End-to-end integration tests that drive the real fetch → extract → convert
//! pipeline against a local mock HTTP server (wiremock). These cover the parts
//! unit tests can't reach: actual HTTP, redirect following, gzip decompression,
//! charset decoding, the response-body byte cap, and the SSRF guard on redirect
//! targets.
//!
//! Every fetch sets `allow_local = true` so the pipeline may reach the mock
//! server on 127.0.0.1. The guard still blocks private LAN / link-local /
//! cloud-metadata even then — which is exactly what the redirect-SSRF test
//! relies on, and what `blocks_loopback_without_allow_local` proves is the
//! default.

use std::io::Write;
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::write::GzEncoder;
use rustbrowser::{DistillOptions, JsMode, distill, distill_many};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A realistic article with surrounding navigation/footer chrome that
/// readability should strip away, leaving just the main content.
const ARTICLE: &str = r#"<!DOCTYPE html>
<html>
<head><title>Test Article</title></head>
<body>
  <nav>HomeLink AboutLink ContactLink</nav>
  <article>
    <h1>The Main Headline</h1>
    <p>This is the first substantial paragraph of the article body, written long
    enough that the readability extractor keeps it as the primary content of the
    page rather than discarding it as boilerplate.</p>
    <p>A second paragraph continues the story with further meaningful prose, so
    the extractor stays confident that this block is the real article worth
    keeping in the distilled output.</p>
  </article>
  <footer>CopyrightFooter 2026</footer>
</body>
</html>"#;

/// Pipeline options that permit reaching the loopback mock server, with caching
/// off (hermetic tests) and headless rendering off (no Chrome in CI).
fn local_opts() -> DistillOptions {
    DistillOptions {
        use_cache: false,
        allow_local: true,
        js_mode: JsMode::Off,
        ..Default::default()
    }
}

/// Mount a single GET route returning the given response, then return the full
/// URL to it.
async fn mount_get(server: &MockServer, route: &str, resp: ResponseTemplate) -> String {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(resp)
        .mount(server)
        .await;
    format!("{}{route}", server.uri())
}

#[tokio::test]
async fn distills_basic_html_to_markdown() {
    let server = MockServer::start().await;
    let url = mount_get(
        &server,
        "/article",
        ResponseTemplate::new(200)
            .insert_header("Content-Type", "text/html; charset=utf-8")
            .set_body_string(ARTICLE),
    )
    .await;

    let d = distill(&url, &local_opts()).await.expect("distill ok");

    assert_eq!(d.status, 200);
    assert!(d.title.contains("Test Article"), "title: {}", d.title);
    assert!(d.markdown.contains("The Main Headline"));
    assert!(d.markdown.contains("first substantial paragraph"));
    // Navigation and footer chrome are stripped by readability.
    assert!(
        !d.markdown.contains("ContactLink"),
        "nav leaked: {}",
        d.markdown
    );
    assert!(
        !d.markdown.contains("CopyrightFooter"),
        "footer leaked: {}",
        d.markdown
    );
}

#[tokio::test]
async fn follows_relative_redirect_to_final_content() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/start",
        ResponseTemplate::new(302).insert_header("Location", "/final"),
    )
    .await;
    let final_url = mount_get(
        &server,
        "/final",
        ResponseTemplate::new(200)
            .insert_header("Content-Type", "text/html")
            .set_body_string(ARTICLE),
    )
    .await;

    let start_url = format!("{}/start", server.uri());
    let d = distill(&start_url, &local_opts())
        .await
        .expect("distill ok");

    assert!(
        d.final_url.ends_with("/final"),
        "final_url: {}",
        d.final_url
    );
    assert_eq!(d.final_url, final_url);
    assert!(d.markdown.contains("The Main Headline"));
}

#[tokio::test]
async fn refuses_redirect_to_metadata_ip() {
    let server = MockServer::start().await;
    // The mock tries to bounce us to the cloud-metadata endpoint — the classic
    // SSRF redirect. allow_local is true, yet 169.254.169.254 must still be
    // refused (allow_local frees only loopback).
    let url = mount_get(
        &server,
        "/evil",
        ResponseTemplate::new(302)
            .insert_header("Location", "http://169.254.169.254/latest/meta-data/"),
    )
    .await;

    let err = distill(&url, &local_opts())
        .await
        .expect_err("redirect to metadata must be refused");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("refus") || msg.contains("metadata") || msg.contains("private"),
        "unexpected error message: {msg}"
    );
}

#[tokio::test]
async fn decompresses_gzip_response() {
    let server = MockServer::start().await;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(ARTICLE.as_bytes()).expect("gzip write");
    let gzipped = enc.finish().expect("gzip finish");

    let url = mount_get(
        &server,
        "/gz",
        ResponseTemplate::new(200)
            .insert_header("Content-Type", "text/html; charset=utf-8")
            .insert_header("Content-Encoding", "gzip")
            .set_body_bytes(gzipped),
    )
    .await;

    let d = distill(&url, &local_opts()).await.expect("distill ok");
    assert!(
        d.markdown.contains("The Main Headline"),
        "gzip body not decoded: {}",
        d.markdown
    );
}

#[tokio::test]
async fn decodes_big5_charset() {
    let server = MockServer::start().await;
    // "<html><body><p>中文</p></body></html>" with 中文 encoded in Big5
    // (中 = A4 A4, 文 = A4 E5). A selector bypasses readability's minimum-content
    // threshold so the short body still produces output.
    let body: &[u8] = b"<html><body><p>\xa4\xa4\xa4\xe5</p></body></html>";
    let url = mount_get(
        &server,
        "/big5",
        ResponseTemplate::new(200).set_body_raw(body.to_vec(), "text/html; charset=big5"),
    )
    .await;

    let opts = DistillOptions {
        selector: Some("p".into()),
        ..local_opts()
    };
    let d = distill(&url, &opts).await.expect("distill ok");
    assert!(
        d.markdown.contains("中文"),
        "charset not decoded: {}",
        d.markdown
    );
}

#[tokio::test]
async fn respects_max_bytes_limit() {
    let server = MockServer::start().await;
    // A 50 KB body; the fetch must keep at most max_bytes of it.
    let big = format!("<html><body><p>{}</p></body></html>", "a".repeat(50_000));
    let url = mount_get(
        &server,
        "/big",
        ResponseTemplate::new(200)
            .insert_header("Content-Type", "text/html")
            .set_body_string(big),
    )
    .await;

    let opts = DistillOptions {
        selector: Some("p".into()),
        max_bytes: 1024,
        ..local_opts()
    };
    let d = distill(&url, &opts).await.expect("distill ok");
    // The retained content is far below the 50 KB original, but non-empty.
    assert!(
        d.markdown.len() > 100,
        "got nothing: len={}",
        d.markdown.len()
    );
    assert!(
        d.markdown.len() < 10_000,
        "body not capped: len={}",
        d.markdown.len()
    );
}

#[tokio::test]
async fn surfaces_http_404_status() {
    let server = MockServer::start().await;
    // A 4xx still returns its body — the pipeline surfaces the status rather
    // than treating it as a transport error.
    let url = mount_get(
        &server,
        "/missing",
        ResponseTemplate::new(404)
            .insert_header("Content-Type", "text/html")
            .set_body_string("<html><body><p>Not Found Here</p></body></html>"),
    )
    .await;

    let opts = DistillOptions {
        selector: Some("p".into()),
        ..local_opts()
    };
    let d = distill(&url, &opts)
        .await
        .expect("distill returns even on 404");
    assert_eq!(d.status, 404);
    assert!(d.markdown.contains("Not Found Here"));
}

#[tokio::test]
async fn blocks_loopback_without_allow_local() {
    let server = MockServer::start().await;
    let url = mount_get(
        &server,
        "/x",
        ResponseTemplate::new(200).set_body_string(ARTICLE),
    )
    .await;

    // Default options (allow_local = false) must refuse the loopback target,
    // proving the guard is on by default and allow_local is what opens it.
    let opts = DistillOptions {
        use_cache: false,
        js_mode: JsMode::Off,
        ..Default::default()
    };
    assert!(
        distill(&url, &opts).await.is_err(),
        "loopback should be blocked without allow_local"
    );
}

#[tokio::test]
async fn batch_fetches_multiple_urls_in_order() {
    let server = MockServer::start().await;
    let a = mount_get(
        &server,
        "/a",
        ResponseTemplate::new(200)
            .insert_header("Content-Type", "text/html")
            .set_body_string(ARTICLE),
    )
    .await;
    let b = mount_get(
        &server,
        "/b",
        ResponseTemplate::new(200)
            .insert_header("Content-Type", "text/html")
            .set_body_string(ARTICLE),
    )
    .await;

    let urls = vec![a.clone(), b.clone()];
    let results = distill_many(&urls, &local_opts(), 4).await;

    assert_eq!(results.len(), 2);
    // Input order is preserved.
    assert_eq!(results[0].0, a);
    assert_eq!(results[1].0, b);
    assert!(
        results.iter().all(|(_, r)| r.is_ok()),
        "a batch fetch failed"
    );
}

#[tokio::test]
async fn retries_transient_5xx_then_succeeds() {
    let server = MockServer::start().await;
    // First hit → 503 (served at most once, higher priority); after it is spent,
    // the 200 mock takes over. So a single retry turns failure into success.
    Mock::given(method("GET"))
        .and(path("/flaky"))
        .respond_with(
            ResponseTemplate::new(503)
                .insert_header("Content-Type", "text/html")
                .set_body_string("<html><body><p>temporarily down</p></body></html>"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/flaky"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(ARTICLE),
        )
        .with_priority(5)
        .mount(&server)
        .await;

    let url = format!("{}/flaky", server.uri());
    let d = distill(&url, &local_opts())
        .await
        .expect("retry should win");
    assert_eq!(d.status, 200);
    assert!(d.markdown.contains("The Main Headline"));
}

#[tokio::test]
async fn surfaces_final_status_after_exhausting_retries() {
    let server = MockServer::start().await;
    // Always 503 — after retries are spent, the status is surfaced (not an error).
    let url = mount_get(
        &server,
        "/down",
        ResponseTemplate::new(503)
            .insert_header("Content-Type", "text/html")
            .set_body_string("<html><body><p>still down</p></body></html>"),
    )
    .await;

    let opts = DistillOptions {
        selector: Some("p".into()),
        max_retries: 1,
        ..local_opts()
    };
    let d = distill(&url, &opts)
        .await
        .expect("503 is surfaced, not errored");
    assert_eq!(d.status, 503);
}

#[tokio::test]
async fn robots_txt_blocks_only_when_respected() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("User-agent: *\nDisallow: /secret"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/secret/page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(ARTICLE),
        )
        .mount(&server)
        .await;
    let url = format!("{}/secret/page", server.uri());

    // Default (respect_robots = false): the disallow is ignored, fetch succeeds.
    let d = distill(&url, &local_opts())
        .await
        .expect("ignored by default");
    assert_eq!(d.status, 200);

    // Opt in: the same path is now refused by robots.txt.
    let opts = DistillOptions {
        respect_robots: true,
        ..local_opts()
    };
    let err = distill(&url, &opts)
        .await
        .expect_err("respect_robots must block a Disallowed path");
    assert!(
        err.to_string().to_lowercase().contains("robots"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn rate_limit_spaces_same_host_requests() {
    let server = MockServer::start().await;
    let mut urls = Vec::new();
    for p in ["/r1", "/r2", "/r3"] {
        urls.push(
            mount_get(
                &server,
                p,
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(ARTICLE),
            )
            .await,
        );
    }

    // 100 ms between request starts → the third can't begin before ~200 ms.
    let opts = DistillOptions {
        min_request_interval: Duration::from_millis(100),
        ..local_opts()
    };
    let start = Instant::now();
    let results = distill_many(&urls, &opts, 8).await;
    let elapsed = start.elapsed();

    assert!(results.iter().all(|(_, r)| r.is_ok()));
    // Robust lower bound: rate limiting deterministically adds spacing, so this
    // can only fail if the limit was not applied at all.
    assert!(
        elapsed >= Duration::from_millis(150),
        "elapsed {elapsed:?} — rate limit not applied"
    );
}
