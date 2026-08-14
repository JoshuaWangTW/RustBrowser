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

/// The computed outcome of a settle, not yet applied to the session. Produced
/// by [`Session::prepare_settle`] (`&self`, may `await`) and applied by
/// [`Session::commit_settle`] (`&mut self`, synchronous).
struct Settled {
    snapshot: Distilled,
    final_url: String,
    failure: Option<String>,
    fallback: Option<String>,
    pending_log: PendingLog,
}

/// A settle that failed before producing a [`Settled`] outcome (distill
/// error). Still carries its log entries: the pre-refactor behaviour logs the
/// failure even though no other session state changes.
#[derive(Debug)]
struct SettleFailure {
    error: anyhow::Error,
    pending_log: PendingLog,
}

/// Operation-log entries computed during [`Session::prepare_settle`], applied
/// to the session in one shot by [`Session::commit_settle`] /
/// [`Session::commit_pending_log`].
#[derive(Debug, Default)]
struct PendingLog(Vec<OpLogEntry>);

impl PendingLog {
    fn push(&mut self, entry: OpLogEntry) {
        self.0.push(entry);
    }
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
        match self
            .prepare_settle("submit_form", &form.action, 1, result, false)
            .await
        {
            Ok(settled) => self.commit_settle(settled),
            Err(failure) => {
                self.commit_pending_log(failure.pending_log);
                return Err(failure.error);
            }
        }
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
                    match self
                        .prepare_settle(op, &target, attempt + 1, result, true)
                        .await
                    {
                        Ok(settled) => self.commit_settle(settled),
                        Err(failure) => {
                            self.commit_pending_log(failure.pending_log);
                            return Err(failure.error);
                        }
                    }
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

