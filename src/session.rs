//! Stateful browsing session for the **RB Action Loop** (Observe → Act → Verify).
//!
//! A `Session` keeps a cookie jar, the current URL, a redirect history, the last
//! observed snapshot (distilled content + action tree), and a debug log of every
//! operation. An agent drives it by `observe`-ing a URL, then `follow`-ing a link
//! or `submit_form`-ing by the stable `action_id`s in the last snapshot — never
//! opening a real browser. After each step, [`Session::loop_view`] yields a
//! compact, planner-friendly view (state + available/recommended actions +
//! failure reason).
//!
//! Verify & retry: after an idempotent step (observe / follow / GET submit) the
//! snapshot is verified; a *retryable* failure (429/5xx, or a transient transport
//! error) is given up to `max_action_retries` more attempts. A non-GET submit is
//! a *dangerous* action: it is refused unless the caller confirms, and is **never
//! retried** — RB never silently re-sends a POST.
//!
//! Fallback: when RB-only extraction looks insufficient (unrendered JS app,
//! anti-bot challenge, action surface built client-side), the Chrome Fallback
//! Broker escalates ONE bounded headless render and re-distills it through the
//! same token-lean pipeline — see [`crate::fallback`]. Only idempotent steps
//! escalate; a confirmed non-GET submit's result page is never re-fetched.
//!
//! Safety: every request reuses the same SSRF-screened path as plain fetches.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::time::sleep;

use crate::actions::FormAction;
use crate::fallback::{self, FallbackReason};
use crate::fetch::{self, FetchOptions, FetchResult, Fetcher, SubmitMethod};
use crate::planner::{self, LoopView, OpLogEntry};
use crate::{DistillOptions, Distilled, JsMode, distill_html, render};

/// Default extra attempts for an idempotent step whose verify failed.
const DEFAULT_MAX_ACTION_RETRIES: usize = 1;
/// Hard ceiling on auto-retries (roadmap: "at most 1–2").
const MAX_ACTION_RETRIES_CAP: usize = 2;
/// Keep at most this many operation-log entries (most recent win).
const MAX_LOG_ENTRIES: usize = 200;
/// Truncate a logged error message to this many characters.
const MAX_LOGGED_ERR_CHARS: usize = 200;

/// A stateful browsing session.
pub struct Session {
    fetcher: Fetcher,
    /// Distill options used for every snapshot (always extracts the action tree).
    opts: DistillOptions,
    current_url: Option<String>,
    redirect_history: Vec<String>,
    last_snapshot: Option<Distilled>,
    /// Extra attempts for an idempotent step that fails verification.
    max_action_retries: usize,
    /// Verify result of the most recent step (`None` = looked OK).
    last_failure: Option<String>,
    /// Operation log for debugging the loop.
    log: Vec<OpLogEntry>,
    /// Logical operation counter: one per observe/follow/submit_form call.
    /// Every log entry an operation produces (retries included) shares it.
    step: usize,
    /// Why the Chrome Fallback Broker escalated the last settled step
    /// (`None` = RB-only extraction was enough).
    last_fallback: Option<String>,
}

/// What happened when a form submit was requested.
#[derive(Debug, Clone)]
pub enum SubmitOutcome {
    /// The form was submitted and the snapshot updated.
    Submitted,
    /// A dangerous (non-GET) submit was withheld pending confirmation. Nothing
    /// was sent; this describes exactly what *would* be sent.
    NeedsConfirmation {
        method: String,
        action: String,
        fields: Vec<(String, String)>,
    },
}

