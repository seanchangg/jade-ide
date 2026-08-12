//! Output-panel model (feature inventory §6 "status lives in the terminal"):
//! a capped scrollback of plain-text lines. The real xterm terminal is Phase-4;
//! for now build/cmake/run/debug output and `[jade]` status lines land here as
//! ANSI-stripped monospace text.
//!
//! Pure, unit-tested helpers — the renderer just paints the tail.

/// Scrollback cap (§10 has no explicit terminal-scrollback constant; 2000 lines
/// is the Phase-3 brief's value — enough for a noisy cmake configure).
pub const MAX_SCROLLBACK: usize = 2000;

/// Strip ANSI escape sequences (CSI `\x1b[...m` etc.) so colored `[jade]` /
/// `[cmake]` / ASan lines render as plain text. A real terminal (Phase-4) will
/// interpret them; here we only need readable text.
pub fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            // ESC — consume a CSI (`\x1b[ ... final`) or a two-byte escape.
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                // Parameter/intermediate bytes 0x20..=0x3f, terminated by a
                // final byte 0x40..=0x7e.
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1; // skip the final byte
                }
            } else {
                i += 1; // lone ESC or two-byte escape — drop the ESC
            }
        } else {
            // Copy this UTF-8 scalar wholesale (find the next char boundary).
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i] & 0xc0) == 0x80 {
                i += 1;
            }
            out.push_str(&s[start..i]);
        }
    }
    out
}

/// Append `text` (possibly multi-line, possibly ANSI-colored) to a scrollback
/// `buf`, splitting on newlines, stripping ANSI, and enforcing [`MAX_SCROLLBACK`]
/// (oldest lines evicted first). Trailing empty line from a terminal newline is
/// dropped so `"a\n"` pushes one line, not two.
pub fn push_output(buf: &mut Vec<String>, text: &str) {
    let stripped = strip_ansi(text);
    let mut lines: Vec<&str> = stripped.split('\n').collect();
    // Drop a single trailing empty produced by a terminating newline.
    if lines.len() > 1 && lines.last() == Some(&"") {
        lines.pop();
    }
    for line in lines {
        buf.push(line.trim_end_matches('\r').to_string());
    }
    if buf.len() > MAX_SCROLLBACK {
        let drop = buf.len() - MAX_SCROLLBACK;
        buf.drain(0..drop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_sgr_color_codes() {
        // The exact `[jade]` status line shape from app.ts:1028.
        let s = "\x1b[36m[jade]\x1b[0m \x1b[32mBuild succeeded\x1b[0m (12ms)";
        assert_eq!(strip_ansi(s), "[jade] Build succeeded (12ms)");
    }

    #[test]
    fn strips_cursor_and_erase_sequences() {
        assert_eq!(strip_ansi("a\x1b[2K\x1b[1;5Hb"), "ab");
    }

    #[test]
    fn preserves_plain_and_unicode() {
        assert_eq!(strip_ansi("▪▪▫ plain"), "▪▪▫ plain");
    }

    #[test]
    fn push_splits_lines_and_drops_trailing_newline() {
        let mut buf = Vec::new();
        push_output(&mut buf, "\r\n\x1b[36m[jade]\x1b[0m one\r\n");
        // "\r\n..." → leading empty line, then the content line.
        assert_eq!(buf, vec!["".to_string(), "[jade] one".to_string()]);
    }

    #[test]
    fn push_multiline_block() {
        let mut buf = Vec::new();
        push_output(&mut buf, "line1\nline2\nline3");
        assert_eq!(buf.len(), 3);
        assert_eq!(buf[2], "line3");
    }

    #[test]
    fn scrollback_caps_at_max() {
        let mut buf = Vec::new();
        for i in 0..(MAX_SCROLLBACK + 500) {
            push_output(&mut buf, &format!("line {i}"));
        }
        assert_eq!(buf.len(), MAX_SCROLLBACK);
        // Oldest 500 evicted: first retained is line 500.
        assert_eq!(buf[0], "line 500");
        assert_eq!(buf.last().unwrap(), &format!("line {}", MAX_SCROLLBACK + 499));
    }
}
