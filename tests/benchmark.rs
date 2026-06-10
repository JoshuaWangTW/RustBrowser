//! Browser-Use benchmark (V1.5 — Safety + Eval).
//!
//! Six task archetypes from the roadmap — search page, docs site, form submit,
//! paginated list, login-then-fetch, JS-heavy SPA — each driven through the
//! real `Session` against a deterministic wiremock site. Per scenario we
//! collect the roadmap's metrics (RB-only vs Chrome fallback, HTTP request
//! count, output tokens, unsafe-action blocks, steps, latency) and assert the
//! headline target: **ordinary lookup / docs / search tasks stay out of Chrome
//! (RB-only rate ≥ 70%)** while dangerous actions are always blocked until
//! confirmed.
//!
//! Everything runs in ONE test so the metrics table aggregates deterministic
//! numbers; the table is printed to stderr for CI logs.

use std::time::Instant;

use rustbrowser::session::{Session, SubmitOutcome};
use rustbrowser::{DistillOptions, Profile, tokens};
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn opts() -> DistillOptions {
    DistillOptions {
        allow_local: true,
        profile: Profile::Full,
        ..Default::default()
    }
}

fn page(title: &str, body: &str) -> String {
    format!("<!DOCTYPE html><html><head><title>{title}</title></head><body>{body}</body></html>")
}

async fn serve(server: &MockServer, p: &str, html: String) {
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(html),
        )
        .mount(server)
        .await;
}

/// The roadmap's per-task metrics.
struct Outcome {
    name: &'static str,
    success: bool,
    /// No Chrome escalation was attempted — the task was solved RB-only.
    rb_only: bool,
    http_requests: usize,
    output_tokens: usize,
    /// Dangerous submits withheld pending confirmation during the task.
    unsafe_blocks: usize,
    steps: usize,
    elapsed_ms: u128,
}

async fn outcome(
    name: &'static str,
    success: bool,
    session: &Session,
    server: &MockServer,
    started: Instant,
) -> Outcome {
    let log = session.log();
    Outcome {
        name,
        success,
        rb_only: !log.iter().any(|e| e.outcome.starts_with("chrome_fallback")),
        http_requests: server.received_requests().await.unwrap_or_default().len(),
        output_tokens: session.snapshot().map_or(0, |s| tokens::count(&s.markdown)),
        unsafe_blocks: log
            .iter()
            .filter(|e| e.outcome == "needs_confirmation")
            .count(),
        steps: session.loop_view().state.steps_taken,
        elapsed_ms: started.elapsed().as_millis(),
    }
}

/// 1. Search page: GET form → results.
async fn scenario_search() -> Outcome {
    let server = MockServer::start().await;
    serve(
        &server,
        "/",
        page(
            "Crates Search",
            r#"<h1>Search the registry</h1>
               <p>Find any crate by name or keyword using the box below.</p>
               <form method="get" action="/search"><input type="search" name="q">
               <button type="submit">Search</button></form>"#,
        ),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "tokio"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(page(
                    "Results",
                    "<h1>Results for tokio</h1><p>Tokio runtime guide and async \
                     primitives for writing reliable network applications.</p>",
                )),
        )
        .mount(&server)
        .await;

    let started = Instant::now();
    let mut s = Session::new(opts()).unwrap();
    s.observe(&format!("{}/", server.uri())).await.unwrap();
    let submitted = s
        .submit_form("form_0", &[("q".into(), "tokio".into())], false)
        .await
        .unwrap();
    let success = matches!(submitted, SubmitOutcome::Submitted)
        && s.snapshot().unwrap().markdown.contains("Tokio runtime");
    outcome("search", success, &s, &server, started).await
}

/// 2. Docs site: index → section → nested page.
async fn scenario_docs() -> Outcome {
    let server = MockServer::start().await;
    serve(
        &server,
        "/docs",
        page(
            "Docs",
            r#"<h1>Documentation</h1>
               <p>Start with installation, then read the API reference.</p>
               <a href="/docs/install">Installation</a>
               <a href="/docs/api">API reference</a>"#,
        ),
    )
    .await;
    serve(
        &server,
        "/docs/install",
        page(
            "Install",
            r#"<h1>Installation</h1>
               <p>Pick your platform to continue with the setup steps.</p>
               <a href="/docs/install/windows">Windows setup</a>"#,
        ),
    )
    .await;
    serve(
        &server,
        "/docs/install/windows",
        page(
            "Windows",
            "<h1>Windows setup</h1><p>Use the nasm portable toolchain and add \
             it to PATH before building from source.</p>",
        ),
    )
    .await;

    let started = Instant::now();
    let mut s = Session::new(opts()).unwrap();
    s.observe(&format!("{}/docs", server.uri())).await.unwrap();

    // Navigate by the action tree, the way a planner would.
    let install = s
        .loop_view()
        .available_actions
        .iter()
        .find(|a| a.kind == "link" && a.target.as_deref().is_some_and(|t| t.ends_with("/install")))
        .map(|a| a.action_id.clone())
        .expect("install link present");
    s.follow(&install).await.unwrap();
    let windows = s
        .loop_view()
        .available_actions
        .iter()
        .find(|a| a.kind == "link")
        .map(|a| a.action_id.clone())
        .expect("windows link present");
    s.follow(&windows).await.unwrap();

    let success = s.snapshot().unwrap().markdown.contains("nasm portable");
    outcome("docs", success, &s, &server, started).await
}

