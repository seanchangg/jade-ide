//! Frequency completion (§4.12) — the Rust port of `editor/frequency-completion.ts`.
//!
//! Scans the whole buffer for identifier-like words and suggests those appearing
//! **≥2 times**, **length ≥3**, that **case-insensitively prefix-match** what the
//! user has typed, excluding the current word itself. Ranked by count (descending,
//! ties alphabetical — the TS `sortText = (10000-count)|word`). Pure + unit-tested;
//! `app.rs` maps the results into `forge_lsp::CompletionItem`s and merges them into
//! the existing LSP popup (LSP wins ties; see `app.rs`).

use forge_lsp::{CompletionItem, CompletionItemKind};

/// Min occurrences to suggest (TS `MIN_OCCURRENCES`, :7).
pub const MIN_OCCURRENCES: usize = 2;
/// Min identifier length to count (TS `MIN_WORD_LENGTH`, :8).
pub const MIN_WORD_LENGTH: usize = 3;

/// One ranked frequency suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreqWord {
    pub word: String,
    pub count: usize,
}

/// Count identifier-like words (`[A-Za-z_][A-Za-z0-9_]*`) of length ≥3 across the
/// whole buffer, then filter/rank per the §4.12 rules against `current_word`
/// (the identifier the caret sits in / after). `current_word` is matched
/// case-insensitively as a prefix; an empty `current_word` keeps all qualifying
/// words (except the empty-string exclusion, which is a no-op).
pub fn suggestions(buffer_text: &str, current_word: &str) -> Vec<FreqWord> {
    use std::collections::HashMap;

    let mut counts: HashMap<&str, usize> = HashMap::new();
    for w in identifiers(buffer_text) {
        if w.chars().count() < MIN_WORD_LENGTH {
            continue;
        }
        *counts.entry(w).or_insert(0) += 1;
    }

    let needle = current_word.to_ascii_lowercase();
    let mut out: Vec<FreqWord> = counts
        .into_iter()
        .filter(|(w, c)| {
            *c >= MIN_OCCURRENCES
                && *w != current_word
                && (needle.is_empty() || w.to_ascii_lowercase().starts_with(&needle))
        })
        .map(|(w, count)| FreqWord {
            word: w.to_string(),
            count,
        })
        .collect();

    // Rank: higher count first, ties alphabetical (mirrors the TS sortText).
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.word.cmp(&b.word)));
    out
}

/// Build `forge_lsp::CompletionItem`s from ranked frequency words, tagged with
/// `detail "×N"` and a `sortText` that keeps them ordered by frequency in the
/// merged popup (TS `sortText`, :49).
pub fn completion_items(buffer_text: &str, current_word: &str) -> Vec<CompletionItem> {
    suggestions(buffer_text, current_word)
        .into_iter()
        .map(|f| CompletionItem {
            label: f.word.clone(),
            kind: Some(CompletionItemKind::TEXT),
            detail: Some(format!("×{}", f.count)),
            insert_text: Some(f.word.clone()),
            sort_text: Some(format!("{:05}{}", 10000usize.saturating_sub(f.count), f.word)),
            filter_text: Some(f.word.clone()),
            ..Default::default()
        })
        .collect()
}

/// Iterate identifier-like tokens (`[A-Za-z_][A-Za-z0-9_]*`) in `text`.
fn identifiers(text: &str) -> impl Iterator<Item = &str> {
    let bytes = text.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < bytes.len() {
            let c = bytes[i];
            if is_ident_start(c) {
                let start = i;
                i += 1;
                while i < bytes.len() && is_ident_cont(bytes[i]) {
                    i += 1;
                }
                return Some(&text[start..i]);
            }
            i += 1;
        }
        None
    })
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_cont(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_ranks_by_frequency() {
        let text = "alpha beta alpha gamma alpha beta";
        let s = suggestions(text, "");
        // alpha ×3, beta ×2 (gamma appears once → excluded).
        assert_eq!(
            s,
            vec![
                FreqWord { word: "alpha".into(), count: 3 },
                FreqWord { word: "beta".into(), count: 2 },
            ]
        );
    }

    #[test]
    fn excludes_short_words_and_singletons() {
        let text = "ab ab cd cd cd longword";
        // "ab" is length 2 → excluded; "cd" length 2 → excluded; "longword" ×1.
        let s = suggestions(text, "");
        assert!(s.is_empty());
    }

    #[test]
    fn case_insensitive_prefix_and_excludes_current() {
        let text = "Vector vector VECTOR value value velocity velocity";
        // typing "v": all v* with count≥2, minus current word "v" (not present as
        // a token anyway). "Vector"/"vector"/"VECTOR" are distinct tokens ×1 each,
        // so excluded; "value" ×2, "velocity" ×2.
        let s = suggestions(text, "v");
        let words: Vec<&str> = s.iter().map(|f| f.word.as_str()).collect();
        assert_eq!(words, vec!["value", "velocity"]);
    }

    #[test]
    fn excludes_the_current_word_itself() {
        let text = "total total total totally totally";
        let s = suggestions(text, "total");
        // "total" is the word being typed → excluded even though it's frequent;
        // "totally" ×2 still prefix-matches "total" and is suggested.
        let words: Vec<&str> = s.iter().map(|f| f.word.as_str()).collect();
        assert_eq!(words, vec!["totally"]);
    }

    #[test]
    fn ties_broken_alphabetically() {
        let text = "beta beta alpha alpha";
        let s = suggestions(text, "");
        assert_eq!(s[0].word, "alpha");
        assert_eq!(s[1].word, "beta");
    }

    #[test]
    fn completion_items_have_detail_and_sort() {
        let items = completion_items("foo foo bar bar bar", "");
        // bar ×3 sorts before foo ×2.
        assert_eq!(items[0].label, "bar");
        assert_eq!(items[0].detail.as_deref(), Some("×3"));
        assert!(items[0].sort_text.as_deref().unwrap() < items[1].sort_text.as_deref().unwrap());
    }

    #[test]
    fn identifiers_skip_numbers_and_punctuation() {
        let toks: Vec<&str> = identifiers("a1 + b2*c3; 42 _x").collect();
        assert_eq!(toks, vec!["a1", "b2", "c3", "_x"]);
    }
}
