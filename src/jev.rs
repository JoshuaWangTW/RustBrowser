//! **Jev planner** — TypeSafe's System One model as the session's
//! recommendation engine (the technique from `browser-use/jev-ultrafast`).
//!
//! RB already exposes a *dynamic indexed action space* (`available_actions`
//! with stable `action_id`s). Given a `goal`, one request asks Jev two typed
//! questions against that state — *which operation* and *which target* — as a
//! speculative fan-out: both heads are answered in the same round trip and
//! the target matching the chosen operation is kept.
//!
//! Jev only **recommends**. The answer lands in
//! `loop.recommended_next_actions` with a calibrated `confidence`; the caller
//! still executes via `session_follow` / `session_submit_form`, and a non-GET
//! form stays `dangerous` (never auto-sent). No key, or any error, silently
//! falls back to the built-in heuristics and surfaces in `loop.planner_note`.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Distilled;
use crate::planner::{self, AvailableAction, OpLogEntry, RecommendedAction};

const DEFAULT_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const DEFAULT_MODEL: &str = "jev-latest";
/// Characters of page text sent as state (mirrors jev-ultrafast's 6000 cap).
const MAX_STATE_TEXT: usize = 6000;
/// Recent operations sent as history.
const HISTORY_TAIL: usize = 10;
const TIMEOUT: Duration = Duration::from_secs(10);

/// Where and how to call Jev.
#[derive(Debug, Clone)]
pub struct JevConfig {
    pub api_key: String,
    pub model: String,
    pub endpoint: String,
}

impl JevConfig {
    /// `TYPESAFE_API_KEY` (required), `TYPESAFE_MODEL`, `TYPESAFE_ENDPOINT`.
    /// `None` when no key is set — the session then keeps its heuristics.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("TYPESAFE_API_KEY").ok()?;
        if api_key.trim().is_empty() {
            return None;
        }
        Some(Self {
            api_key,
            model: std::env::var("TYPESAFE_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into()),
            endpoint: std::env::var("TYPESAFE_ENDPOINT")
                .unwrap_or_else(|_| DEFAULT_ENDPOINT.into()),
        })
    }
}

/// Ask Jev what to do next toward `goal` on the current snapshot. Returns the
/// chosen operation's target (or a `done` / `blocked` verdict) as hints.
pub async fn recommend(
    cfg: &JevConfig,
    goal: &str,
    snap: &Distilled,
    history: &[OpLogEntry],
) -> Result<Vec<RecommendedAction>> {
    let actions = snap
        .actions
        .as_ref()
        .map(planner::flatten_actions)
        .unwrap_or_default();
    let body = build_request(cfg, goal, snap, &actions, history);

    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .context("building Jev client")?;
    let resp = client
        .post(&cfg.endpoint)
        .bearer_auth(&cfg.api_key)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .context("Jev request failed")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("Jev returned HTTP {}", status.as_u16());
    }
    let raw = resp.text().await.context("reading Jev response")?;
    let answer: Value = serde_json::from_str(&raw).context("Jev returned non-JSON")?;
    interpret(&answer, &actions)
}

/// Build the speculative fan-out: `operation` plus one target head per
/// operation that has candidates. Pure, so it is unit-testable offline.
fn build_request(
    cfg: &JevConfig,
    goal: &str,
    snap: &Distilled,
    actions: &[AvailableAction],
    history: &[OpLogEntry],
) -> Value {
    let (follow, submit) = split_targets(actions);

    let operations: BTreeMap<&str, &str> = offered_operations(actions)
        .into_iter()
        .map(|op| (op, operation_hint(op)))
        .collect();

    let mut questions = serde_json::Map::new();
    questions.insert(
        "operation".into(),
        json!({
            "type": "choice",
            "criteria": operations,
            "instructions": {
                "goal": goal,
                "rules": [
                    "Pick the single operation that most directly advances the goal from the current page.",
                    "Choose done only when the page already shows what the goal asks for.",
                    "Choose blocked only when nothing offered can help.",
                ],
            },
        }),
    );
    for (name, cands) in [("follow_target", &follow), ("submit_form_target", &submit)] {
        if cands.is_empty() {
            continue;
        }
        let criteria: serde_json::Map<String, Value> = cands
            .iter()
            .map(|a| {
                let mut c =
                    json!({ "element": format!("[{}] {} · {}", a.action_id, a.kind, a.label) });
                if let Some(t) = &a.target {
                    c["target"] = json!(t);
                }
                if a.dangerous {
                    c["dangerous"] = json!(true);
                }
                if !a.fields.is_empty() {
                    c["fields"] = json!(a.fields);
                }
                (a.action_id.clone(), c)
            })
            .collect();
        questions.insert(
            name.into(),
            json!({
                "type": "choice",
                "criteria": criteria,
                "instructions": {
                    "goal": goal,
                    "rules": ["Pick the one element that best advances the goal for this operation."],
                },
            }),
        );
    }

    let text: String = snap.text.chars().take(MAX_STATE_TEXT).collect();
    let recent: Vec<Value> = history
        .iter()
        .rev()
        .take(HISTORY_TAIL)
        .rev()
        .map(|h| json!({ "op": h.op, "target": h.target, "outcome": h.outcome }))
        .collect();

    json!({
        "model": cfg.model,
        "state": {
            "page": { "url": snap.final_url, "title": snap.title, "text": text },
            "elements": actions.iter().map(|a| json!({
                "action_id": a.action_id, "kind": a.kind, "label": a.label,
                "dangerous": a.dangerous,
            })).collect::<Vec<_>>(),
            "recent_actions": recent,
        },
        "questions": questions,
    })
}

