//! Token accounting — quantifies how much the extraction pipeline saved.
//!
//! Uses the cl100k_base BPE as a stable, well-known approximation. Claude's
//! tokenizer differs, so treat these as *estimates*; the ratio between raw
//! and distilled is what matters and that holds across tokenizers.

use std::sync::OnceLock;

use tiktoken_rs::CoreBPE;

static BPE: OnceLock<CoreBPE> = OnceLock::new();

fn bpe() -> &'static CoreBPE {
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().expect("load cl100k_base tokenizer"))
}

/// Count tokens in a string (loads the tokenizer once, then reuses it).
pub fn count(text: &str) -> usize {
    bpe().encode_with_special_tokens(text).len()
}

/// Before/after token comparison for a single fetch.
#[derive(Debug, Clone, Copy)]
pub struct TokenStats {
    /// Tokens the raw HTML would have cost if sent as-is.
    pub raw_tokens: usize,
    /// Tokens the distilled output actually costs.
    pub output_tokens: usize,
}

impl TokenStats {
    pub fn measure(raw_html: &str, output: &str) -> Self {
        Self {
            raw_tokens: count(raw_html),
            output_tokens: count(output),
        }
    }

    pub fn saved(&self) -> usize {
        self.raw_tokens.saturating_sub(self.output_tokens)
    }

    /// Fraction of tokens saved, 0.0..=1.0.
    pub fn saved_ratio(&self) -> f64 {
        if self.raw_tokens == 0 {
            0.0
        } else {
            self.saved() as f64 / self.raw_tokens as f64
        }
    }
}
