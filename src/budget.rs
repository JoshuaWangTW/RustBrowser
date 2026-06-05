//! Token-budget truncation: cap distilled output to a maximum token count.
//!
//! Truncates at paragraph boundaries (blank-line separated) and appends a marker
//! so the consumer knows content was cut. Uses `tokens::count`, which is precise
//! with the `stats` feature and a word-count estimate without it — so the budget
//! is approximate without `stats`, but the truncation behaviour is identical.

use crate::tokens;

/// Appended to truncated output so the consumer knows content was dropped.
pub const TRUNCATION_MARKER: &str = "\n\n…(truncated to fit token budget)";

const MARKER_FALLBACKS: [&str; 5] = [TRUNCATION_MARKER, "\n\n…(truncated)", "\n\n…", "…", ""];

/// Fit `markdown` within `max_tokens`, truncating at paragraph boundaries when
/// possible and falling back to a bounded prefix when a single paragraph is too
/// large. The truncation marker is counted inside the budget.
/// Returns the (possibly truncated) text and whether truncation occurred. A
/// `max_tokens` of 0 means "no budget" and passes the text through untouched.
pub fn fit(markdown: &str, max_tokens: usize) -> (String, bool) {
    if max_tokens == 0 || tokens::count(markdown) <= max_tokens {
        return (markdown.to_string(), false);
    }

    let marker = marker_for(max_tokens);
    let mut out = String::new();
    for para in markdown.split("\n\n") {
        let para = para.trim_end();
        if para.is_empty() {
            continue;
        }

        let candidate = append_paragraph(&out, para);
        if fits(&candidate, marker, max_tokens) {
            out = candidate;
            continue;
        }

        if out.is_empty() {
            out = bounded_prefix(para, marker, max_tokens);
        }
        break;
    }

    if out.is_empty() {
        out = bounded_prefix(markdown.trim_end(), marker, max_tokens);
    }

    let mut final_out = out;
    final_out.push_str(marker);
    if tokens::count(&final_out) <= max_tokens {
        (final_out, true)
    } else {
        let marker_only = marker_for(max_tokens).to_string();
        debug_assert!(tokens::count(&marker_only) <= max_tokens);
        (marker_only, true)
    }
}

fn marker_for(max_tokens: usize) -> &'static str {
    MARKER_FALLBACKS
        .iter()
        .copied()
        .find(|marker| tokens::count(marker) <= max_tokens)
        .unwrap_or("")
}

fn append_paragraph(existing: &str, para: &str) -> String {
    if existing.is_empty() {
        para.to_string()
    } else {
        let mut out = String::with_capacity(existing.len() + para.len() + 2);
        out.push_str(existing);
        out.push_str("\n\n");
        out.push_str(para);
        out
    }
}

fn fits(body: &str, marker: &str, max_tokens: usize) -> bool {
    let mut candidate = String::with_capacity(body.len() + marker.len());
    candidate.push_str(body);
    candidate.push_str(marker);
    tokens::count(&candidate) <= max_tokens
}

fn bounded_prefix(text: &str, marker: &str, max_tokens: usize) -> String {
    let text = text.trim_end();
    if text.is_empty() || tokens::count(marker) > max_tokens {
        return String::new();
    }

    let mut boundaries = vec![0usize];
    boundaries.extend(text.char_indices().skip(1).map(|(idx, _)| idx));
    boundaries.push(text.len());

    let mut low = 0usize;
    let mut high = boundaries.len() - 1;
    while low < high {
        let mid = (low + high).div_ceil(2);
        let prefix = text[..boundaries[mid]].trim_end();
        if fits(prefix, marker, max_tokens) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }

    text[..boundaries[low]].trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_budget_is_unchanged() {
        let md = "first paragraph\n\nsecond paragraph";
        let (out, truncated) = fit(md, 100_000);
        assert!(!truncated);
        assert_eq!(out, md);
    }

    #[test]
    fn zero_budget_means_no_limit() {
        let (out, truncated) = fit("a\n\nb", 0);
        assert!(!truncated);
        assert_eq!(out, "a\n\nb");
    }

    #[test]
    fn over_budget_truncates_with_marker() {
        let md = (0..40)
            .map(|i| format!("Paragraph {i} carries a handful of words to spend tokens."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let (out, truncated) = fit(&md, 20);
        assert!(truncated);
        assert!(out.contains("truncated"));
        assert!(out.starts_with("Paragraph 0"));
        assert!(tokens::count(&out) <= 20);
        assert!(tokens::count(&out) < tokens::count(&md));
    }

    #[test]
    fn oversized_first_paragraph_respects_budget() {
        let big = "word ".repeat(500);
        let (out, truncated) = fit(&big, 5);
        assert!(truncated);
        assert!(tokens::count(&out) <= 5);
        assert!(out.starts_with("word") || out.starts_with('…'));
    }

    #[test]
    fn tiny_budget_still_respects_budget() {
        let big = "word ".repeat(20);
        let (out, truncated) = fit(&big, 1);
        assert!(truncated);
        assert!(tokens::count(&out) <= 1);
    }
}
