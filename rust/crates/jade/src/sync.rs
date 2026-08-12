//! Cross-file consistency engine (§4.13).
//!
//! Three detectors run after an edit settles (see `OpenTab::sync_debounce`):
//!
//! 1. Kernel rename: a `kernel void name(` definition in a `.metal` file
//!    changed its name. The fix updates each `"name"` string literal in the
//!    host sources.
//! 2. Similar lines: an edit replaced exactly one token on one line
//!    (`p.M` → `p.T`). The fix applies the same replacement to the other
//!    occurrences in the file.
//! 3. Hyperparameters: a constant with the same name has declaration sites in
//!    more than one file (`#define N_EMBED_CFG 384` in host and shader). A
//!    value change at one site propagates to the other sites.
//!
//! All functions here are pure text analysis. File and buffer access stays in
//! `app.rs`.

use std::ops::Range;
use std::path::Path;

/// A pending suggestion, shown in the editor banner until the user applies
/// (⌘⏎) or dismisses (Esc) it.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncSuggestion {
    /// A kernel definition in `path` was renamed; `refs` host references to
    /// the old name exist across `files` files.
    RenameKernel {
        old: String,
        new: String,
        refs: usize,
        files: usize,
    },
    /// One token changed on one line; `count` more occurrences of the old
    /// token remain in the same file.
    SimilarLines {
        from: String,
        to: String,
        count: usize,
    },
    /// A hyperparameter value changed at one declaration site; `sites`
    /// declaration sites in other files still hold a different value.
    Hyperparam {
        name: String,
        to: String,
        sites: usize,
        files: usize,
    },
}

impl SyncSuggestion {
    /// The banner message for this suggestion.
    pub fn message(&self) -> String {
        match self {
            SyncSuggestion::RenameKernel {
                old,
                new,
                refs,
                files,
            } => format!(
                "kernel '{old}' → '{new}': update {refs} host reference{} in {files} file{}",
                plural(*refs),
                plural(*files)
            ),
            SyncSuggestion::SimilarLines { from, to, count } => format!(
                "'{from}' → '{to}': apply to {count} more occurrence{} in this file",
                plural(*count)
            ),
            SyncSuggestion::Hyperparam {
                name,
                to,
                sites,
                files,
            } => format!(
                "{name} = {to}: propagate to {sites} declaration{} in {files} other file{}",
                plural(*sites),
                plural(*files)
            ),
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// True when `c` is part of a sync token. `.` is included so a member path
/// (`p.M`) is one token.
fn is_token_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.'
}

/// True when `c` starts an identifier.
fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

/// True when `c` continues an identifier.
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// ── File classification ───────────────────────────────────────────────────────

/// True for a Metal shader file.
pub fn is_metal(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("metal")
}

/// True for a host source file that can hold kernel-name string literals.
pub fn is_host_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("c" | "cc" | "cpp" | "cxx" | "m" | "mm" | "h" | "hh" | "hpp" | "swift")
    )
}

/// True for a file that can hold hyperparameter declarations.
pub fn is_hyperparam_file(path: &Path) -> bool {
    if is_host_source(path) || is_metal(path) {
        return true;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name == "CMakeLists.txt" || name.ends_with(".cmake")
}

// ── 1. Kernel definitions and host references ─────────────────────────────────

/// The kernel function names defined in a `.metal` source, in order.
/// Matches `kernel void <name> (` with any whitespace between the parts.
pub fn scan_kernel_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(pos) = text[i..].find("kernel") {
        let start = i + pos;
        i = start + "kernel".len();
        // Reject `mykernel` / `kernels`: both neighbors must be non-ident.
        let before_ok = start == 0 || !is_ident_char(bytes[start - 1] as char);
        let after = text[i..].chars().next();
        if !before_ok || after.is_none_or(is_ident_char) {
            continue;
        }
        let rest = text[i..].trim_start();
        let Some(rest) = rest.strip_prefix("void") else {
            continue;
        };
        if rest.chars().next().is_none_or(is_ident_char) {
            continue;
        }
        let rest = rest.trim_start();
        let name: String = rest.chars().take_while(|&c| is_ident_char(c)).collect();
        if name.is_empty() || !is_ident_start(name.chars().next().unwrap()) {
            continue;
        }
        if rest[name.len()..].trim_start().starts_with('(') {
            names.push(name);
        }
    }
    names
}

