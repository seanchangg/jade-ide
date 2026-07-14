//! Execution-flow analysis (feature inventory §4.8, port of `execution-flow.ts`
//! `analyzeFlowFromSource`/`traceBody`, `:478-694`).
//!
//! A static regex/brace-count pass (no AST, no editing). It finds every
//! function/method definition with its line range, a coarse class-range map
//! (used to disambiguate same-named methods across classes), and then traces
//! each body into a list of [`FlowSegment`]s:
//!   - `sequential` — statement → next statement,
//!   - `call` + `return` — a resolved call site paired with its target's
//!     entry/exit lines (call resolution is variable-type aware, with a
//!     line-proximity fallback),
//!   - `branch` — `if`/`else`/`switch`,
//!   - `loop-back` — the closing brace of a `for`/`while`/`do` back to its head.
//!
//! From the segments a per-line [`FlowGlyph`] map is derived (which glyph a line
//! shows in the margin + its Cmd+Click navigation targets), mirroring
//! `applyFlowDecorations`' `lineTypes` logic (`:335-349`) and the `onMouseDown`
//! navigation rules (`:279-319`).
//!
//! ── Parity note ──
//! This is a faithful port of the regex heuristics, including their limits: the
//! function-signature regex requires the opening `{` on the signature line
//! (multi-line signatures are missed), the class-range map is the same coarse
//! `startLine = defEnd - 10` approximation the TS uses (`:506`), and greedy
//! template matching in `pair<A, pair<B,C>>` splits at the inner comma. What it
//! *does* resolve: free functions, single-definition calls, and same-named
//! methods in two classes picked by the receiver's declared type.

use std::collections::HashMap;

use regex::Regex;

/// Kind of a traced flow segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegKind {
    Sequential,
    Call,
    Return,
    Branch,
    LoopBack,
}

/// One traced edge between two 1-based source lines.
#[derive(Debug, Clone, PartialEq)]
pub struct FlowSegment {
    pub from_line: usize,
    pub to_line: usize,
    pub kind: SegKind,
    /// Present on `call` segments: `Type::method` or `func`.
    pub label: Option<String>,
}

/// Which glyph a line shows in the margin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphKind {
    Sequential, // ●
    Call,       // →
    Return,     // ↩
    Loop,       // ↺
    Branch,     // ⑂
    Error,      // ⊘ (applied at render from the error line, not by analysis)
}

impl GlyphKind {
    /// The margin glyph character (§4.8).
    pub fn glyph(self) -> char {
        match self {
            GlyphKind::Sequential => '\u{25cf}', // ●
            GlyphKind::Call => '\u{2192}',       // →
            GlyphKind::Return => '\u{21a9}',     // ↩
            GlyphKind::Loop => '\u{21ba}',       // ↺
            GlyphKind::Branch => '\u{2442}',     // ⑂
            GlyphKind::Error => '\u{2298}',      // ⊘
        }
    }
}

/// A per-line glyph plus its Cmd+Click navigation targets (1-based line numbers,
/// in priority order; navigation uses the first).
#[derive(Debug, Clone, PartialEq)]
pub struct FlowGlyph {
    pub kind: GlyphKind,
    pub targets: Vec<usize>,
}

/// The full flow analysis of a source buffer.
#[derive(Debug, Clone, Default)]
pub struct FlowAnalysis {
    pub segments: Vec<FlowSegment>,
    pub glyphs: HashMap<usize, FlowGlyph>,
}

// ── Cached regexes ─────────────────────────────────────────────────────────

macro_rules! lazy_re {
    ($f:ident, $p:expr) => {
        fn $f() -> &'static Regex {
            static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
            RE.get_or_init(|| Regex::new($p).unwrap())
        }
    };
}

lazy_re!(re_class, r"^(?:struct|class)\s+(\w+)");
lazy_re!(
    re_func,
    r"^(?:[\w:<>*&\s]+?)\s+(\w+(?:::\w+)?)\s*\([^)]*\)\s*(?:const\s*)?(?:override\s*)?(?:noexcept\s*)?\{"
);
lazy_re!(re_ctrl_head, r"^(?:if|for|while|switch|do)\b");
lazy_re!(re_loop, r"^(?:for|while|do)\b");
lazy_re!(re_branch, r"^(?:if|else|switch)\b");
lazy_re!(re_call, r"(?:(\w+)\s*[.\->]+\s*)?(\w+)\s*\(");
lazy_re!(
    re_call_skip,
    r"^(?:if|for|while|switch|return|case|class|struct|int|void|double|float|auto|bool|char)\b"
);

