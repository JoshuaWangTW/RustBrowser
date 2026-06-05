//! RustBrowser MCP server — exposes the distillation pipeline as MCP tools so
//! Claude Code can fetch web content natively, without raw HTML ever entering
//! the conversation. This is where the token savings really land.
//!
//! Transport is stdio: the MCP protocol speaks over stdout, so NOTHING may be
//! printed to stdout except protocol frames. All diagnostics go to stderr.

use std::process::ExitCode;
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
use serde_json::json;

use rustbrowser::{DistillOptions, Distilled, JsMode, distill, distill_many};

#[derive(Debug, Clone)]
struct RustBrowserServer {
    // Read by the `#[tool_handler]` macro's generated ServerHandler impl,
    // which the dead-code lint can't see through.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl RustBrowserServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
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
}

fn parse_js_mode(js: Option<&str>) -> JsMode {
    match js {
        Some("off") => JsMode::Off,
        Some("always") => JsMode::Always,
        _ => JsMode::Auto,
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
        );
        let result = distill(&p.url, &opts)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(format!("fetch failed: {e}"), None))?;
        let fmt = p.format.as_deref().unwrap_or("markdown");
        render(&result, fmt)
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
}

#[tool_handler]
impl ServerHandler for RustBrowserServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "RustBrowser: token-lean web fetching. fetch_url distills one page; fetch_urls \
             fetches many concurrently. Both return clean Markdown instead of raw HTML, saving \
             75-98% of tokens, with an on-disk cache and automatic headless rendering for \
             JS-heavy pages.",
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
}
