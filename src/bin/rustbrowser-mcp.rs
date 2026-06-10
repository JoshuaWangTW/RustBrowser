//! RustBrowser MCP server — exposes the distillation pipeline as MCP tools so
//! Claude Code can fetch web content natively, without raw HTML ever entering
//! the conversation. This is where the token savings really land.
//!
//! Transport is stdio: the MCP protocol speaks over stdout, so NOTHING may be
//! printed to stdout except protocol frames. All diagnostics go to stderr.

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars,
    service::QuitReason,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;

use rustbrowser::session::{Session, SubmitOutcome};
use rustbrowser::{DistillOptions, Distilled, JsMode, Profile, distill, distill_many};

/// One live browsing session, guarded so a single session's actions serialise.
type SharedSession = Arc<AsyncMutex<Session>>;

const MAX_LIVE_SESSIONS: usize = 64;

#[derive(Clone)]
struct RustBrowserServer {
    // Read by the `#[tool_handler]` macro's generated ServerHandler impl,
    // which the dead-code lint can't see through.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    /// Live browsing sessions, keyed by id. The map lock is held only briefly to
    /// look up / insert; per-session work locks the inner `AsyncMutex`.
    sessions: Arc<Mutex<HashMap<String, SharedSession>>>,
}

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

impl RustBrowserServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn next_session_id() -> String {
        format!("sess_{}", SESSION_COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    fn lookup_session(&self, id: &str) -> Result<SharedSession, rmcp::ErrorData> {
        self.sessions
            .lock()
            .expect("session map mutex poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| rmcp::ErrorData::invalid_params(format!("unknown session '{id}'"), None))
    }

    fn ensure_session_capacity(&self) -> Result<(), rmcp::ErrorData> {
        let len = self
            .sessions
            .lock()
            .expect("session map mutex poisoned")
            .len();
        if len >= MAX_LIVE_SESSIONS {
            return Err(rmcp::ErrorData::invalid_params(
                format!(
                    "too many live sessions; close one with session_close first (limit {MAX_LIVE_SESSIONS})"
                ),
                None,
            ));
        }
        Ok(())
    }

    fn insert_session(&self, id: String, session: Session) -> Result<(), rmcp::ErrorData> {
        let mut sessions = self.sessions.lock().expect("session map mutex poisoned");
        if sessions.len() >= MAX_LIVE_SESSIONS {
            return Err(rmcp::ErrorData::invalid_params(
                format!(
                    "too many live sessions; close one with session_close first (limit {MAX_LIVE_SESSIONS})"
                ),
                None,
            ));
        }
        sessions.insert(id, Arc::new(AsyncMutex::new(session)));
        Ok(())
    }

    fn close_session(&self, id: &str) -> Result<(), rmcp::ErrorData> {
        let removed = self
            .sessions
            .lock()
            .expect("session map mutex poisoned")
            .remove(id);
        if removed.is_some() {
            Ok(())
        } else {
            Err(rmcp::ErrorData::invalid_params(
                format!("unknown session '{id}'"),
                None,
            ))
        }
    }
}

/// How many recent operation-log entries to include in a session view.
const SESSION_LOG_TAIL: usize = 25;

/// Serialise a session's public state as JSON for return to the agent.
///
/// The original 1.x fields (`session_id`, `current_url`, `redirect_history`,
/// `snapshot`) are kept for compatibility; the Action-Loop view (`loop`) and the
/// debug `operation_log` are **added** on top (see docs/API.md).
fn session_view_value(id: &str, session: &Session) -> Value {
    json!({
        "session_id": id,
        "current_url": session.current_url(),
        "redirect_history": session.redirect_history(),
        "snapshot": session.snapshot(),
        "loop": session.loop_view(),
        "operation_log": session.recent_log(SESSION_LOG_TAIL),
    })
}

fn session_view(id: &str, session: &Session) -> Result<String, rmcp::ErrorData> {
    let view = session_view_value(id, session);
    serde_json::to_string_pretty(&view)
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))
}

