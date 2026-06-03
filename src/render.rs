//! Optional headless rendering via the system browser's `--dump-dom`.
//!
//! Zero compile-time dependency on a browser engine: when a page looks like an
//! unrendered single-page app, we shell out to the user's existing Chrome (or
//! Edge — both are Chromium), let it run the JS, and capture the rendered DOM.
//! The lean HTTP path stays the default; this is strictly a fallback.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// Below this many characters of extracted text, an Auto-mode page is a
/// candidate for headless re-rendering (if it also looks like a JS app).
pub const JS_TEXT_THRESHOLD: usize = 200;

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

/// Render `url` with a headless browser and return the post-JavaScript DOM.
///
/// `wait` doubles as the virtual-time budget — how long to let JS run.
pub async fn render_html(url: &str, wait: Duration) -> Result<String> {
    let chrome = find_chrome()
        .context("no Chrome/Chromium/Edge found; set RUSTBROWSER_CHROME to its full path")?;

    let budget = wait.as_millis().max(1000).to_string();
    let run = Command::new(&chrome)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--no-sandbox",
            "--disable-dev-shm-usage",
            &format!("--virtual-time-budget={budget}"),
            "--dump-dom",
            url,
        ])
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
    let html = String::from_utf8_lossy(&out.stdout).into_owned();
    if html.trim().is_empty() {
        bail!("headless render produced an empty DOM");
    }
    Ok(html)
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Render `url` over the Chrome DevTools Protocol, waiting until `wait_for` (a
/// CSS selector) appears in the DOM before capturing it. If the selector never
/// shows up within `budget`, we capture whatever is present. Heavier than
/// `--dump-dom`, but lets you wait for content that loads asynchronously.
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

    let mut child = Command::new(&chrome)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--no-sandbox",
            "--disable-dev-shm-usage",
            "--remote-debugging-port=0",
            &format!("--user-data-dir={}", user_dir.display()),
            "about:blank",
        ])
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
        .filter(|s| !s.trim().is_empty())
        .context("CDP render produced an empty DOM")
}

/// Send one CDP request and return the `result` of the matching-id response.
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
}