impl Session {
    /// Start a session. The given options seed every snapshot; cookies persist
    /// across requests and the action tree is always extracted.
    pub fn new(opts: DistillOptions) -> Result<Self> {
        let mut fopts = FetchOptions {
            timeout: opts.timeout,
            max_bytes: opts.max_bytes,
            allow_local: opts.allow_local,
            // The Action Loop owns the per-step retry budget. Keep each loop
            // attempt to one HTTP attempt so `max_action_retries` maps directly
            // to actual network retries.
            max_retries: 0,
            per_host_concurrency: opts.per_host_concurrency,
            min_request_interval: opts.min_request_interval,
            respect_robots: opts.respect_robots,
            cookie_store: true,
            ..Default::default()
        };
        if let Some(ua) = &opts.user_agent {
            fopts.user_agent = ua.clone();
        }

        // Snapshots always carry the action tree and diagnostics; caching is off
        // so a session always sees live state.
        let mut snapshot_opts = opts;
        snapshot_opts.extract_actions = true;
        snapshot_opts.diagnostics = true;
        snapshot_opts.use_cache = false;

        Ok(Self {
            fetcher: Fetcher::new(fopts)?,
            opts: snapshot_opts,
            current_url: None,
            redirect_history: Vec::new(),
            last_snapshot: None,
            max_action_retries: DEFAULT_MAX_ACTION_RETRIES,
            last_failure: None,
            log: Vec::new(),
            step: 0,
            last_fallback: None,
        })
    }

    /// Set how many extra attempts an idempotent step gets when verification
    /// fails (clamped to the roadmap's 0–2). Non-GET submits are never retried.
    pub fn with_max_action_retries(mut self, n: usize) -> Self {
        self.max_action_retries = n.min(MAX_ACTION_RETRIES_CAP);
        self
    }

    pub fn current_url(&self) -> Option<&str> {
        self.current_url.as_deref()
    }

    pub fn redirect_history(&self) -> &[String] {
        &self.redirect_history
    }

    pub fn snapshot(&self) -> Option<&Distilled> {
        self.last_snapshot.as_ref()
    }

    /// The verify result of the most recent step (`None` = looked OK).
    pub fn last_failure(&self) -> Option<&str> {
        self.last_failure.as_deref()
    }

    /// Why the Chrome Fallback Broker escalated the last settled step
    /// (`challenge`, `js_app`, `no_actions`, `forced`); `None` = no escalation.
    pub fn last_fallback(&self) -> Option<&str> {
        self.last_fallback.as_deref()
    }

    /// The full operation log.
    pub fn log(&self) -> &[OpLogEntry] {
        &self.log
    }

    /// The most recent `n` operation-log entries.
    pub fn recent_log(&self, n: usize) -> &[OpLogEntry] {
        let start = self.log.len().saturating_sub(n);
        &self.log[start..]
    }

    /// A compact, planner-friendly view of the current state, available actions,
    /// recommended next actions, and any failure reason.
    pub fn loop_view(&self) -> LoopView {
        let mut view = planner::loop_view(
            self.last_snapshot.as_ref(),
            self.last_failure.clone(),
            self.step,
        );
        view.state.fallback_reason = self.last_fallback.clone();
        view
    }

    /// Fetch `url` and make it the current snapshot (idempotent: verified +
    /// retried on a transient failure).
    pub async fn observe(&mut self, url: &str) -> Result<&Distilled> {
        self.run_idempotent("observe", url.to_string(), url).await
    }

    /// Follow a `link_*` / `download_*` action from the last snapshot
    /// (idempotent: verified + retried on a transient failure).
    pub async fn follow(&mut self, action_id: &str) -> Result<&Distilled> {
        let href = self.resolve_followable(action_id)?;
        self.run_idempotent("follow", href.clone(), &href).await
    }

    /// Submit a `form_*` from the last snapshot, merging the form's own default
    /// values (hidden fields, selected options) with the caller's `values`.
    /// A non-GET submit requires `confirm = true` and is never auto-retried.
    pub async fn submit_form(
        &mut self,
        form_id: &str,
        values: &[(String, String)],
        confirm: bool,
    ) -> Result<SubmitOutcome> {
        let form = self.resolve_form(form_id)?;
        let method = if form.method.eq_ignore_ascii_case("POST") {
            SubmitMethod::Post
        } else {
            SubmitMethod::Get
        };
        let fields = merge_form_values(&form, values);

        // Non-GET is a dangerous action: never auto-execute without confirmation.
        if method != SubmitMethod::Get && !confirm {
            self.begin_step();
            self.log_attempt(
                "submit_form",
                &form.action,
                None,
                0,
                "needs_confirmation",
                None,
            );
            return Ok(SubmitOutcome::NeedsConfirmation {
                method: form.method.clone(),
                action: form.action.clone(),
                fields,
            });
        }

        // GET form submit is idempotent: the fields become the query string and
        // the step is verified + retried like an observe.
        if method == SubmitMethod::Get {
            let url = fetch::build_query_url(&form.action, &fields)?;
            self.run_idempotent("submit_form", form.action.clone(), &url)
                .await?;
            return Ok(SubmitOutcome::Submitted);
        }

        // Confirmed non-GET: a single attempt, never silently retried.
        self.begin_step();
        let result = match self.fetcher.submit(&form.action, method, &fields).await {
            Ok(r) => r,
            Err(e) => {
                self.log_attempt(
                    "submit_form",
                    &form.action,
                    None,
                    1,
                    "error",
                    Some(short_err(&e)),
                );
                return Err(e);
            }
        };
        self.settle("submit_form", &form.action, 1, result, false)
            .await?;
        Ok(SubmitOutcome::Submitted)
    }