fn session_confirmation_view(
    id: &str,
    session: &Session,
    method: String,
    action: String,
    fields: Vec<(String, String)>,
) -> Result<String, rmcp::ErrorData> {
    let mut view = session_view_value(id, session);
    let obj = view
        .as_object_mut()
        .expect("session_view_value always returns a JSON object");
    obj.insert("needs_confirmation".to_string(), json!(true));
    obj.insert(
        "message".to_string(),
        json!(
            "Dangerous (non-GET) submit withheld. Re-call session_submit_form with confirm=true to send it."
        ),
    );
    obj.insert(
        "would_submit".to_string(),
        json!({ "method": method, "action": action, "fields": fields }),
    );
    serde_json::to_string_pretty(&view)
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FetchParams {
    /// The URL to fetch and distill.
    url: String,
    /// Output format: "markdown" (default), "text", or "json".
    #[serde(default)]
    format: Option<String>,
    /// Optional CSS selector — extract only matching elements instead of
    /// running full-page readability.
    #[serde(default)]
    selector: Option<String>,
    /// Append token-savings stats (raw vs distilled) to the response.
    #[serde(default)]
    stats: Option<bool>,
    /// Request timeout in seconds (default 20).
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Maximum response bytes to keep before decoding (default 8 MiB).
    #[serde(default)]
    max_bytes: Option<usize>,
    /// Skip the on-disk cache and always fetch fresh.
    #[serde(default)]
    no_cache: Option<bool>,
    /// Cache freshness window in seconds (default 3600).
    #[serde(default)]
    cache_ttl: Option<u64>,
    /// Also extract all links from the main content as structured data.
    #[serde(default)]
    extract_links: Option<bool>,
    /// Also extract all tables from the main content as structured data.
    #[serde(default)]
    extract_tables: Option<bool>,
    /// Extract ALL links incl. nav/footer (whole page) instead of main content.
    #[serde(default)]
    links_full: Option<bool>,
    /// Headless JS rendering: "off", "auto" (default), or "always".
    #[serde(default)]
    js: Option<String>,
    /// Headless wait / virtual-time budget in milliseconds.
    #[serde(default)]
    js_wait: Option<u64>,
    /// Wait until this CSS selector appears before capturing (uses CDP).
    #[serde(default)]
    js_wait_for: Option<String>,
    /// Permit loopback/localhost targets (local dev servers). Only loopback is
    /// freed; private LAN, link-local, and cloud-metadata stay blocked.
    #[serde(default)]
    allow_local: Option<bool>,
    /// Retry transient failures (connect/timeout, 429, 5xx) this many times
    /// with exponential backoff (default 2).
    #[serde(default)]
    max_retries: Option<usize>,
    /// Max simultaneous requests to any single host (default 4; 0 = unlimited).
    #[serde(default)]
    per_host_concurrency: Option<usize>,
    /// Per-host rate limit in requests/second (default 0 = off).
    #[serde(default)]
    rate_limit: Option<f64>,
    /// Respect each host's robots.txt and skip disallowed paths (default false).
    #[serde(default)]
    respect_robots: Option<bool>,
    /// Content profile: "article" (default), "full" (whole body), or "metadata".
    #[serde(default)]
    profile: Option<String>,
    /// Truncate the Markdown/text output to fit this many tokens (default: no limit).
    #[serde(default)]
    max_output_tokens: Option<usize>,
    /// Attach extraction-quality diagnostics to the result (default false).
    #[serde(default)]
    diagnostics: Option<bool>,
    /// Extract the operable action tree (links/forms/buttons/downloads).
    #[serde(default)]
    extract_actions: Option<bool>,
    /// Cap each action category, form fields, and select options at this many
    /// entries (avoids action-tree blowup).
    #[serde(default)]
    max_actions: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FetchManyParams {
    /// The URLs to fetch — fetched concurrently.
    urls: Vec<String>,
    /// Output format: "markdown" (default), "text", or "json".
    #[serde(default)]
    format: Option<String>,
    /// Append token-savings stats to each page.
    #[serde(default)]
    stats: Option<bool>,
    /// Request timeout in seconds (default 20).
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Maximum response bytes to keep before decoding (default 8 MiB).
    #[serde(default)]
    max_bytes: Option<usize>,
    /// Skip the on-disk cache and always fetch fresh.
    #[serde(default)]
    no_cache: Option<bool>,
    /// Cache freshness window in seconds (default 3600).
    #[serde(default)]
    cache_ttl: Option<u64>,
    /// Max concurrent requests (default 8).
    #[serde(default)]
    concurrency: Option<usize>,
    /// Also extract all links from each page's main content.
    #[serde(default)]
    extract_links: Option<bool>,
    /// Also extract all tables from each page's main content.
    #[serde(default)]
    extract_tables: Option<bool>,
    /// Extract ALL links incl. nav/footer (whole page) instead of main content.
    #[serde(default)]
    links_full: Option<bool>,
    /// Headless JS rendering: "off", "auto" (default), or "always".
    #[serde(default)]
    js: Option<String>,
    /// Headless wait / virtual-time budget in milliseconds.
    #[serde(default)]
    js_wait: Option<u64>,
    /// Wait until this CSS selector appears before capturing (uses CDP).
    #[serde(default)]
    js_wait_for: Option<String>,
    /// Permit loopback/localhost targets (local dev servers). Only loopback is
    /// freed; private LAN, link-local, and cloud-metadata stay blocked.
    #[serde(default)]
    allow_local: Option<bool>,
    /// Retry transient failures (connect/timeout, 429, 5xx) this many times
    /// with exponential backoff (default 2).
    #[serde(default)]
    max_retries: Option<usize>,
    /// Max simultaneous requests to any single host (default 4; 0 = unlimited).
    #[serde(default)]
    per_host_concurrency: Option<usize>,
    /// Per-host rate limit in requests/second (default 0 = off).
    #[serde(default)]
    rate_limit: Option<f64>,
    /// Respect each host's robots.txt and skip disallowed paths (default false).
    #[serde(default)]
    respect_robots: Option<bool>,
    /// Content profile: "article" (default), "full" (whole body), or "metadata".
    #[serde(default)]
    profile: Option<String>,
    /// Truncate the Markdown/text output to fit this many tokens (default: no limit).
    #[serde(default)]
    max_output_tokens: Option<usize>,
    /// Attach extraction-quality diagnostics to each result (default false).
    #[serde(default)]
    diagnostics: Option<bool>,
    /// Extract the operable action tree (links/forms/buttons/downloads).
    #[serde(default)]
    extract_actions: Option<bool>,
    /// Cap each action category, form fields, and select options at this many
    /// entries (avoids action-tree blowup).
    #[serde(default)]
    max_actions: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SessionStartParams {
    /// The URL to open as the session's first page.
    url: String,
    /// Content profile: "article" (default), "full" (whole body), or "metadata".
    #[serde(default)]
    profile: Option<String>,
    /// Cap each action category, form fields, and select options at this many.
    #[serde(default)]
    max_actions: Option<usize>,
    /// Request timeout in seconds (default 20).
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Permit loopback/localhost targets (only loopback freed).
    #[serde(default)]
    allow_local: Option<bool>,
    /// Respect each host's robots.txt (default false).
    #[serde(default)]
    respect_robots: Option<bool>,
    /// Extra attempts for an idempotent step (observe/follow/GET submit) when it
    /// fails verification — a 429/5xx or transient transport error. Clamped to
    /// 0–2 (default 1). Non-GET submits are NEVER auto-retried.
    #[serde(default)]
    max_action_retries: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SessionObserveParams {
    /// The session id from session_start.
    session_id: String,
    /// The URL to navigate to in this session.
    url: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SessionFollowParams {
    /// The session id from session_start.
    session_id: String,
    /// A `link_*` or `download_*` action id from the last snapshot.
    action_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SessionCloseParams {
    /// The session id to close and forget.
    session_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SessionSubmitFormParams {
    /// The session id from session_start.
    session_id: String,
    /// A `form_*` action id from the last snapshot.
    form_id: String,
    /// Field name → value to submit (merged over the form's own defaults such as
    /// hidden CSRF fields and selected options).
    #[serde(default)]
    values: HashMap<String, String>,
    /// Required to actually send a non-GET (dangerous) submit. Without it, a
    /// POST/etc. is described but NOT sent.
    #[serde(default)]
    confirm: Option<bool>,
}

/// Build the per-session distill options. Sessions always observe the action
/// tree and run live (no cache).
fn session_opts(p: &SessionStartParams) -> DistillOptions {
    DistillOptions {
        timeout: Duration::from_secs(p.timeout_secs.unwrap_or(20)),
        profile: parse_profile(p.profile.as_deref()),
        max_actions: p.max_actions,
        allow_local: p.allow_local.unwrap_or(false),
        respect_robots: p.respect_robots.unwrap_or(false),
        extract_actions: true,
        diagnostics: true,
        use_cache: false,
        ..Default::default()
    }
}

fn parse_js_mode(js: Option<&str>) -> JsMode {
    match js {
        Some("off") => JsMode::Off,
        Some("always") => JsMode::Always,
        _ => JsMode::Auto,
    }
}

fn parse_profile(profile: Option<&str>) -> Profile {
    match profile {
        Some("full") => Profile::Full,
        Some("metadata") => Profile::Metadata,
        _ => Profile::Article,
    }
}

/// Build pipeline options from optional MCP parameters.
#[allow(clippy::too_many_arguments)]
fn opts_from(
    timeout_secs: Option<u64>,
    max_bytes: Option<usize>,
    selector: Option<String>,
    stats: Option<bool>,
    no_cache: Option<bool>,
    cache_ttl: Option<u64>,
    links: Option<bool>,
    tables: Option<bool>,
    links_full: Option<bool>,
    js: Option<&str>,
    js_wait: Option<u64>,
    js_wait_for: Option<String>,
    allow_local: Option<bool>,
    max_retries: Option<usize>,
    per_host_concurrency: Option<usize>,
    rate_limit: Option<f64>,
    respect_robots: Option<bool>,
    profile: Option<&str>,
    max_output_tokens: Option<usize>,
    diagnostics: Option<bool>,
    extract_actions: Option<bool>,
    max_actions: Option<usize>,
) -> DistillOptions {
    let links_full = links_full.unwrap_or(false);
    DistillOptions {
        timeout: Duration::from_secs(timeout_secs.unwrap_or(20)),
        user_agent: None,
        selector,
        measure_tokens: stats.unwrap_or(false),
        use_cache: !no_cache.unwrap_or(false),
        cache_ttl: cache_ttl.unwrap_or(3600),
        extract_links: links.unwrap_or(false) || links_full,
        extract_tables: tables.unwrap_or(false),
        links_full,
        js_mode: parse_js_mode(js),
        js_wait,
        js_wait_for,
        max_bytes: max_bytes.unwrap_or(8 * 1024 * 1024),
        allow_local: allow_local.unwrap_or(false),
        max_retries: max_retries.unwrap_or(2),
        per_host_concurrency: per_host_concurrency.unwrap_or(4),
        min_request_interval: rate_to_interval(rate_limit.unwrap_or(0.0)),
        respect_robots: respect_robots.unwrap_or(false),
        profile: parse_profile(profile),
        max_output_tokens,
        diagnostics: diagnostics.unwrap_or(false),
        extract_actions: extract_actions.unwrap_or(false),
        max_actions,
    }
}

/// Per-host requests/second → minimum spacing between requests (0 = off).
fn rate_to_interval(reqs_per_sec: f64) -> Duration {
    if reqs_per_sec.is_finite() && reqs_per_sec > 0.0 {
        Duration::from_secs_f64(1.0 / reqs_per_sec)
    } else {
        Duration::ZERO
    }
}

/// Render one distilled page in the requested format. Links/tables and stats
/// (when present) are appended for the human-readable formats; JSON carries
/// everything in the serialised result already.
fn render(result: &Distilled, fmt: &str) -> Result<String, rmcp::ErrorData> {
    if fmt == "json" {
        return serde_json::to_string_pretty(result)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None));
    }

    let mut out = if fmt == "text" {
        result.text.clone()
    } else {
        let mut s = String::new();
        if !result.title.is_empty() {
            s.push_str(&format!("# {}\n\n", result.title));
        }
        s.push_str(&result.markdown);
        s
    };

    if let Some(links) = &result.links {
        out.push_str(&format!("\n\n## Links ({})\n", links.len()));
        for l in links {
            let label = if l.text.is_empty() { &l.href } else { &l.text };
            out.push_str(&format!("\n- [{label}]({})", l.href));
        }
    }
    if let Some(tables) = &result.tables {
        for (i, t) in tables.iter().enumerate() {
            out.push_str(&format!("\n\n## Table {}\n\n", i + 1));
            if !t.headers.is_empty() {
                out.push_str(&format!("| {} |\n", t.headers.join(" | ")));
                let sep: Vec<&str> = t.headers.iter().map(|_| "---").collect();
                out.push_str(&format!("| {} |\n", sep.join(" | ")));
            }
            for row in &t.rows {
                out.push_str(&format!("| {} |\n", row.join(" | ")));
            }
        }
    }

    if let Some(a) = &result.actions {
        out.push_str(&format!("\n\n## Actions ({})\n", a.len()));
        for l in &a.links {
            out.push_str(&format!("\n- [{}] {} → {}", l.action_id, l.text, l.href));
        }
        for f in &a.forms {
            out.push_str(&format!(
                "\n- [{}] form {} {} ({} fields, submit={})",
                f.action_id,
                f.method,
                f.action,
                f.fields.len(),
                f.submit_id
            ));
        }
        for b in &a.buttons {
            out.push_str(&format!("\n- [{}] button: {}", b.action_id, b.text));
        }
        for d in &a.downloads {
            out.push_str(&format!(
                "\n- [{}] download: {} → {}",
                d.action_id, d.text, d.href
            ));
        }
    }

    if let Some(st) = &result.stats {
        out.push_str(&format!(
            "\n\n---\n_token stats: raw {} → output {} ({:.1}% saved)_",
            st.raw_tokens,
            st.output_tokens,
            st.saved_ratio * 100.0
        ));
    }

    Ok(out)
}

#[tool_router]
impl RustBrowserServer {
    #[tool(
        description = "Fetch a web page and return its MAIN CONTENT distilled to clean Markdown — navigation, ads, scripts, and boilerplate stripped out. Use this instead of fetching raw HTML whenever you need to read web content: it typically costs 75-98% fewer tokens than the raw page. Results are cached on disk for repeat fetches; JS-heavy pages are auto-rendered via headless Chrome. Optionally target a CSS selector, extract links/tables as structured data, or request plain text / JSON."
    )]
    async fn fetch_url(
        &self,
        Parameters(p): Parameters<FetchParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let opts = opts_from(
            p.timeout_secs,
            p.max_bytes,
            p.selector,
            p.stats,
            p.no_cache,
            p.cache_ttl,
            p.extract_links,
            p.extract_tables,
            p.links_full,
            p.js.as_deref(),
            p.js_wait,
            p.js_wait_for,
            p.allow_local,
            p.max_retries,
            p.per_host_concurrency,
            p.rate_limit,
            p.respect_robots,
            p.profile.as_deref(),
            p.max_output_tokens,
            p.diagnostics,
            p.extract_actions,
            p.max_actions,
        );
        let result = distill(&p.url, &opts)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(format!("fetch failed: {e}"), None))?;
        let fmt = p.format.as_deref().unwrap_or("markdown");
        render(&result, fmt)
    }

    #[tool(
        description = "OBSERVE a web page for Browser Use: returns its distilled content AND an action tree — the links, forms (with their fields and a submit id), standalone buttons, and downloads on the page, each tagged with a stable action_id (e.g. link_3, form_0.submit). Use this when you need to know what is OPERABLE on a page (to follow a link or submit a form next), not just read it. Always returns JSON; action categories, form fields, select options, and long scalar fields are capped to stay token-lean."
    )]
    async fn observe_url(
        &self,
        Parameters(p): Parameters<FetchParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let mut opts = opts_from(
            p.timeout_secs,
            p.max_bytes,
            p.selector,
            p.stats,
            p.no_cache,
            p.cache_ttl,
            p.extract_links,
            p.extract_tables,
            p.links_full,
            p.js.as_deref(),
            p.js_wait,
            p.js_wait_for,
            p.allow_local,
            p.max_retries,
            p.per_host_concurrency,
            p.rate_limit,
            p.respect_robots,
            p.profile.as_deref(),
            p.max_output_tokens,
            p.diagnostics,
            p.extract_actions,
            p.max_actions,
        );
        // Observe always surfaces the action tree (and diagnostics), as JSON.
        opts.extract_actions = true;
        opts.diagnostics = true;
        let result = distill(&p.url, &opts)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(format!("observe failed: {e}"), None))?;
        render(&result, "json")
    }

    #[tool(
        description = "Fetch MULTIPLE web pages concurrently and return all distilled contents at once. Use this when you need several pages in one go — far faster than calling fetch_url repeatedly, and still token-lean. Pages are separated by a divider; pass format=json for a structured array."
    )]
    async fn fetch_urls(
        &self,
        Parameters(p): Parameters<FetchManyParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let opts = opts_from(
            p.timeout_secs,
            p.max_bytes,
            None,
            p.stats,
            p.no_cache,
            p.cache_ttl,
            p.extract_links,
            p.extract_tables,
            p.links_full,
            p.js.as_deref(),
            p.js_wait,
            p.js_wait_for,
            p.allow_local,
            p.max_retries,
            p.per_host_concurrency,
            p.rate_limit,
            p.respect_robots,
            p.profile.as_deref(),
            p.max_output_tokens,
            p.diagnostics,
            p.extract_actions,
            p.max_actions,
        );
        let results = distill_many(&p.urls, &opts, p.concurrency.unwrap_or(8)).await;
        let fmt = p.format.as_deref().unwrap_or("markdown");

        if fmt == "json" {
            let arr: Vec<_> = results
                .iter()
                .map(|(url, r)| match r {
                    Ok(d) => json!({ "url": url, "ok": true, "result": d }),
                    Err(e) => json!({ "url": url, "ok": false, "error": e.to_string() }),
                })
                .collect();
            return serde_json::to_string_pretty(&arr)
                .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None));
        }

        let mut out = String::new();
        for (i, (url, r)) in results.iter().enumerate() {
            if i > 0 {
                out.push_str("\n\n═══════════════════════════════\n\n");
            }
            match r {
                Ok(d) => {
                    out.push_str(&format!("<!-- {url} -->\n"));
                    out.push_str(&render(d, fmt)?);
                }
                Err(e) => out.push_str(&format!("✗ {url}: {e}")),
            }
        }
        Ok(out)
    }

    #[tool(
        description = "START a stateful browsing SESSION (Browser Use). Opens a URL and keeps a cookie jar, the current URL, a redirect history, and the page snapshot with its action tree. Returns a session_id plus that snapshot. Drive the session afterwards with session_observe / session_follow / session_submit_form using the action_ids in the snapshot. Cookies persist, so login and search flows work without a real browser."
    )]
    async fn session_start(
        &self,
        Parameters(p): Parameters<SessionStartParams>,
    ) -> Result<String, rmcp::ErrorData> {
        self.ensure_session_capacity()?;
        let mut session = Session::new(session_opts(&p))
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        if let Some(n) = p.max_action_retries {
            session = session.with_max_action_retries(n);
        }
        session.observe(&p.url).await.map_err(|e| {
            rmcp::ErrorData::internal_error(format!("session start failed: {e}"), None)
        })?;
        let id = Self::next_session_id();
        let view = session_view(&id, &session)?;
        self.insert_session(id, session)?;
        Ok(view)
    }

    #[tool(
        description = "Navigate an existing SESSION to a new URL (keeps its cookies). Returns the updated snapshot + action tree."
    )]
    async fn session_observe(
        &self,
        Parameters(p): Parameters<SessionObserveParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let shared = self.lookup_session(&p.session_id)?;
        let mut session = shared.lock().await;
        session
            .observe(&p.url)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(format!("observe failed: {e}"), None))?;
        session_view(&p.session_id, &session)
    }

    #[tool(
        description = "FOLLOW a link (or download) in a SESSION by its action_id (e.g. link_3) from the last snapshot. Returns the updated snapshot + action tree."
    )]
    async fn session_follow(
        &self,
        Parameters(p): Parameters<SessionFollowParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let shared = self.lookup_session(&p.session_id)?;
        let mut session = shared.lock().await;
        session
            .follow(&p.action_id)
            .await
            .map_err(|e| rmcp::ErrorData::invalid_params(format!("follow failed: {e}"), None))?;
        session_view(&p.session_id, &session)
    }

    #[tool(
        description = "Close a SESSION and forget its cookies, current URL, history, and snapshot."
    )]
    async fn session_close(
        &self,
        Parameters(p): Parameters<SessionCloseParams>,
    ) -> Result<String, rmcp::ErrorData> {
        self.close_session(&p.session_id)?;
        let view = json!({
            "session_id": p.session_id,
            "closed": true,
        });
        serde_json::to_string_pretty(&view)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))
    }

    #[tool(
        description = "SUBMIT a form in a SESSION by its form_id (e.g. form_0), merging your field values over the form's own defaults (hidden CSRF fields, selected options). GET forms (search/filter) submit immediately. A non-GET (POST/etc.) form is DANGEROUS: it is only described, not sent, unless you pass confirm=true — RB never silently submits on your behalf."
    )]
    async fn session_submit_form(
        &self,
        Parameters(p): Parameters<SessionSubmitFormParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let shared = self.lookup_session(&p.session_id)?;
        let mut session = shared.lock().await;
        let values: Vec<(String, String)> = p.values.into_iter().collect();
        let outcome = session
            .submit_form(&p.form_id, &values, p.confirm.unwrap_or(false))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(format!("submit failed: {e}"), None))?;
        match outcome {
            SubmitOutcome::Submitted => session_view(&p.session_id, &session),
            SubmitOutcome::NeedsConfirmation {
                method,
                action,
                fields,
            } => session_confirmation_view(&p.session_id, &session, method, action, fields),
        }
    }
}

