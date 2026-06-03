//! RustBrowser CLI binary — a thin shell over the rustbrowser core library.

mod cli;

use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use serde_json::json;

use cli::{Cli, Command, FetchArgs, Format};
use rustbrowser::{DistillOptions, Distilled, distill, distill_many};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Fetch(args) => run_fetch(args).await,
    }
}

async fn run_fetch(args: FetchArgs) -> Result<()> {
    let opts = DistillOptions {
        timeout: Duration::from_secs(args.timeout),
        user_agent: None,
        selector: args.selector.clone(),
        measure_tokens: args.stats,
        use_cache: !args.no_cache,
        cache_ttl: args.cache_ttl,
        extract_links: args.links,
        extract_tables: args.tables,
        js_mode: match args.js {
            cli::JsMode::Off => rustbrowser::JsMode::Off,
            cli::JsMode::Auto => rustbrowser::JsMode::Auto,
            cli::JsMode::Always => rustbrowser::JsMode::Always,
        },
    };

    if args.urls.len() == 1 {
        let result = distill(&args.urls[0], &opts).await?;
        print_result(&result, args.format);
        print_extras(&result, args.format);
        print_stats(&result);
    } else {
        run_batch(&args, &opts).await;
    }
    Ok(())
}

/// Fetch multiple URLs concurrently and print them in input order.
async fn run_batch(args: &FetchArgs, opts: &DistillOptions) {
    let results = distill_many(&args.urls, opts, args.concurrency).await;

    if matches!(args.format, Format::Json) {
        let arr: Vec<_> = results
            .iter()
            .map(|(url, r)| match r {
                Ok(d) => json!({ "url": url, "ok": true, "result": d }),
                Err(e) => json!({ "url": url, "ok": false, "error": e.to_string() }),
            })
            .collect();
        match serde_json::to_string_pretty(&arr) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("JSON output failed: {e}"),
        }
        return;
    }

    let mut first = true;
    for (url, r) in &results {
        match r {
            Ok(d) => {
                if !first {
                    println!("\n\n═══════════════════════════════\n");
                }
                first = false;
                println!("<!-- {url} -->");
                print_result(d, args.format);
                print_extras(d, args.format);
            }
            Err(e) => eprintln!("✗ {url}: {e}"),
        }
    }
}

fn print_result(result: &Distilled, format: Format) {
    match format {
        Format::Markdown => {
            if !result.title.is_empty() {
                println!("# {}\n", result.title);
            }
            println!("{}", result.markdown);
        }
        Format::Text => println!("{}", result.text),
        Format::Json => match serde_json::to_string_pretty(result) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("JSON output failed: {e}"),
        },
    }
}

/// In non-JSON output, append structured links/tables as readable Markdown.
/// (JSON already carries them in the serialised result.)
fn print_extras(result: &Distilled, format: Format) {
    if matches!(format, Format::Json) {
        return;
    }
    if let Some(links) = &result.links {
        println!("\n## Links ({})\n", links.len());
        for l in links {
            let label = if l.text.is_empty() { &l.href } else { &l.text };
            println!("- [{label}]({})", l.href);
        }
    }
    if let Some(tables) = &result.tables {
        for (i, t) in tables.iter().enumerate() {
            println!("\n## Table {}\n", i + 1);
            if !t.headers.is_empty() {
                println!("| {} |", t.headers.join(" | "));
                let sep: Vec<&str> = t.headers.iter().map(|_| "---").collect();
                println!("| {} |", sep.join(" | "));
            }
            for row in &t.rows {
                println!("| {} |", row.join(" | "));
            }
        }
    }
}

fn print_stats(result: &Distilled) {
    // `result.stats` is Some only when --stats was passed.
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
}
