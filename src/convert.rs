//! HTML → Markdown conversion via htmd, plus whitespace normalisation.
//!
//! Markdown is dramatically cheaper than HTML in tokens: no tags, no inline
//! styles, no attributes. We additionally collapse runs of blank lines that
//! would otherwise inflate the token count for no semantic gain.

use anyhow::{Result, anyhow};

/// Convert cleaned HTML to compact Markdown.
pub fn to_markdown(html: &str) -> Result<String> {
    let md = htmd::convert(html).map_err(|e| anyhow!("HTML→Markdown conversion failed: {e}"))?;
    Ok(normalize(&md))
}

/// Tidy Markdown for minimal token footprint:
/// - trim trailing whitespace
/// - collapse multiple blank lines into one
/// - collapse runs of 2+ spaces (e.g. table-cell alignment padding) into one,
///   which is pure token waste for an LLM — while preserving leading indent
///   and the contents of fenced code blocks.
fn normalize(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut blank_run = 0u32;
    let mut in_code = false;
    for line in md.lines() {
        let line = line.trim_end();
        // Toggle on fenced code blocks; leave their content untouched.
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            blank_run = 0;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if line.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
            continue;
        }
        blank_run = 0;
        if in_code {
            out.push_str(line);
        } else {
            out.push_str(&collapse_spaces(line));
        }
        out.push('\n');
    }
    out.trim().to_string()
}

/// Collapse runs of 2+ spaces outside the leading indent into a single space.
fn collapse_spaces(line: &str) -> String {
    let indent_len = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(indent_len);
    let mut s = String::with_capacity(line.len());
    s.push_str(indent);
    let mut prev_space = false;
    for ch in rest.chars() {
        if ch == ' ' {
            if !prev_space {
                s.push(' ');
            }
            prev_space = true;
        } else {
            s.push(ch);
            prev_space = false;
        }
    }
    s
}