    /// Run an idempotent step (a GET of `url`) with verify + bounded retry. Only
    /// the result we actually keep is recorded, so a discarded retryable attempt
    /// never advances session state, and `redirect_history` gets one entry per
    /// settled navigation. Retries back off — honouring the server's
    /// `Retry-After` when it sent one — so the loop never hammers a host that
    /// just asked us to slow down.
    async fn run_idempotent(&mut self, op: &str, target: String, url: &str) -> Result<&Distilled> {
        self.begin_step();
        let mut attempt = 0usize;
        loop {
            match self.fetcher.fetch_attempt(url).await {
                Ok((result, retry_after)) => {
                    let status = result.status;
                    // Server said "try later" and we still have budget: discard
                    // this response (don't advance state), back off, retry.
                    if attempt < self.max_action_retries && fetch::is_retryable_status(status) {
                        self.log_attempt(
                            op,
                            &target,
                            Some(status),
                            attempt + 1,
                            "retryable_status",
                            Some(format!("http_status_{status}")),
                        );
                        sleep(retry_after.unwrap_or_else(|| fetch::backoff_delay(attempt))).await;
                        attempt += 1;
                        continue;
                    }
                    // Keep this result (idempotent step: the broker may escalate).
                    self.settle(op, &target, attempt + 1, result, true).await?;
                    return self.snapshot_ref();
                }
                Err(e) => {
                    // A transient transport error gets one more whole-step try
                    // under the Action Loop budget.
                    let retry = attempt < self.max_action_retries && fetch::is_transient_error(&e);
                    self.log_attempt(
                        op,
                        &target,
                        None,
                        attempt + 1,
                        if retry {
                            "transient_error_retry"
                        } else {
                            "error"
                        },
                        Some(short_err(&e)),
                    );
                    if retry {
                        sleep(fetch::backoff_delay(attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Keep a settled fetch result: distill it into the snapshot (atomic — a
    /// distill failure leaves prior state intact), let the Chrome Fallback
    /// Broker escalate once when RB-only extraction looks insufficient, verify,
    /// commit the navigation to history, and log the outcome. Shared by the
    /// idempotent retry loop and the single-attempt confirmed non-GET submit —
    /// the latter passes `allow_fallback = false`, because a POST's result page
    /// must never be re-fetched by a browser.
    async fn settle(
        &mut self,
        op: &str,
        target: &str,
        attempt: usize,
        result: FetchResult,
        allow_fallback: bool,
    ) -> Result<()> {
        let status = result.status;
        let mut snap = match self.distill_result(&result) {
            Ok(s) => s,
            Err(e) => {
                self.log_attempt(
                    op,
                    target,
                    Some(status),
                    attempt,
                    "distill_failed",
                    Some(short_err(&e)),
                );
                return Err(e);
            }
        };

        // Chrome Fallback Broker (Observe → Act → Verify → **Fallback**): one
        // bounded headless render when RB alone is not enough; the rendered DOM
        // is re-distilled through the same token-lean pipeline, so the caller
        // still gets compressed content + action tree — never a raw DOM. A
        // failed render is non-fatal: the HTTP snapshot stands.
        self.last_fallback = None;
        if allow_fallback && let Some(reason) = self.fallback_decision(&snap, &result.html) {
            self.last_fallback = Some(reason.label().to_string());
            match self.render_fallback(&result.final_url).await {
                Ok(rendered) => {
                    let rendered_result = FetchResult {
                        final_url: result.final_url.clone(),
                        status,
                        content_type: result.content_type.clone(),
                        raw_bytes: rendered.len(),
                        html: rendered,
                    };
                    match self.distill_result(&rendered_result) {
                        Ok(mut rendered_snap) => {
                            if let Some(d) = rendered_snap.diagnostics.as_mut() {
                                d.used_headless = true;
                            }
                            snap = rendered_snap;
                            self.log_attempt(
                                op,
                                target,
                                Some(status),
                                attempt,
                                "chrome_fallback",
                                Some(reason.label().to_string()),
                            );
                        }
                        Err(e) => self.log_attempt(
                            op,
                            target,
                            Some(status),
                            attempt,
                            "chrome_fallback_failed",
                            Some(short_err(&e)),
                        ),
                    }
                }
                Err(e) => self.log_attempt(
                    op,
                    target,
                    Some(status),
                    attempt,
                    "chrome_fallback_failed",
                    Some(short_err(&e)),
                ),
            }
        }

        let failure = planner::verify(&snap);
        self.last_failure = failure.clone();
        self.current_url = Some(result.final_url);
        self.last_snapshot = Some(snap);
        self.commit_navigation();
        self.log_attempt(
            op,
            target,
            Some(status),
            attempt,
            outcome_label(&failure),
            failure,
        );
        Ok(())
    }

    /// Distill a fetch result into a snapshot. Pure with respect to session
    /// state — nothing is mutated here, so a distill failure leaves the
    /// previous snapshot/URL/history intact.
    fn distill_result(&self, result: &FetchResult) -> Result<Distilled> {
        let mut snap = distill_html(&result.html, &result.final_url, &self.opts)
            .with_context(|| format!("distilling session snapshot for {}", result.final_url))?;
        // distill_html stamps a synthetic 200; carry the real HTTP status.
        snap.status = result.status;
        Ok(snap)
    }

    /// The broker's policy gate: `Off` never escalates, `Always` forces one
    /// render per settled step, `Auto` asks [`fallback::assess`].
    fn fallback_decision(&self, snap: &Distilled, raw_html: &str) -> Option<FallbackReason> {
        match self.opts.js_mode {
            JsMode::Off => None,
            JsMode::Always => Some(FallbackReason::Forced),
            JsMode::Auto => fallback::assess(snap, raw_html),
        }
    }

    /// One bounded headless render of `url`. The render is a separate browser
    /// process: the session's cookie jar is NOT carried into it (see
    /// SECURITY.md), so a page behind a session login may render differently.
    async fn render_fallback(&self, url: &str) -> Result<String> {
        let budget = self
            .opts
            .js_wait
            .map(Duration::from_millis)
            .unwrap_or(self.opts.timeout);
        render::render_html(url, budget).await
    }

    /// Record the settled current URL in the redirect history (one entry per
    /// successful navigation, not per retry attempt).
    fn commit_navigation(&mut self) {
        if let Some(url) = self.current_url.clone() {
            self.redirect_history.push(url);
        }
    }

    /// Start a new logical operation: every log entry it produces — including
    /// discarded retry attempts — shares this step number, with `attempt`
    /// telling them apart.
    fn begin_step(&mut self) {
        self.step += 1;
    }

    fn log_attempt(
        &mut self,
        op: &str,
        target: &str,
        status: Option<u16>,
        attempt: usize,
        outcome: &str,
        failure_reason: Option<String>,
    ) {
        self.log.push(OpLogEntry {
            step: self.step,
            op: op.to_string(),
            target: target.to_string(),
            status,
            attempt,
            outcome: outcome.to_string(),
            failure_reason,
        });
        if self.log.len() > MAX_LOG_ENTRIES {
            let drop = self.log.len() - MAX_LOG_ENTRIES;
            self.log.drain(0..drop);
        }
    }

    fn snapshot_ref(&self) -> Result<&Distilled> {
        self.last_snapshot
            .as_ref()
            .ok_or_else(|| anyhow!("snapshot could not be distilled"))
    }

    /// Resolve a `link_*` or `download_*` action id to its absolute URL.
    fn resolve_followable(&self, action_id: &str) -> Result<String> {
        let actions = self
            .last_snapshot
            .as_ref()
            .and_then(|s| s.actions.as_ref())
            .ok_or_else(|| anyhow!("no action tree to follow; observe a page first"))?;
        if let Some(l) = actions.links.iter().find(|l| l.action_id == action_id) {
            return Ok(l.href.clone());
        }
        if let Some(d) = actions.downloads.iter().find(|d| d.action_id == action_id) {
            return Ok(d.href.clone());
        }
        bail!("no followable action '{action_id}' in the current snapshot")
    }

    fn resolve_form(&self, form_id: &str) -> Result<FormAction> {
        self.last_snapshot
            .as_ref()
            .and_then(|s| s.actions.as_ref())
            .and_then(|a| a.forms.iter().find(|f| f.action_id == form_id))
            .cloned()
            .ok_or_else(|| anyhow!("no form '{form_id}' in the current snapshot"))
    }
}

/// Map a verify result to an operation-log outcome label.
fn outcome_label(failure: &Option<String>) -> &'static str {
    if failure.is_some() {
        "verify_failed"
    } else {
        "ok"
    }
}

/// Render an error chain to a single bounded line for the operation log.
fn short_err(e: &anyhow::Error) -> String {
    let s = format!("{e:#}");
    if s.chars().count() > MAX_LOGGED_ERR_CHARS {
        let head: String = s.chars().take(MAX_LOGGED_ERR_CHARS).collect();
        format!("{head}…")
    } else {
        s
    }
}

/// Merge a form's own default field values with caller-supplied `values`
/// (caller wins). Hidden fields (e.g. CSRF tokens) and selected options are
/// carried automatically so the submit is well-formed.
fn merge_form_values(form: &FormAction, values: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for f in &form.fields {
        if f.name.is_empty() {
            continue;
        }
        if let Some(v) = &f.value {
            out.push((f.name.clone(), v.clone()));
        } else if f.kind == "select"
            && let Some(opt) = f.options.iter().find(|o| o.selected).or(f.options.first())
        {
            out.push((f.name.clone(), opt.value.clone()));
        }
    }
    for (k, v) in values {
        out.retain(|(ek, _)| ek != k);
        out.push((k.clone(), v.clone()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{FormField, FormOption};

    fn form() -> FormAction {
        FormAction {
            action_id: "form_0".into(),
            method: "POST".into(),
            action: "https://example.com/login".into(),
            submit_id: "form_0.submit".into(),
            fields: vec![
                FormField {
                    name: "csrf".into(),
                    kind: "hidden".into(),
                    value: Some("tok".into()),
                    options: vec![],
                    required: false,
                },
                FormField {
                    name: "user".into(),
                    kind: "text".into(),
                    value: None,
                    options: vec![],
                    required: true,
                },
                FormField {
                    name: "role".into(),
                    kind: "select".into(),
                    value: None,
                    options: vec![
                        FormOption {
                            value: "admin".into(),
                            label: "Admin".into(),
                            selected: false,
                        },
                        FormOption {
                            value: "user".into(),
                            label: "User".into(),
                            selected: true,
                        },
                    ],
                    required: false,
                },
            ],
        }
    }

    #[test]
    fn merge_keeps_defaults_and_applies_user_values() {
        let merged = merge_form_values(&form(), &[("user".into(), "alice".into())]);
        // Hidden csrf carried automatically.
        assert!(merged.contains(&("csrf".into(), "tok".into())));
        // User value applied.
        assert!(merged.contains(&("user".into(), "alice".into())));
        // Selected option's value used for the select.
        assert!(merged.contains(&("role".into(), "user".into())));
    }

    #[test]
    fn user_value_overrides_default() {
        let merged = merge_form_values(&form(), &[("csrf".into(), "evil".into())]);
        let csrf: Vec<_> = merged.iter().filter(|(k, _)| k == "csrf").collect();
        assert_eq!(csrf.len(), 1);
        assert_eq!(csrf[0].1, "evil");
    }

    #[test]
    fn max_action_retries_is_clamped() {
        let opts = DistillOptions::default();
        let s = Session::new(opts).unwrap().with_max_action_retries(99);
        assert_eq!(s.max_action_retries, MAX_ACTION_RETRIES_CAP);
    }
}
