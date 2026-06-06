# Security Policy

RustBrowser fetches web content from URLs that are often **chosen by an LLM or
an untrusted upstream** — exactly the setting where Server-Side Request Forgery
(SSRF) and resource-exhaustion attacks matter. Security is therefore a primary
design goal, not an afterthought. This document describes the threat model, the
defences in place, and how to report a vulnerability.

## Supported versions

| Version | Supported |
|---|---|
| `1.x`   | ✅ security fixes |
| `< 1.0` | ❌ please upgrade |

## Reporting a vulnerability

**Please do not open a public issue for a security vulnerability.**

- Preferred: open a **private GitHub Security Advisory** on the repository
  (`Security` → `Report a vulnerability`).
- Include: affected version/commit, a description, and a minimal reproduction
  (a URL, an HTML fixture, or a sequence of options).

We aim to acknowledge a report within a few days, agree on a disclosure
timeline, and credit reporters who wish to be named.

## Threat model

The adversary controls, or can influence, **the target URL and the bytes the
server returns** (including redirects, DNS answers, and response size). The
attacker's goals we defend against:

1. **SSRF** — coerce RustBrowser into reaching internal/non-public network
   space: loopback, private LANs, cloud metadata (`169.254.169.254`),
   link-local, CGNAT, or IPv6-embedded equivalents.
2. **DNS rebinding** — pass the safety check with a public IP, then have the
   *connection* land on internal space via a second, attacker-controlled DNS
   answer (low TTL).
3. **Redirect / cache laundering** — slip an internal target in through an HTTP
   redirect, or through a URL previously written to the on-disk cache.
4. **Resource exhaustion** — exhaust memory/CPU with enormous responses, a
   gigantic post-JavaScript DOM, or a flood of requests.

Out of scope: the security of the host's Chrome/Edge install, the user's own
network policy, and content correctness (RustBrowser distils, it does not
vouch for what a page claims).

## Defences

### URL & network boundary (SSRF)

- **Scheme allowlist** — only `http` and `https` are fetched; everything else
  (`file:`, `ftp:`, `gopher:`, …) is refused before any I/O.
- **IP block-list** — connections to non-public addresses are refused:
  loopback, RFC 1918 private ranges, link-local **including the
  `169.254.169.254` cloud-metadata address**, unspecified, broadcast,
  multicast, **CGNAT `100.64.0.0/10`**, and **`0.0.0.0/8`**. For IPv6:
  loopback, unspecified, multicast, unique-local (`fc00::/7`) and unicast
  link-local (`fe80::/10`). **IPv4-mapped** (`::ffff:a.b.c.d`) and **NAT64
  well-known** (`64:ff9b::/96`) addresses are decoded back to their embedded
  IPv4 and re-checked, so they cannot smuggle an internal address past the
  filter.
- **DNS-rebinding protection at the connection layer** — a custom
  `reqwest::dns::Resolve` resolver screens **every resolved address against the
  same block-list** and hands reqwest only the addresses that pass. reqwest
  dials exactly those addresses, so there is no second, unscreened DNS lookup a
  rebinding/low-TTL record could divert. (TLS SNI and certificate validation
  still use the original hostname.)
- **Per-hop redirect re-validation** — every redirect `Location`, after being
  resolved, is re-checked against all of the above before it is followed.
- **Cache re-validation** — a URL read back from the on-disk cache is
  re-validated before it is served, so a stale entry cannot bypass current
  policy.

### `allow_local` opt-in

`--allow-local` / `allow_local` exists for hitting local dev servers and for
the integration test suite. It frees **loopback only** (`127.0.0.0/8`, `::1`,
`localhost`). Private LANs, link-local, CGNAT, and the cloud-metadata address
**remain blocked even when it is set** — including across redirects, which the
test suite explicitly verifies.

### robots.txt fetches

When `--respect-robots` is enabled, the `robots.txt` request reuses the same
SSRF-safe client, so a `robots.txt` that redirects toward internal space is
screened by the connection-layer resolver just like any other request.

### Resource limits

- **Response body cap** — the decoded body is bounded (`--max-bytes`, default
  8 MiB), read incrementally so an unbounded response cannot be buffered.
- **Headless DOM cap** — post-JavaScript DOM capture is streamed and stopped at
  16 MiB; the browser is then killed. The CDP path also slices the DOM in the
  browser so the captured payload itself is bounded.
- **Token-output budget** — `--max-output-tokens` hard-caps the distilled
  output size.
- **Politeness limits** — per-host concurrency and an optional per-host rate
  limit bound the load RustBrowser places on any single host.
- **No retry of blocked requests** — an SSRF-blocked connection is never
  retried (it would never succeed); only genuine transient failures back off.

### Headless rendering

Headless Chrome/Edge renders untrusted pages, so the **sandbox is kept enabled
by default**. `--no-sandbox` is *not* passed unless the operator explicitly
sets `RUSTBROWSER_NO_SANDBOX=1` (for containers or root where the sandbox
cannot initialise). Headless rendering is also off the hot path: the lean HTTP
fetch is the default and the browser is only launched when needed.

### Sessions & form submission

Stateful sessions (`session_*`) keep a per-session cookie jar and submit HTML
forms. The relevant safeguards:

- **Same SSRF boundary** — `follow`/`submit` resolve URLs from the page's action
  tree (already restricted to `http`/`https`) and send them through the exact
  same screened path as a plain fetch; redirects are re-validated per hop.
- **Confirmation for dangerous actions** — a non-GET submit (POST/PUT/DELETE/…)
  is **never sent automatically**. It is described back to the caller and only
  executed when `confirm=true` is passed. RB never POSTs on an agent's behalf
  without an explicit go-ahead.
- **POST is single-attempt** — a POST is never silently retried, so a
  non-idempotent action cannot be double-submitted by the retry logic.
- **Cookies are in-memory and per-session** — they live only for the life of the
  session object and are not written to disk.

## Hardening checklist for operators

- Run with the default profile of limits; only raise `--max-bytes` /
  `--max-output-tokens` when you understand the memory cost.
- Leave `RUSTBROWSER_NO_SANDBOX` **unset** unless you must run headless as root.
- Do not pass `--allow-local` to pipelines that fetch attacker-influenced URLs.
- Keep the host browser updated; RustBrowser relies on it for JS rendering.
