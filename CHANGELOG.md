# Changelog

All notable changes to RustBrowser are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/) from `1.0.0` onward (see
[Stability & versioning](README.md#穩定性與版本-stability--versioning)).

## [1.2.0]

Second Browser-Use step: **stateful sessions** so an agent can act, not just
observe.

### Added
- **Sessions** (`Session` in the library; `session_start` / `session_observe` /
  `session_follow` / `session_submit_form` / `session_close` MCP tools) — keep
  a cookie jar, current URL, redirect history, and the last snapshot's action
  tree.
- **HTML form submission** — GET forms become a query string; POST forms send a
  urlencoded body. The form's own defaults (hidden CSRF fields, selected
  options) are merged with the caller's values automatically. POST is single-
  attempt (never silently retried).
- **Dangerous-action confirmation** — a non-GET submit is described but **not
  sent** unless the caller passes `confirm=true`.
- **Session lifecycle guardrails** — `session_close` forgets cookies/snapshots,
  and the MCP server caps live sessions.
- **POST redirect hardening** — 307/308 POST redirects only preserve submitted
  body on same-origin hops; cross-origin body forwarding is blocked.
- Every session request reuses the same SSRF-screened path as a plain fetch.

## [1.1.0]

First step toward **RB-first Browser Use**: RB can now tell an agent what is
*operable* on a page, not just what it says.

### Added
- **Action tree** (`--actions` / MCP `extract_actions`) — extracts links, forms
  (with their fields and a `submit_id`), standalone buttons, and downloads, each
  with a stable `action_id` (e.g. `link_3`, `form_0.submit`). Per-category caps
  (`--max-actions`) keep it token-lean.
- **MCP `observe_url` tool** — returns a page's distilled content plus its action
  tree as JSON, for "what can I do next?" decisions. `fetch_url` / `fetch_urls`
  stay backward compatible (opt in via `extract_actions`).
- `Diagnostics.action_count`; an `actions` fixture + eval coverage.

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
