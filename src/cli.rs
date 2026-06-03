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
    /// Fetch a URL and output its distilled content.
    Fetch(FetchArgs),
}

#[derive(Args)]
pub struct FetchArgs {
    /// URL to fetch.
    pub url: String,

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
