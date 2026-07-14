//! Static size annotations (feature inventory §4.5, port of
//! `memory-decorations.ts` `collectSizeAnnotations`/`parseUserTypes`/
//! `estimateStructSize`/`annotateVariableLine`, `:244-478`).
//!
//! Pure, buffer-only static analysis: parse the source to annotate
//!   - `struct`/`class` declarations with their estimated size — `[N B]`;
//!   - qualifying local variables (non-primitives, or primitives >8 B, so the
//!     clutter rule at TS `:461-466` holds) — `[N B]`;
//!   - `new T` / `new T[n]` heap expressions — `[heap N]` / `[heap N×c=total]`.
//!
//! The size model is the LP64 / Apple-Silicon [`PRIMITIVE_SIZES`] table, the
//! [`STL_SIZES`] container approximations, recursive computation for the
//! parameterized `array`/`pair`/`optional`/`atomic` templates, and a
//! `min(size,8)`-alignment struct estimator.
//!
//! ── Rendering-string note (deviation, documented) ──
//! The TS `formatBytes` renders the byte case with no space (`24B`). Every
//! authoritative *spec* example writes the bracketed static annotation with a
//! space (`[24 B]`, and the merged example `  [24 B]  ×1.5K↑  ← 3 calls,
//! 1.2KB`). We therefore render the **bracketed** byte case as `N B` (space)
//! via [`fmt_size`], while KB/MB keep `formatBytes`' spacing and the runtime
//! allocation suffix (`exec_annotations`/rendering) keeps the plain
//! [`crate::format::format_bytes`] (`1.2KB`, no space) — matching every example.

use std::collections::HashMap;

use regex::Regex;

/// Known C++ primitive sizes (LP64 / Apple Silicon), `memory-decorations.ts:40-56`.
pub fn primitive_size(t: &str) -> Option<i64> {
    Some(match t {
        "bool" => 1,
        "char" | "signed char" | "unsigned char" => 1,
        "int8_t" | "uint8_t" => 1,
        "short" | "unsigned short" => 2,
        "int16_t" | "uint16_t" => 2,
        "int" | "unsigned int" | "unsigned" => 4,
        "int32_t" | "uint32_t" => 4,
        "float" => 4,
        "long" | "unsigned long" => 8,
        "long long" | "unsigned long long" => 8,
        "int64_t" | "uint64_t" => 8,
        "double" => 8,
        "long double" => 16,
        "size_t" | "ssize_t" | "ptrdiff_t" => 8,
        "void*" | "nullptr_t" => 8,
        _ => return None,
    })
}

/// Common STL container sizes (`memory-decorations.ts:59-84`). `-1` marks a
/// parameterized type whose size is computed from its template arguments.
pub fn stl_size(base: &str) -> Option<i64> {
    Some(match base {
        "std::string" => 24,
        "std::string_view" => 16,
        "std::vector" => 24,
        "std::array" => -1, // element size * count
        "std::map" => 48,
        "std::unordered_map" => 56,
        "std::set" => 48,
        "std::unordered_set" => 56,
        "std::list" => 24,
        "std::deque" => 80,
        "std::queue" => 80,
        "std::stack" => 80,
        "std::pair" => -1, // sum of both types
        "std::tuple" => -1,
        "std::optional" => -1, // sizeof(T) + 1 (padded)
        "std::unique_ptr" => 8,
        "std::shared_ptr" => 16,
        "std::weak_ptr" => 16,
        "std::function" => 32,
        "std::mutex" => 64,
        "std::atomic" => -1,
        "std::thread" => 8,
        "std::chrono::steady_clock::time_point" => 8,
        "std::chrono::system_clock::time_point" => 8,
        _ => return None,
    })
}

/// `offset` rounded up to a multiple of `alignment` (`:439-442`).
pub fn align_to(offset: i64, alignment: i64) -> i64 {
    if alignment <= 0 {
        return offset;
    }
    ((offset + alignment - 1) / alignment) * alignment
}

/// Estimate a struct/class size from its member sizes with the simple alignment
/// model (`:420-437`): each member aligned to `min(own_size, 8)`, struct padded
/// to its largest member alignment.
pub fn estimate_struct_size(member_sizes: &[i64]) -> i64 {
    if member_sizes.is_empty() {
        return 0;
    }
    let mut offset = 0i64;
    let mut max_align = 1i64;
    for &size in member_sizes {
        let align = size.min(8);
        max_align = max_align.max(align);
        offset = align_to(offset, align);
        offset += size;
    }
    align_to(offset, max_align)
}

