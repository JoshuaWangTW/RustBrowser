//! Stateful browsing session for Browser Use (Observe → Act → Verify).
//!
//! A `Session` keeps a cookie jar, the current URL, a redirect history, and the
//! last observed snapshot (distilled content + action tree). An agent drives it
//! by `observe`-ing a URL, then `follow`-ing a link or `submit_form`-ing by the
//! stable `action_id`s in the last snapshot — never opening a real browser.
//!
//! Safety: every request reuses the same SSRF-screened path as plain fetches.
//! A non-GET form submit is a *dangerous* action and is refused unless the
//! caller explicitly confirms — RB never silently POSTs on an agent's behalf.

use anyhow::{Result, anyhow, bail};

use crate::actions::FormAction;
use crate::fetch::{FetchOptions, FetchResult, Fetcher, SubmitMethod};
use crate::{DistillOptions, Distilled, distill_html};

/// A stateful browsing session.
pub struct Session {
    fetcher: Fetcher,
    /// Distill options used for every snapshot (always extracts the action tree).
    opts: DistillOptions,
    current_url: Option<String>,
    redirect_history: Vec<String>,
    last_snapshot: Option<Distilled>,
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
            max_retries: opts.max_retries,
            per_host_concurrency: opts.per_host_concurrency,
            min_request_interval: opts.min_request_interval,
            respect_robots: opts.respect_robots,
            cookie_store: true,
            ..Default::default()
        };
        if let Some(ua) = &opts.user_agent {
            fopts.user_agent = ua.clone();
        }

        // Snapshots always carry the action tree; caching is off so a session
        // always sees live state.
        let mut snapshot_opts = opts;
        snapshot_opts.extract_actions = true;
        snapshot_opts.use_cache = false;

        Ok(Self {
            fetcher: Fetcher::new(fopts)?,
            opts: snapshot_opts,
            current_url: None,
            redirect_history: Vec::new(),
            last_snapshot: None,
        })
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

    /// Fetch `url` and make it the current snapshot.
    pub async fn observe(&mut self, url: &str) -> Result<&Distilled> {
        let result = self.fetcher.fetch(url).await?;
        self.record(result);
        self.snapshot_ref()
    }

    /// Follow a `link_*` / `download_*` action from the last snapshot.
    pub async fn follow(&mut self, action_id: &str) -> Result<&Distilled> {
        let href = self.resolve_followable(action_id)?;
        let result = self.fetcher.fetch(&href).await?;
        self.record(result);
        self.snapshot_ref()
    }

    /// Submit a `form_*` from the last snapshot, merging the form's own default
    /// values (hidden fields, selected options) with the caller's `values`.
    /// A non-GET submit requires `confirm = true`.
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
            return Ok(SubmitOutcome::NeedsConfirmation {
                method: form.method.clone(),
                action: form.action.clone(),
                fields,
            });
        }

        let result = self.fetcher.submit(&form.action, method, &fields).await?;
        self.record(result);
        Ok(SubmitOutcome::Submitted)
    }

    fn record(&mut self, result: FetchResult) {
        self.current_url = Some(result.final_url.clone());
        self.redirect_history.push(result.final_url.clone());
        if let Ok(mut snap) = distill_html(&result.html, &result.final_url, &self.opts) {
            // distill_html stamps a synthetic 200; carry the real HTTP status.
            snap.status = result.status;
            self.last_snapshot = Some(snap);
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
}