/// Detect a single rename between two kernel-name lists: exactly one name
/// disappeared and exactly one appeared.
pub fn detect_rename(old: &[String], new: &[String]) -> Option<(String, String)> {
    let removed: Vec<&String> = old.iter().filter(|n| !new.contains(n)).collect();
    let added: Vec<&String> = new.iter().filter(|n| !old.contains(n)).collect();
    match (removed.as_slice(), added.as_slice()) {
        ([r], [a]) => Some(((*r).clone(), (*a).clone())),
        _ => None,
    }
}

/// Byte ranges of every `"name"` string literal whose content is exactly
/// `name`. Each range covers the content between the quotes.
pub fn string_literal_sites(text: &str, name: &str) -> Vec<Range<usize>> {
    let mut sites = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if c != '"' {
            continue;
        }
        // Walk to the closing quote; honor backslash escapes.
        let content_start = start + 1;
        let mut end = None;
        while let Some((j, cj)) = chars.next() {
            match cj {
                '\\' => {
                    chars.next();
                }
                '"' => {
                    end = Some(j);
                    break;
                }
                '\n' => break,
                _ => {}
            }
        }
        if let Some(end) = end {
            if &text[content_start..end] == name {
                sites.push(content_start..end);
            }
        }
    }
    sites
}

/// Byte ranges in a host source that reference kernel `name`:
///
/// - the content of `"name"` string literals, and
/// - identifiers that are exactly `name`, or that start with `name` at a
///   camel/snake boundary (`residualAddPipeline` for kernel `residualAdd`).
///
/// Each range covers exactly the `name` characters. One replacement renames a
/// derived identifier's kernel part and keeps its suffix. `residualAddition`
/// does not match: the next character is lowercase, so it is a different word.
pub fn kernel_ref_sites(text: &str, name: &str) -> Vec<Range<usize>> {
    let mut sites = string_literal_sites(text, name);
    if name.is_empty() {
        return sites;
    }
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(pos) = text[i..].find(name) {
        let start = i + pos;
        let end = start + name.len();
        i = start + 1;
        if start > 0 && is_ident_char(bytes[start - 1] as char) {
            continue;
        }
        let boundary = match text[end..].chars().next() {
            None => true,
            Some(c) if !is_ident_char(c) => true, // standalone identifier
            Some(c) => c.is_uppercase() || c == '_' || c.is_ascii_digit(), // derived
        };
        if !boundary || sites.iter().any(|r| r.start == start) {
            continue;
        }
        sites.push(start..end);
    }
    sites.sort_by_key(|r| r.start);
    sites
}

// ── 2. Similar lines ──────────────────────────────────────────────────────────

/// Split a line into alternating token / separator segments.
fn segments(line: &str) -> Vec<(bool, &str)> {
    let mut out = Vec::new();
    let mut rest = line;
    let mut base = 0usize;
    while !rest.is_empty() {
        let first_is_token = rest.chars().next().map(is_token_char).unwrap_or(false);
        let len = rest
            .char_indices()
            .find(|&(_, c)| is_token_char(c) != first_is_token)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        out.push((first_is_token, &line[base..base + len]));
        base += len;
        rest = &line[base..];
    }
    out
}

/// Detect a one-token replacement between two versions of a line. All other
/// segments (tokens and separators) must be identical.
pub fn single_token_diff(old_line: &str, new_line: &str) -> Option<(String, String)> {
    let a = segments(old_line);
    let b = segments(new_line);
    if a.len() != b.len() {
        return None;
    }
    let mut diff = None;
    for (&(ta, sa), &(tb, sb)) in a.iter().zip(b.iter()) {
        if sa == sb {
            continue;
        }
        if !ta || !tb || diff.is_some() {
            return None;
        }
        diff = Some((sa.to_string(), sb.to_string()));
    }
    // Reject a pure-number swap ("128" → "256"): too noisy as a suggestion.
    match &diff {
        Some((from, _)) if from.chars().any(is_ident_start) => diff,
        _ => None,
    }
}

/// The single differing line between two texts, when exactly one line changed
/// and the line count is equal. Returns `(row, old_line, new_line)`.
pub fn single_line_diff<'a>(old: &'a str, new: &'a str) -> Option<(usize, &'a str, &'a str)> {
    let a: Vec<&str> = old.split('\n').collect();
    let b: Vec<&str> = new.split('\n').collect();
    if a.len() != b.len() {
        return None;
    }
    let mut found = None;
    for (row, (la, lb)) in a.iter().zip(b.iter()).enumerate() {
        if la != lb {
            if found.is_some() {
                return None;
            }
            found = Some((row, *la, *lb));
        }
    }
    found
}

