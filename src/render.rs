//! Optional headless rendering via the system browser's `--dump-dom`.
//!
//! Zero compile-time dependency on a browser engine: when a page looks like an
//! unrendered single-page app, we shell out to the user's existing Chrome (or
//! Edge — both are Chromium), let it run the JS, and capture the rendered DOM.
//! The lean HTTP path stays the default; this is strictly a fallback.

#[cfg(feature = "js")]
use std::path::Path;
#[cfg(feature = "js")]
use std::process::Stdio;
use std::time::Duration;
#[cfg(feature = "js")]
use std::time::Instant;

#[cfg(feature = "js")]
use anyhow::Context;
use anyhow::{Result, bail};
#[cfg(feature = "js")]
use futures::{SinkExt, StreamExt};
#[cfg(feature = "js")]
use serde_json::{Value, json};
#[cfg(feature = "js")]
use tokio::net::TcpStream;
#[cfg(feature = "js")]
use tokio::process::Command;
#[cfg(feature = "js")]
use tokio::time::timeout;
#[cfg(feature = "js")]
use tokio_tungstenite::tungstenite::Message;
#[cfg(feature = "js")]
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// Below this many characters of extracted text, an Auto-mode page is a
/// candidate for headless re-rendering (if it also looks like a JS app).
pub const JS_TEXT_THRESHOLD: usize = 200;

/// Hard cap on the rendered DOM we retain. Headless rendering of a hostile (or
/// merely enormous) page could otherwise return an unbounded amount of HTML and
/// blow up memory/tokens. 16 MiB is generous for real content.
pub const MAX_RENDER_BYTES: usize = 16 * 1024 * 1024;

/// Heuristic: does this HTML look like a client-rendered app whose real content
/// only appears after JavaScript runs?
pub fn looks_like_js_app(html: &str) -> bool {
    const MARKERS: &[&str] = &[
        "__NEXT_DATA__",
        "id=\"__next\"",
        "id=\"root\"",
        "id=\"app\"",
        "data-reactroot",
        "ng-app",
        "<app-root",
        "data-server-rendered",
        "window.__NUXT__",
    ];
    if MARKERS.iter().any(|m| html.contains(m)) {
        return true;
    }
    // No explicit framework marker, but a script-heavy page is a good bet for
    // client-side rendering — especially combined (by the caller) with very
    // little extracted body text.
    html.matches("<script").count() >= 2
}

/// Locate a Chrome/Chromium/Edge executable, honouring `RUSTBROWSER_CHROME`.
#[cfg(feature = "js")]
fn find_chrome() -> Option<String> {
    if let Ok(p) = std::env::var("RUSTBROWSER_CHROME")
        && !p.is_empty()
        && Path::new(&p).exists()
    {
        return Some(p);
    }
    const CANDIDATES: &[&str] = &[
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files\Chromium\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ];
    CANDIDATES
        .iter()
        .find(|c| Path::new(c).exists())
        .map(|c| c.to_string())
}

/// Parse a truthy environment-flag value (`1`/`true`/`yes`/anything non-empty
/// that isn't `0`/`false`).
#[cfg(feature = "js")]
fn is_truthy_flag(v: &str) -> bool {
    let v = v.trim();
    !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
}

/// Whether to pass Chrome `--no-sandbox`. The sandbox is a primary defense when
/// rendering untrusted web pages, so it stays ENABLED by default. Users in
/// containers or running as root (where Chrome's sandbox can't initialize) can
/// opt out by setting `RUSTBROWSER_NO_SANDBOX=1`.
#[cfg(feature = "js")]
fn no_sandbox_requested() -> bool {
    std::env::var("RUSTBROWSER_NO_SANDBOX")
        .map(|v| is_truthy_flag(&v))
        .unwrap_or(false)
}

/// Headless flags shared by both render paths. Keeps the sandbox on unless
/// explicitly opted out via `no_sandbox_requested`.
#[cfg(feature = "js")]
fn base_headless_args() -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--headless=new".into(),
        "--disable-gpu".into(),
        "--disable-dev-shm-usage".into(),
    ];
    if no_sandbox_requested() {
        args.push("--no-sandbox".into());
    }
    args
}