/// The operations Jev may choose from on this page: only those with at least
/// one candidate target, plus the two verdicts. Shared by request building and
/// answer validation so the two can never disagree.
fn offered_operations(actions: &[AvailableAction]) -> Vec<&'static str> {
    let (follow, submit) = split_targets(actions);
    let mut ops = Vec::with_capacity(4);
    if !follow.is_empty() {
        ops.push("follow");
    }
    if !submit.is_empty() {
        ops.push("submit_form");
    }
    ops.extend(["done", "blocked"]);
    ops
}

fn operation_hint(op: &str) -> &'static str {
    match op {
        "follow" => "Open a link or download by its action_id to navigate toward the goal.",
        "submit_form" => {
            "Submit a form (search, filter, login) by its action_id; the caller fills the fields."
        }
        "done" => "Every requirement of the goal is visibly satisfied on this page.",
        _ => "No available action can make progress toward the goal.",
    }
}

fn split_targets(actions: &[AvailableAction]) -> (Vec<&AvailableAction>, Vec<&AvailableAction>) {
    let follow = actions
        .iter()
        .filter(|a| a.kind == "link" || a.kind == "download")
        .collect();
    let submit = actions.iter().filter(|a| a.kind == "form").collect();
    (follow, submit)
}

#[derive(Deserialize)]
struct Choice {
    choice: String,
    probabilities: BTreeMap<String, f64>,
    confidence: f64,
}

/// Turn Jev's answers into hints. Validates like jev-ultrafast's
/// `validate_choice`: the chosen id must be one we offered, and probabilities
/// must be a sane distribution — otherwise nothing is recommended.
fn interpret(answer: &Value, actions: &[AvailableAction]) -> Result<Vec<RecommendedAction>> {
    let answers = answer
        .get("answers")
        .ok_or_else(|| anyhow!("Jev response has no answers"))?;
    let op = choice(answers, "operation", &offered_operations(actions))?;

    let target_head = match op.choice.as_str() {
        "follow" => "follow_target",
        "submit_form" => "submit_form_target",
        verdict => {
            return Ok(vec![RecommendedAction {
                action_id: String::new(),
                kind: verdict.to_string(),
                why: format!("jev: {verdict} (p={:.2})", op.probabilities[verdict]),
                confidence: Some(op.confidence),
            }]);
        }
    };
    let wanted_kind = |a: &&AvailableAction| match target_head {
        "follow_target" => a.kind == "link" || a.kind == "download",
        _ => a.kind == "form",
    };
    let ids: Vec<&str> = actions
        .iter()
        .filter(wanted_kind)
        .map(|a| a.action_id.as_str())
        .collect();
    let target = choice(answers, target_head, &ids)?;
    let picked = actions
        .iter()
        .find(|a| a.action_id == target.choice)
        .ok_or_else(|| anyhow!("Jev chose an unknown target"))?;

    Ok(vec![RecommendedAction {
        action_id: picked.action_id.clone(),
        kind: picked.kind.clone(),
        why: format!(
            "jev: {} (p={:.2}) → {} (p={:.2}){}",
            op.choice,
            op.probabilities[&op.choice],
            picked.label,
            target.probabilities[&target.choice],
            if picked.dangerous {
                " — dangerous, needs confirm"
            } else {
                ""
            }
        ),
        confidence: Some(op.confidence.min(target.confidence)),
    }])
}

