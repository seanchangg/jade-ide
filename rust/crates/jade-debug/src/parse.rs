//! Pure parsers for lldb textual output, ported line-for-line from
//! `debug-driver.ts`:
//!   - [`parse_variables`]  <- `parseVariables` (`debug-driver.ts:322-376`)
//!   - [`parse_backtrace`]  <- `parseBacktrace` (`debug-driver.ts:388-405`)
//!
//! These are deterministic string→struct transforms with no I/O, so they carry
//! the bulk of the unit-test coverage (real captured lldb output as fixtures at
//! the bottom of this file).

use std::sync::LazyLock;

use regex::Regex;

use crate::types::{LocalVariable, StackFrame};

/// Matches one variable line:
///   optional indent, optional `(type)` prefix, `name`, `=`, value.
/// Direct port of the regex at `debug-driver.ts:334`.
static VAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\s*)(?:\(([^=]*?)\)\s+)?([^=(){}]+?)\s*=\s*(.*)$").unwrap());

/// Matches a pointer-looking hex value: `0x` followed by hex digits, nothing
/// else. Port of `/^0x[0-9a-fA-F]+$/`.
static HEX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^0x[0-9a-fA-F]+$").unwrap());

/// Matches an all-zero (null) pointer value. Port of `/^0x0+$/`.
static NULL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^0x0+$").unwrap());

/// Backtrace frame line. Port of the regex at `debug-driver.ts:394`:
/// ``frame #N: 0xADDR module`func at file:line[:col]``.
static BT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"frame #(\d+):.+?`(.+?)\s+at\s+(\S+?):(\d+)").unwrap());

/// Node accumulated while walking the indented lldb tree. We build an arena of
/// these (parent/child by index) because the TS version mutates a shared
/// `children` array through a parent-pointer stack, which is awkward under
/// Rust's borrow rules — the arena reproduces the same shape without aliasing.
struct Raw {
    var: LocalVariable,
    indent: usize,
    opens_block: bool,
    children: Vec<usize>,
}

/// Parse lldb `frame variable` output into a variable tree. Faithful port of
/// `parseVariables` (`debug-driver.ts:322-376`).
///
/// Handles (see the branch-covering fixtures in the tests below):
///   - leaf `(type) name = value`
///   - aggregate blocks `(T) m = { ... }` with de-typed members inside
///   - inline aggregates `inner = (q = 7, w = 9.5)` (opaque leaf value)
///   - sized containers `(vector<int>) vec = size=3: { [0]=10 ... }`
///   - pointers `(float *) buf = 0x...` (expandable when non-null)
///   - null pointers `(int *) nullp = 0x0` (not expandable)
///   - c-strings `(const char *) s = 0x... "hi"` (not expandable — value inline)
///
/// `root_path` seeds the top-level node's `path` (used by `get_var_children`
/// so a fetched sub-expression keeps its full dotted path).
pub fn parse_variables(output: &str, root_path: Option<&str>) -> Vec<LocalVariable> {
    let mut arena: Vec<Raw> = Vec::new();
    let mut roots: Vec<usize> = Vec::new();
    // Stack of open-block ancestor indices (mirrors the TS `stack` of
    // { node, indent }); we read `arena[idx].indent` for the comparison.
    let mut stack: Vec<usize> = Vec::new();

    for raw in output.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim();
        if trimmed == "}" || trimmed == "}," {
            stack.pop();
            continue;
        }

        let Some(caps) = VAR_RE.captures(line) else {
            continue;
        };

        let indent = caps.get(1).map_or(0, |m| m.as_str().len());
        // Pop ancestors we've dedented out of.
        while let Some(&top) = stack.last() {
            if indent <= arena[top].indent {
                stack.pop();
            } else {
                break;
            }
        }
        let parent = stack.last().copied();

        let type_cap = caps.get(2).map(|m| m.as_str().trim().to_string());
        // Members inside a block have no "(type)" prefix; top-level entries do.
        // A prefix-less line at indent 0 is lldb noise, not a variable.
        if parent.is_none() && type_cap.is_none() {
            continue;
        }

        let name = caps.get(3).map_or("", |m| m.as_str()).trim().to_string();
        let mut value = caps.get(4).map_or("", |m| m.as_str()).trim().to_string();
        let opens_block = value.ends_with('{');
        if opens_block {
            value = value.trim_end_matches('{').trim_end().to_string();
        }

        let path = match parent {
            None => root_path.map(str::to_string).unwrap_or_else(|| name.clone()),
            Some(p) => {
                let pp = arena[p].var.path.as_deref().unwrap_or("");
                if name.starts_with('[') {
                    format!("{pp}{name}")
                } else if name.starts_with('*') {
                    format!("*{pp}")
                } else {
                    format!("{pp}.{name}")
                }
            }
        };

        let display_value = if opens_block {
            if value.is_empty() {
                "{…}".to_string()
            } else {
                format!("{value} {{…}}")
            }
        } else {
            value.clone()
        };

        let type_str = type_cap.unwrap_or_default();
        let mut expandable: Option<bool> = None;
        // Pointers expand lazily via get_var_children — skip c-strings (string
        // already in the value) and null pointers (nothing behind them). The
        // test runs against `display_value`, matching the TS ordering: a
        // block-opening pointer fails this regex (trailing " {…}") but is caught
        // by the opens_block branch below.
        if type_str.contains('*')
            && HEX_RE.is_match(&display_value)
            && !NULL_RE.is_match(&display_value)
        {
            expandable = Some(true);
        }
        if opens_block {
            expandable = Some(true);
        }

        let var = LocalVariable {
            name,
            type_: type_str,
            value: display_value,
            path: Some(path),
            children: None,
            expandable,
        };

        let idx = arena.len();
        arena.push(Raw {
            var,
            indent,
            opens_block,
            children: Vec::new(),
        });
        match parent {
            Some(p) => arena[p].children.push(idx),
            None => roots.push(idx),
        }
        if opens_block {
            stack.push(idx);
        }
    }

    roots.iter().map(|&i| assemble(&arena, i)).collect()
}

