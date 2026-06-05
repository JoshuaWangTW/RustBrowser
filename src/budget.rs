//! Token-budget truncation: cap distilled output to a maximum token count.
//!
//! Truncates at paragraph boundaries (blank-line separated) and appends a marker
//! so the consumer knows content was cut. Uses `tokens::count`, which is precise
//! with the `stats` feature and a word-count estimate without it — so the budget
//! is approximate without `stats`, but the truncation behaviour is identical.

use crate::tokens;

/// Appended to truncated output so the consumer knows content was dropped.
pub const TRUNCATION_MARKER: &str = "\n\n…(truncated to fit token budget)";

/// Fit `markdown` within `max_tokens`, truncating at paragraph boundaries.
/// Returns the (possibly truncated) text and whether truncation occurred. A
/// `max_tokens` of 0 means "no budget" and passes the text through untouched.
pub fn fit(markdown: &str, max_tokens: usize) -> (String, bool) {
    if max_tokens == 0 || tokens::count(markdown) <= max_tokens {
        return (markdown.to_string(), false);
    }

    let mut out = String::new();
    let mut used = 0usize;
    for para in markdown.split("\n\n") {
        let para = para.trim_end();
        if para.is_empty() {
            continue;
        }
        let cost = tokens::count(para);
        // Always keep the first paragraph even if it alone blows the budget —
        // returning nothing is worse than a slight overshoot.
        if !out.is_empty() && used + cost > max_tokens {
            break;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(para);
        used += cost;
        if used >= max_tokens {
            break;
        }
    }

    out.push_str(TRUNCATION_MARKER);
    (out, true)
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
        assert!(tokens::count(&out) < tokens::count(&md));
    }

    #[test]
    fn keeps_first_paragraph_even_if_oversized() {
        let big = "word ".repeat(500);
        let (out, truncated) = fit(&big, 5);
        assert!(truncated);
        // The single oversized paragraph is retained rather than dropped.
        assert!(out.starts_with("word word"));
    }
}