/// Byte ranges of `token` in `text`, with token-boundary checks on both sides.
pub fn token_sites(text: &str, token: &str) -> Vec<Range<usize>> {
    let mut sites = Vec::new();
    if token.is_empty() {
        return sites;
    }
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(pos) = text[i..].find(token) {
        let start = i + pos;
        let end = start + token.len();
        let before_ok = start == 0 || !is_token_char(bytes[start - 1] as char);
        let after_ok = end >= text.len() || !is_token_char(text[end..].chars().next().unwrap());
        if before_ok && after_ok {
            sites.push(start..end);
            i = end;
        } else {
            i = start + 1;
        }
    }
    sites
}

// ── 3. Hyperparameters ────────────────────────────────────────────────────────

/// One hyperparameter declaration site: `name = value` in one of the
/// recognized forms. `value_range` is the byte range of the value token.
#[derive(Debug, Clone, PartialEq)]
pub struct HyperDecl {
    pub name: String,
    pub value: String,
    pub value_range: Range<usize>,
}

/// Scan one file for hyperparameter declarations. Recognized forms:
///
/// - `#define NAME VALUE`
/// - `set(NAME VALUE ...)` (CMake files only)
/// - `const` / `constexpr` / `constant` declarations: `... NAME = VALUE;`
///
/// VALUE must be one token (a literal or an identifier).
pub fn scan_hyperparams(text: &str, path: &Path) -> Vec<HyperDecl> {
    let is_cmake = !is_host_source(path) && !is_metal(path);
    let mut out = Vec::new();
    let mut base = 0usize;
    for line in text.split('\n') {
        let decl = if is_cmake {
            parse_cmake_set(line, base)
        } else {
            parse_define(line, base).or_else(|| parse_const_assign(line, base))
        };
        if let Some(d) = decl {
            // Keep the first site per name in a file (`#ifndef` guards can
            // repeat a name).
            if !out.iter().any(|e: &HyperDecl| e.name == d.name) {
                out.push(d);
            }
        }
        base += line.len() + 1;
    }
    out
}

/// `#define NAME VALUE` (VALUE = one token, nothing else after it but a comment).
fn parse_define(line: &str, base: usize) -> Option<HyperDecl> {
    let t = line.trim_start();
    let indent = line.len() - t.len();
    let rest = t.strip_prefix("#define")?;
    if rest.chars().next().is_none_or(|c| c != ' ' && c != '\t') {
        return None;
    }
    let rest_trim = rest.trim_start();
    let name: String = rest_trim.chars().take_while(|&c| is_ident_char(c)).collect();
    if name.is_empty() || !is_ident_start(name.chars().next().unwrap()) {
        return None;
    }
    let after_name = &rest_trim[name.len()..];
    let value_off = after_name.len() - after_name.trim_start().len();
    let value: String = after_name
        .trim_start()
        .chars()
        .take_while(|&c| is_token_char(c))
        .collect();
    if value.is_empty() {
        return None;
    }
    let tail = after_name.trim_start()[value.len()..].trim_start();
    if !(tail.is_empty() || tail.starts_with("//") || tail.starts_with("/*")) {
        return None;
    }
    let value_start = base + indent + ("#define".len() + (rest.len() - rest_trim.len()))
        + name.len()
        + value_off;
    Some(HyperDecl {
        name,
        value_range: value_start..value_start + value.len(),
        value,
    })
}

/// CMake `set(NAME VALUE ...)`.
fn parse_cmake_set(line: &str, base: usize) -> Option<HyperDecl> {
    let t = line.trim_start();
    let rest = t.strip_prefix("set(").or_else(|| t.strip_prefix("SET("))?;
    let name: String = rest.chars().take_while(|&c| is_ident_char(c)).collect();
    if name.is_empty() || !is_ident_start(name.chars().next().unwrap()) {
        return None;
    }
    let after_name = &rest[name.len()..];
    let value_off = after_name.len() - after_name.trim_start().len();
    if value_off == 0 {
        return None;
    }
    let value: String = after_name
        .trim_start()
        .chars()
        .take_while(|&c| is_token_char(c))
        .collect();
    if value.is_empty() {
        return None;
    }
    let value_start = base + (line.len() - t.len()) + "set(".len() + name.len() + value_off;
    Some(HyperDecl {
        name,
        value_range: value_start..value_start + value.len(),
        value,
    })
}

