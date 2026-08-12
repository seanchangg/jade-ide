//! AI inline-completion ("ghost text") pure logic — Rust port of the renderer's
//! `editor/inline-completion.ts` (§4.11).
//!
//! Everything here is deterministic and side-effect free so it can be unit-tested
//! without a llama-server: the [`GhostCache`] (with **typed-through** hits) and the
//! line-aware [`post_process`] pass. The app (`app.rs`) owns the debounce, the
//! generation-counter supersede, and the `forge-ai` `/infill` call; it feeds raw
//! model output through these functions to decide what ghost run to paint.
//!
//! Lessons ported verbatim from JetBrains' FLCC paper (arXiv 2405.08704, cited in
//! the TS at `:7-11`): debounce+cancel aggressively, cache **raw** model output so
//! typing through a suggestion never re-queries, and post-process line-aware — cut
//! at blank lines, cap length, never duplicate what already follows the cursor.

/// Debounce before issuing an `/infill` request (TS `DEBOUNCE_MS`, :17).
pub const DEBOUNCE_MS: u64 = 120;
/// Prefix cap in chars sent to the model (TS `MAX_PREFIX_CHARS`, :18).
pub const MAX_PREFIX_CHARS: usize = 6000;
/// Suffix cap in chars (TS `MAX_SUFFIX_CHARS`, :19).
pub const MAX_SUFFIX_CHARS: usize = 2000;
/// Multiline mode line cap (TS `MAX_LINES`, :20). Single-line mode uses 1.
pub const MAX_LINES: usize = 6;
/// Ring capacity of the suggestion cache (TS `CACHE_SIZE`, :21).
pub const CACHE_SIZE: usize = 48;

/// One cached raw suggestion, keyed by the exact `(prefix, suffix)` it was
/// generated for (TS `CacheEntry`, :23-27). `content` is the *raw* model output;
/// it is post-processed afresh at serve time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub prefix: String,
    pub suffix: String,
    pub content: String,
}

/// A bounded FIFO of raw suggestions with exact + typed-through lookup
/// (TS module-level `cache` + `cachePut`/`cacheLookup`, :29-51).
#[derive(Debug, Default)]
pub struct GhostCache {
    entries: Vec<CacheEntry>,
    capacity: usize,
}