/// Truncate a rendered DOM string to `MAX_RENDER_BYTES`, respecting char
/// boundaries so the result stays valid UTF-8.
#[cfg(feature = "js")]
fn cap_dom(mut html: String) -> String {
    if html.len() > MAX_RENDER_BYTES {
        let mut end = MAX_RENDER_BYTES;
        while end > 0 && !html.is_char_boundary(end) {
            end -= 1;
        }
        html.truncate(end);
    }
    html
}

/// Read up to `max` bytes from `reader`, stopping the moment the cap is reached
/// rather than buffering the entire stream first. This is what makes the render
/// cap a real memory bound: a hostile page that emits gigabytes of DOM costs us
/// at most ~`max` bytes before we stop reading and tear the browser down.
#[cfg(feature = "js")]
async fn read_capped<R>(reader: &mut R, max: usize) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    while buf.len() < max {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break; // EOF
        }
        let take = n.min(max - buf.len());
        buf.extend_from_slice(&chunk[..take]);
        if take < n {
            break; // hit the cap mid-chunk
        }
    }
    Ok(buf)
}

#[cfg(feature = "js")]
fn cdp_byte_capped_outer_html_expr(max: usize) -> String {
    format!(
        r#"(() => {{
const html = document.documentElement.outerHTML;
const encoder = new TextEncoder();
const bytes = encoder.encode(html);
const capped = bytes.length > {max} ? bytes.slice(0, {max}) : bytes;
return new TextDecoder("utf-8", {{ fatal: false }}).decode(capped);
}})()"#
    )
}

/// Render `url` with a headless browser and return the post-JavaScript DOM.
///
/// `wait` doubles as the virtual-time budget — how long to let JS run.
#[cfg(feature = "js")]
pub async fn render_html(url: &str, wait: Duration) -> Result<String> {
    let chrome = find_chrome()
        .context("no Chrome/Chromium/Edge found; set RUSTBROWSER_CHROME to its full path")?;

    let budget = wait.as_millis().max(1000).to_string();
    let mut args = base_headless_args();
    args.push(format!("--virtual-time-budget={budget}"));
    args.push("--dump-dom".into());
    args.push(url.to_string());

    let mut child = Command::new(&chrome)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("launching headless browser")?;

    let mut stdout = child
        .stdout
        .take()
        .context("capturing headless browser stdout")?;

    // Stream the DOM with a hard cap instead of buffering all of stdout: this is
    // what actually bounds memory. The whole read is bounded by the timeout.
    let bytes = timeout(
        wait + Duration::from_secs(15),
        read_capped(&mut stdout, MAX_RENDER_BYTES),
    )
    .await
    .context("headless render timed out")?
    .context("reading headless DOM")?;

    let hit_cap = bytes.len() == MAX_RENDER_BYTES;
    if hit_cap {
        // We intentionally stop reading once the cap is reached; kill Chrome so
        // it cannot keep writing a huge DOM into the pipe.
        let _ = child.kill().await;
    } else {
        let status = child.wait().await.context("waiting for headless browser")?;
        if !status.success() {
            bail!("headless browser exited unsuccessfully");
        }
    }

    let html = String::from_utf8_lossy(&bytes).into_owned();
    if html.trim().is_empty() {
        bail!("headless render produced an empty DOM");
    }
    Ok(html)
}

/// Stub used when built without the `js` feature.
#[cfg(not(feature = "js"))]
pub async fn render_html(_url: &str, _wait: Duration) -> Result<String> {
    bail!("headless rendering requires the 'js' feature")
}

