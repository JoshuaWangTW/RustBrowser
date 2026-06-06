//! End-to-end session flow against a wiremock server: observe → follow link →
//! submit GET form → submit POST form (with the confirmation gate) → cookie
//! persistence. Drives the real `Session` over a loopback mock.

use rustbrowser::DistillOptions;
use rustbrowser::session::{Session, SubmitOutcome};
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HOME: &str = r#"<!DOCTYPE html><html><head><title>Home</title></head><body>
  <h1>Welcome</h1>
  <p>This is the home page with enough text for a clean snapshot.</p>
  <a href="/page2">Go to page two</a>
  <form method="get" action="/search">
    <input type="search" name="q">
    <button type="submit">Search</button>
  </form>
  <form method="post" action="/login">
    <input type="text" name="user">
    <input type="password" name="pass">
    <input type="hidden" name="csrf" value="tok42">
    <button type="submit">Log in</button>
  </form>
</body></html>"#;

/// Session options that can reach the loopback mock and use the `full` profile
/// (no readability minimum-content threshold).
fn opts() -> DistillOptions {
    DistillOptions {
        allow_local: true,
        profile: rustbrowser::Profile::Full,
        ..Default::default()
    }
}

async fn home(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .insert_header("Set-Cookie", "sid=abc123; Path=/")
                .set_body_string(HOME),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn observe_extracts_actions_and_follows_link() {
    let server = MockServer::start().await;
    home(&server).await;
    Mock::given(method("GET"))
        .and(path("/page2"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(
                    "<html><body><h1>Page Two</h1><p>Second page body.</p></body></html>",
                ),
        )
        .mount(&server)
        .await;

    let mut s = Session::new(opts()).unwrap();
    let snap = s.observe(&format!("{}/", server.uri())).await.unwrap();
    let actions = snap.actions.as_ref().expect("action tree");
    assert_eq!(actions.links.len(), 1, "one nav link");
    assert_eq!(actions.forms.len(), 2, "search + login forms");

    // Follow the link by its stable id.
    s.follow("link_0").await.unwrap();
    assert!(s.current_url().unwrap().ends_with("/page2"));
    assert!(s.snapshot().unwrap().markdown.contains("Page Two"));
}

#[tokio::test]
async fn get_form_submits_as_query() {
    let server = MockServer::start().await;
    home(&server).await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "rustlang"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(
                    "<html><body><h1>Results</h1><p>Found rustlang.</p></body></html>",
                ),
        )
        .mount(&server)
        .await;

    let mut s = Session::new(opts()).unwrap();
    s.observe(&format!("{}/", server.uri())).await.unwrap();
    let outcome = s
        .submit_form("form_0", &[("q".into(), "rustlang".into())], false)
        .await
        .unwrap();
    assert!(matches!(outcome, SubmitOutcome::Submitted));
    assert!(s.current_url().unwrap().contains("q=rustlang"));
    assert!(s.snapshot().unwrap().markdown.contains("Results"));
}

#[tokio::test]
async fn post_form_needs_confirmation_then_submits() {
    let server = MockServer::start().await;
    home(&server).await;
    Mock::given(method("POST"))
        .and(path("/login"))
        .and(body_string_contains("user=alice"))
        .and(body_string_contains("csrf=tok42")) // hidden field carried automatically
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(
                    "<html><body><h1>Logged in</h1><p>Welcome alice.</p></body></html>",
                ),
        )
        .mount(&server)
        .await;

    let mut s = Session::new(opts()).unwrap();
    s.observe(&format!("{}/", server.uri())).await.unwrap();

    // Without confirmation, the POST is described but NOT sent.
    let values = [
        ("user".to_string(), "alice".to_string()),
        ("pass".to_string(), "secret".to_string()),
    ];
    let withheld = s.submit_form("form_1", &values, false).await.unwrap();
    match withheld {
        SubmitOutcome::NeedsConfirmation { method, fields, .. } => {
            assert_eq!(method, "POST");
            assert!(fields.iter().any(|(k, v)| k == "csrf" && v == "tok42"));
            assert!(fields.iter().any(|(k, v)| k == "user" && v == "alice"));
        }
        SubmitOutcome::Submitted => panic!("POST must not auto-submit without confirm"),
    }
    // The snapshot must be unchanged (still the home page).
    assert!(s.snapshot().unwrap().markdown.contains("Welcome"));

    // With confirmation, it goes through.
    let done = s.submit_form("form_1", &values, true).await.unwrap();
    assert!(matches!(done, SubmitOutcome::Submitted));
    assert!(s.snapshot().unwrap().markdown.contains("Logged in"));
}

