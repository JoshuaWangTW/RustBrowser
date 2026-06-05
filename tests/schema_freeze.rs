//! Schema-freeze guard for the CLI surface.
//!
//! The flag set below is frozen for `1.x` (see `docs/API.md`). Adding a flag is
//! fine — extend the list. Removing or renaming one is a breaking change and
//! should turn this test red first. Runs the built binary's `--help`, so it
//! checks the real parser, not a copy of it.

/// Every long flag `fetch` must keep exposing in `1.x`.
const FETCH_FLAGS: &[&str] = &[
    "--format",
    "--selector",
    "--profile",
    "--stats",
    "--diagnostics",
    "--max-output-tokens",
    "--timeout",
    "--max-bytes",
    "--no-cache",
    "--cache-ttl",
    "--concurrency",
    "--links",
    "--links-all",
    "--tables",
    "--js",
    "--js-wait",
    "--js-wait-for",
    "--allow-local",
    "--max-retries",
    "--per-host-concurrency",
    "--rate-limit",
    "--respect-robots",
    "--actions",
    "--max-actions",
];

/// Run the built `rustbrowser` binary and capture combined stdout+stderr.
/// Returns `None` if the binary was not built (e.g. `--no-default-features`),
/// in which case the freeze check is skipped rather than failing to compile.
fn help(args: &[&str]) -> Option<String> {
    let bin = option_env!("CARGO_BIN_EXE_rustbrowser")?;
    let out = std::process::Command::new(bin)
        .args(args)
        .output()
        .expect("running the rustbrowser binary");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(s)
}

#[test]
fn fetch_flag_set_is_frozen() {
    let Some(h) = help(&["fetch", "--help"]) else {
        return; // binary not built without the `cli` feature
    };
    for flag in FETCH_FLAGS {
        assert!(
            h.contains(flag),
            "frozen flag {flag} is missing from `fetch --help`"
        );
    }
}

#[test]
fn cache_subcommands_are_frozen() {
    let Some(h) = help(&["cache", "--help"]) else {
        return;
    };
    for sub in ["info", "prune", "clear"] {
        assert!(
            h.contains(sub),
            "frozen `cache` subcommand {sub} is missing"
        );
    }
}

#[test]
fn cache_prune_flags_are_frozen() {
    let Some(h) = help(&["cache", "prune", "--help"]) else {
        return;
    };
    assert!(
        h.contains("--older-than"),
        "frozen `cache prune` flag --older-than is missing"
    );
}