fn choice(answers: &Value, name: &str, ids: &[&str]) -> Result<Choice> {
    let c: Choice = serde_json::from_value(
        answers
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("Jev answer `{name}` missing"))?,
    )
    .with_context(|| format!("Jev answer `{name}` malformed"))?;
    let unit = |x: f64| x.is_finite() && (0.0..=1.0).contains(&x);
    let sum: f64 = c.probabilities.values().sum();
    let max = c.probabilities.values().cloned().fold(0.0, f64::max);
    let ok = ids.contains(&c.choice.as_str())
        && c.probabilities.len() == ids.len()
        && ids.iter().all(|id| c.probabilities.contains_key(*id))
        && c.probabilities.values().all(|p| unit(*p))
        && unit(c.confidence)
        && (sum - 1.0).abs() < 0.02
        && c.probabilities[&c.choice] >= max - 1e-6;
    if !ok {
        bail!("Jev answer `{name}` failed validation; nothing recommended");
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(id: &str, kind: &str, dangerous: bool) -> AvailableAction {
        AvailableAction {
            action_id: id.into(),
            kind: kind.into(),
            label: format!("{kind} {id}"),
            target: None,
            method: None,
            dangerous,
            fields: Vec::new(),
        }
    }

    fn cfg() -> JevConfig {
        JevConfig {
            api_key: "k".into(),
            model: "jev-latest".into(),
            endpoint: "http://x".into(),
        }
    }

    fn snap() -> Distilled {
        Distilled {
            final_url: "http://x/".into(),
            status: 200,
            title: "T".into(),
            byline: None,
            excerpt: None,
            markdown: String::new(),
            text: "hello".repeat(2000),
            stats: None,
            links: None,
            tables: None,
            actions: None,
            diagnostics: None,
        }
    }

    #[test]
    fn request_offers_only_operations_with_candidates_and_caps_text() {
        let actions = [act("link_0", "link", false)];
        let body = build_request(&cfg(), "find x", &snap(), &actions, &[]);
        let q = body["questions"].as_object().unwrap();
        assert!(q.contains_key("follow_target"));
        assert!(!q.contains_key("submit_form_target"));
        let ops = q["operation"]["criteria"].as_object().unwrap();
        assert!(ops.contains_key("follow") && !ops.contains_key("submit_form"));
        assert!(ops.contains_key("done") && ops.contains_key("blocked"));
        assert_eq!(
            body["state"]["page"]["text"].as_str().unwrap().len(),
            MAX_STATE_TEXT
        );
    }

    #[test]
    fn interpret_keeps_target_matching_operation_and_flags_dangerous() {
        let actions = [act("link_0", "link", false), act("form_0", "form", true)];
        let answer = json!({ "answers": {
            "operation": { "choice": "submit_form",
                "probabilities": { "follow": 0.1, "submit_form": 0.8, "done": 0.05, "blocked": 0.05 },
                "confidence": 0.9 },
            "follow_target": { "choice": "link_0", "probabilities": { "link_0": 1.0 }, "confidence": 0.5 },
            "submit_form_target": { "choice": "form_0", "probabilities": { "form_0": 1.0 }, "confidence": 0.7 },
        }});
        let recs = interpret(&answer, &actions).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].action_id, "form_0");
        assert_eq!(recs[0].confidence, Some(0.7));
        assert!(recs[0].why.contains("dangerous"));
    }

    #[test]
    fn interpret_rejects_unknown_choice_or_bad_distribution() {
        let actions = [act("link_0", "link", false)];
        let bad_id = json!({ "answers": { "operation": { "choice": "click",
            "probabilities": { "follow": 0.5, "done": 0.25, "blocked": 0.25 }, "confidence": 0.9 }}});
        assert!(interpret(&bad_id, &actions).is_err());
        let bad_sum = json!({ "answers": { "operation": { "choice": "follow",
            "probabilities": { "follow": 0.9, "done": 0.9, "blocked": 0.9 }, "confidence": 0.9 }}});
        assert!(interpret(&bad_sum, &actions).is_err());
    }

    #[test]
    fn interpret_done_verdict_has_no_target() {
        let answer = json!({ "answers": { "operation": { "choice": "done",
            "probabilities": { "follow": 0.0, "done": 1.0, "blocked": 0.0 }, "confidence": 0.99 }}});
        let recs = interpret(&answer, &[act("link_0", "link", false)]).unwrap();
        assert_eq!(recs[0].kind, "done");
        assert!(recs[0].action_id.is_empty());
    }
}