/// Options for [`render_html_cdp_with`]: which selector (if any) to wait for
/// before capturing the DOM, and which cookies (if any) to inject into the
/// isolated render profile before navigating.
#[derive(Debug, Default)]
pub struct CdpRender<'a> {
    /// CSS selector to wait for before capturing the DOM. `None` waits for
    /// `document.readyState === "complete"` instead.
    pub wait_for: Option<&'a str>,
    /// `Cookie:`-header-style `name=value; name2=value2` pairs to inject into
    /// the isolated render profile before navigating. Only sent to the exact
    /// `url` being rendered — never a whole jar, never logged.
    pub cookies: Option<&'a str>,
}

#[cfg(feature = "js")]
type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Removes the Chrome `--user-data-dir` temp directory on drop, so cleanup
/// still runs if `render_html_cdp`'s future is cancelled or panics before
/// reaching its normal teardown path (a plain end-of-function cleanup call
/// would miss both cases). Retries a few times on Windows, where an
/// about-to-exit Chrome process can transiently hold the directory (or a
/// cookie file inside it) locked — leaving injected cookies on disk would be
/// a security regression, so we don't give up after one attempt.
#[cfg(feature = "js")]
struct TempDirGuard(std::path::PathBuf);

#[cfg(feature = "js")]
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        if !self.0.exists() {
            return;
        }
        for attempt in 0..3 {
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(_) if attempt < 2 => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => eprintln!(
                    "rustbrowser: could not remove render temp profile {} after 3 attempts: {e}",
                    self.0.display()
                ),
            }
        }
    }
}

/// Render `url` over the Chrome DevTools Protocol, waiting until `wait_for` (a
/// CSS selector) appears in the DOM before capturing it. If the selector never
/// shows up within `budget`, we capture whatever is present. Heavier than
/// `--dump-dom`, but lets you wait for content that loads asynchronously.
///
/// Thin wrapper over [`render_html_cdp_with`] for callers that only need the
/// selector-wait behaviour (no cookie injection).
#[cfg(feature = "js")]
pub async fn render_html_cdp(url: &str, wait_for: &str, budget: Duration) -> Result<String> {
    render_html_cdp_with(
        url,
        budget,
        CdpRender {
            wait_for: Some(wait_for),
            cookies: None,
        },
    )
    .await
}

/// Stub used when built without the `js` feature.
#[cfg(not(feature = "js"))]
pub async fn render_html_cdp(_url: &str, _wait_for: &str, _budget: Duration) -> Result<String> {
    bail!("headless rendering requires the 'js' feature")
}

/// Render `url` over the Chrome DevTools Protocol with the given [`CdpRender`]
/// options: an optional selector to wait for, and optional cookies to inject
/// into an isolated, single-use Chrome profile before navigating (see
/// `SECURITY.md`). The profile is deleted and its cookies cleared on the way
/// out, whether or not cookies were injected.
#[cfg(feature = "js")]
pub async fn render_html_cdp_with(url: &str, budget: Duration, r: CdpRender<'_>) -> Result<String> {
    let chrome = find_chrome()
        .context("no Chrome/Chromium/Edge found; set RUSTBROWSER_CHROME to its full path")?;

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let user_dir =
        std::env::temp_dir().join(format!("rustbrowser-cdp-{}-{uniq}", std::process::id()));
    let _ = std::fs::create_dir_all(&user_dir);
    // Guarantees the temp dir is removed even on early return, cancellation,
    // or panic — a plain call at the end of this function would miss all three.
    let _guard = TempDirGuard(user_dir.clone());

    let mut args = base_headless_args();
    args.push("--remote-debugging-port=0".into());
    args.push(format!("--user-data-dir={}", user_dir.display()));
    args.push("about:blank".into());

    let mut child = Command::new(&chrome)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("launching headless Chrome for CDP")?;

    // Bound the whole session so a Chrome that connects but never responds
    // can't hang this future forever. The +15s margin mirrors render_html's
    // `wait + 15s` timeout convention. A failure here is non-fatal: the
    // caller falls back to the plain HTTP snapshot.
    let outcome = match timeout(
        budget + Duration::from_secs(15),
        cdp_session(url, r.wait_for, r.cookies, budget, &user_dir),
    )
    .await
    {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!("CDP render timed out")),
    };
    // Always tear the browser down; `kill_on_drop` is the backstop if this
    // future itself gets cancelled before reaching this line.
    let _ = child.kill().await;
    outcome
}

