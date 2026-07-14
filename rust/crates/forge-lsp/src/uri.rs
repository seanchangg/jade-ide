//! Path ↔ `file://` URI conversion and the language-id mapping.
//!
//! lsp-types 0.97 models URIs with `fluent_uri::Uri` and provides no
//! `from_file_path` helper (unlike the old `url::Url`), so we build and parse
//! `file://` URIs ourselves with minimal percent-encoding.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use lsp_types::Uri;

/// Map a file path to clangd's `languageId`. Direct port of `getLanguageId`
/// (`src/main/lsp-client.ts:234-241`): `.h/.hpp/.hxx → cpp`, `.c → c`,
/// `.m → objective-c`, `.mm → objective-cpp`, `.cu → cuda`, else `cpp`.
///
/// Note the TS default (and the `.h*` family) is `cpp`, matching a C++-first
/// codebase; a bare `.h` is treated as a C++ header.
pub fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("h") | Some("hpp") | Some("hxx") => "cpp",
        Some("c") => "c",
        Some("m") => "objective-c",
        Some("mm") => "objective-cpp",
        Some("cu") => "cuda",
        _ => "cpp",
    }
}

/// Percent-encode the bytes of an absolute path that are not allowed unescaped
/// in a URI path. We keep the RFC 3986 unreserved set plus `/`, `.`, `-`, `_`,
/// `~`, and `:` (Windows drive letters, harmless on POSIX) verbatim.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b'/' | b'.' | b'-' | b'_' | b'~' | b':');
        if keep {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Build a `file://` [`Uri`] from an absolute path. Mirrors the TS
/// `file://${rootPath}` construction (`lsp-client.ts:38`) but with the
/// percent-encoding `url.Url` did for free.
pub fn uri_from_path(path: &Path) -> Uri {
    let encoded = encode_path(&path.to_string_lossy());
    Uri::from_str(&format!("file://{encoded}"))
        .expect("absolute path yields a valid file:// URI")
}

/// Recover a filesystem path from a `file://` URI, reversing percent-encoding.
/// Used to key [`crate::LspEvent::Diagnostics`] by path for jade.
pub fn path_from_uri(uri: &Uri) -> PathBuf {
    let s = uri.as_str();
    let rest = s.strip_prefix("file://").unwrap_or(s);
    PathBuf::from(percent_decode(rest))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_id_mapping_matches_ts() {
        assert_eq!(language_id(Path::new("/a/b.h")), "cpp");
        assert_eq!(language_id(Path::new("/a/b.hpp")), "cpp");
        assert_eq!(language_id(Path::new("/a/b.hxx")), "cpp");
        assert_eq!(language_id(Path::new("/a/b.c")), "c");
        assert_eq!(language_id(Path::new("/a/b.m")), "objective-c");
        assert_eq!(language_id(Path::new("/a/b.mm")), "objective-cpp");
        assert_eq!(language_id(Path::new("/a/b.cu")), "cuda");
        assert_eq!(language_id(Path::new("/a/b.cpp")), "cpp");
        assert_eq!(language_id(Path::new("/a/b.cc")), "cpp");
        assert_eq!(language_id(Path::new("/a/noext")), "cpp");
    }

    #[test]
    fn uri_round_trips_plain_path() {
        let p = Path::new("/tmp/forge/main.cpp");
        let uri = uri_from_path(p);
        assert_eq!(uri.as_str(), "file:///tmp/forge/main.cpp");
        assert_eq!(path_from_uri(&uri), p);
    }

    #[test]
    fn uri_encodes_and_decodes_spaces() {
        let p = Path::new("/tmp/a b/main.cpp");
        let uri = uri_from_path(p);
        assert_eq!(uri.as_str(), "file:///tmp/a%20b/main.cpp");
        assert_eq!(path_from_uri(&uri), p);
    }
}