/// `const` / `constexpr` / `constant` declaration with `NAME = VALUE;`.
/// The name is the last identifier before `=`; VALUE is one token.
fn parse_const_assign(line: &str, base: usize) -> Option<HyperDecl> {
    let t = line.trim_start();
    let has_kw = ["const ", "constexpr ", "constant ", "static const "]
        .iter()
        .any(|kw| t.starts_with(kw) || t.contains(&format!(" {kw}")));
    if !has_kw {
        return None;
    }
    let eq = line.find('=')?;
    // No `==`, `<=` etc.
    if line[eq + 1..].starts_with('=') || (eq > 0 && "<>!+-*/&|^".contains(&line[eq - 1..eq])) {
        return None;
    }
    let before = &line[..eq];
    let name_end = before.trim_end();
    let name_start = name_end
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_ident_char(c))
        .last()
        .map(|(i, _)| i)?;
    let name = &name_end[name_start..];
    if name.is_empty() || !is_ident_start(name.chars().next().unwrap()) {
        return None;
    }
    let after = &line[eq + 1..];
    let value_off = after.len() - after.trim_start().len();
    let value: String = after
        .trim_start()
        .chars()
        .take_while(|&c| is_token_char(c))
        .collect();
    if value.is_empty() {
        return None;
    }
    let tail = after.trim_start()[value.len()..].trim_start();
    if !tail.starts_with(';') {
        return None;
    }
    let value_start = base + eq + 1 + value_off;
    Some(HyperDecl {
        name: name.to_string(),
        value_range: value_start..value_start + value.len(),
        value,
    })
}