/// Stub used when built without the `js` feature.
#[cfg(not(feature = "js"))]
pub async fn render_html_cdp_with(
    _url: &str,
    _budget: Duration,
    _r: CdpRender<'_>,
) -> Result<String> {
    bail!("headless rendering requires the 'js' feature")
}

/// The JS expression used to decide whether the page is ready to capture:
/// wait for `wait_for` (a CSS selector) if given, otherwise fall back to
/// `document.readyState === "complete"` rather than probing for an empty
/// selector.
#[cfg(feature = "js")]
fn cdp_ready_probe(wait_for: Option<&str>) -> String {
    match wait_for {
        Some(sel) => {
            let selector_json = serde_json::to_string(sel).unwrap_or_else(|_| "\"\"".into());
            format!("!!document.querySelector({selector_json})")
        }
        None => "document.readyState === \"complete\"".to_string(),
    }
}

/// Parse a `Cookie:`-header-style string (`name=value; name2=value2`) into
/// `(name, value)` pairs, skipping malformed or empty segments.
#[cfg(feature = "js")]
fn parse_cookie_pairs(s: &str) -> Vec<(String, String)> {
    s.split(';')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            let (name, value) = part.split_once('=')?;
            let (name, value) = (name.trim(), value.trim());
            (!name.is_empty()).then(|| (name.to_string(), value.to_string()))
        })
        .collect()
}