#[tokio::test]
async fn post_redirect_does_not_forward_body_cross_origin() {
    let source = MockServer::start().await;
    let sink = MockServer::start().await;
    home(&source).await;

    Mock::given(method("POST"))
        .and(path("/login"))
        .respond_with(
            ResponseTemplate::new(307).insert_header("Location", format!("{}/steal", sink.uri())),
        )
        .mount(&source)
        .await;
    Mock::given(method("POST"))
        .and(path("/steal"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string("<html><body><h1>Leaked</h1></body></html>"),
        )
        .mount(&sink)
        .await;

    let mut s = Session::new(opts()).unwrap();
    s.observe(&format!("{}/", source.uri())).await.unwrap();
    let values = [
        ("user".to_string(), "alice".to_string()),
        ("pass".to_string(), "secret".to_string()),
    ];

    let err = s
        .submit_form("form_1", &values, true)
        .await
        .expect_err("cross-origin POST redirect must be blocked");

    assert!(
        err.to_string()
            .contains("cross-origin POST redirect blocked")
    );
    assert!(
        s.snapshot().unwrap().markdown.contains("Welcome"),
        "failed submit should leave the previous snapshot intact"
    );
    let received = sink.received_requests().await.expect("recording enabled");
    assert!(
        received.is_empty(),
        "form body was forwarded to redirected origin"
    );
}

#[tokio::test]
async fn failed_distill_does_not_advance_session_state() {
    let server = MockServer::start().await;
    home(&server).await;
    Mock::given(method("GET"))
        .and(path("/no-heading"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string("<html><body><p>No selected heading here.</p></body></html>"),
        )
        .mount(&server)
        .await;

    let mut s = Session::new(DistillOptions {
        allow_local: true,
        selector: Some("h1".to_string()),
        ..Default::default()
    })
    .unwrap();
    let first_url = format!("{}/", server.uri());
    s.observe(&first_url).await.unwrap();
    assert_eq!(s.current_url(), Some(first_url.as_str()));
    assert_eq!(s.redirect_history().len(), 1);
    assert!(s.snapshot().unwrap().markdown.contains("Welcome"));

    let err = s
        .observe(&format!("{}/no-heading", server.uri()))
        .await
        .expect_err("selector miss should fail");

    assert!(format!("{err:#}").contains("selector 'h1' matched no elements"));
    assert_eq!(s.current_url(), Some(first_url.as_str()));
    assert_eq!(s.redirect_history().len(), 1);
    assert!(s.snapshot().unwrap().markdown.contains("Welcome"));
}

#[tokio::test]
async fn cookies_persist_across_requests() {
    let server = MockServer::start().await;
    home(&server).await; // sets Set-Cookie: sid=abc123
    // /whoami only matches when the session replays the cookie.
    Mock::given(method("GET"))
        .and(path("/whoami"))
        .and(header("cookie", "sid=abc123"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string("<html><body><h1>You are abc123</h1></body></html>"),
        )
        .mount(&server)
        .await;

    let mut s = Session::new(opts()).unwrap();
    s.observe(&format!("{}/", server.uri())).await.unwrap(); // receives the cookie
    let snap = s
        .observe(&format!("{}/whoami", server.uri()))
        .await
        .unwrap();
    assert_eq!(snap.status, 200, "cookie was not replayed");
    assert!(snap.markdown.contains("abc123"));
}