/// Detect a value change between two declaration scans of the same file.
/// Returns `(name, old_value, new_value)` for the first changed name.
pub fn detect_value_change(old: &[HyperDecl], new: &[HyperDecl]) -> Option<(String, String, String)> {
    for d in new {
        if let Some(prev) = old.iter().find(|p| p.name == d.name) {
            if prev.value != d.value {
                return Some((d.name.clone(), prev.value.clone(), d.value.clone()));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn kernel_scan_finds_definitions() {
        let src = "\
#include <metal_stdlib>
kernel void MPPMatMul(device const float* A [[buffer(0)]]) {}
kernel void  embedForward (device float* x) {}
kernel
void lossBackward(uint gid) {}
void helper() {} // not a kernel
int kernels = 0; // ident containing 'kernel'
";
        assert_eq!(
            scan_kernel_names(src),
            vec!["MPPMatMul", "embedForward", "lossBackward"]
        );
    }

    #[test]
    fn rename_detected_only_for_single_swap() {
        let old = vec!["a".to_string(), "b".to_string()];
        let renamed = vec!["a".to_string(), "c".to_string()];
        assert_eq!(
            detect_rename(&old, &renamed),
            Some(("b".to_string(), "c".to_string()))
        );
        // Addition only: no rename.
        let grown = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(detect_rename(&old, &grown), None);
        // Two swaps: ambiguous, no rename.
        let two = vec!["x".to_string(), "y".to_string()];
        assert_eq!(detect_rename(&old, &two), None);
    }

    #[test]
    fn literal_sites_exact_match_only() {
        let src = r#"
p1 = makePipeline(library, "embedForward");
p2 = makePipeline(library, "embedForwardTwo");
static constexpr const char* name = "embedForward";
"#;
        let sites = string_literal_sites(src, "embedForward");
        assert_eq!(sites.len(), 2);
        for r in &sites {
            assert_eq!(&src[r.clone()], "embedForward");
        }
    }

    #[test]
    fn kernel_ref_sites_include_derived_identifiers() {
        let src = r#"
residualAddPipeline = makePipeline(library, "residualAdd");
residualAdd_pso = other;
encoder->setLabel(residualAddition);
myresidualAdd = 0;
dispatch(residualAdd);
"#;
        let sites = kernel_ref_sites(src, "residualAdd");
        // Literal + residualAddPipeline + residualAdd_pso + standalone
        // dispatch arg. NOT residualAddition (lowercase continuation) and NOT
        // myresidualAdd (no start boundary).
        assert_eq!(sites.len(), 4);
        for r in &sites {
            assert_eq!(&src[r.clone()], "residualAdd");
        }
        // A replacement renames only the kernel part of a derived identifier.
        let mut out = src.to_string();
        for r in sites.iter().rev() {
            out.replace_range(r.clone(), "residualAdd2");
        }
        assert!(out.contains("residualAdd2Pipeline"));
        assert!(out.contains("residualAdd2_pso"));
        assert!(out.contains("\"residualAdd2\""));
        assert!(out.contains("dispatch(residualAdd2)"));
        assert!(out.contains("residualAddition"), "different word untouched");
        assert!(out.contains("myresidualAdd"), "mid-identifier untouched");
    }

    #[test]
    fn single_token_diff_detects_member_swap() {
        assert_eq!(
            single_token_diff("Tensor q({p.M, p.K});", "Tensor q({p.T, p.K});"),
            Some(("p.M".to_string(), "p.T".to_string()))
        );
        // Two changed tokens: no suggestion.
        assert_eq!(
            single_token_diff("f(p.M, p.K)", "f(p.T, p.N)"),
            None
        );
        // Pure number change: no suggestion.
        assert_eq!(single_token_diff("x = 128;", "x = 256;"), None);
        // Separator change: no suggestion.
        assert_eq!(single_token_diff("f(a, b)", "f(a; b)"), None);
    }

    #[test]
    fn token_sites_respect_boundaries() {
        let src = "qw(p.M, x); kw(p.M2, p.M); s.p.M = 1;";
        let sites = token_sites(src, "p.M");
        // `p.M2` must not match; `s.p.M` is one token `s.p.M`, so no match.
        assert_eq!(sites.len(), 2);
        for r in &sites {
            assert_eq!(&src[r.clone()], "p.M");
        }
    }

    #[test]
    fn single_line_diff_finds_one_changed_row() {
        let old = "a\nb\nc";
        let new = "a\nB\nc";
        assert_eq!(single_line_diff(old, new), Some((1, "b", "B")));
        assert_eq!(single_line_diff("a\nb", "a\nb"), None);
        assert_eq!(single_line_diff("a\nb\nc", "a\nB\nC"), None);
    }

    #[test]
    fn hyperparam_scan_define_and_const() {
        let cpp = PathBuf::from("main.cpp");
        let src = "\
#ifndef N_EMBED_CFG
#define N_EMBED_CFG 384
#endif
static const int N_LAYERS = 6;
static const int N_EMBED = N_EMBED_CFG;
int not_const = 3;
";
        let decls = scan_hyperparams(src, &cpp);
        let names: Vec<&str> = decls.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["N_EMBED_CFG", "N_LAYERS", "N_EMBED"]);
        let d = &decls[0];
        assert_eq!(d.value, "384");
        assert_eq!(&src[d.value_range.clone()], "384");
        assert_eq!(decls[2].value, "N_EMBED_CFG");
    }

    #[test]
    fn hyperparam_scan_metal_constant() {
        let metal = PathBuf::from("linear.metal");
        let src = "#define N_EMBED_CFG 384\nconstant uint N_EMBED = N_EMBED_CFG; //comment\n";
        let decls = scan_hyperparams(src, &metal);
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0].name, "N_EMBED_CFG");
        assert_eq!(decls[1].name, "N_EMBED");
        assert_eq!(&src[decls[1].value_range.clone()], "N_EMBED_CFG");
    }

    #[test]
    fn hyperparam_scan_cmake_set() {
        let cmake = PathBuf::from("CMakeLists.txt");
        let src = "set(N_EMBED 384 CACHE STRING \"embedding dim\")\nset(BLOCK_SIZE 256 CACHE STRING \"seq len\")\n";
        let decls = scan_hyperparams(src, &cmake);
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0].name, "N_EMBED");
        assert_eq!(&src[decls[0].value_range.clone()], "384");
        assert_eq!(decls[1].value, "256");
    }

    #[test]
    fn value_change_detected_by_name() {
        let cpp = PathBuf::from("main.cpp");
        let old = scan_hyperparams("#define N_EMBED_CFG 384\n", &cpp);
        let new = scan_hyperparams("#define N_EMBED_CFG 512\n", &cpp);
        assert_eq!(
            detect_value_change(&old, &new),
            Some((
                "N_EMBED_CFG".to_string(),
                "384".to_string(),
                "512".to_string()
            ))
        );
        assert_eq!(detect_value_change(&old, &old), None);
    }

    #[test]
    fn banner_messages_read_well() {
        let s = SyncSuggestion::RenameKernel {
            old: "MPPMatMul".into(),
            new: "MPPMatMulTiled".into(),
            refs: 4,
            files: 2,
        };
        assert_eq!(
            s.message(),
            "kernel 'MPPMatMul' → 'MPPMatMulTiled': update 4 host references in 2 files"
        );
        let s = SyncSuggestion::SimilarLines {
            from: "p.M".into(),
            to: "p.T".into(),
            count: 1,
        };
        assert_eq!(
            s.message(),
            "'p.M' → 'p.T': apply to 1 more occurrence in this file"
        );
    }
}