/// Build a regex matching `(\w+)\s+NAME\s*[=(]` for a specific variable name.
fn re_var_decl(name: &str) -> Regex {
    Regex::new(&format!(r"(\w+)\s+{}\s*[=(]", regex::escape(name))).unwrap()
}

/// Build a regex matching `(\w+)[*&<].*\s+NAME\b` for a specific variable name.
fn re_var_ptr(name: &str) -> Regex {
    Regex::new(&format!(r"(\w+)[*&<].*\s+{}\b", regex::escape(name))).unwrap()
}

#[derive(Clone)]
struct FuncDef {
    start_line: usize,
    end_line: usize,
    class_name: Option<String>,
}

struct FuncRange {
    start_idx: usize,
    end_idx: usize,
    start_line: usize,
}

/// Analyze a source buffer into segments + per-line glyphs.
pub fn analyze(source: &str) -> FlowAnalysis {
    let lines: Vec<&str> = source.split('\n').collect();
    let segments = analyze_segments(&lines);
    let glyphs = derive_glyphs(&segments);
    FlowAnalysis { segments, glyphs }
}

/// Pass 1+2: find all function bodies, then trace each (`:478-599`).
fn analyze_segments(lines: &[&str]) -> Vec<FlowSegment> {
    let mut segments: Vec<FlowSegment> = Vec::new();

    // shortName → all definitions (handles overloads / same-name methods).
    let mut func_defs_all: HashMap<String, Vec<FuncDef>> = HashMap::new();
    let mut all_func_ranges: Vec<FuncRange> = Vec::new();

    // Coarse pre-scan of class ranges (`:493-510`). NB the `startLine` is the
    // TS's deliberate approximation (`i - 10`).
    let mut class_ranges: Vec<(String, i64, i64)> = Vec::new();
    {
        let mut tmp_class: Option<String> = None;
        let mut tmp_depth = 0i64;
        for (i, raw) in lines.iter().enumerate() {
            let t = raw.trim();
            if tmp_class.is_none() {
                if let Some(c) = re_class().captures(t) {
                    if t.contains('{') {
                        tmp_class = Some(c[1].to_string());
                        tmp_depth = 0;
                    }
                }
            }
            if tmp_class.is_some() {
                for ch in t.chars() {
                    if ch == '{' {
                        tmp_depth += 1;
                    }
                    if ch == '}' {
                        tmp_depth -= 1;
                    }
                }
                if tmp_depth <= 0 {
                    let name = tmp_class.take().unwrap();
                    class_ranges.push((name, i as i64 - 10, i as i64 + 1));
                }
            }
        }
    }

    let class_for_line = |line: usize| -> Option<String> {
        for (name, start, end) in &class_ranges {
            if line as i64 >= *start && line as i64 <= *end {
                return Some(name.clone());
            }
        }
        None
    };

    // Pass 1: function/method definitions with line ranges (`:519-551`).
    {
        let mut cur_func: Option<String> = None;
        let mut cur_depth = 0i64;
        let mut cur_start_idx = 0usize;
        let mut cur_start_line = 0usize;

        for (i, raw) in lines.iter().enumerate() {
            let t = raw.trim();
            if cur_func.is_none() {
                if let Some(c) = re_func().captures(t) {
                    if !re_ctrl_head().is_match(t) {
                        cur_func = Some(c[1].to_string());
                        cur_depth = 0;
                        cur_start_idx = i;
                        cur_start_line = i + 1;
                    }
                }
            }
            if cur_func.is_some() {
                for ch in t.chars() {
                    if ch == '{' {
                        cur_depth += 1;
                    }
                    if ch == '}' {
                        cur_depth -= 1;
                    }
                }
                if cur_depth <= 0 {
                    let name = cur_func.take().unwrap();
                    let short = name
                        .rsplit("::")
                        .next()
                        .unwrap_or(&name)
                        .to_string();
                    let class_name = class_for_line(cur_start_line);
                    let def = FuncDef {
                        start_line: cur_start_line,
                        end_line: i + 1,
                        class_name,
                    };
                    func_defs_all.entry(short.clone()).or_default().push(def);
                    all_func_ranges.push(FuncRange {
                        start_idx: cur_start_idx,
                        end_idx: i,
                        start_line: cur_start_line,
                    });
                }
            }
        }
    }

    // Pass 2: trace every function body.
    for func in &all_func_ranges {
        trace_body(
            lines,
            func.start_idx,
            func.end_idx,
            func.start_line,
            &func_defs_all,
            &mut segments,
        );
    }

    segments
}

