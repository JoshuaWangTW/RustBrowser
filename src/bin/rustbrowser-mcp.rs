//! RustBrowser MCP server — exposes the distillation pipeline as an MCP tool
//! so Claude Code can fetch web content natively, without raw HTML ever
//! entering the conversation. This is where the token savings really land.
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

use rustbrowser::{DistillOptions, distill};

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
}

#[tool_router]
impl RustBrowserServer {
    #[tool(
        description = "Fetch a web page and return its MAIN CONTENT distilled to clean Markdown — navigation, ads, scripts, and boilerplate stripped out. Use this instead of fetching raw HTML whenever you need to read web content: it typically costs 75-98% fewer tokens than the raw page. Optionally target a CSS selector or request plain text / JSON."
    )]
    async fn fetch_url(
        &self,
        Parameters(p): Parameters<FetchParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let opts = DistillOptions {
            timeout: Duration::from_secs(p.timeout_secs.unwrap_or(20)),
            user_agent: None,
            selector: p.selector,
            measure_tokens: p.stats.unwrap_or(false),
        };

        let result = distill(&p.url, &opts)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(format!("fetch failed: {e}"), None))?;

        let fmt = p.format.as_deref().unwrap_or("markdown");
        let body = match fmt {
            "json" => serde_json::to_string_pretty(&result)
                .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?,
            "text" => result.text.clone(),
            _ => {
                let mut s = String::new();
                if !result.title.is_empty() {
                    s.push_str(&format!("# {}\n\n", result.title));
                }
                s.push_str(&result.markdown);
                s
            }
        };

        let out = match (p.stats.unwrap_or(false), &result.stats) {
            (true, Some(st)) => format!(
                "{body}\n\n---\n_token stats: raw {} → output {} ({:.1}% saved)_",
                st.raw_tokens,
                st.output_tokens,
                st.saved_ratio * 100.0
            ),
            _ => body,
        };

        Ok(out)
    }
}

#[tool_handler]
impl ServerHandler for RustBrowserServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "RustBrowser: token-lean web fetching. The fetch_url tool returns distilled \
                 main content (clean Markdown) instead of heavy raw HTML, saving 75-98% of \
                 tokens. Prefer it over fetching raw pages.",
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
