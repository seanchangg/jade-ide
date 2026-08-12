//! JSON-RPC-over-stdio framing: `Content-Length` headers + raw JSON bodies.
//!
//! The TypeScript client (`src/main/lsp-client.ts:64-66`) delegated this to
//! `vscode-jsonrpc`'s `StreamMessageReader`/`StreamMessageWriter`. We own the
//! framing directly so we depend on no editor/LSP transport crates (recorded
//! decision: direct thin client). This is the base-protocol framing from the
//! LSP spec: each message is `Content-Length: <n>\r\n\r\n` followed by exactly
//! `<n>` bytes of UTF-8 JSON. (`Content-Type` is accepted but ignored.)

/// Frame a JSON payload for the wire: a `Content-Length` header, a blank line,
/// then the raw body bytes.
pub fn encode_message(payload: &[u8]) -> Vec<u8> {
    let header = format!("Content-Length: {}\r\n\r\n", payload.len());
    let mut out = Vec::with_capacity(header.len() + payload.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(payload);
    out
}

/// Incremental decoder that reassembles whole message bodies from an arbitrary
/// byte stream. Bytes may arrive split across TCP/pipe reads in any way (mid
/// header, mid body); [`push`] accumulates and [`next_message`] yields complete
/// bodies one at a time. This is the piece the split-buffer unit test exercises.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Append freshly-read bytes to the internal buffer.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pop the next complete message body, or `None` if a full frame has not yet
    /// arrived. Call repeatedly after each [`push`] until it returns `None`.
    ///
    /// Malformed headers (a `Content-Length` that is not a valid integer) cause
    /// the offending header block to be skipped so the stream can resync rather
    /// than wedge forever.
    pub fn next_message(&mut self) -> Option<Vec<u8>> {
        loop {
            // Locate the CRLFCRLF separating headers from body.
            let sep = self.buf.windows(4).position(|w| w == b"\r\n\r\n")?;
            let header = &self.buf[..sep];
            let body_start = sep + 4;

            let content_length = header
                .split(|&b| b == b'\n')
                .filter_map(|line| {
                    let line = std::str::from_utf8(line).ok()?.trim();
                    let rest = line.strip_prefix("Content-Length:")?;
                    rest.trim().parse::<usize>().ok()
                })
                .next();

            let Some(len) = content_length else {
                // No valid Content-Length in this header block: drop it and
                // retry so a single bad frame can't stall the connection.
                self.buf.drain(..body_start);
                continue;
            };

            if self.buf.len() < body_start + len {
                // Body not fully arrived yet.
                return None;
            }

            let body = self.buf[body_start..body_start + len].to_vec();
            self.buf.drain(..body_start + len);
            return Some(body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_has_correct_header_and_body() {
        let framed = encode_message(br#"{"jsonrpc":"2.0"}"#);
        let text = String::from_utf8(framed).unwrap();
        assert_eq!(text, "Content-Length: 17\r\n\r\n{\"jsonrpc\":\"2.0\"}");
    }

    #[test]
    fn round_trip_single_message() {
        let body = br#"{"id":1,"result":null}"#;
        let framed = encode_message(body);
        let mut dec = FrameDecoder::new();
        dec.push(&framed);
        assert_eq!(dec.next_message().unwrap(), body);
        assert!(dec.next_message().is_none());
    }

    #[test]
    fn two_messages_in_one_push() {
        let mut dec = FrameDecoder::new();
        let mut stream = encode_message(b"aaa");
        stream.extend(encode_message(b"bbbb"));
        dec.push(&stream);
        assert_eq!(dec.next_message().unwrap(), b"aaa");
        assert_eq!(dec.next_message().unwrap(), b"bbbb");
        assert!(dec.next_message().is_none());
    }

    #[test]
    fn split_buffer_delivery_byte_by_byte() {
        // Deliver a framed message one byte at a time — the decoder must yield
        // nothing until the final byte lands, then exactly one body.
        let body = br#"{"method":"initialized","params":{}}"#;
        let framed = encode_message(body);
        let mut dec = FrameDecoder::new();
        for (i, b) in framed.iter().enumerate() {
            dec.push(&[*b]);
            if i + 1 < framed.len() {
                assert!(dec.next_message().is_none(), "premature message at byte {i}");
            }
        }
        assert_eq!(dec.next_message().unwrap(), body);
    }

    #[test]
    fn split_across_header_and_body_boundaries() {
        let body = br#"{"x":42}"#;
        let framed = encode_message(body);
        let mut dec = FrameDecoder::new();
        // Split mid-header.
        dec.push(&framed[..8]);
        assert!(dec.next_message().is_none());
        // Split mid-separator / mid-body.
        dec.push(&framed[8..framed.len() - 3]);
        assert!(dec.next_message().is_none());
        dec.push(&framed[framed.len() - 3..]);
        assert_eq!(dec.next_message().unwrap(), body);
    }

    #[test]
    fn resyncs_past_a_bad_header() {
        let mut dec = FrameDecoder::new();
        // A header block with no valid Content-Length, then a good frame.
        dec.push(b"Garbage: yes\r\n\r\n");
        dec.push(&encode_message(b"ok"));
        assert_eq!(dec.next_message().unwrap(), b"ok");
    }
}