/// Pick the best `FuncDef` for a call given the receiver's type (`:568-592`).
fn resolve_func_def(
    func_defs_all: &HashMap<String, Vec<FuncDef>>,
    called: &str,
    object_type: Option<&str>,
    call_line: usize,
    own_start_line: usize,
) -> Option<FuncDef> {
    let defs = func_defs_all.get(called)?;
    if defs.is_empty() {
        return None;
    }
    if defs.len() == 1 {
        return if defs[0].start_line != own_start_line {
            Some(defs[0].clone())
        } else {
            None
        };
    }
    // Match by class name.
    if let Some(obj) = object_type {
        if let Some(m) = defs
            .iter()
            .find(|d| d.class_name.as_deref() == Some(obj) && d.start_line != own_start_line)
        {
            return Some(m.clone());
        }
    }
    // Fall back to closest definition by line proximity.
    let mut best: Option<FuncDef> = None;
    let mut best_dist = i64::MAX;
    for d in defs {
        if d.start_line == own_start_line {
            continue;
        }
        let dist = (d.start_line as i64 - call_line as i64).abs();
        if dist < best_dist {
            best_dist = dist;
            best = Some(d.clone());
        }
    }
    best
}

/// Trace a single function body into segments (`:602-694`).
fn trace_body(
    lines: &[&str],
    start_idx: usize,
    end_idx: usize,
    own_start_line: usize,
    func_defs_all: &HashMap<String, Vec<FuncDef>>,
    segments: &mut Vec<FlowSegment>,
) {
    let mut brace_depth = 0i64;
    let mut entered = false;
    let mut last_line: i64 = -1;
    // (loop-head line, depth before the loop head).
    let mut loop_stack: Vec<(usize, i64)> = Vec::new();

    for i in start_idx..=end_idx.min(lines.len().saturating_sub(1)) {
        let line = lines[i].trim();
        let line_num = i + 1;

        let prev_depth = brace_depth;
        for ch in line.chars() {
            if ch == '{' {
                brace_depth += 1;
                entered = true;
            }
            if ch == '}' {
                brace_depth -= 1;
            }
        }

        if entered && brace_depth <= 0 {
            break;
        }
        if !entered {
            continue;
        }

        // Loop-back: a closing brace returning to (or below) a loop head's depth.
        // DEVIATION (documented): the TS checks this *after* skipping bare `}`
        // lines (`:627-628`), which makes `↺` never fire for the common
        // `}`-on-its-own loop close. We run it first so loop-back actually works,
        // as the spec (§4.8) and deliverable require. Non-loop `}` (empty stack)
        // still falls through to the bare-brace skip below.
        if brace_depth < prev_depth && !loop_stack.is_empty() {
            let top = *loop_stack.last().unwrap();
            if brace_depth <= top.1 {
                loop_stack.pop();
                segments.push(FlowSegment {
                    from_line: line_num,
                    to_line: top.0,
                    kind: SegKind::LoopBack,
                    label: None,
                });
                last_line = line_num as i64;
            }
            continue;
        }

        if line.is_empty() || line == "{" || line == "}" || line.starts_with("//") {
            continue;
        }

        // Loops.
        if re_loop().is_match(line) {
            if last_line > 0 {
                segments.push(FlowSegment {
                    from_line: last_line as usize,
                    to_line: line_num,
                    kind: SegKind::Sequential,
                    label: None,
                });
            }
            loop_stack.push((line_num, prev_depth));
            last_line = line_num as i64;
            continue;
        }

        // Branches.
        if re_branch().is_match(line) {
            if last_line > 0 {
                segments.push(FlowSegment {
                    from_line: last_line as usize,
                    to_line: line_num,
                    kind: SegKind::Branch,
                    label: None,
                });
            }
            last_line = line_num as i64;
            continue;
        }

        // Function/method calls.
        if let Some(cm) = re_call().captures(line) {
            if !re_call_skip().is_match(line) {
                if last_line > 0 && last_line as usize != line_num {
                    segments.push(FlowSegment {
                        from_line: last_line as usize,
                        to_line: line_num,
                        kind: SegKind::Sequential,
                        label: None,
                    });
                }

                let object_name = cm.get(1).map(|m| m.as_str().to_string());
                let called_name = cm[2].to_string();

                // Resolve the receiver's type from declarations above the call.
                let mut object_type: Option<String> = None;
                if let Some(obj) = &object_name {
                    let decl_re = re_var_decl(obj);
                    let ptr_re = re_var_ptr(obj);
                    let mut j = i as i64 - 1;
                    while j >= start_idx as i64 {
                        let prev = lines[j as usize].trim();
                        if let Some(c) = decl_re.captures(prev) {
                            object_type = Some(c[1].to_string());
                            break;
                        }
                        if let Some(c) = ptr_re.captures(prev) {
                            object_type = Some(c[1].to_string());
                            break;
                        }
                        j -= 1;
                    }
                }

                if let Some(def) = resolve_func_def(
                    func_defs_all,
                    &called_name,
                    object_type.as_deref(),
                    line_num,
                    own_start_line,
                ) {
                    let label = match &object_type {
                        Some(ot) => format!("{ot}::{called_name}"),
                        None => called_name.clone(),
                    };
                    segments.push(FlowSegment {
                        from_line: line_num,
                        to_line: def.start_line,
                        kind: SegKind::Call,
                        label: Some(label),
                    });
                    segments.push(FlowSegment {
                        from_line: def.end_line,
                        to_line: line_num,
                        kind: SegKind::Return,
                        label: None,
                    });
                }

                last_line = line_num as i64;
                continue;
            }
        }

        // Regular statement.
        if last_line > 0 && last_line as usize != line_num {
            segments.push(FlowSegment {
                from_line: last_line as usize,
                to_line: line_num,
                kind: SegKind::Sequential,
                label: None,
            });
        }
        last_line = line_num as i64;
    }
}

