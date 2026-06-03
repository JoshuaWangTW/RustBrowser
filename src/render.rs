//! Optional headless rendering via the system browser's `--dump-dom`.
//!
//! Zero compile-time dependency on a browser engine: when a page looks like an
//! unrendered single-page app, we shell out to the user's existing Chrome (or
//! Edge — both are Chromium), let it run the JS, and capture the rendered DOM.
//! The lean HTTP path stays the default; this is strictly a fallback.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tokio::time::timeout;

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
