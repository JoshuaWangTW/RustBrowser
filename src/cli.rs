//! Command-line surface (clap). Kept in the binary so the core library stays
//! free of CLI dependencies and can be reused by an MCP server later.

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "rustbrowser",
    version,
    about = "Token-lean web content fetcher for LLMs — fetch, distill, output."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Fetch one or more URLs and output distilled content.
    Fetch(FetchArgs),
    /// Inspect or clean the on-disk cache.
    Cache(CacheArgs),
}

#[derive(Args)]
pub struct CacheArgs {
    #[command(subcommand)]
    pub action: CacheAction,
}

#[derive(Subcommand)]
pub enum CacheAction {
    /// Show cache entry counts and total size.
    Info,
    /// Remove entries older than the given age.
    Prune {
        /// Age threshold in seconds; entries older than this are removed.
        #[arg(long, default_value_t = 3600)]
        older_than: u64,
    },
    /// Remove all cached entries (both fetch and render).
    Clear,
}

#[derive(Args)]
pub struct FetchArgs {
    /// One or more URLs. Multiple URLs are fetched concurrently as a batch.
    #[arg(required = true)]
    pub urls: Vec<String>,

    /// Output format.
    #[arg(short, long, value_enum, default_value_t = Format::Markdown)]
    pub format: Format,

    /// Only extract content matching this CSS selector (skips readability).
    #[arg(short, long)]
    pub selector: Option<String>,

    /// Print token-savings stats to stderr.
    #[arg(long)]
    pub stats: bool,

    /// Request timeout in seconds.
    #[arg(long, default_value_t = 20)]
    pub timeout: u64,

    /// Maximum response bytes to keep before decoding.
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    pub max_bytes: usize,

    /// Skip the on-disk cache (always fetch fresh).
    #[arg(long)]
    pub no_cache: bool,

    /// Cache freshness window in seconds.
    #[arg(long, default_value_t = 3600)]
    pub cache_ttl: u64,

    /// Max concurrent requests when fetching multiple URLs.
    #[arg(long, default_value_t = 8)]
    pub concurrency: usize,

    /// Also extract all links from the main content as structured data.
    #[arg(long)]
    pub links: bool,

    /// Extract ALL links including nav/footer (whole page) — for crawling.
    #[arg(long)]
    pub links_all: bool,

    /// Also extract all tables from the main content as structured data.
    #[arg(long)]
    pub tables: bool,

    /// Headless JS-rendering fallback: off, auto (default), or always.
    #[arg(long, value_enum, default_value_t = JsMode::Auto)]
    pub js: JsMode,

    /// Headless wait / virtual-time budget in milliseconds.
    #[arg(long)]
    pub js_wait: Option<u64>,

    /// Wait until this CSS selector appears before capturing (uses CDP).
    #[arg(long)]
    pub js_wait_for: Option<String>,

    /// Permit loopback/localhost targets (e.g. http://127.0.0.1:8080) — for
    /// local dev servers. Only loopback is freed; private LAN, link-local, and
    /// cloud-metadata addresses stay blocked.
    #[arg(long)]
    pub allow_local: bool,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum Format {
    /// Distilled Markdown (default).
    Markdown,
    /// Plain text only.
    Text,
    /// Structured JSON (metadata + markdown + stats).
    Json,
}

/// Headless JS-rendering policy.
#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum JsMode {
    /// Never use headless rendering.
    Off,
    /// Auto-detect unrendered JS apps and render only those (default).
    Auto,
    /// Always render with a headless browser.
    Always,
}