/// Derive per-line glyphs from segments, mirroring `applyFlowDecorations`'
/// `lineTypes` logic (`:335-349`) and the navigation-target rules (`:279-319`).
fn derive_glyphs(segments: &[FlowSegment]) -> HashMap<usize, FlowGlyph> {
    // First establish each flow line's glyph kind.
    let mut line_types: HashMap<usize, GlyphKind> = HashMap::new();
    for seg in segments {
        let kind = seg_glyph(seg.kind);
        // Prefer more-specific kinds over sequential for the to-line.
        let overwrite = matches!(
            line_types.get(&seg.to_line),
            None | Some(GlyphKind::Sequential)
        );
        if overwrite {
            line_types.insert(seg.to_line, kind);
        }
        // The from-line: call → call glyph, else sequential (don't clobber).
        line_types.entry(seg.from_line).or_insert(if seg.kind == SegKind::Call {
            GlyphKind::Call
        } else {
            GlyphKind::Sequential
        });
    }

    // Collect Cmd+Click navigation targets per line (priority order matches the
    // TS `onMouseDown` cascade: call-from, return-to, call-to, return-from).
    let mut out: HashMap<usize, FlowGlyph> = line_types
        .into_iter()
        .map(|(line, kind)| {
            (
                line,
                FlowGlyph {
                    kind,
                    targets: Vec::new(),
                },
            )
        })
        .collect();

    for seg in segments {
        match seg.kind {
            SegKind::Call => {
                if let Some(g) = out.get_mut(&seg.from_line) {
                    push_unique(&mut g.targets, seg.to_line); // call → definition
                }
                if let Some(g) = out.get_mut(&seg.to_line) {
                    push_unique(&mut g.targets, seg.from_line); // entry → caller
                }
            }
            SegKind::Return => {
                if let Some(g) = out.get_mut(&seg.to_line) {
                    push_unique(&mut g.targets, seg.from_line); // return-to → source
                }
                if let Some(g) = out.get_mut(&seg.from_line) {
                    push_unique(&mut g.targets, seg.to_line); // return-stmt → dest
                }
            }
            SegKind::LoopBack => {
                if let Some(g) = out.get_mut(&seg.from_line) {
                    push_unique(&mut g.targets, seg.to_line);
                }
            }
            _ => {}
        }
    }

    out
}

fn push_unique(v: &mut Vec<usize>, x: usize) {
    if !v.contains(&x) {
        v.push(x);
    }
}

