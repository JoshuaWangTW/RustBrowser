//! Planner-friendly view for the **RB Action Loop** (Observe → Act → Verify).
//!
//! A `Session` produces a [`LoopView`] after every step so an LLM planner can
//! decide what to do next from a compact, uniform shape rather than re-reading a
//! raw `Distilled` blob:
//!
//! - `state` — the current page (url/status/title/excerpt + a few health flags).
//! - `available_actions` — every operable element flattened to one shape, each
//!   with its stable `action_id`, a `kind`, a human `label`, and (for forms) the
//!   `method`, fillable `fields`, and whether it is `dangerous` (non-GET).
//! - `recommended_next_actions` — cheap, honest heuristics ("this looks like a
//!   search form / a next-page link") pointing at real `action_id`s. These are
//!   *hints*, never executed automatically.
//! - `failure_reason` — set when the last step did not verify (an HTTP error
//!   status). `None` means the step looked OK.
//!
//! The view is built from data already in the snapshot; it never fetches.

use serde::Serialize;

use crate::Distilled;
use crate::actions::{ActionTree, FormAction};

/// A planner-friendly snapshot of the session after one Observe/Act step.
#[derive(Debug, Clone, Serialize)]
pub struct LoopView {
    pub state: PageState,
    pub available_actions: Vec<AvailableAction>,
    pub recommended_next_actions: Vec<RecommendedAction>,
    /// Why the last step failed verification (HTTP error). `None` = looks OK.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
}

/// The current page state — the "where am I" half of the loop.
#[derive(Debug, Clone, Serialize)]
pub struct PageState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub status: u16,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    /// Characters of distilled Markdown on this page.
    pub content_chars: usize,
    /// Total operable actions found.
    pub action_count: usize,
    /// The distilled content is suspiciously short (possible JS-only page).
    pub low_content: bool,
    /// Headless rendering was used for this page.
    pub used_headless: bool,
    /// How many operations (observe / follow / submit_form) the session has
    /// run so far. Retry attempts within an operation do not add steps.
    pub steps_taken: usize,
    /// Why the Chrome Fallback Broker escalated the last settled step
    /// (`challenge`, `js_app`, `no_actions`, `forced`); `None` = RB-only
    /// extraction was enough. See [`crate::fallback`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

impl PageState {
    fn empty(steps_taken: usize) -> Self {
        Self {
            url: None,
            status: 0,
            title: String::new(),
            excerpt: None,
            content_chars: 0,
            action_count: 0,
            low_content: false,
            used_headless: false,
            steps_taken,
            fallback_reason: None,
        }
    }
}

/// One operable element, flattened to a single uniform shape for the planner.
#[derive(Debug, Clone, Serialize)]
pub struct AvailableAction {
    pub action_id: String,
    /// `link`, `form`, `button`, or `download`.
    pub kind: String,
    /// Human-readable label (link text, form summary, button caption, …).
    pub label: String,
    /// Target URL for links/downloads; the submit URL for forms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// `GET`/`POST` for forms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// True for actions RB will not run without explicit confirmation
    /// (non-GET form submits). Such actions are never auto-executed or retried.
    #[serde(skip_serializing_if = "is_false")]
    pub dangerous: bool,
    /// For forms: field names a caller may fill. Hidden/default-only controls
    /// are intentionally omitted; RB still carries their defaults internally.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
}

/// A heuristic suggestion. A hint for the planner — never executed by RB itself.
#[derive(Debug, Clone, Serialize)]
pub struct RecommendedAction {
    pub action_id: String,
    pub kind: String,
    pub why: String,
}

/// One recorded operation in a session's debug log.
#[derive(Debug, Clone, Serialize)]
pub struct OpLogEntry {
    /// The logical operation this entry belongs to (1-based, monotonic).
    /// Retries of the same operation share a step; `attempt` tells them apart.
    pub step: usize,
    /// `observe`, `follow`, or `submit_form`.
    pub op: String,
    /// The URL or form action the operation targeted.
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// 1-based attempt number for this operation (0 = not attempted, e.g. a
    /// non-GET submit withheld pending confirmation).
    pub attempt: usize,
    /// `ok`, `verify_failed`, `retryable_status`, `transient_error_retry`,
    /// `error`, `distill_failed`, `needs_confirmation`, `chrome_fallback`, or
    /// `chrome_fallback_failed`.
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
}

/// The **Verify** step: decide whether a freshly recorded snapshot represents a
/// failed step. Today that is purely an HTTP error status — sparse content is a
/// soft signal surfaced via `PageState.low_content`, not a hard failure (a short
/// page is often legitimate, so it must not be treated as an error or retried).
pub fn verify(snapshot: &Distilled) -> Option<String> {
    status_failure(snapshot.status)
}

fn status_failure(status: u16) -> Option<String> {
    (status >= 400).then(|| format!("http_status_{status}"))
}