    /// Compute the outcome of settling a fetch result: distill it into a
    /// snapshot, let the Chrome Fallback Broker escalate once when RB-only
    /// extraction looks insufficient, and verify — all without touching
    /// session state. This is the only place a settle `await`s (the fallback
    /// render), and it takes `&self` so the compiler guarantees nothing here
    /// can mutate the session while that await is in flight: if the caller's
    /// future is cancelled mid-computation, the session is simply left as it
    /// was before the call. [`Self::commit_settle`] applies the result.
    ///
    /// Shared by the idempotent retry loop and the single-attempt confirmed
    /// non-GET submit — the latter passes `allow_fallback = false`, because a
    /// POST's result page must never be re-fetched by a browser.
    async fn prepare_settle(
        &self,
        op: &str,
        target: &str,
        attempt: usize,
        result: FetchResult,
        allow_fallback: bool,
    ) -> Result<Settled, SettleFailure> {
        let status = result.status;
        let mut pending_log = PendingLog::default();

        let mut snap = match self.distill_result(&result) {
            Ok(s) => s,
            Err(e) => {
                pending_log.push(self.pending_log_entry(
                    op,
                    target,
                    Some(status),
                    attempt,
                    "distill_failed",
                    Some(short_err(&e)),
                ));
                return Err(SettleFailure {
                    error: e,
                    pending_log,
                });
            }
        };

        // Chrome Fallback Broker (Observe → Act → Verify → **Fallback**): one
        // bounded headless render when RB alone is not enough; the rendered DOM
        // is re-distilled through the same token-lean pipeline, so the caller
        // still gets compressed content + action tree — never a raw DOM. A
        // failed render is non-fatal: the HTTP snapshot stands.
        let mut fallback = None;
        if allow_fallback && let Some(reason) = self.fallback_decision(&snap, &result.html) {
            fallback = Some(reason.label().to_string());
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
                            pending_log.push(self.pending_log_entry(
                                op,
                                target,
                                Some(status),
                                attempt,
                                "chrome_fallback",
                                Some(reason.label().to_string()),
                            ));
                        }
                        Err(e) => pending_log.push(self.pending_log_entry(
                            op,
                            target,
                            Some(status),
                            attempt,
                            "chrome_fallback_failed",
                            Some(short_err(&e)),
                        )),
                    }
                }
                Err(e) => pending_log.push(self.pending_log_entry(
                    op,
                    target,
                    Some(status),
                    attempt,
                    "chrome_fallback_failed",
                    Some(short_err(&e)),
                )),
            }
        }

        let failure = planner::verify(&snap);
        pending_log.push(self.pending_log_entry(
            op,
            target,
            Some(status),
            attempt,
            outcome_label(&failure),
            failure.clone(),
        ));

        Ok(Settled {
            snapshot: snap,
            final_url: result.final_url,
            failure,
            fallback,
            pending_log,
        })
    }

    /// Commit a computed [`Settled`] outcome: purely synchronous, no `await`,
    /// so once called it cannot be interrupted partway through. The five
    /// session fields that describe "where we are" — `current_url`,
    /// `last_snapshot`, `redirect_history`, `last_failure`, `last_fallback` —
    /// change together here and nowhere else.
    fn commit_settle(&mut self, s: Settled) {
        self.last_fallback = s.fallback;
        self.last_failure = s.failure;
        self.current_url = Some(s.final_url);
        self.last_snapshot = Some(s.snapshot);
        self.commit_navigation();
        self.commit_pending_log(s.pending_log);
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
        let entry = self.pending_log_entry(op, target, status, attempt, outcome, failure_reason);
        self.log.push(entry);
        self.trim_log();
    }

    /// Build a log entry without mutating the session — used by
    /// [`Self::prepare_settle`], which only holds `&self`.
    fn pending_log_entry(
        &self,
        op: &str,
        target: &str,
        status: Option<u16>,
        attempt: usize,
        outcome: &str,
        failure_reason: Option<String>,
    ) -> OpLogEntry {
        OpLogEntry {
            step: self.step,
            op: op.to_string(),
            target: target.to_string(),
            status,
            attempt,
            outcome: outcome.to_string(),
            failure_reason,
        }
    }

    /// Append log entries computed by [`Self::prepare_settle`] (used both on
    /// the success and the failure path, so a failed settle still leaves its
    /// diagnostic trail — matching the pre-refactor behaviour).
    fn commit_pending_log(&mut self, pending: PendingLog) {
        self.log.extend(pending.0);
        self.trim_log();
    }

    fn trim_log(&mut self) {
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

    /// The five fields that describe "where the session is": `current_url`,
    /// `last_snapshot` (as its markdown), `redirect_history`, `last_failure`,
    /// `last_fallback`.
    type SessionState = (
        Option<String>,
        Option<String>,
        Vec<String>,
        Option<String>,
        Option<String>,
    );

    /// Snapshot of [`SessionState`] — checked before and after
    /// `prepare_settle` to prove it left them untouched.
    fn state_tuple(s: &Session) -> SessionState {
        (
            s.current_url.clone(),
            s.last_snapshot.as_ref().map(|d| d.markdown.clone()),
            s.redirect_history.clone(),
            s.last_failure.clone(),
            s.last_fallback.clone(),
        )
    }

    fn fetch_result(html: &str) -> FetchResult {
        FetchResult {
            final_url: "https://example.com/page".into(),
            status: 200,
            content_type: Some("text/html".into()),
            html: html.to_string(),
            raw_bytes: html.len(),
        }
    }

    #[tokio::test]
    async fn settle_computation_does_not_mutate_session() {
        let opts = DistillOptions {
            allow_local: true,
            js_mode: JsMode::Off,
            ..Default::default()
        };
        let mut s = Session::new(opts).unwrap();
        // Give the session some baseline state to prove untouched.
        s.current_url = Some("https://example.com/prior".into());
        s.redirect_history.push("https://example.com/prior".into());

        let before = state_tuple(&s);
        let html = "<html><body><h1>Title</h1><p>Enough body text for a clean snapshot here.</p></body></html>";
        let settled = s
            .prepare_settle(
                "observe",
                "https://example.com/page",
                1,
                fetch_result(html),
                true,
            )
            .await
            .expect("prepare_settle should succeed");
        let after = state_tuple(&s);

        assert_eq!(
            before, after,
            "prepare_settle must not mutate session state"
        );

        // Sanity: the computed outcome is the one we'd expect to commit.
        assert_eq!(settled.final_url, "https://example.com/page");
        assert!(settled.failure.is_none());

        // Committing it now does change state.
        s.commit_settle(settled);
        assert_eq!(s.current_url().unwrap(), "https://example.com/page");
    }

    #[tokio::test]
    async fn cancelled_step_leaves_session_consistent() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(
                        "<html><body><h1>Home</h1><p>Enough body text for a clean snapshot.</p></body></html>",
                    ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/slow"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(
                        "<html><body><h1>Slow</h1><p>Enough body text for a clean snapshot.</p></body></html>",
                    )
                    .set_delay(std::time::Duration::from_secs(5)),
            )
            .mount(&server)
            .await;

        let opts = DistillOptions {
            allow_local: true,
            js_mode: JsMode::Off,
            ..Default::default()
        };
        let mut s = Session::new(opts).unwrap();
        s.observe(&format!("{}/", server.uri())).await.unwrap();
        let before = state_tuple(&s);

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            s.observe(&format!("{}/slow", server.uri())),
        )
        .await;
        assert!(result.is_err(), "the slow request must time out");

        let after = state_tuple(&s);
        assert_eq!(
            before, after,
            "a cancelled step must leave the session exactly as it was"
        );
    }
}