fn seg_glyph(kind: SegKind) -> GlyphKind {
    match kind {
        SegKind::Sequential => GlyphKind::Sequential,
        SegKind::Call => GlyphKind::Call,
        SegKind::Return => GlyphKind::Return,
        SegKind::Branch => GlyphKind::Branch,
        SegKind::LoopBack => GlyphKind::Loop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(segs: &[FlowSegment], kind: SegKind) -> usize {
        segs.iter().filter(|s| s.kind == kind).count()
    }

    #[test]
    fn free_functions_call_and_return_pair() {
        // NB the call site must NOT begin with a type keyword — the port skips
        // call detection on lines starting with `int`/`void`/… (`:657`, faithful
        // to the TS heuristic), so `helper(5);` on its own line is used.
        let src = "\
int helper(int x) {
    return x + 1;
}

int main() {
    helper(5);
    return 0;
}
";
        let a = analyze(src);
        // main calls helper → one call + one return segment pair.
        assert_eq!(count(&a.segments, SegKind::Call), 1);
        assert_eq!(count(&a.segments, SegKind::Return), 1);
        let call = a
            .segments
            .iter()
            .find(|s| s.kind == SegKind::Call)
            .unwrap();
        assert_eq!(call.to_line, 1); // helper starts at line 1
        assert_eq!(call.from_line, 6); // called from line 6
        assert_eq!(call.label.as_deref(), Some("helper"));
        // Faithful glyph placement: the callee's DEFINITION line carries the `→`
        // call glyph, while the call SITE carries `↩` (its return-to overwrites
        // the sequential kind — `applyFlowDecorations:343-348`). Both navigate to
        // their counterparts.
        assert_eq!(a.glyphs.get(&1).map(|g| g.kind), Some(GlyphKind::Call));
        let site = a.glyphs.get(&6).unwrap();
        assert_eq!(site.kind, GlyphKind::Return);
        assert!(site.targets.contains(&1)); // Cmd+Click → the definition
    }

    #[test]
    fn loop_produces_loop_back() {
        let src = "\
void run() {
    for (int i = 0; i < 10; i++) {
        work();
    }
}
";
        let a = analyze(src);
        assert_eq!(count(&a.segments, SegKind::LoopBack), 1);
        let lb = a
            .segments
            .iter()
            .find(|s| s.kind == SegKind::LoopBack)
            .unwrap();
        assert_eq!(lb.to_line, 2); // back to the for-head
        // The loop head carries a loop glyph.
        assert_eq!(a.glyphs.get(&2).map(|g| g.kind), Some(GlyphKind::Loop));
    }

    #[test]
    fn if_else_makes_branches() {
        // `else` must lead its own line — `} else {` starts with `}`, which the
        // `^(?:if|else|switch)` head test (faithful to TS) does not match.
        let src = "\
void run(int x) {
    if (x > 0) {
        a();
    }
    else {
        b();
    }
}
";
        let a = analyze(src);
        assert!(count(&a.segments, SegKind::Branch) >= 2); // if + else
        assert_eq!(a.glyphs.get(&2).map(|g| g.kind), Some(GlyphKind::Branch));
    }

    #[test]
    fn same_name_method_resolved_by_object_type() {
        // Two classes each define `print`; the call resolves by the receiver's
        // declared type, not by proximity.
        // The receiver's declared type is resolved from a pointer/`<`-style decl
        // (`Cat* c = …`); a plain `Cat c;` isn't matched by the TS var-type regex
        // (`:670-674`), so a typed declaration is used to exercise the resolver.
        let src = "\
struct Dog {
    void print() {
        bark();
    }
};

struct Cat {
    void print() {
        meow();
    }
};

int main() {
    Cat* c = makeCat();
    c->print();
    return 0;
}
";
        let a = analyze(src);
        // The call at `c->print()` must resolve to Cat::print (starts at line 8),
        // not Dog::print (line 2), by the receiver's declared type.
        let call = a
            .segments
            .iter()
            .find(|s| s.kind == SegKind::Call && s.label.as_deref() == Some("Cat::print"))
            .expect("resolved Cat::print call");
        assert_eq!(call.to_line, 8);
    }

    #[test]
    fn sequential_segments_chain_statements() {
        let src = "\
void run() {
    int a = 1;
    int b = 2;
    int c = 3;
}
";
        let a = analyze(src);
        // Three statements → two sequential edges (2→3, 3→4).
        assert!(count(&a.segments, SegKind::Sequential) >= 2);
    }

    #[test]
    fn empty_source_is_empty() {
        let a = analyze("");
        assert!(a.segments.is_empty());
        assert!(a.glyphs.is_empty());
    }

    #[test]
    fn glyph_chars_match_spec() {
        assert_eq!(GlyphKind::Sequential.glyph(), '\u{25cf}');
        assert_eq!(GlyphKind::Call.glyph(), '\u{2192}');
        assert_eq!(GlyphKind::Return.glyph(), '\u{21a9}');
        assert_eq!(GlyphKind::Loop.glyph(), '\u{21ba}');
        assert_eq!(GlyphKind::Branch.glyph(), '\u{2442}');
        assert_eq!(GlyphKind::Error.glyph(), '\u{2298}');
    }
}