/// Build the planner view from the last snapshot (if any), the last verify
/// result, and how many steps have been taken. Pure — never fetches.
pub fn loop_view(
    snapshot: Option<&Distilled>,
    failure_reason: Option<String>,
    steps_taken: usize,
) -> LoopView {
    let Some(snap) = snapshot else {
        return LoopView {
            state: PageState::empty(steps_taken),
            available_actions: Vec::new(),
            recommended_next_actions: Vec::new(),
            failure_reason,
        };
    };

    let tree = snap.actions.as_ref();
    let available_actions = tree.map(flatten_actions).unwrap_or_default();
    let recommended_next_actions = tree.map(recommend).unwrap_or_default();
    let diag = snap.diagnostics.as_ref();

    LoopView {
        state: PageState {
            url: Some(snap.final_url.clone()),
            status: snap.status,
            title: snap.title.clone(),
            excerpt: snap.excerpt.clone(),
            content_chars: snap.markdown.chars().count(),
            action_count: tree.map_or(0, ActionTree::len),
            low_content: diag.is_some_and(|d| d.low_content),
            used_headless: diag.is_some_and(|d| d.used_headless),
            steps_taken,
            // The broker's reason lives on the Session; Session::loop_view
            // patches it in after building this view.
            fallback_reason: None,
        },
        available_actions,
        recommended_next_actions,
        failure_reason,
    }
}

/// Flatten the categorised action tree into one uniform list, preserving each
/// element's stable `action_id`.
fn flatten_actions(tree: &ActionTree) -> Vec<AvailableAction> {
    let mut out = Vec::with_capacity(tree.len());
    for l in &tree.links {
        out.push(AvailableAction {
            action_id: l.action_id.clone(),
            kind: "link".to_string(),
            label: label_or(&l.text, &l.href),
            target: Some(l.href.clone()),
            method: None,
            dangerous: false,
            fields: Vec::new(),
        });
    }
    for f in &tree.forms {
        out.push(AvailableAction {
            action_id: f.action_id.clone(),
            kind: "form".to_string(),
            label: format!("{} form → {}", f.method, f.action),
            target: Some(f.action.clone()),
            method: Some(f.method.clone()),
            dangerous: is_dangerous_method(&f.method),
            fields: f
                .fields
                .iter()
                .filter(|fl| is_fillable_field(&fl.name, &fl.kind))
                .map(|fl| fl.name.clone())
                .collect(),
        });
    }
    for b in &tree.buttons {
        out.push(AvailableAction {
            action_id: b.action_id.clone(),
            kind: "button".to_string(),
            label: b.text.clone(),
            target: None,
            method: None,
            dangerous: false,
            fields: Vec::new(),
        });
    }
    for d in &tree.downloads {
        let fallback = d.filename.as_deref().unwrap_or(&d.href);
        out.push(AvailableAction {
            action_id: d.action_id.clone(),
            kind: "download".to_string(),
            label: label_or(&d.text, fallback),
            target: Some(d.href.clone()),
            method: None,
            dangerous: false,
            fields: Vec::new(),
        });
    }
    out
}

/// Cheap, honest "what next" hints pointing at real `action_id`s. At most two:
/// a GET search/filter form and/or a pagination/next link, falling back to the
/// first link only when neither matched.
fn recommend(tree: &ActionTree) -> Vec<RecommendedAction> {
    let mut recs = Vec::new();

    if let Some(f) = tree
        .forms
        .iter()
        .find(|f| !is_dangerous_method(&f.method) && form_looks_searchy(f))
    {
        recs.push(RecommendedAction {
            action_id: f.action_id.clone(),
            kind: "form".to_string(),
            why: "search/filter form (GET) — submit it to query the site".to_string(),
        });
    }

    if let Some(l) = tree.links.iter().find(|l| link_looks_paginated(&l.text)) {
        recs.push(RecommendedAction {
            action_id: l.action_id.clone(),
            kind: "link".to_string(),
            why: "looks like pagination / next — follow it to walk the list".to_string(),
        });
    }

    if recs.is_empty()
        && let Some(l) = tree.links.first()
    {
        recs.push(RecommendedAction {
            action_id: l.action_id.clone(),
            kind: "link".to_string(),
            why: "primary link on the page — follow it to navigate".to_string(),
        });
    }

    recs
}

/// Anything other than GET is a "dangerous" action RB will not run unconfirmed.
fn is_dangerous_method(method: &str) -> bool {
    !method.eq_ignore_ascii_case("GET")
}

fn is_fillable_field(name: &str, kind: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    !matches!(
        kind.to_ascii_lowercase().as_str(),
        "hidden" | "submit" | "button" | "reset" | "image"
    )
}

fn form_looks_searchy(form: &FormAction) -> bool {
    form.fields.iter().any(|f| {
        let kind = f.kind.to_ascii_lowercase();
        let name = f.name.to_ascii_lowercase();
        kind == "search"
            || name == "q"
            || ["query", "search", "keyword", "term", "find"]
                .iter()
                .any(|s| name.contains(s))
    })
}

