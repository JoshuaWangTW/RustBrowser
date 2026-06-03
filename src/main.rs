//! RustBrowser CLI binary — a thin shell over the rustbrowser core library.

mod cli;

use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command, Format};
use rustbrowser::{DistillOptions, distill};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Fetch(args) => run_fetch(args).await,
    }
}

async fn run_fetch(args: cli::FetchArgs) -> Result<()> {
    let opts = DistillOptions {
        timeout: Duration::from_secs(args.timeout),
        user_agent: None,
        selector: args.selector,
        measure_tokens: args.stats,
    };

    let result = distill(&args.url, &opts).await?;

    match args.format {
        Format::Markdown => {
            if !result.title.is_empty() {
                println!("# {}\n", result.title);
            }
            println!("{}", result.markdown);
        }
        Format::Text => println!("{}", result.text),
        Format::Json => println!("{}", serde_json::to_string_pretty(&result)?),
    }

    // `result.stats` is Some only when stats were requested, so this also
    // gates on the --stats flag.
    if let Some(s) = &result.stats {
        eprintln!();
        eprintln!("── token stats ───────────────");
        eprintln!("raw bytes     : {:>8}", s.raw_bytes);
        eprintln!("raw tokens    : {:>8}", s.raw_tokens);
        eprintln!("output tokens : {:>8}", s.output_tokens);
        eprintln!(
            "saved         : {:>8}  ({:.1}%)",
            s.saved_tokens,
            s.saved_ratio * 100.0
        );
    }

    Ok(())
}