/// JS `typeStr.replace(/<.*>/, '')`: greedily drop from the first `<` to the
/// last `>`, then trim. Used for STL container-base lookup.
fn strip_template(type_str: &str) -> String {
    match (type_str.find('<'), type_str.rfind('>')) {
        (Some(a), Some(b)) if b > a => {
            let mut s = String::with_capacity(type_str.len());
            s.push_str(&type_str[..a]);
            s.push_str(&type_str[b + 1..]);
            s.trim().to_string()
        }
        _ => type_str.trim().to_string(),
    }
}

/// Resolve a type string to a byte size, or `0` if unknown (`:359-418`).
/// Handles primitives, pointers/references, STL containers (incl. the
/// recursive `array`/`optional`/`pair`/`atomic` templates) and user types.
pub fn resolve_type_size(type_str: &str, user_types: &HashMap<String, i64>) -> i64 {
    if let Some(s) = primitive_size(type_str) {
        return s;
    }
    if type_str.ends_with('*') || type_str.ends_with('&') {
        return 8;
    }

    let stl_base = strip_template(type_str);
    if let Some(size) = stl_size(&stl_base) {
        if size > 0 {
            return size;
        }
        // Parameterized templates.
        if stl_base == "std::array" {
            if let Some(c) = re_array().captures(type_str) {
                let elem = resolve_type_size(c[1].trim(), user_types);
                if let Ok(count) = c[2].parse::<i64>() {
                    if elem > 0 {
                        return elem * count;
                    }
                }
            }
        }
        if stl_base == "std::optional" {
            if let Some(c) = re_optional().captures(type_str) {
                let inner = resolve_type_size(c[1].trim(), user_types);
                if inner > 0 {
                    return align_to(inner + 1, 8);
                }
            }
        }
        if stl_base == "std::pair" {
            if let Some(c) = re_pair().captures(type_str) {
                let a = resolve_type_size(c[1].trim(), user_types);
                let b = resolve_type_size(c[2].trim(), user_types);
                if a > 0 && b > 0 {
                    return align_to(a, b.min(8)) + b;
                }
            }
        }
        if stl_base == "std::atomic" {
            if let Some(c) = re_atomic().captures(type_str) {
                return resolve_type_size(c[1].trim(), user_types);
            }
        }
        return 0;
    }

    // Re-try with a std:: prefix (bare `vector`, `string`, …).
    if !type_str.starts_with("std::") {
        if let Some(s) = stl_size(&format!("std::{stl_base}")) {
            return s;
        }
    }

    if let Some(s) = user_types.get(type_str) {
        return *s;
    }
    let bare = strip_template(type_str);
    if let Some(s) = user_types.get(&bare) {
        return *s;
    }
    0
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

lazy_re!(re_array, r"std::array<(.+),\s*(\d+)>");
lazy_re!(re_optional, r"std::optional<(.+)>");
lazy_re!(re_pair, r"std::pair<(.+),\s*(.+)>");
lazy_re!(re_atomic, r"std::atomic<(.+)>");
lazy_re!(re_struct_decl, r"^(struct|class)\s+(\w+)");
lazy_re!(re_access, r"^(public|private|protected)\s*:");
// Member: prefer the template-terminated form (`vector<double> m;`), else the
// space-separated form. Ported from `:351-352` / `:447-448`.
lazy_re!(re_member_tmpl, r"^\s*([\w:<>,\s*&]+?>)\s*(\w+)\s*(?:=.*)?;");
lazy_re!(re_member_plain, r"^\s*([\w:<>,\s*&]+?)\s+(\w+)\s*(?:=.*)?;");
lazy_re!(re_var_tmpl, r"^\s*(const\s+)?([\w:<>,\s*&]+?>)\s*(\w+)\s*[=({;]");
lazy_re!(re_var_plain, r"^\s*(const\s+)?([\w:<>,\s*&]+?)\s+(\w+)\s*[=({;]");
lazy_re!(
    re_new,
    r"new\s+(\w+)(?:<[^>]*>)?\s*(?:\[([^\]]+)\])?\s*(?:\(|\{|;)"
);

// ── User-type (struct/class) parsing at brace depth 1 (`:287-341`) ─────────

/// Parse all `struct`/`class` bodies, returning a size map keyed by both the
/// kind-prefixed name (`struct_Foo`) and the plain name (`Foo`, first def wins).
pub fn parse_user_types(lines: &[&str]) -> HashMap<String, i64> {
    let mut user_types: HashMap<String, i64> = HashMap::new();

    let mut current_type: Option<String> = None;
    let mut current_kind: Option<String> = None;
    let mut brace_depth = 0i64;
    let mut member_sizes: Vec<i64> = Vec::new();
    let mut in_type = false;

    for line in lines {
        let trimmed = line.trim();

        if let Some(c) = re_struct_decl().captures(trimmed) {
            if trimmed.contains('{') {
                current_kind = Some(c[1].to_string());
                current_type = Some(c[2].to_string());
                brace_depth = 0;
                member_sizes.clear();
                in_type = true;
            }
        }

        if in_type {
            for ch in trimmed.chars() {
                if ch == '{' {
                    brace_depth += 1;
                }
                if ch == '}' {
                    brace_depth -= 1;
                }
            }

            // Direct members only (depth 1), skipping comment lines.
            if brace_depth == 1 && !trimmed.starts_with("//") && !trimmed.starts_with("/*") {
                let member_size = parse_member_size(trimmed, &user_types);
                if member_size > 0 {
                    member_sizes.push(member_size);
                }
            }

            if brace_depth <= 0 {
                if let (Some(ct), Some(ck)) = (&current_type, &current_kind) {
                    let total = estimate_struct_size(&member_sizes);
                    if total > 0 {
                        let prefixed = format!("{ck}_{ct}");
                        user_types.entry(prefixed).or_insert(total);
                        user_types.entry(ct.clone()).or_insert(total);
                    }
                }
                current_type = None;
                current_kind = None;
                in_type = false;
            }
        }
    }

    user_types
}

/// Size of a single member declaration line, or 0 if it isn't one (`:343-357`).
fn parse_member_size(line: &str, user_types: &HashMap<String, i64>) -> i64 {
    if re_access().is_match(line) {
        return 0;
    }
    if line.contains('(') && !line.contains('=') {
        return 0; // method declaration
    }
    if line == "{" || line == "}" || line.is_empty() {
        return 0;
    }
    let caps = re_member_tmpl()
        .captures(line)
        .or_else(|| re_member_plain().captures(line));
    let Some(c) = caps else {
        return 0;
    };
    resolve_type_size(c[1].trim(), user_types)
}

/// Annotate a local variable declaration line, or `None` (`:444-469`).
/// Returns the *inner* size text (no brackets) — e.g. `24 B`.
pub fn annotate_variable_line(line: &str, user_types: &HashMap<String, i64>) -> Option<String> {
    let caps = re_var_tmpl()
        .captures(line)
        .or_else(|| re_var_plain().captures(line))?;
    let type_str = caps[2].trim();

    // Skip control-flow / non-declaration leading words.
    if matches!(
        type_str,
        "return" | "if" | "for" | "while" | "switch" | "case" | "void" | "auto"
    ) {
        return None;
    }

    let size = resolve_type_size(type_str, user_types);
    if size <= 0 {
        return None;
    }
    // Skip small primitives to avoid clutter (`:465-466`).
    let is_primitive = primitive_size(type_str).is_some();
    if is_primitive && size <= 8 {
        return None;
    }
    Some(fmt_size(size))
}

/// Render a byte size for the **bracketed** static annotations: byte case gets a
/// space (`N B`) per the spec examples; KB/MB match `formatBytes`. See the
/// module note.
pub fn fmt_size(bytes: i64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Scan a whole source buffer for static size annotations, returning a map of
/// 1-based line number → bracketed annotation text (`:244-285`).
pub fn collect_size_annotations(source: &str) -> HashMap<usize, String> {
    let lines: Vec<&str> = source.split('\n').collect();
    let user_types = parse_user_types(&lines);
    let mut result: HashMap<usize, String> = HashMap::new();

    for (i, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim();
        let line_num = i + 1;
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }

        // struct/class declaration.
        if let Some(c) = re_struct_decl().captures(trimmed) {
            if trimmed.contains('{') {
                let kind = &c[1];
                let name = &c[2];
                let prefixed = format!("{kind}_{name}");
                let size = user_types
                    .get(&prefixed)
                    .or_else(|| user_types.get(name))
                    .copied();
                if let Some(sz) = size {
                    if sz > 0 {
                        result.insert(line_num, format!("[{}]", fmt_size(sz)));
                    }
                }
                continue;
            }
        }

        // qualifying local variable.
        if let Some(inner) = annotate_variable_line(trimmed, &user_types) {
            result.insert(line_num, format!("[{inner}]"));
            continue;
        }

        // new / new[] expression.
        if let Some(c) = re_new().captures(trimmed) {
            let type_name = &c[1];
            let type_size = resolve_type_size(type_name, &user_types);
            if type_size > 0 {
                if let Some(arr) = c.get(2) {
                    let arr = arr.as_str();
                    match arr.trim().parse::<i64>() {
                        Ok(count) => {
                            result.insert(
                                line_num,
                                format!(
                                    "[heap {}×{}={}]",
                                    fmt_size(type_size),
                                    count,
                                    fmt_size(type_size * count)
                                ),
                            );
                        }
                        Err(_) => {
                            result.insert(
                                line_num,
                                format!("[heap {}×{}]", fmt_size(type_size), arr),
                            );
                        }
                    }
                } else {
                    result.insert(line_num, format!("[heap {}]", fmt_size(type_size)));
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ut(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn primitive_table_spot_checks() {
        assert_eq!(primitive_size("bool"), Some(1));
        assert_eq!(primitive_size("int"), Some(4));
        assert_eq!(primitive_size("double"), Some(8));
        assert_eq!(primitive_size("long double"), Some(16));
        assert_eq!(primitive_size("size_t"), Some(8));
        assert_eq!(primitive_size("void*"), Some(8));
        assert_eq!(primitive_size("MyType"), None);
    }

    #[test]
    fn stl_table_spot_checks() {
        assert_eq!(stl_size("std::string"), Some(24));
        assert_eq!(stl_size("std::vector"), Some(24));
        assert_eq!(stl_size("std::map"), Some(48));
        assert_eq!(stl_size("std::unordered_map"), Some(56));
        assert_eq!(stl_size("std::deque"), Some(80));
        assert_eq!(stl_size("std::mutex"), Some(64));
        assert_eq!(stl_size("std::shared_ptr"), Some(16));
        assert_eq!(stl_size("std::array"), Some(-1)); // parameterized sentinel
    }

    #[test]
    fn alignment_and_padding() {
        // char(1) then double(8): pad to 8, +8 → 16.
        assert_eq!(estimate_struct_size(&[1, 8]), 16);
        // int(4), int(4) → 8, no pad.
        assert_eq!(estimate_struct_size(&[4, 4]), 8);
        // char(1), int(4): pad char→4, +4 → 8.
        assert_eq!(estimate_struct_size(&[1, 4]), 8);
        // empty → 0.
        assert_eq!(estimate_struct_size(&[]), 0);
    }

    #[test]
    fn pointers_and_refs_are_8() {
        let u = HashMap::new();
        assert_eq!(resolve_type_size("Foo*", &u), 8);
        assert_eq!(resolve_type_size("Foo&", &u), 8);
    }

    #[test]
    fn recursive_array_pair_optional_atomic() {
        let u = HashMap::new();
        // array<int, 16> = 4*16 = 64.
        assert_eq!(resolve_type_size("std::array<int, 16>", &u), 64);
        // optional<int> = align(4+1, 8) = 8.
        assert_eq!(resolve_type_size("std::optional<int>", &u), 8);
        // pair<int, double> = align(4, min(8,8)) + 8 = 8 + 8 = 16.
        assert_eq!(resolve_type_size("std::pair<int, double>", &u), 16);
        // pair<char, double> = align(1, 8) + 8 = 0 + ... align(1,8)=8? no: align(1,8)=8 → +8 = 16.
        assert_eq!(resolve_type_size("std::pair<char, double>", &u), 16);
        // atomic<int> = 4.
        assert_eq!(resolve_type_size("std::atomic<int>", &u), 4);
    }

    #[test]
    fn bare_stl_gets_std_prefix() {
        let u = HashMap::new();
        assert_eq!(resolve_type_size("vector<double>", &u), 24);
        assert_eq!(resolve_type_size("string", &u), 24);
    }

    #[test]
    fn user_type_lookup() {
        let u = ut(&[("Vec3", 12), ("struct_Vec3", 12)]);
        assert_eq!(resolve_type_size("Vec3", &u), 12);
        assert_eq!(resolve_type_size("Unknown", &u), 0);
    }

    #[test]
    fn parse_user_types_simple_struct() {
        let src = "struct Vec3 {\n  float x;\n  float y;\n  float z;\n};\n";
        let lines: Vec<&str> = src.split('\n').collect();
        let u = parse_user_types(&lines);
        // 3 floats = 12 bytes (align 4).
        assert_eq!(u.get("Vec3"), Some(&12));
        assert_eq!(u.get("struct_Vec3"), Some(&12));
    }

    #[test]
    fn method_body_members_skipped_at_depth() {
        // Declarations inside a method body are at brace-depth 2 and must not be
        // counted toward the struct; only the depth-1 data members are.
        let src = "\
struct Widget {
  int id;
  void run() {
    double scratch;
    double more;
  }
  int flags;
};
";
        let lines: Vec<&str> = src.split('\n').collect();
        let u = parse_user_types(&lines);
        // Only id(4) + flags(4) at depth 1 → 8. The method decl is skipped (has
        // `(` without `=`), and `scratch`/`more` at depth 2 are never members.
        assert_eq!(u.get("Widget"), Some(&8));
    }

    #[test]
    fn nested_struct_decl_terminates_outer_parse() {
        // Faithful TS quirk (`:297-304`): a nested `struct`/`class` declaration
        // RESETS the parser to the inner type, so the inner size is recorded but
        // the outer is never finalized. Documented, not fixed.
        let src = "\
struct Outer {
  int a;
  struct Inner {
    double d;
    double e;
  };
  int b;
};
";
        let lines: Vec<&str> = src.split('\n').collect();
        let u = parse_user_types(&lines);
        assert_eq!(u.get("Inner"), Some(&16)); // two doubles
        assert_eq!(u.get("Outer"), None); // outer parse broken by the nested decl
    }

    #[test]
    fn struct_with_containers() {
        let src = "\
struct Model {
  std::vector<float> weights;
  std::string name;
  int epoch;
};
";
        let lines: Vec<&str> = src.split('\n').collect();
        let u = parse_user_types(&lines);
        // vector(24) + string(24) + int(4): offsets 0..24, 24..48, 48..52 → pad to
        // max_align 8 → 56.
        assert_eq!(u.get("Model"), Some(&56));
    }

    #[test]
    fn struct_declaration_annotation() {
        let src = "struct Vec3 {\n  float x;\n  float y;\n  float z;\n};\n";
        let ann = collect_size_annotations(src);
        assert_eq!(ann.get(&1).map(String::as_str), Some("[12 B]"));
    }

    #[test]
    fn local_variable_skips_small_primitive() {
        // `int x = 5;` is a primitive ≤ 8 → no annotation.
        let src = "int main() {\n  int x = 5;\n  std::vector<double> v;\n}\n";
        let ann = collect_size_annotations(src);
        assert_eq!(ann.get(&2), None); // int skipped
        assert_eq!(ann.get(&3).map(String::as_str), Some("[24 B]")); // vector shown
    }

    #[test]
    fn local_variable_shows_double_array_type() {
        // long double is a primitive but > 8 bytes → shown.
        let src = "int main() {\n  long double big = 0;\n}\n";
        let ann = collect_size_annotations(src);
        assert_eq!(ann.get(&2).map(String::as_str), Some("[16 B]"));
    }

    #[test]
    fn typed_new_decl_shows_pointer_not_heap() {
        // Faithful TS ordering: a typed `new` declaration matches the variable
        // rule FIRST, so it annotates the pointer size (8 B), not the heap block.
        let src = "int main() {\n  auto* p = new double;\n}\n";
        let ann = collect_size_annotations(src);
        assert_eq!(ann.get(&2).map(String::as_str), Some("[8 B]"));
    }

    #[test]
    fn heap_new_scalar_and_array() {
        // The heap annotation fires only when the line ISN'T a typed declaration
        // — e.g. a bare `return new …;` (the `return` type word is skipped, so it
        // falls through to the `new` rule).
        let src = "\
void a() {
  return new double;
}
void b() {
  return new int[10];
}
void c() {
  return new float[n];
}
";
        let ann = collect_size_annotations(src);
        assert_eq!(ann.get(&2).map(String::as_str), Some("[heap 8 B]"));
        assert_eq!(ann.get(&5).map(String::as_str), Some("[heap 4 B×10=40 B]"));
        // Non-numeric count keeps the symbolic form.
        assert_eq!(ann.get(&8).map(String::as_str), Some("[heap 4 B×n]"));
    }

    #[test]
    fn fmt_size_units() {
        assert_eq!(fmt_size(24), "24 B");
        assert_eq!(fmt_size(2048), "2.0KB");
        assert_eq!(fmt_size(3 * 1024 * 1024), "3.0MB");
    }
}
