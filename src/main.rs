//! RustBrowser CLI binary — a thin shell over the rustbrowser core library.

mod cli;

use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::json;

use cli::{CacheAction, CacheArgs, Cli, Command, FetchArgs, Format};
use rustbrowser::cache;
use rustbrowser::{DistillOptions, Distilled, distill, distill_many};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Fetch(args) => run_fetch(args).await,
        Command::Cache(args) => run_cache(args),
    }
}

/// Inspect or clean the on-disk cache. Returns an error — and so a non-zero exit
/// code — when a prune/clear operation fails, so scripts can detect it.
fn run_cache(args: CacheArgs) -> Result<()> {
    match args.action {
        CacheAction::Info => {
            let r = cache::report();
            println!(
                "fetch  : {:>5} entries  {}",
                r.fetch_entries,
                human_bytes(r.fetch_bytes)
            );
            println!(
                "render : {:>5} entries  {}",
                r.render_entries,
                human_bytes(r.render_bytes)
            );
            println!(
                "total  : {:>5} entries  {}",
                r.total_entries(),
                human_bytes(r.total_bytes())
            );
        }
        CacheAction::Prune { older_than } => {
            let p = cache::prune(older_than).context("pruning cache")?;
            println!(
                "pruned {} entries older than {older_than}s ({} freed)",
                p.removed,
                human_bytes(p.bytes)
            );
        }
        CacheAction::Clear => {
            let p = cache::clear().context("clearing cache")?;
            println!(
                "cleared {} entries ({} freed)",
                p.removed,
                human_bytes(p.bytes)
            );
        }
    }
    Ok(())
}

/// Convert a per-host rate limit in requests/second to a minimum spacing
/// between requests. Non-positive (or absurd) rates disable rate limiting.
fn rate_to_interval(reqs_per_sec: f64) -> Duration {
    if reqs_per_sec.is_finite() && reqs_per_sec > 0.0 {
        Duration::from_secs_f64(1.0 / reqs_per_sec)
    } else {
        Duration::ZERO
    }
}

/// Human-readable byte size (binary units).
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
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
        extract_links: args.links || args.links_all,
        extract_tables: args.tables,
        links_full: args.links_all,
        js_mode: match args.js {
            cli::JsMode::Off => rustbrowser::JsMode::Off,
            cli::JsMode::Auto => rustbrowser::JsMode::Auto,
            cli::JsMode::Always => rustbrowser::JsMode::Always,
        },
        js_wait: args.js_wait,
        js_wait_for: args.js_wait_for.clone(),
        max_bytes: args.max_bytes,
        allow_local: args.allow_local,
        max_retries: args.max_retries,
        per_host_concurrency: args.per_host_concurrency,
        min_request_interval: rate_to_interval(args.rate_limit),
        respect_robots: args.respect_robots,
        profile: match args.profile {
            cli::Profile::Article => rustbrowser::Profile::Article,
            cli::Profile::Full => rustbrowser::Profile::Full,
            cli::Profile::Metadata => rustbrowser::Profile::Metadata,
        },
        max_output_tokens: args.max_output_tokens,
        diagnostics: args.diagnostics,
    };

    if args.urls.len() == 1 {
        let result = distill(&args.urls[0], &opts).await?;
        print_result(&result, args.format);
        print_extras(&result, args.format);
        print_stats(&result);
        print_diagnostics(&result);
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

/// Print extraction-quality diagnostics to stderr (only when --diagnostics set).
fn print_diagnostics(result: &Distilled) {
    if let Some(d) = &result.diagnostics {
        eprintln!();
        eprintln!("── diagnostics ───────────────");
        eprintln!("profile          : {}", d.profile);
        eprintln!("raw bytes        : {:>8}", d.raw_bytes);
        eprintln!("output chars     : {:>8}", d.output_chars);
        eprintln!("output tokens    : {:>8}", d.output_tokens);
        eprintln!("extraction ratio : {:>8.4}", d.extraction_ratio);
        eprintln!("links / tables   : {} / {}", d.link_count, d.table_count);
        eprintln!("headless         : {}", d.used_headless);
        eprintln!("truncated        : {}", d.truncated);
        if d.low_content {
            eprintln!("⚠ low content — extraction may have failed; try --profile full");
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