impl GhostCache {
    /// A cache with the ported 48-entry capacity.
    pub fn new() -> Self {
        Self::with_capacity(CACHE_SIZE)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        GhostCache {
            entries: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    /// Number of live entries (test/introspection).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drop everything (called when the multiline mode flips — cached output was
    /// generated for one mode, TS `store.on('aiMultiline', …)`, :81).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Insert a raw suggestion, evicting the oldest past capacity (TS `cachePut`,
    /// :31-34).
    pub fn put(&mut self, prefix: impl Into<String>, suffix: impl Into<String>, content: impl Into<String>) {
        self.entries.push(CacheEntry {
            prefix: prefix.into(),
            suffix: suffix.into(),
            content: content.into(),
        });
        if self.entries.len() > self.capacity {
            self.entries.remove(0);
        }
    }

    /// Exact hit, or a **typed-through** hit: the user typed characters that match
    /// the start of an earlier suggestion, so serve the remainder instantly
    /// (TS `cacheLookup`, :38-51). Newest entries win (reverse scan).
    ///
    /// Returns the *raw* remainder to be post-processed by the caller.
    pub fn lookup(&self, prefix: &str, suffix: &str) -> Option<String> {
        for e in self.entries.iter().rev() {
            if e.suffix != suffix {
                continue;
            }
            if e.prefix == prefix {
                return Some(e.content.clone());
            }
            if let Some(typed) = prefix.strip_prefix(e.prefix.as_str()) {
                // The typed run must be a strict prefix of the suggestion, and
                // shorter than it, so there is a non-empty remainder to serve.
                if typed.len() < e.content.len() && e.content.starts_with(typed) {
                    return Some(e.content[typed.len()..].to_string());
                }
            }
        }
        None
    }
}

/// Post-process raw model output into the ghost text to paint, or `None` to
/// suppress it (TS `postProcess`, :53-73). Steps, in order:
///   1. strip `\r`,
///   2. drop leading blank lines (so a suggestion the model prefixed with an
///      empty line still shows real content on the ghost's first row),
///   3. truncate at the first blank line (`\n[ \t]*\n`),
///   4. cap to `max_lines` lines,
///   5. strip trailing whitespace,
///   6. drop a trailing duplicate of the trimmed text after the cursor,
///   7. suppress if only whitespace remains.
pub fn post_process(raw: &str, line_suffix: &str, max_lines: usize) -> Option<String> {
    let mut text: String = raw.replace('\r', "");

    // Drop leading whitespace-only lines. Right after Enter, FIM models often
    // emit a blank line before their code; without this the ghost's first
    // rendered line would be empty and read as "no suggestion".
    while let Some(nl) = text.find('\n') {
        if text[..nl].trim().is_empty() {
            text.drain(..=nl);
        } else {
            break;
        }
    }

    // Stop at the first blank line — beyond it the model is usually rambling.
    if let Some(idx) = find_blank_line(&text) {
        text.truncate(idx);
    }

    // Cap the number of lines.
    if max_lines >= 1 {
        let lines: Vec<&str> = text.split('\n').collect();
        if lines.len() > max_lines {
            text = lines[..max_lines].join("\n");
        }
    }

    // Strip trailing whitespace (JS `/\s+$/`).
    text = text.trim_end().to_string();

    // Don't duplicate what already sits after the cursor on this line (e.g. an
    // auto-inserted closing paren the model also generated).
    let tail = line_suffix.trim();
    if !tail.is_empty() && text.ends_with(tail) {
        let cut = text.len() - tail.len();
        text.truncate(cut);
        text = text.trim_end().to_string();
    }

    // Degenerate repetition (`1000000000000…`, a small-model decode failure)
    // means the whole candidate is garbage — drop it entirely, mirroring
    // FLCC's post-decode low-score candidate elimination (arXiv 2405.08704).
    // Whitespace runs are exempt: indentation legitimately repeats.
    if has_char_run(&text, 12) {
        return None;
    }

    // Suppress empty (JS `text.trim() ? text : null` — returns the *untrimmed*
    // text when non-empty, preserving any leading indentation).
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// True when `s` contains a run of at least `n` identical consecutive
/// non-whitespace chars (the repetition-blowup detector for [`post_process`]).
fn has_char_run(s: &str, n: usize) -> bool {
    let mut prev = None;
    let mut run = 0usize;
    for c in s.chars() {
        if c.is_whitespace() {
            prev = None;
            run = 0;
            continue;
        }
        if Some(c) == prev {
            run += 1;
            if run >= n {
                return true;
            }
        } else {
            prev = Some(c);
            run = 1;
        }
    }
    false
}

/// The byte index of the first `\n[ \t]*\n` blank-line separator (the position of
/// the leading `\n`), or `None`. Mirrors the JS regex `text.search(/\n[ \t]*\n/)`
/// without pulling in a regex.
fn find_blank_line(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'\n' {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// The prefix of `text` a word-level partial accept inserts (JetBrains FLCC's
/// "insert inline proposal word", Ctrl/Alt+Right): leading horizontal
/// whitespace plus either one identifier run (`alnum`/`_`) or one run of other
/// symbols. A leading newline is accepted alone (the break itself is the
/// "word"). Returns the whole text when there is nothing left to split.
pub fn first_word(text: &str) -> &str {
    #[derive(PartialEq)]
    enum S {
        LeadWs,
        Word,
        Symbols,
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut state = S::LeadWs;
    for (i, c) in text.char_indices() {
        match state {
            S::LeadWs => {
                if c == '\n' {
                    return &text[..i + 1];
                } else if c == ' ' || c == '\t' {
                } else if is_word(c) {
                    state = S::Word;
                } else {
                    state = S::Symbols;
                }
            }
            S::Word => {
                if !is_word(c) {
                    return &text[..i];
                }
            }
            S::Symbols => {
                if is_word(c) || c.is_whitespace() {
                    return &text[..i];
                }
            }
        }
    }
    text
}

/// Take the last `max` chars of `s` (prefix cap, TS `fullPrefix.slice(-MAX)`).
pub fn cap_prefix(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    s.chars().skip(n - max).collect()
}

/// Take the first `max` chars of `s` (suffix cap, TS `fullSuffix.slice(0, MAX)`).
pub fn cap_suffix(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Byte offset where `/infill`'s `input_suffix` starts for a caret at `caret`:
/// the END of the caret's line, not the caret itself.
///
/// This is the model's contract, not a detail. `corpus/fim_pack.py` builds every
/// training example as prefix = text up to the caret, middle = **the rest of
/// that line**, suffix = **the newline and everything after it**. The text
/// between the caret and the end of the line is the answer, so it is never in
/// the suffix. `eval_fim.py` scores the model the same way.
///
/// Sending `full[caret..]` instead put that text in the suffix, a shape the
/// model never saw in training, and the suggestions showed it: with the caret
/// before existing text the model read its own suffix back out (suggesting a
/// copy of the line already there), or completed a call it could see the end of
/// with different arguments. Caret at end of line — the common case while
/// typing — is unaffected, since there the two are the same string.
///
/// [`post_process`] then drops the part of the answer that duplicates
/// `line_suffix`, which is why the model may safely regenerate the whole line.
pub fn fim_suffix_start(full: &str, caret: usize) -> usize {
    match full[caret..].find('\n') {
        Some(offset) => caret + offset, // keep the '\n': training's suffix opens with it
        None => full.len(),             // last line, no trailing newline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_exact_hit_serves_full_content() {
        let mut c = GhostCache::new();
        c.put("foo(", ")", "bar, baz");
        assert_eq!(c.lookup("foo(", ")").as_deref(), Some("bar, baz"));
        // Suffix must match.
        assert_eq!(c.lookup("foo(", "").as_deref(), None);
        // Different prefix, no typed-through relationship.
        assert_eq!(c.lookup("zzz", ")").as_deref(), None);
    }

    #[test]
    fn cache_typed_through_serves_remainder() {
        let mut c = GhostCache::new();
        c.put("v.", "", "push_back(x)");
        // User typed "push" through the suggestion → remainder "_back(x)".
        assert_eq!(c.lookup("v.push", "").as_deref(), Some("_back(x)"));
        assert_eq!(c.lookup("v.push_back(", "").as_deref(), Some("x)"));
        // Typing the whole thing (no remainder) is not a hit.
        assert_eq!(c.lookup("v.push_back(x)", "").as_deref(), None);
        // Typing something that diverges is not a hit.
        assert_eq!(c.lookup("v.pop", "").as_deref(), None);
    }

    #[test]
    fn cache_newest_entry_wins() {
        let mut c = GhostCache::new();
        c.put("a", "", "OLD");
        c.put("a", "", "NEW");
        assert_eq!(c.lookup("a", "").as_deref(), Some("NEW"));
    }

    #[test]
    fn cache_evicts_oldest_past_capacity() {
        let mut c = GhostCache::with_capacity(2);
        c.put("1", "", "one");
        c.put("2", "", "two");
        c.put("3", "", "three"); // evicts "1"
        assert_eq!(c.len(), 2);
        assert_eq!(c.lookup("1", "").as_deref(), None);
        assert_eq!(c.lookup("3", "").as_deref(), Some("three"));
    }

    #[test]
    fn clear_drops_all() {
        let mut c = GhostCache::new();
        c.put("a", "", "x");
        c.clear();
        assert!(c.is_empty());
        assert_eq!(c.lookup("a", "").as_deref(), None);
    }

    #[test]
    fn post_process_truncates_at_blank_line() {
        let out = post_process("foo();\n\nunrelated stuff", "", 6);
        assert_eq!(out.as_deref(), Some("foo();"));
    }

    #[test]
    fn post_process_blank_line_with_trailing_ws() {
        // The blank line has spaces/tabs — still a separator.
        let out = post_process("a();\n   \t\nb();", "", 6);
        assert_eq!(out.as_deref(), Some("a();"));
    }

    #[test]
    fn post_process_caps_lines() {
        let raw = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8";
        // Single-line mode.
        assert_eq!(post_process(raw, "", 1).as_deref(), Some("l1"));
        // 6-line multiline cap.
        assert_eq!(
            post_process(raw, "", 6).as_deref(),
            Some("l1\nl2\nl3\nl4\nl5\nl6")
        );
    }

    #[test]
    fn post_process_strips_trailing_duplicate_tail() {
        // Model generated "…))" but the buffer already has ")" after the cursor.
        let out = post_process("compute(a, b))", ")", 6);
        assert_eq!(out.as_deref(), Some("compute(a, b)"));
    }

    #[test]
    fn post_process_ignores_empty_tail() {
        let out = post_process("x + y", "   ", 6);
        assert_eq!(out.as_deref(), Some("x + y"));
    }

    #[test]
    fn post_process_suppresses_empty() {
        assert_eq!(post_process("", "", 6), None);
        assert_eq!(post_process("   \n  ", "", 6), None);
        assert_eq!(post_process("\r\n", "", 6), None);
        // Text that is entirely the duplicate tail collapses to nothing.
        assert_eq!(post_process(")", ")", 6), None);
    }

    #[test]
    fn post_process_strips_cr() {
        assert_eq!(post_process("a();\r", "", 6).as_deref(), Some("a();"));
    }

    #[test]
    fn post_process_drops_leading_blank_lines() {
        // Right after Enter the model prefixes a blank line; the ghost should
        // still start with real content (the indentation of the first real line
        // is preserved).
        assert_eq!(
            post_process("\n    return 0;", "", 6).as_deref(),
            Some("    return 0;")
        );
        // Multiple leading blanks (incl. whitespace-only) are all dropped.
        assert_eq!(
            post_process("\n \t\nfoo();", "", 6).as_deref(),
            Some("foo();")
        );
        // A leading blank followed by a two-line body keeps both lines.
        assert_eq!(
            post_process("\na();\nb();", "", 6).as_deref(),
            Some("a();\nb();")
        );
    }

    #[test]
    fn post_process_drops_repetition_blowups() {
        // The 0.5B tier's observed failure mode: an unbounded digit run.
        assert_eq!(
            post_process("printf(\"%d\\n\", 1000000000000000000000);", "", 6),
            None
        );
        // Short legitimate repeats survive…
        assert_eq!(
            post_process("x == 1000000;", "", 6).as_deref(),
            Some("x == 1000000;")
        );
        // …and indentation (whitespace runs) is always exempt.
        assert_eq!(
            post_process("            deeply_indented();", "", 6).as_deref(),
            Some("            deeply_indented();")
        );
    }

    #[test]
    fn first_word_splits_at_boundaries() {
        // Identifier run, then the call punctuation.
        assert_eq!(first_word("printf(\"hi\");"), "printf");
        assert_eq!(first_word("(\"hi\");"), "(\"");
        // Leading indent rides along with the first word.
        assert_eq!(first_word("    return 0;"), "    return");
        // Single word → everything.
        assert_eq!(first_word("alpha"), "alpha");
        // A leading line break is accepted alone.
        assert_eq!(first_word("\n    foo();"), "\n");
        // Symbol run stops at whitespace or identifier.
        assert_eq!(first_word("+= 2;"), "+=");
    }

    #[test]
    fn cap_prefix_keeps_tail() {
        assert_eq!(cap_prefix("abcdef", 3), "def");
        assert_eq!(cap_prefix("ab", 3), "ab");
        // char-based, not byte-based.
        assert_eq!(cap_prefix("αβγδ", 2), "γδ");
    }

    #[test]
    fn cap_suffix_keeps_head() {
        assert_eq!(cap_suffix("abcdef", 3), "abc");
        assert_eq!(cap_suffix("ab", 3), "ab");
        assert_eq!(cap_suffix("αβγδ", 2), "αβ");
    }

    /// The `/infill` suffix must start at the end of the caret's line, matching
    /// `corpus/fim_pack.py` (middle = rest of the line, suffix = the newline
    /// onwards). Handing the line's tail to the model instead made it repeat
    /// text that was already on screen.
    #[test]
    fn fim_suffix_starts_at_the_end_of_the_caret_line() {
        let src = "int main() {\n    encoder->setBuffer(ABuffer, 0, 0);\n    return 0;\n}\n";
        let caret = src.find("ABuffer").unwrap(); // just after `setBuffer(`

        let start = fim_suffix_start(src, caret);
        assert_eq!(
            &src[start..],
            "\n    return 0;\n}\n",
            "the rest of the caret's line is the answer, so it stays out of the suffix"
        );

        // Caret already at end of line: the shape is unchanged from `full[caret..]`.
        let eol = src.find('\n').unwrap();
        assert_eq!(fim_suffix_start(src, eol), eol);

        // Last line with no trailing newline: the suffix is empty, not a panic.
        let tail = "int x = 1;";
        assert_eq!(fim_suffix_start(tail, 5), tail.len());
        assert_eq!(&tail[fim_suffix_start(tail, 5)..], "");

        // Multi-byte characters before the caret must not shift the boundary.
        let utf8 = "// αβγ done\nnext();\n";
        let caret = utf8.find(" done").unwrap();
        assert_eq!(&utf8[fim_suffix_start(utf8, caret)..], "\nnext();\n");
    }
}
