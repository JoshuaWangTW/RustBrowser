//! RustBrowser MCP server — exposes the distillation pipeline as MCP tools so
//! Claude Code can fetch web content natively, without raw HTML ever entering
//! the conversation. This is where the token savings really land.
//!
//! Transport is stdio: the MCP protocol speaks over stdout, so NOTHING may be
//! printed to stdout except protocol frames. All diagnostics go to stderr.

use std::time::Duration;

use anyhow::Result;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
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
    selector: Option<String>,
    stats: Option<bool>,
    no_cache: Option<bool>,
    cache_ttl: Option<u64>,
    links: Option<bool>,
    tables: Option<bool>,
    links_full: Option<bool>,
    js: Option<&str>,
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
            p.selector,
            p.stats,
            p.no_cache,
            p.cache_ttl,
            p.extract_links,
            p.extract_tables,
            p.links_full,
            p.js.as_deref(),
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
            None,
            p.stats,
            p.no_cache,
            p.cache_ttl,
            p.extract_links,
            p.extract_tables,
            p.links_full,
            p.js.as_deref(),
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
async fn main() -> Result<()> {
    // stdout is reserved for the MCP protocol; never print to it.
    let service = RustBrowserServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