fn link_looks_paginated(text: &str) -> bool {
    let t = text.trim().to_ascii_lowercase();
    if t == ">" || t == ">>" {
        return true;
    }
    ["next", "more", "older", "newer", "›", "»", "→"]
        .iter()
        .any(|s| t.contains(s))
}

fn label_or(text: &str, fallback: &str) -> String {
    if text.trim().is_empty() {
        fallback.to_string()
    } else {
        text.to_string()
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{ButtonAction, DownloadAction, FormField, FormOption, LinkAction};

    fn link(id: &str, text: &str, href: &str) -> LinkAction {
        LinkAction {
            action_id: id.to_string(),
            text: text.to_string(),
            href: href.to_string(),
        }
    }

    fn field(name: &str, kind: &str) -> FormField {
        FormField {
            name: name.to_string(),
            kind: kind.to_string(),
            value: None,
            options: Vec::<FormOption>::new(),
            required: false,
        }
    }

    #[test]
    fn status_failure_flags_only_4xx_5xx() {
        assert_eq!(status_failure(200), None);
        assert_eq!(status_failure(301), None);
        assert_eq!(status_failure(404).as_deref(), Some("http_status_404"));
        assert_eq!(status_failure(503).as_deref(), Some("http_status_503"));
    }

    #[test]
    fn flatten_marks_non_get_forms_dangerous_and_lists_fields() {
        let tree = ActionTree {
            links: vec![link("link_0", "About", "https://e.com/about")],
            forms: vec![
                FormAction {
                    action_id: "form_0".to_string(),
                    method: "GET".to_string(),
                    action: "https://e.com/search".to_string(),
                    submit_id: "form_0.submit".to_string(),
                    fields: vec![field("q", "search")],
                },
                FormAction {
                    action_id: "form_1".to_string(),
                    method: "POST".to_string(),
                    action: "https://e.com/login".to_string(),
                    submit_id: "form_1.submit".to_string(),
                    fields: vec![
                        field("csrf", "hidden"),
                        field("user", "text"),
                        field("pass", "password"),
                        field("submit", "submit"),
                        field("", "text"),
                    ],
                },
            ],
            buttons: vec![ButtonAction {
                action_id: "button_0".to_string(),
                text: "Load more".to_string(),
                kind: "button".to_string(),
            }],
            downloads: vec![DownloadAction {
                action_id: "download_0".to_string(),
                text: String::new(),
                href: "https://e.com/report.pdf".to_string(),
                filename: Some("report.pdf".to_string()),
            }],
        };

        let flat = flatten_actions(&tree);
        assert_eq!(flat.len(), 5);

        let get_form = flat.iter().find(|a| a.action_id == "form_0").unwrap();
        assert!(!get_form.dangerous);
        assert_eq!(get_form.method.as_deref(), Some("GET"));

        let post_form = flat.iter().find(|a| a.action_id == "form_1").unwrap();
        assert!(post_form.dangerous, "non-GET form must be dangerous");
        assert_eq!(post_form.fields, vec!["user", "pass"]);

        // Empty link/download text falls back to a useful label.
        let dl = flat.iter().find(|a| a.action_id == "download_0").unwrap();
        assert_eq!(dl.label, "report.pdf");
    }

    #[test]
    fn recommend_prefers_search_form_then_pagination_then_first_link() {
        let searchy = ActionTree {
            links: vec![link("link_0", "Home", "https://e.com/")],
            forms: vec![FormAction {
                action_id: "form_0".to_string(),
                method: "GET".to_string(),
                action: "https://e.com/search".to_string(),
                submit_id: "form_0.submit".to_string(),
                fields: vec![field("q", "search")],
            }],
            buttons: Vec::new(),
            downloads: Vec::new(),
        };
        let recs = recommend(&searchy);
        assert_eq!(recs[0].action_id, "form_0");
        assert_eq!(recs[0].kind, "form");

        let paginated = ActionTree {
            links: vec![
                link("link_0", "Article", "https://e.com/a"),
                link("link_1", "Next ›", "https://e.com/p2"),
            ],
            forms: Vec::new(),
            buttons: Vec::new(),
            downloads: Vec::new(),
        };
        let recs = recommend(&paginated);
        assert_eq!(recs[0].action_id, "link_1", "pagination link preferred");

        let plain = ActionTree {
            links: vec![link("link_0", "Read", "https://e.com/read")],
            forms: Vec::new(),
            buttons: Vec::new(),
            downloads: Vec::new(),
        };
        let recs = recommend(&plain);
        assert_eq!(recs[0].action_id, "link_0");
    }

    #[test]
    fn empty_snapshot_yields_empty_view() {
        let v = loop_view(None, Some("boom".to_string()), 3);
        assert_eq!(v.state.status, 0);
        assert_eq!(v.state.steps_taken, 3);
        assert!(v.available_actions.is_empty());
        assert_eq!(v.failure_reason.as_deref(), Some("boom"));
    }
}
