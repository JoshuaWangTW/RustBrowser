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

    /// Retry transient failures (connect/timeout, 429, 5xx) this many times
    /// with exponential backoff. 0 disables retrying.
    #[arg(long, default_value_t = 2)]
    pub max_retries: usize,

    /// Max simultaneous requests to any single host. 0 = unlimited.
    #[arg(long, default_value_t = 4)]
    pub per_host_concurrency: usize,

    /// Rate limit per host, in requests per second (e.g. 2 = one every 500 ms).
    /// 0 disables rate limiting.
    #[arg(long, default_value_t = 0.0)]
    pub rate_limit: f64,

    /// Respect each host's robots.txt and skip disallowed paths.
    #[arg(long)]
    pub respect_robots: bool,

    /// Content profile: article (readability, default), full (whole body, no
    /// readability filtering), or metadata (title + short summary only).
    #[arg(long, value_enum, default_value_t = Profile::Article)]
    pub profile: Profile,

    /// Truncate the Markdown output to fit this many tokens (at a paragraph
    /// boundary, with a marker). Unset = no limit.
    #[arg(long)]
    pub max_output_tokens: Option<usize>,

    /// Print extraction-quality diagnostics to stderr (always in --format json).
    #[arg(long)]
    pub diagnostics: bool,
}

/// Content-selection profile.
#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum Profile {
    /// Readability main-content extraction (default).
    Article,
    /// Whole `<body>`, scripts/styles removed, no readability filtering.
    Full,
    /// Title + a short summary only.
    Metadata,
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