/// Materialize an arena node (and its subtree) into a `LocalVariable`. Only
/// block-opening nodes carry a `children` vector (possibly empty), matching the
/// TS invariant that `node.children` is set exactly when `opensBlock`.
fn assemble(arena: &[Raw], idx: usize) -> LocalVariable {
    let raw = &arena[idx];
    let mut var = raw.var.clone();
    if raw.opens_block {
        var.children = Some(raw.children.iter().map(|&c| assemble(arena, c)).collect());
    }
    var
}

/// Parse lldb `bt` output into stack frames. Faithful port of `parseBacktrace`
/// (`debug-driver.ts:388-405`).
pub fn parse_backtrace(output: &str) -> Vec<StackFrame> {
    let mut frames = Vec::new();
    for line in output.split('\n') {
        if let Some(m) = BT_RE.captures(line) {
            frames.push(StackFrame {
                index: m[1].parse().unwrap_or(0),
                function_name: m[2].to_string(),
                file: m[3].to_string(),
                line: m[4].parse().unwrap_or(0),
            });
        }
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─────────────────────────────────────────────────────────────────────
    // Fixtures: REAL `lldb` output captured on macOS/arm64 (lldb from the
    // Command Line Tools) by compiling a small C++ program with structs,
    // nested aggregates, std::vector, a live pointer, a null pointer and a
    // c-string, then running `frame variable [-T] [...]` at a breakpoint.
    // Reproduce with the program in the crate's integration test.
    // ─────────────────────────────────────────────────────────────────────

    /// `frame variable -T` — the driver's on-stop locals dump.
    const FRAME_VARIABLE_T: &str = r#"(Matrix &) m = 0x000000016fdfe7d0: {
  (int) rows = 128
  (int) cols = 64
  (Inner) inner = {
    (int) q = 7
    (double) w = 9.5
  }
  (float *) buf = 0x000000016fdfe810
  (int *) nullp = 0x0000000000000000
  (const char *) name = 0x0000000100001dc0 "hello"
}
(std::vector<int> &) vec = size=3: {
  (int) [0] = 10
  (int) [1] = 20
  (int) [2] = 30
}
(int) total = 1
JADE_LLDB> "#;

    /// `frame variable` (no `-T`) — de-typed members, inline aggregate.
    const FRAME_VARIABLE_NO_T: &str = r#"(Matrix &) m = 0x000000016fdfe7d0: {
  rows = 128
  cols = 64
  inner = (q = 7, w = 9.5)
  buf = 0x000000016fdfe810
  nullp = 0x0000000000000000
  name = 0x0000000100001dc0 "hello"
}
(std::vector<int> &) vec = size=3: {
  [0] = 10
  [1] = 20
  [2] = 30
}
(int) total = 1"#;

    /// `frame variable -T -P 1 -- m.buf` — pointer expansion (block-opening
    /// pointer with a single dereferenced child `*buf`).
    const FRAME_VARIABLE_PTR_CHILDREN: &str = r#"(float *) m.buf = 0x000000016fdfe810 {
  (float) *buf = 1.5
}"#;

    /// `frame variable -T -P 1 -- vec` — sized container expansion.
    const FRAME_VARIABLE_VEC_CHILDREN: &str = r#"(std::vector<int> &) vec = size=3: {
  (int) [0] = 10
  (int) [1] = 20
  (int) [2] = 30
}"#;

    fn find<'a>(vars: &'a [LocalVariable], name: &str) -> &'a LocalVariable {
        vars.iter()
            .find(|v| v.name == name)
            .unwrap_or_else(|| panic!("variable {name:?} not found in {vars:?}"))
    }

    #[test]
    fn parses_top_level_roots_and_skips_prompt() {
        let vars = parse_variables(FRAME_VARIABLE_T, None);
        // m, vec, total — the trailing "JADE_LLDB> " prompt is not a variable.
        assert_eq!(
            vars.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            vec!["m", "vec", "total"]
        );
    }

    #[test]
    fn leaf_scalar_has_type_value_path() {
        let vars = parse_variables(FRAME_VARIABLE_T, None);
        let total = find(&vars, "total");
        assert_eq!(total.type_, "int");
        assert_eq!(total.value, "1");
        assert_eq!(total.path.as_deref(), Some("total"));
        assert_eq!(total.expandable, None);
        assert!(total.children.is_none());
    }

    #[test]
    fn aggregate_block_becomes_expandable_with_dotted_children() {
        let vars = parse_variables(FRAME_VARIABLE_T, None);
        let m = find(&vars, "m");
        assert_eq!(m.type_, "Matrix &");
        // Reference address kept, block collapsed to the ellipsis marker.
        assert_eq!(m.value, "0x000000016fdfe7d0: {…}");
        assert_eq!(m.expandable, Some(true));
        let kids = m.children.as_ref().expect("m has children");
        assert_eq!(
            kids.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            vec!["rows", "cols", "inner", "buf", "nullp", "name"]
        );
        // Dotted paths built from parent.
        assert_eq!(find(kids, "rows").path.as_deref(), Some("m.rows"));
    }

    #[test]
    fn nested_aggregate_builds_multi_segment_paths() {
        let vars = parse_variables(FRAME_VARIABLE_T, None);
        let m = find(&vars, "m");
        let inner = find(m.children.as_ref().unwrap(), "inner");
        assert_eq!(inner.type_, "Inner");
        assert_eq!(inner.expandable, Some(true));
        assert_eq!(inner.path.as_deref(), Some("m.inner"));
        let q = find(inner.children.as_ref().unwrap(), "q");
        assert_eq!(q.value, "7");
        assert_eq!(q.path.as_deref(), Some("m.inner.q"));
        let w = find(inner.children.as_ref().unwrap(), "w");
        assert_eq!(w.value, "9.5");
        assert_eq!(w.path.as_deref(), Some("m.inner.w"));
    }

    #[test]
    fn live_pointer_is_expandable() {
        let vars = parse_variables(FRAME_VARIABLE_T, None);
        let m = find(&vars, "m");
        let buf = find(m.children.as_ref().unwrap(), "buf");
        assert_eq!(buf.type_, "float *");
        assert_eq!(buf.value, "0x000000016fdfe810");
        assert_eq!(buf.expandable, Some(true));
        // A pointer leaf gets no eager children array.
        assert!(buf.children.is_none());
    }

    #[test]
    fn null_pointer_is_not_expandable() {
        let vars = parse_variables(FRAME_VARIABLE_T, None);
        let m = find(&vars, "m");
        let nullp = find(m.children.as_ref().unwrap(), "nullp");
        assert_eq!(nullp.type_, "int *");
        assert_eq!(nullp.expandable, None);
    }

    #[test]
    fn cstring_pointer_is_not_expandable() {
        let vars = parse_variables(FRAME_VARIABLE_T, None);
        let m = find(&vars, "m");
        let name = find(m.children.as_ref().unwrap(), "name");
        assert_eq!(name.type_, "const char *");
        // The string is inline in the value; regex requires a bare hex value.
        assert_eq!(name.value, "0x0000000100001dc0 \"hello\"");
        assert_eq!(name.expandable, None);
    }

    #[test]
    fn sized_container_children_use_bracket_paths() {
        let vars = parse_variables(FRAME_VARIABLE_T, None);
        let vec = find(&vars, "vec");
        assert_eq!(vec.type_, "std::vector<int> &");
        assert_eq!(vec.value, "size=3: {…}");
        assert_eq!(vec.expandable, Some(true));
        let elems = vec.children.as_ref().unwrap();
        assert_eq!(elems.len(), 3);
        assert_eq!(elems[0].name, "[0]");
        assert_eq!(elems[0].value, "10");
        // Bracketed index appended without a dot.
        assert_eq!(elems[0].path.as_deref(), Some("vec[0]"));
        assert_eq!(elems[2].path.as_deref(), Some("vec[2]"));
    }

    #[test]
    fn no_type_members_and_inline_aggregate() {
        let vars = parse_variables(FRAME_VARIABLE_NO_T, None);
        let m = find(&vars, "m");
        let rows = find(m.children.as_ref().unwrap(), "rows");
        // De-typed member: empty type, value preserved.
        assert_eq!(rows.type_, "");
        assert_eq!(rows.value, "128");
        assert_eq!(rows.path.as_deref(), Some("m.rows"));
        // Inline aggregate stays an opaque leaf value.
        let inner = find(m.children.as_ref().unwrap(), "inner");
        assert_eq!(inner.value, "(q = 7, w = 9.5)");
        assert_eq!(inner.expandable, None);
        assert!(inner.children.is_none());
    }

    #[test]
    fn pointer_children_with_deref_star_path() {
        // Mirrors get_var_children("m.buf"): root path seeded, one child.
        let roots = parse_variables(FRAME_VARIABLE_PTR_CHILDREN, Some("m.buf"));
        assert_eq!(roots.len(), 1);
        let root = &roots[0];
        assert_eq!(root.path.as_deref(), Some("m.buf"));
        assert_eq!(root.expandable, Some(true));
        let kids = root.children.as_ref().unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].name, "*buf");
        assert_eq!(kids[0].value, "1.5");
        // Dereference paths collapse to `*<parent>`.
        assert_eq!(kids[0].path.as_deref(), Some("*m.buf"));
    }

    #[test]
    fn vec_children_root_path_seeded() {
        let roots = parse_variables(FRAME_VARIABLE_VEC_CHILDREN, Some("vec"));
        assert_eq!(roots.len(), 1);
        let kids = roots[0].children.as_ref().unwrap();
        assert_eq!(kids.len(), 3);
        assert_eq!(kids[1].path.as_deref(), Some("vec[1]"));
    }

    #[test]
    fn empty_output_yields_no_variables() {
        assert!(parse_variables("", None).is_empty());
        assert!(parse_variables("JADE_LLDB> ", None).is_empty());
    }

    // ─── backtrace ───

    /// Real `bt` output at the same breakpoint.
    const BACKTRACE: &str = r#"* thread #1, queue = 'com.apple.main-thread', stop reason = breakpoint 1.1
  * frame #0: 0x000000010000054c prog`compute(m=0x000000016fdfe7d0, vec=size=3) at prog.cpp:15:15
    frame #1: 0x000000010000066c prog`main at prog.cpp:30:11
    frame #2: 0x0000000196ffdd54 dyld`start + 7184
JADE_LLDB> "#;

    #[test]
    fn parses_backtrace_frames_with_at_location() {
        let frames = parse_backtrace(BACKTRACE);
        // frame #2 (dyld`start + 7184) has no `at file:line` → excluded.
        assert_eq!(frames.len(), 2);

        assert_eq!(frames[0].index, 0);
        assert_eq!(
            frames[0].function_name,
            "compute(m=0x000000016fdfe7d0, vec=size=3)"
        );
        assert_eq!(frames[0].file, "prog.cpp");
        assert_eq!(frames[0].line, 15);

        assert_eq!(frames[1].index, 1);
        assert_eq!(frames[1].function_name, "main");
        assert_eq!(frames[1].file, "prog.cpp");
        assert_eq!(frames[1].line, 30);
    }

    #[test]
    fn backtrace_without_frames_is_empty() {
        assert!(parse_backtrace("Process 1 exited with status = 0").is_empty());
    }
}