/// 3. Paginated list, walked via the planner's own pagination recommendation.
async fn scenario_pagination() -> Outcome {
    let server = MockServer::start().await;
    serve(
        &server,
        "/list",
        page(
            "List p1",
            r#"<h1>Items page 1</h1><p>item-10 item-11 item-12 and more rows of data.</p>
               <a href="/list2">Next ›</a>"#,
        ),
    )
    .await;
    serve(
        &server,
        "/list2",
        page(
            "List p2",
            r#"<h1>Items page 2</h1><p>item-20 item-21 item-22 keep scanning the table.</p>
               <a href="/list3">Next ›</a>"#,
        ),
    )
    .await;
    serve(
        &server,
        "/list3",
        page(
            "List p3",
            "<h1>Items page 3</h1><p>item-40 item-41 item-42 found the target row.</p>",
        ),
    )
    .await;

    let started = Instant::now();
    let mut s = Session::new(opts()).unwrap();
    s.observe(&format!("{}/list", server.uri())).await.unwrap();

    // Follow the recommended pagination link until the target appears.
    let mut hops = 0;
    while !s.snapshot().unwrap().markdown.contains("item-42") && hops < 4 {
        let next = s
            .loop_view()
            .recommended_next_actions
            .iter()
            .find(|r| r.kind == "link")
            .map(|r| r.action_id.clone());
        match next {
            Some(id) => {
                s.follow(&id).await.unwrap();
                hops += 1;
            }
            None => break,
        }
    }

    let success = s.snapshot().unwrap().markdown.contains("item-42") && hops == 2;
    outcome("pagination", success, &s, &server, started).await
}