#[tool_handler]
impl ServerHandler for RustBrowserServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "RustBrowser: token-lean web fetching and Browser Use. fetch_url distills one page; \
             fetch_urls fetches many concurrently; observe_url returns a page's distilled content \
             plus an action tree (links/forms/buttons/downloads with stable action_ids). For \
             multi-step flows use a SESSION: session_start opens a page and keeps cookies + \
             history, then session_observe / session_follow / session_submit_form drive it by \
             action_id (GET forms submit immediately; non-GET needs confirm=true). Every session \
             reply includes a planner-friendly `loop` view (state, available_actions, \
             recommended_next_actions, failure_reason) and a debug `operation_log`; idempotent \
             steps are verified and retried up to max_action_retries on a transient failure, while \
             dangerous (non-GET) submits are never auto-executed or retried. Use session_close to \
             forget cookies and release the session. All return clean output instead of raw HTML, \
             saving 75-98% of tokens.",
        )
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // stdout carries the MCP protocol frames; every diagnostic goes to stderr so
    // it can never corrupt the stream.
    eprintln!("rustbrowser-mcp: starting on stdio transport");

    let service = match RustBrowserServer::new().serve(stdio()).await {
        Ok(service) => service,
        Err(e) => {
            if is_clean_startup_disconnect(&e) {
                eprintln!(
                    "rustbrowser-mcp: client disconnected before initialization, shutting down"
                );
                return ExitCode::SUCCESS;
            }
            eprintln!("rustbrowser-mcp: failed to start MCP service: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Run until the client disconnects (stdin EOF) or the service is cancelled —
    // both are clean shutdowns. Only a panicked/aborted service task or a
    // transport error is a genuine failure worth a non-zero exit.
    match service.waiting().await {
        Ok(QuitReason::Closed) => {
            eprintln!("rustbrowser-mcp: client disconnected, shutting down");
            ExitCode::SUCCESS
        }
        Ok(QuitReason::Cancelled) => {
            eprintln!("rustbrowser-mcp: service cancelled, shutting down");
            ExitCode::SUCCESS
        }
        Ok(QuitReason::JoinError(e)) => {
            eprintln!("rustbrowser-mcp: service task failed: {e}");
            ExitCode::FAILURE
        }
        Ok(other) => {
            // Future rmcp versions may add shutdown reasons; treat an unknown one
            // as a clean stop, but log it so the cause is visible.
            eprintln!("rustbrowser-mcp: shutting down ({other:?})");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("rustbrowser-mcp: transport error while serving: {e}");
            ExitCode::FAILURE
        }
    }
}

fn is_clean_startup_disconnect(e: &impl std::fmt::Display) -> bool {
    let msg = e.to_string().to_ascii_lowercase();
    msg.contains("connection closed") && msg.contains("initialize request")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DisplayErr(&'static str);

    impl std::fmt::Display for DisplayErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    #[test]
    fn startup_initialize_eof_is_clean_disconnect() {
        assert!(is_clean_startup_disconnect(&DisplayErr(
            "connection closed: initialize request"
        )));
    }

    #[test]
    fn unrelated_startup_error_is_not_clean_disconnect() {
        assert!(!is_clean_startup_disconnect(&DisplayErr(
            "failed to bind stdio transport"
        )));
    }

    #[test]
    fn confirmation_view_keeps_action_loop_state() {
        let session = Session::new(DistillOptions::default()).unwrap();
        let out = session_confirmation_view(
            "sess_test",
            &session,
            "POST".to_string(),
            "https://example.com/login".to_string(),
            vec![("csrf".to_string(), "tok".to_string())],
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();

        assert_eq!(value["session_id"], "sess_test");
        assert_eq!(value["needs_confirmation"], true);
        assert!(value.get("would_submit").is_some());
        assert!(value.get("loop").is_some());
        assert!(value.get("operation_log").is_some());
        assert!(value.get("snapshot").is_some());
    }

    /// Property names present in a generated JSON schema's `properties` object.
    fn schema_props(schema: schemars::Schema) -> std::collections::BTreeSet<String> {
        serde_json::to_value(schema)
            .expect("schema serialises")
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    }

    // The MCP parameter sets are frozen for 1.x (see docs/API.md). Adding a param
    // is fine — extend the list. Removing/renaming one should fail here first.

    #[test]
    fn fetch_url_params_are_frozen() {
        let props = schema_props(schemars::schema_for!(FetchParams));
        for key in [
            "url",
            "format",
            "selector",
            "profile",
            "stats",
            "diagnostics",
            "max_output_tokens",
            "timeout_secs",
            "max_bytes",
            "no_cache",
            "cache_ttl",
            "extract_links",
            "extract_tables",
            "links_full",
            "js",
            "js_wait",
            "js_wait_for",
            "allow_local",
            "max_retries",
            "per_host_concurrency",
            "rate_limit",
            "respect_robots",
            "extract_actions",
            "max_actions",
        ] {
            assert!(
                props.contains(key),
                "frozen FetchParams field {key} missing"
            );
        }
    }

    #[test]
    fn fetch_urls_params_are_frozen() {
        let props = schema_props(schemars::schema_for!(FetchManyParams));
        for key in [
            "urls",
            "concurrency",
            "format",
            "profile",
            "stats",
            "diagnostics",
            "max_output_tokens",
            "timeout_secs",
            "max_bytes",
            "no_cache",
            "cache_ttl",
            "extract_links",
            "extract_tables",
            "links_full",
            "js",
            "js_wait",
            "js_wait_for",
            "allow_local",
            "max_retries",
            "per_host_concurrency",
            "rate_limit",
            "respect_robots",
            "extract_actions",
            "max_actions",
        ] {
            assert!(
                props.contains(key),
                "frozen FetchManyParams field {key} missing"
            );
        }
    }

    #[test]
    fn session_tool_params_are_frozen() {
        let start = schema_props(schemars::schema_for!(SessionStartParams));
        for key in [
            "url",
            "profile",
            "max_actions",
            "timeout_secs",
            "allow_local",
            "respect_robots",
            "max_action_retries",
        ] {
            assert!(
                start.contains(key),
                "frozen SessionStartParams field {key} missing"
            );
        }
        let observe = schema_props(schemars::schema_for!(SessionObserveParams));
        for key in ["session_id", "url"] {
            assert!(
                observe.contains(key),
                "frozen SessionObserveParams field {key} missing"
            );
        }
        let follow = schema_props(schemars::schema_for!(SessionFollowParams));
        for key in ["session_id", "action_id"] {
            assert!(
                follow.contains(key),
                "frozen SessionFollowParams field {key} missing"
            );
        }
        let close = schema_props(schemars::schema_for!(SessionCloseParams));
        assert!(
            close.contains("session_id"),
            "frozen SessionCloseParams field session_id missing"
        );
        let submit = schema_props(schemars::schema_for!(SessionSubmitFormParams));
        for key in ["session_id", "form_id", "values", "confirm"] {
            assert!(
                submit.contains(key),
                "frozen SessionSubmitFormParams field {key} missing"
            );
        }
    }
}
