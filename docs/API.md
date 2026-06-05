# API Reference (frozen for 1.x)

This is the stable public surface of RustBrowser. Under
[semver](../README.md#穩定性與版本-stability--versioning), nothing here is removed
or renamed within `1.x`; new flags/params/fields may be **added**. A guard test
(`tests/schema_freeze.rs` and the MCP schema test) fails if the CLI flag set or
MCP parameter set drifts from this document.

## CLI

### `rustbrowser fetch <url>...`

Positional: one or more URLs (multiple are fetched concurrently as a batch).

| Flag | Type / values | Default | Meaning |
|---|---|---|---|
| `--format` | `markdown` \| `text` \| `json` | `markdown` | Output format. |
| `--selector` | CSS selector | — | Extract matching elements instead of running a profile. |
| `--profile` | `article` \| `full` \| `metadata` | `article` | Content selection (ignored when `--selector` is set). |
| `--stats` | flag | off | Print token-savings stats to stderr. |
| `--diagnostics` | flag | off | Print extraction-quality diagnostics to stderr; with `--format json`, include diagnostics in the JSON result. |
| `--max-output-tokens` | integer | — | Truncate Markdown/text output to this many tokens. |
| `--timeout` | seconds | `20` | Request timeout. |
| `--max-bytes` | bytes | `8388608` | Cap on decoded response body. |
| `--no-cache` | flag | off | Skip the on-disk cache. |
| `--cache-ttl` | seconds | `3600` | Cache freshness window. |
| `--concurrency` | integer | `8` | Max concurrent requests for a multi-URL batch. |
| `--links` | flag | off | Extract main-content links as structured data. |
| `--links-all` | flag | off | Extract ALL links (whole page) for crawling. |
| `--tables` | flag | off | Extract tables as structured data. |
| `--js` | `off` \| `auto` \| `always` | `auto` | Headless JS-rendering fallback. |
| `--js-wait` | milliseconds | — | Headless wait / virtual-time budget. |
| `--js-wait-for` | CSS selector | — | Wait until this selector appears (CDP). |
| `--allow-local` | flag | off | Permit loopback targets (only loopback; see SECURITY.md). |
| `--max-retries` | integer | `2` | Retry transient failures (connect/timeout, 429, 5xx). |
| `--per-host-concurrency` | integer | `4` | Max simultaneous requests per host (0 = unlimited). |
| `--rate-limit` | requests/sec | `0` | Per-host rate limit (0 = off). |
| `--respect-robots` | flag | off | Honour each host's robots.txt. |

### `rustbrowser cache <action>`

| Action | Meaning |
|---|---|
| `info` | Show cache entry counts and total size. |
| `prune --older-than <secs>` | Remove entries older than the given age (default `3600`). |
| `clear` | Remove all cached entries. |

Exit code is non-zero if a `prune`/`clear` operation fails.

## MCP tools

The `rustbrowser-mcp` stdio server exposes two tools.

### `fetch_url`

| Param | Type | Default |
|---|---|---|
| `url` | string (required) | — |
| `format` | string (`markdown`/`text`/`json`) | `markdown` |
| `selector` | string | — |
| `profile` | string (`article`/`full`/`metadata`) | `article` |
| `stats` | bool | `false` |
| `diagnostics` | bool | `false` |
| `max_output_tokens` | integer | — |
| `timeout_secs` | integer | `20` |
| `max_bytes` | integer | `8388608` |
| `no_cache` | bool | `false` |
| `cache_ttl` | integer | `3600` |
| `extract_links` | bool | `false` |
| `extract_tables` | bool | `false` |
| `links_full` | bool | `false` |
| `js` | string (`off`/`auto`/`always`) | `auto` |
| `js_wait` | integer (ms) | — |
| `js_wait_for` | string | — |
| `allow_local` | bool | `false` |
| `max_retries` | integer | `2` |
| `per_host_concurrency` | integer | `4` |
| `rate_limit` | number (req/sec) | `0` |
| `respect_robots` | bool | `false` |

### `fetch_urls`

Same parameters as `fetch_url` **except** `url`/`selector`, plus:

| Param | Type | Default |
|---|---|---|
| `urls` | array of string (required) | — |
| `concurrency` | integer | `8` |

## JSON output schema (`Distilled`)

`--format json` (and MCP `format=json`) serialise this object. Optional fields
are omitted when absent.

| Field | Type | Notes |
|---|---|---|
| `final_url` | string | URL after redirects. |
| `status` | integer | HTTP status. |
| `title` | string | Extracted title. |
| `byline` | string? | Author, if found. |
| `excerpt` | string? | Summary, if found. |
| `markdown` | string | Distilled Markdown (token-budgeted if requested). |
| `stats` | object? | `{raw_bytes, raw_tokens, output_tokens, saved_tokens, saved_ratio}` — present with `stats`. |
| `links` | array? | `{href, text}` — present with link extraction. |
| `tables` | array? | `{headers, rows}` — present with table extraction. |
| `diagnostics` | object? | `{profile, raw_bytes, output_chars, output_tokens, extraction_ratio, link_count, table_count, used_headless, truncated, low_content}` — present with `diagnostics`. |

> The exact Markdown text and token numbers are **not** semver-stable — see the
> stability policy. The field names and shapes above are.