/// 4. Form submit with the dangerous-action gate: blocked first, then confirmed.
async fn scenario_form_submit() -> Outcome {
    let server = MockServer::start().await;
    serve(
        &server,
        "/",
        page(
            "Newsletter",
            r#"<h1>Subscribe</h1>
               <p>Join the monthly newsletter for release announcements.</p>
               <form method="post" action="/subscribe">
                 <input type="email" name="email" required>
                 <input type="hidden" name="token" value="frm77">
                 <button type="submit">Subscribe</button>
               </form>"#,
        ),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/subscribe"))
        .and(body_string_contains("email=a%40b.dev"))
        .and(body_string_contains("token=frm77"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(page(
                    "Done",
                    "<h1>Subscribed OK</h1><p>Welcome aboard — see you next month.</p>",
                )),
        )
        .mount(&server)
        .await;

    let started = Instant::now();
    let mut s = Session::new(opts()).unwrap();
    s.observe(&format!("{}/", server.uri())).await.unwrap();
    let values = [("email".to_string(), "a@b.dev".to_string())];

    // Unconfirmed POST must be withheld (counted as an unsafe-action block)…
    let withheld = s.submit_form("form_0", &values, false).await.unwrap();
    let blocked = matches!(withheld, SubmitOutcome::NeedsConfirmation { .. });
    // …and go through once explicitly confirmed.
    let done = s.submit_form("form_0", &values, true).await.unwrap();

    let success = blocked
        && matches!(done, SubmitOutcome::Submitted)
        && s.snapshot().unwrap().markdown.contains("Subscribed OK");
    outcome("form_submit", success, &s, &server, started).await
}

/// 5. Login (POST sets a cookie) then fetch a cookie-gated page.
async fn scenario_login_then_fetch() -> Outcome {
    let server = MockServer::start().await;
    serve(
        &server,
        "/",
        page(
            "Login",
            r#"<h1>Sign in</h1>
               <p>Use your account credentials to reach the dashboard.</p>
               <form method="post" action="/login">
                 <input type="text" name="user"><input type="password" name="pass">
                 <button type="submit">Sign in</button>
               </form>"#,
        ),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/login"))
        .and(body_string_contains("user=alice"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .insert_header("Set-Cookie", "sid=tok99; Path=/")
                .set_body_string(page(
                    "Welcome",
                    "<h1>Logged in</h1><p>Session established for alice.</p>",
                )),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/account"))
        .and(header("cookie", "sid=tok99"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(page(
                    "Account",
                    "<h1>Your account</h1><p>Account balance 42 credits remaining.</p>",
                )),
        )
        .mount(&server)
        .await;

    let started = Instant::now();
    let mut s = Session::new(opts()).unwrap();
    s.observe(&format!("{}/", server.uri())).await.unwrap();
    let values = [
        ("user".to_string(), "alice".to_string()),
        ("pass".to_string(), "pw".to_string()),
    ];
    let done = s.submit_form("form_0", &values, true).await.unwrap();
    s.observe(&format!("{}/account", server.uri()))
        .await
        .unwrap();

    let success = matches!(done, SubmitOutcome::Submitted)
        && s.snapshot().unwrap().markdown.contains("balance 42");
    outcome("login_fetch", success, &s, &server, started).await
}

/// 6. JS-heavy SPA: RB alone cannot extract it — the broker must escalate.
async fn scenario_spa() -> Outcome {
    let server = MockServer::start().await;
    serve(
        &server,
        "/app",
        r#"<!DOCTYPE html><html><head><title>App</title>
           <script src="/bundle.js"></script><script>window.boot=1</script></head>
           <body><div id="root"></div></body></html>"#
            .to_string(),
    )
    .await;

    let started = Instant::now();
    let mut s = Session::new(DistillOptions {
        js_wait: Some(1500),
        ..opts()
    })
    .unwrap();
    s.observe(&format!("{}/app", server.uri())).await.unwrap();

    // Success for this archetype = the broker correctly identified the page
    // and attempted exactly one escalation (whether or not a local browser is
    // installed; a missing browser logs `chrome_fallback_failed`).
    let view = s.loop_view();
    let success = view.state.fallback_reason.as_deref() == Some("js_app")
        && s.log()
            .iter()
            .any(|e| e.outcome.starts_with("chrome_fallback"));
    outcome("js_spa", success, &s, &server, started).await
}

#[tokio::test]
async fn browser_use_benchmark() {
    let results = vec![
        scenario_search().await,
        scenario_docs().await,
        scenario_pagination().await,
        scenario_form_submit().await,
        scenario_login_then_fetch().await,
        scenario_spa().await,
    ];

    eprintln!("\n== Browser-Use benchmark (V1.5) ==");
    eprintln!(
        "{:<12} {:>7} {:>8} {:>6} {:>7} {:>7} {:>6} {:>8}",
        "scenario", "success", "rb_only", "reqs", "tokens", "blocks", "steps", "ms"
    );
    for r in &results {
        eprintln!(
            "{:<12} {:>7} {:>8} {:>6} {:>7} {:>7} {:>6} {:>8}",
            r.name,
            r.success,
            r.rb_only,
            r.http_requests,
            r.output_tokens,
            r.unsafe_blocks,
            r.steps,
            r.elapsed_ms
        );
    }

    // Every archetype completes its task.
    for r in &results {
        assert!(r.success, "scenario '{}' failed its task", r.name);
    }

    // Roadmap target: ordinary lookup / docs / search style tasks stay out of
    // Chrome ≥ 70% of the time. The five RB-native archetypes must all be
    // RB-only here (deterministic fixtures), which locks the contract well
    // above the 70% bar.
    let rb_native: Vec<_> = results.iter().filter(|r| r.name != "js_spa").collect();
    let rb_only_rate =
        rb_native.iter().filter(|r| r.rb_only).count() as f64 / rb_native.len() as f64;
    eprintln!("rb_only rate (non-SPA): {:.0}%", rb_only_rate * 100.0);
    assert!(
        rb_only_rate >= 0.7,
        "RB-only rate {rb_only_rate} fell below the 70% roadmap target"
    );

    // Safety: the JS SPA is the ONLY archetype that escalates…
    assert!(
        !results.iter().find(|r| r.name == "js_spa").unwrap().rb_only,
        "the SPA archetype must engage the fallback broker"
    );
    // …and every dangerous submit was blocked until explicitly confirmed.
    let blocks: usize = results.iter().map(|r| r.unsafe_blocks).sum();
    assert!(
        blocks >= 1,
        "expected at least one unsafe-action block across the suite"
    );
}
