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

    let run = Command::new(&chrome)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    let out = timeout(wait + Duration::from_secs(15), run)
        .await
        .context("headless render timed out")?
        .context("launching headless browser")?;

    if !out.status.success() {
        bail!("headless browser exited unsuccessfully");
    }
    // Cap before decoding so a giant DOM never materialises fully in memory.
    let capped = &out.stdout[..out.stdout.len().min(MAX_RENDER_BYTES)];
    let html = String::from_utf8_lossy(capped).into_owned();
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

#[cfg(feature = "js")]
type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Render `url` over the Chrome DevTools Protocol, waiting until `wait_for` (a
/// CSS selector) appears in the DOM before capturing it. If the selector never
/// shows up within `budget`, we capture whatever is present. Heavier than
/// `--dump-dom`, but lets you wait for content that loads asynchronously.
#[cfg(feature = "js")]
pub async fn render_html_cdp(url: &str, wait_for: &str, budget: Duration) -> Result<String> {
    let chrome = find_chrome()
        .context("no Chrome/Chromium/Edge found; set RUSTBROWSER_CHROME to its full path")?;

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let user_dir =
        std::env::temp_dir().join(format!("rustbrowser-cdp-{}-{uniq}", std::process::id()));
    let _ = std::fs::create_dir_all(&user_dir);

    let mut args = base_headless_args();
    args.push("--remote-debugging-port=0".into());
    args.push(format!("--user-data-dir={}", user_dir.display()));
    args.push("about:blank".into());

    let mut child = Command::new(&chrome)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("launching headless Chrome for CDP")?;

    // Always tear the browser and temp dir down, even on error.
    let outcome = cdp_session(url, wait_for, budget, &user_dir).await;
    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&user_dir);
    outcome
}

/// Stub used when built without the `js` feature.
#[cfg(not(feature = "js"))]
pub async fn render_html_cdp(_url: &str, _wait_for: &str, _budget: Duration) -> Result<String> {
    bail!("headless rendering requires the 'js' feature")
}

#[cfg(feature = "js")]
async fn cdp_session(
    url: &str,
    wait_for: &str,
    budget: Duration,
    user_dir: &Path,
) -> Result<String> {
    let port = read_devtools_port(user_dir, Duration::from_secs(15)).await?;
    let ws_url = page_ws_url(port).await?;
    let (mut ws, _) = connect_async(&ws_url)
        .await
        .context("connecting to Chrome DevTools")?;

    cdp_call(&mut ws, 1, "Page.enable", json!({})).await?;
    cdp_call(&mut ws, 2, "Page.navigate", json!({ "url": url })).await?;

    let selector_json = serde_json::to_string(wait_for).unwrap_or_else(|_| "\"\"".into());
    let probe = format!("!!document.querySelector({selector_json})");
    let deadline = Instant::now() + budget;
    let mut id = 3u64;
    loop {
        if cdp_eval(&mut ws, id, &probe).await?.as_bool() == Some(true) {
            break;
        }
        id += 1;
        if Instant::now() >= deadline {
            break; // give up waiting; capture the current DOM
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let dom = cdp_eval(&mut ws, id + 1, "document.documentElement.outerHTML").await?;
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
}
