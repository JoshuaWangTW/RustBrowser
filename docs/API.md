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
| `--actions` | flag | off | Extract the operable action tree (links/forms/buttons/downloads). |
| `--max-actions` | integer | — | Cap each action category, form fields, and select options at this many entries. |

### `rustbrowser cache <action>`

| Action | Meaning |
|---|---|
| `info` | Show cache entry counts and total size. |
| `prune --older-than <secs>` | Remove entries older than the given age (default `3600`). |
| `clear` | Remove all cached entries. |

Exit code is non-zero if a `prune`/`clear` operation fails.

## MCP tools

The `rustbrowser-mcp` stdio server exposes stateless fetch tools and stateful
session tools.

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
| `extract_actions` | bool | `false` |
| `max_actions` | integer | — |

### `observe_url`

Takes the **same parameters as `fetch_url`**. Always extracts the action tree
and diagnostics, and always returns JSON (`Distilled` with `actions`). Use it
to learn what is *operable* on a page (links/forms/buttons/downloads, each with
a stable `action_id`) for a Browser-Use loop.

### `fetch_urls`

Same parameters as `fetch_url` **except** `url`/`selector`, plus:

| Param | Type | Default |
|---|---|---|
| `urls` | array of string (required) | — |
| `concurrency` | integer | `8` |

### Session tools (stateful Browser Use)

A session keeps a cookie jar, the current URL, a redirect history, and the last
snapshot (with its action tree). Navigation and successful submit tools return
the session view as JSON. The view's 1.x fields
(`{session_id, current_url, redirect_history, snapshot}`) are stable; since 1.3
it **also** carries an Action-Loop `loop` object and a debug `operation_log`
(both additive):

- `loop.state` — `{url?, status, title, excerpt?, content_chars, action_count, low_content, used_headless, steps_taken}`.
- `loop.available_actions` — array of `{action_id, kind, label, target?, method?, dangerous?, fields?}` (links/forms/buttons/downloads flattened).
- `loop.recommended_next_actions` — array of `{action_id, kind, why}` (heuristic hints, never auto-executed).
- `loop.failure_reason` — string, present only when the last step failed verification (an HTTP error status).
- `operation_log` — recent array of `{step, op, target, status?, attempt, outcome, failure_reason?}`.

`session_close` forgets the session and returns `{session_id, closed}`.

| Tool | Params | Notes |
|---|---|---|
| `session_start` | `url` (required); `profile`, `max_actions`, `timeout_secs`, `allow_local`, `respect_robots`, `max_action_retries` | Opens `url`, returns a `session_id` + first snapshot + `loop`. |
| `session_observe` | `session_id`, `url` | Navigate the session to `url` (keeps cookies). |
| `session_follow` | `session_id`, `action_id` | Follow a `link_*`/`download_*` from the last snapshot. |
| `session_submit_form` | `session_id`, `form_id`, `values` (object), `confirm` (bool) | Submit a `form_*`, merging `values` over the form's defaults. GET submits immediately; a non-GET is **withheld unless `confirm=true`** (returns `{needs_confirmation, would_submit}`). |
| `session_close` | `session_id` | Close a session and forget its cookies, URL, history, and snapshot. |

`max_action_retries` (default `1`, clamped to `0`–`2`) gives an idempotent step
(observe / follow / GET submit) extra attempts when it fails verification
(429/5xx or a transient transport error). A non-GET submit is **never**
auto-retried.

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
| `actions` | object? | `{links, forms, buttons, downloads}` — present with action extraction. Each entry has a stable `action_id`; forms carry `method`, `action`, `fields`, and `submit_id`; select options are `{value, label, selected}`. Only `http`/`https` URLs are emitted; `<base href>` is respected. |
| `diagnostics` | object? | `{profile, raw_bytes, output_chars, output_tokens, extraction_ratio, link_count, table_count, action_count, used_headless, truncated, low_content}` — present with `diagnostics`. |

> The exact Markdown text and token numbers are **not** semver-stable — see the
> stability policy. The field names and shapes above are.