#[cfg(feature = "js")]
async fn cdp_session(
    url: &str,
    wait_for: Option<&str>,
    cookies: Option<&str>,
    budget: Duration,
    user_dir: &Path,
) -> Result<String> {
    let port = read_devtools_port(user_dir, Duration::from_secs(15)).await?;
    let ws_url = page_ws_url(port).await?;
    let (mut ws, _) = connect_async(&ws_url)
        .await
        .context("connecting to Chrome DevTools")?;

    let mut id = 1u64;
    cdp_call(&mut ws, id, "Page.enable", json!({})).await?;
    id += 1;

    // Cookies are injected before navigation so they're present on the very
    // first request. `url` scopes each `Network.setCookie` call so Chrome
    // derives the right domain/path — only the URL being rendered ever sees
    // them, never a whole jar. Never logged: values only ever cross this
    // local CDP WebSocket.
    let cookie_pairs = cookies.map(parse_cookie_pairs).unwrap_or_default();
    if !cookie_pairs.is_empty() {
        cdp_call(&mut ws, id, "Network.enable", json!({})).await?;
        id += 1;
        for (name, value) in &cookie_pairs {
            cdp_call(
                &mut ws,
                id,
                "Network.setCookie",
                json!({ "name": name, "value": value, "url": url }),
            )
            .await?;
            id += 1;
        }
    }

    cdp_call(&mut ws, id, "Page.navigate", json!({ "url": url })).await?;
    id += 1;

    let probe = cdp_ready_probe(wait_for);
    let deadline = Instant::now() + budget;
    loop {
        if cdp_eval(&mut ws, id, &probe).await?.as_bool() == Some(true) {
            id += 1;
            break;
        }
        id += 1;
        if Instant::now() >= deadline {
            break; // give up waiting; capture the current DOM
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Cap by UTF-8 bytes in the browser so the CDP payload itself is bounded.
    // `cap_dom` remains a byte-level backstop after JSON decoding.
    let expr = cdp_byte_capped_outer_html_expr(MAX_RENDER_BYTES);
    let dom = cdp_eval(&mut ws, id, &expr).await?;
    id += 1;

    // Teardown: clear any cookies we injected before the process (and its
    // temp profile directory, via TempDirGuard) go away.
    if !cookie_pairs.is_empty() {
        let _ = cdp_call(&mut ws, id, "Network.clearBrowserCookies", json!({})).await;
    }

    dom.as_str()
        .map(str::to_string)
        .map(cap_dom)
        .filter(|s| !s.trim().is_empty())
        .context("CDP render produced an empty DOM")
}

/// Send one CDP request and return the `result` of the matching-id response.
#[cfg(feature = "js")]
async fn cdp_call(ws: &mut Ws, id: u64, method: &str, params: Value) -> Result<Value> {
    let req = json!({ "id": id, "method": method, "params": params });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .context("sending CDP request")?;
    while let Some(msg) = ws.next().await {
        if let Message::Text(text) = msg.context("CDP stream error")? {
            let v: Value = serde_json::from_str(&text).context("parsing CDP message")?;
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(v.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }
    bail!("CDP connection closed before response")
}

/// `Runtime.evaluate`, returning the JS value (by value).
#[cfg(feature = "js")]
async fn cdp_eval(ws: &mut Ws, id: u64, expr: &str) -> Result<Value> {
    let r = cdp_call(
        ws,
        id,
        "Runtime.evaluate",
        json!({ "expression": expr, "returnByValue": true }),
    )
    .await?;
    Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
}

/// Read the port Chrome wrote to `DevToolsActivePort` in its user-data dir.
#[cfg(feature = "js")]
async fn read_devtools_port(user_dir: &Path, wait: Duration) -> Result<u16> {
    let path = user_dir.join("DevToolsActivePort");
    let deadline = Instant::now() + wait;
    loop {
        if let Ok(content) = std::fs::read_to_string(&path)
            && let Some(line) = content.lines().next()
            && let Ok(port) = line.trim().parse::<u16>()
        {
            return Ok(port);
        }
        if Instant::now() >= deadline {
            bail!("Chrome did not expose a debugging port in time");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Ask Chrome's HTTP endpoint for the first page target's WebSocket URL.
#[cfg(feature = "js")]
async fn page_ws_url(port: u16) -> Result<String> {
    let url = format!("http://127.0.0.1:{port}/json/list");
    let body = reqwest::get(&url)
        .await
        .context("querying CDP targets")?
        .text()
        .await
        .context("reading CDP targets")?;
    let targets: Vec<Value> = serde_json::from_str(&body).context("parsing CDP targets")?;
    targets
        .iter()
        .find(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .and_then(|t| t.get("webSocketDebuggerUrl").and_then(Value::as_str))
        .map(str::to_string)
        .context("no CDP page target found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_spa_markers() {
        assert!(looks_like_js_app(r#"<div id="root"></div>"#));
        assert!(looks_like_js_app(r#"<script>window.__NUXT__={}</script>"#));
        assert!(looks_like_js_app(r#"<app-root></app-root>"#));
    }

    #[test]
    fn plain_article_is_not_a_js_app() {
        let html = "<html><body><article><h1>Title</h1><p>Real content here.</p>\
                    </article></body></html>";
        assert!(!looks_like_js_app(html));
    }

    #[test]
    fn script_heavy_page_is_flagged() {
        let html = "<html><head><script src=\"a.js\"></script>\
                    <script>var x = 1;</script></head><body><div></div></body></html>";
        assert!(looks_like_js_app(html));
    }

    #[cfg(feature = "js")]
    #[test]
    fn truthy_flag_parsing() {
        assert!(is_truthy_flag("1"));
        assert!(is_truthy_flag("true"));
        assert!(is_truthy_flag("YES"));
        assert!(!is_truthy_flag("0"));
        assert!(!is_truthy_flag("false"));
        assert!(!is_truthy_flag("False"));
        assert!(!is_truthy_flag(""));
        assert!(!is_truthy_flag("   "));
    }

    #[cfg(feature = "js")]
    #[test]
    fn base_args_keep_sandbox_by_default() {
        // We can't safely mutate process env in parallel tests, but we can assert
        // the static invariant: the base flags never hard-code --no-sandbox.
        let args = base_headless_args();
        assert!(args.iter().any(|a| a == "--headless=new"));
        // --no-sandbox only ever appears via the explicit opt-in path.
        assert_eq!(
            args.iter().any(|a| a == "--no-sandbox"),
            no_sandbox_requested()
        );
    }

    #[cfg(feature = "js")]
    #[test]
    fn cap_dom_truncates_oversized_input() {
        let big = "a".repeat(MAX_RENDER_BYTES + 1000);
        let capped = cap_dom(big);
        assert!(capped.len() <= MAX_RENDER_BYTES);
    }

    #[cfg(feature = "js")]
    #[test]
    fn cap_dom_passes_small_input_through() {
        let small = "<html><body>hi</body></html>".to_string();
        assert_eq!(cap_dom(small.clone()), small);
    }

    #[cfg(feature = "js")]
    #[tokio::test]
    async fn read_capped_stops_at_limit() {
        let data = vec![b'x'; 100_000];
        let mut src: &[u8] = &data;
        let out = read_capped(&mut src, 4096).await.unwrap();
        assert_eq!(out.len(), 4096); // stopped at the cap, did not read all 100 KB
    }

    #[cfg(feature = "js")]
    #[tokio::test]
    async fn read_capped_returns_all_when_under_limit() {
        let data = vec![b'y'; 1000];
        let mut src: &[u8] = &data;
        let out = read_capped(&mut src, 4096).await.unwrap();
        assert_eq!(out.len(), 1000); // whole stream fits under the cap
    }

    #[cfg(feature = "js")]
    #[test]
    fn cdp_capture_expression_caps_utf8_bytes_in_browser() {
        let expr = cdp_byte_capped_outer_html_expr(4096);
        assert!(expr.contains("new TextEncoder()"));
        assert!(expr.contains("bytes.slice(0, 4096)"));
        assert!(!expr.contains("outerHTML.slice"));
    }

    #[cfg(feature = "js")]
    #[test]
    fn cap_dom_respects_char_boundaries() {
        // A multi-byte char straddling the cap must not be split into invalid
        // UTF-8 (truncation only happens past the limit; just assert validity).
        let s = "界".repeat(MAX_RENDER_BYTES); // 3 bytes each → well over the cap
        let capped = cap_dom(s);
        assert!(capped.len() <= MAX_RENDER_BYTES);
        // If it compiled to a String it is valid UTF-8; round-trip to be sure.
        assert!(std::str::from_utf8(capped.as_bytes()).is_ok());
    }

    #[cfg(feature = "js")]
    #[test]
    fn ready_probe_waits_for_selector_when_given() {
        let probe = cdp_ready_probe(Some("#root"));
        assert!(probe.contains("querySelector"));
        assert!(probe.contains("#root"));
    }

    #[cfg(feature = "js")]
    #[test]
    fn ready_probe_falls_back_to_document_ready_state_without_selector() {
        assert_eq!(
            cdp_ready_probe(None),
            "document.readyState === \"complete\""
        );
    }

    #[cfg(feature = "js")]
    #[test]
    fn parses_cookie_header_into_pairs() {
        assert_eq!(
            parse_cookie_pairs("sid=abc; theme=dark"),
            vec![
                ("sid".to_string(), "abc".to_string()),
                ("theme".to_string(), "dark".to_string()),
            ]
        );
    }

    #[cfg(feature = "js")]
    #[test]
    fn parse_cookie_pairs_skips_malformed_or_empty_segments() {
        assert_eq!(
            parse_cookie_pairs("sid=abc; ; malformed; =novalue; k=v"),
            vec![
                ("sid".to_string(), "abc".to_string()),
                ("k".to_string(), "v".to_string()),
            ]
        );
    }

    #[cfg(feature = "js")]
    #[test]
    fn temp_dir_guard_removes_dir_on_drop() {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "rustbrowser-test-guard-{}-{uniq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(dir.exists());

        {
            let _guard = TempDirGuard(dir.clone());
        } // guard drops here, removing the directory

        assert!(!dir.exists());
    }
}
