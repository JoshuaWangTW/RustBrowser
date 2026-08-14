//! v1.7 R4 smoke test: a login-gated, JS-rendered page renders as logged-in
//! in the Chrome fallback because the session's cookies are injected into the
//! isolated render profile.
//!
//! Requires a local Chrome/Chromium/Edge, so it's `#[ignore]`d like the live
//! tests; run explicitly with:
//!
//! ```text
//! cargo test --test smoke_login -- --ignored
//! ```
//!
//! The "site" is wiremock: POST /login sets a cookie; GET /dashboard serves a
//! thin JS shell whose script writes the dashboard content — logged-in
//! content only when the request carries the cookie, an equally thin
//! "please log in" shell otherwise. RB's HTTP snapshot of the shell is thin
//! enough to trigger the `js_app` fallback, and only a cookie-carrying render
//! can prove the injection worked.

use rustbrowser::session::{Session, SubmitOutcome};
use rustbrowser::{DistillOptions, JsMode};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LOGIN_PAGE: &str = r#"<!DOCTYPE html><html><head><title>Login</title></head>
<body><h1>Sign in</h1>
<p>Please sign in to view your dashboard content. Your dashboard shows your
recent orders, open invoices and account balance, and is only available to
authenticated users of this service. Enter your username and password below
to continue to the protected area.</p>
<form method="POST" action="/login">
  <input type="text" name="user">
  <input type="password" name="pass">
  <button type="submit">Sign in</button>
</form></body></html>"#;

/// Thin JS-app shell: the visible content only exists after script runs.
fn shell(script_text: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html><head><title>Dashboard</title>
<script src="/bundle.js"></script><script>window.boot=1</script></head>
<body><div id="root"></div>
<script>document.getElementById('root').textContent = {script_text};</script>
</body></html>"#
    )
}

#[tokio::test]
#[ignore = "requires a local Chrome; run with --ignored"]
async fn login_gated_js_page_renders_logged_in_via_cookie_fallback() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/login"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(LOGIN_PAGE),
        )
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/login"))
        .and(body_string_contains("user=alice"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .insert_header("Set-Cookie", "auth=s3cr3t; Path=/")
                .set_body_string(
                    "<html><body><h1>Logged in</h1><p>Welcome alice, session started.</p></body></html>",
                ),
        )
        .mount(&server)
        .await;

    // Cookie-carrying request wins (lower number = higher wiremock priority).
    Mock::given(method("GET"))
        .and(path("/dashboard"))
        .and(header("cookie", "auth=s3cr3t"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(shell(
                    "'SECRET-DASHBOARD-42: your private orders, invoices and account balance.'",
                )),
        )
        .with_priority(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/dashboard"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(shell("'PLEASE-LOG-IN to view this page.'")),
        )
        .mount(&server)
        .await;

    let mut s = Session::new(DistillOptions {
        allow_local: true,
        profile: rustbrowser::Profile::Full,
        js_mode: JsMode::Auto,
        js_wait: Some(5000),
        ..Default::default()
    })
    .expect("session construction");

    // 1. Observe the login page, find its form.
    let snap = s
        .observe(&format!("{}/login", server.uri()))
        .await
        .expect("observe login page");
    let form_id = snap
        .actions
        .as_ref()
        .and_then(|a| a.forms.first())
        .map(|f| f.action_id.clone())
        .expect("login page should expose its form");

    // 2. Confirmed POST login: the session's jar now holds auth=s3cr3t.
    let values = [
        ("user".to_string(), "alice".to_string()),
        ("pass".to_string(), "secret".to_string()),
    ];
    let done = s
        .submit_form(&form_id, &values, true)
        .await
        .expect("confirmed login submit");
    assert!(matches!(done, SubmitOutcome::Submitted));

    // 3. Idempotent observe of the JS-shell dashboard: the broker escalates
    //    (js_app) and the render must carry the session cookie.
    s.observe(&format!("{}/dashboard", server.uri()))
        .await
        .expect("observe dashboard");

    let view = s.loop_view();
    assert!(
        view.state.fallback_reason.is_some(),
        "thin JS shell should trigger the fallback broker, got none"
    );
    assert!(
        view.state.used_headless,
        "dashboard snapshot should come from the headless render"
    );

    let markdown = &s.snapshot().expect("dashboard snapshot").markdown;
    assert!(
        markdown.contains("SECRET-DASHBOARD-42"),
        "rendered dashboard must show logged-in content (cookie reached Chrome); got:\n{markdown}"
    );
    assert!(
        !markdown.contains("PLEASE-LOG-IN"),
        "rendered dashboard must not be the anonymous variant"
    );
}
