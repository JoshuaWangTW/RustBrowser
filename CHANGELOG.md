# Changelog

All notable changes to RustBrowser are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/) from `1.0.0` onward (see
[Stability & versioning](README.md#穩定性與版本-stability--versioning)).

## [1.0.0]

First stable release. The public surface is now **frozen under semver**: CLI
flags, MCP tool parameters, and the library's public API will not break within
`1.x`.

### Added
- **Stability guarantees** — documented semver policy for the CLI, MCP schema,
  and library API.
- **`SECURITY.md`** — threat model, the full SSRF/DNS-rebinding/resource-limit
  defence set, and a vulnerability-reporting process.
- **`docs/API.md`** — frozen reference for the CLI, the MCP tools, and the
  `Distilled` JSON output schema, plus a guard test that fails if the CLI flag
  set or MCP parameter set changes unexpectedly.
- **CI across Linux, Windows, and macOS**; release binaries for all three
  (Linux x86_64, Windows x86_64, macOS x86_64 + aarch64) with SHA-256
  checksums.

## [0.9.0]
### Added
- **Extraction profiles** — `article` (Readability, default), `full` (whole
  `<body>`, no Readability filtering), `metadata` (title + short summary).
- **Token-output budget** — `--max-output-tokens` truncates Markdown/text at a
  paragraph boundary; the truncation marker counts toward the hard cap.
- **Quality diagnostics** — `--diagnostics` reports extraction ratio, link/table
  counts, headless use, truncation, and a `low_content` warning.
- **`distill_html`** offline pipeline and a fixed extraction-quality eval set
  (`tests/fixtures/` + `tests/eval.rs`).

## [0.8.0]
### Added
- **Automatic retry** with exponential backoff + jitter for transient failures
  (connect/timeout, `429`, `5xx`), honouring `Retry-After`.
- **Per-host concurrency** limit and optional **per-host rate limit**.
- **robots.txt** support (opt-in `--respect-robots`) via `texting_robots`,
  enforced per request hop (redirects included) and on cache hits.

## [0.7.0]
### Fixed
- Headless DOM cap now **streams** stdout and bounds memory for real.
- `cache` subcommand returns a non-zero exit code on failure.
- MCP server handles transport disconnects/errors cleanly with explicit exit
  codes; diagnostics never pollute stdout.

## [0.6.0]
### Added
- End-to-end integration tests (wiremock).
- `--allow-local` loopback opt-in (private/link-local/metadata stay blocked).
- `cache info` / `prune` / `clear` maintenance subcommands.
### Changed
- Headless sandbox kept enabled by default (`RUSTBROWSER_NO_SANDBOX` to opt out);
  rendered DOM size cap.

## [0.5.0]
### Added
- Whole-page link extraction; headless wait controls (`--js-wait`,
  CDP `--js-wait-for`); release-binary automation.

## [0.4.0]
### Added
- Automatic headless fallback for JS-rendered pages; structured link/table
  extraction.

## [0.3.0]
### Added
- MCP server exposing `fetch_url` / `fetch_urls`.

## [0.2.0]
### Added
- On-disk cache (layered fetch + render) and concurrent batch fetching.

## [0.1.0]
### Added
- Initial pipeline: fetch → Readability → Markdown → token stats, with a CLI.
