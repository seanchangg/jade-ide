//! Tree-sitter C++ symbol outline (feature inventory §5.5).
//!
//! Replaces the Electron regex parser (`panels/structure-panel.ts`) with a real
//! parse: the same `tree-sitter-cpp` grammar already used by [`crate::highlight`]
//! drives a focused query ([`STRUCTURE_QUERY`]) that captures namespaces,
//! classes/structs, enums, free functions, and (inside a class/struct body)
//! methods + member variables. Nesting is recovered from the syntax tree (each
//! captured node's nearest captured ancestor is its parent), so the visual
//! "mermaid tree" treatment and the §4.7 tint hookup are preserved while the
//! brittle line-by-line regex logic is gone.
//!
//! Pure module: no GPUI. `parse_symbols` returns an owned [`Symbol`] tree with
//! 1-based `line`/`end_line` per symbol; the panel renders it and the code viewer
//! reads [`line_tint`] for the per-block background tint.

use tree_sitter::{Node, Query, QueryCursor, StreamingIterator};

use crate::theme::Theme;

/// Symbol classes shown in the outline (§5.5). Order/naming mirror the TS
/// `SymbolKind` union so the visual treatment maps 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Namespace,
    Class,
    Struct,
    Function,
    Method,
    Member,
    Enum,
}

/// C++ access level for a class/struct member (best-effort; the grammar exposes
/// `access_specifier` nodes so we track the section a member falls under).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Public,
    Private,
    Protected,
}

impl Access {
    pub fn label(self) -> &'static str {
        match self {
            Access::Public => "public",
            Access::Private => "private",
            Access::Protected => "protected",
        }
    }
}

/// One node in the outline tree. `line`/`end_line` are 1-based (matching the
/// editor gutter and the TS parser); `end_line == line` marks a single-line
/// symbol (no §4.7 tint). `access` is set only for class/struct members.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub line: usize,
    pub end_line: usize,
    pub access: Option<Access>,
    pub children: Vec<Symbol>,
}

/// Number of colors in the §4.7 tint cycle (green/blue/amber/red/blue-gray),
/// resolved at paint time from [`Theme::series`] so the light theme gets its own
/// tints (the cycle *index* is what [`line_tint`] returns; the caller maps it
/// through `theme.series`).
pub const TINT_COUNT: usize = 5;

/// Kind → dot color (§5.5), resolved from the active [`Theme`]: class/function
/// periwinkle, struct accent (emerald), method amber, member muted, enum
/// blue-gray, namespace amber. Theme-aware so the light palette applies.
pub fn kind_color(kind: SymbolKind, theme: &Theme) -> u32 {
    match kind {
        SymbolKind::Class | SymbolKind::Function => theme.periwinkle,
        SymbolKind::Struct => theme.accent,
        SymbolKind::Method => theme.amber,
        SymbolKind::Member => theme.muted,
        SymbolKind::Enum => theme.blue_gray,
        SymbolKind::Namespace => theme.amber,
    }
}

/// The §4.7 background-tint **cycle index** for a 1-based source `line`, or `None`
/// if the line is not inside a top-level multi-line symbol. The index cycles over
/// the top-level multi-line symbols in document order (so adjacent blocks differ);
/// the caller maps it through `theme.series[idx]` at paint time.
pub fn line_tint(symbols: &[Symbol], line: usize) -> Option<usize> {
    let mut idx = 0usize;
    for s in symbols {
        if s.end_line > s.line {
            if line >= s.line && line <= s.end_line {
                return Some(idx % TINT_COUNT);
            }
            idx += 1;
        }
    }
    None
}

/// Total symbols in a tree (headless-smoke metric).
pub fn count(symbols: &[Symbol]) -> usize {
    symbols.iter().map(|s| 1 + count(&s.children)).sum()
}

/// The outline query. Each pattern captures a container node (`@def.<kind>`) and
/// its `@name`; nesting + method/function disambiguation happen in Rust from the
/// tree, so the patterns stay flat and robust. Function-like captures are all
/// `@def.func` and become `Method` only when their nearest captured ancestor is a
/// class/struct (else `Function`).
pub const STRUCTURE_QUERY: &str = r#"
; ── Namespaces ──
(namespace_definition name: (namespace_identifier) @name) @def.namespace

; ── Classes / structs / enums ──
(class_specifier name: (type_identifier) @name) @def.class
(struct_specifier name: (type_identifier) @name) @def.struct
(enum_specifier name: (type_identifier) @name) @def.enum

; ── Free functions & inline methods (definitions with a body) ──
(function_definition
  declarator: (function_declarator declarator: (identifier) @name)) @def.func
(function_definition
  declarator: (function_declarator declarator: (field_identifier) @name)) @def.func
(function_definition
  declarator: (function_declarator
    declarator: (qualified_identifier name: (identifier) @name))) @def.func
(function_definition
  declarator: (pointer_declarator
    declarator: (function_declarator declarator: (identifier) @name))) @def.func

; ── Method declarations (no body) inside a class/struct ──
(field_declaration
  declarator: (function_declarator declarator: (field_identifier) @name)) @def.func
(field_declaration
  declarator: (pointer_declarator
    declarator: (function_declarator declarator: (field_identifier) @name))) @def.func

; ── Constructors / destructors (declared, no body) ──
(declaration
  declarator: (function_declarator declarator: (identifier) @name)) @def.func
(declaration
  declarator: (function_declarator declarator: (destructor_name) @name)) @def.func

; ── Member variables ──
(field_declaration declarator: (field_identifier) @name) @def.member
(field_declaration
  declarator: (pointer_declarator declarator: (field_identifier) @name)) @def.member
"#;

/// Kind of a container node keyed by its `@def.<suffix>` capture name. `func` is
/// deferred (resolved to Method/Function by context).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RawKind {
    Namespace,
    Class,
    Struct,
    Enum,
    Func,
    Member,
}

/// Flat parse entry, before nesting is applied.
struct Raw {
    name: String,
    raw_kind: RawKind,
    line: usize,
    end_line: usize,
    parent: Option<usize>,
    children: Vec<usize>,
}

/// Parse C++/Metal source into a nested outline. Returns an empty vec if the
/// grammar/query fails to load or the parse produces no symbols.
pub fn parse_symbols(source: &str) -> Vec<Symbol> {
    let language = tree_sitter_cpp::LANGUAGE;
    let Ok(query) = Query::new(&language.into(), STRUCTURE_QUERY) else {
        return Vec::new();
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language.into()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let src = source.as_bytes();

    // Which capture index carries `@name`; the container capture is any `@def.*`.
    let names = query.capture_names();

    // 1. Collect raw entries, deduped by container node id.
    let mut raws: Vec<Raw> = Vec::new();
    let mut id_to_idx: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&query, tree.root_node(), src);
    while let Some(m) = it.next() {
        let mut container: Option<Node> = None;
        let mut raw_kind: Option<RawKind> = None;
        let mut name_node: Option<Node> = None;
        for cap in m.captures {
            let cap_name = names[cap.index as usize];
            if cap_name == "name" {
                name_node = Some(cap.node);
            } else if let Some(suffix) = cap_name.strip_prefix("def.") {
                container = Some(cap.node);
                raw_kind = Some(match suffix {
                    "namespace" => RawKind::Namespace,
                    "class" => RawKind::Class,
                    "struct" => RawKind::Struct,
                    "enum" => RawKind::Enum,
                    "member" => RawKind::Member,
                    _ => RawKind::Func,
                });
            }
        }
        let (Some(node), Some(kind), Some(name_node)) = (container, raw_kind, name_node) else {
            continue;
        };
        if id_to_idx.contains_key(&node.id()) {
            continue;
        }
        let name = name_node.utf8_text(src).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        id_to_idx.insert(node.id(), raws.len());
        raws.push(Raw {
            name,
            raw_kind: kind,
            line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            parent: None,
            children: Vec::new(),
        });
    }

    // 2. Resolve each entry's parent = nearest captured ancestor in the tree.
    //    Walk the syntax tree once, keeping a stack of captured ancestors.
    resolve_parents(tree.root_node(), &id_to_idx, &mut raws, &mut Vec::new());

    // 3. Wire children (build() sorts them into document order by line).
    let parents: Vec<Option<usize>> = raws.iter().map(|r| r.parent).collect();
    for (i, p) in parents.iter().enumerate() {
        if let Some(pi) = p {
            raws[*pi].children.push(i);
        }
    }

    // 4. Build the owned Symbol tree from roots, resolving Func→Method/Function
    //    and member/method access from context.
    let roots: Vec<usize> = raws
        .iter()
        .enumerate()
        .filter(|(_, r)| r.parent.is_none())
        .map(|(i, _)| i)
        .collect();
    let mut out: Vec<Symbol> = roots
        .iter()
        .map(|&i| build(i, &raws, source))
        .collect();
    out.sort_by_key(|s| s.line);
    out
}

/// Depth-first walk that assigns each captured node its nearest captured
/// ancestor. `stack` holds the raw indices of captured ancestors on the path.
fn resolve_parents(
    node: Node,
    id_to_idx: &std::collections::HashMap<usize, usize>,
    raws: &mut [Raw],
    stack: &mut Vec<usize>,
) {
    let mut pushed = false;
    if let Some(&idx) = id_to_idx.get(&node.id()) {
        if let Some(&parent_idx) = stack.last() {
            raws[idx].parent = Some(parent_idx);
        }
        stack.push(idx);
        pushed = true;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        resolve_parents(child, id_to_idx, raws, stack);
    }
    if pushed {
        stack.pop();
    }
}

/// Convert a raw entry (and its subtree) into an owned [`Symbol`], resolving the
/// final kind and access.
fn build(idx: usize, raws: &[Raw], source: &str) -> Symbol {
    let r = &raws[idx];
    let parent_kind = r.parent.map(|p| raws[p].raw_kind);
    let in_aggregate = matches!(parent_kind, Some(RawKind::Class) | Some(RawKind::Struct));

    let kind = match r.raw_kind {
        RawKind::Namespace => SymbolKind::Namespace,
        RawKind::Class => SymbolKind::Class,
        RawKind::Struct => SymbolKind::Struct,
        RawKind::Enum => SymbolKind::Enum,
        RawKind::Member => SymbolKind::Member,
        RawKind::Func => {
            if in_aggregate {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            }
        }
    };

    // Access only meaningful for members/methods of a class/struct.
    let access = if in_aggregate && matches!(kind, SymbolKind::Method | SymbolKind::Member) {
        let parent_line = r.parent.map(|p| raws[p].line).unwrap_or(0);
        Some(access_at(r.line, parent_line, source, parent_kind))
    } else {
        None
    };

    let mut children: Vec<Symbol> = raws[idx]
        .children
        .iter()
        .map(|&c| build(c, raws, source))
        .collect();
    children.sort_by_key(|s| s.line);

    Symbol {
        name: r.name.clone(),
        kind,
        line: r.line,
        end_line: r.end_line,
        access,
        children,
    }
}

/// Best-effort access level for a member on 1-based `member_line`, given the
/// enclosing aggregate's opening line `parent_line` and kind. Scans
/// `access_specifier` lines **inside that aggregate** (between `parent_line` and
/// the member); defaults to `private` for a class and `public` for a struct.
fn access_at(
    member_line: usize,
    parent_line: usize,
    source: &str,
    parent_kind: Option<RawKind>,
) -> Access {
    let mut access = if matches!(parent_kind, Some(RawKind::Struct)) {
        Access::Public
    } else {
        Access::Private
    };
    for (i, raw) in source.lines().enumerate() {
        let line = i + 1;
        if line <= parent_line {
            continue;
        }
        if line >= member_line {
            break;
        }
        let t = raw.trim_start();
        if t.starts_with("public:") {
            access = Access::Public;
        } else if t.starts_with("private:") {
            access = Access::Private;
        } else if t.starts_with("protected:") {
            access = Access::Protected;
        }
    }
    access
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
namespace app {

class Widget {
public:
    Widget();
    ~Widget();
    int area() const;
    void resize(int w, int h);
private:
    int width_;
    float* data_;
};

struct Point {
    int x;
    int y;
};

enum class Color { Red, Green, Blue };

} // namespace app

int add(int a, int b) {
    return a + b;
}
"#;

    fn find<'a>(syms: &'a [Symbol], name: &str) -> Option<&'a Symbol> {
        for s in syms {
            if s.name == name {
                return Some(s);
            }
            if let Some(f) = find(&s.children, name) {
                return Some(f);
            }
        }
        None
    }

    #[test]
    fn query_compiles() {
        let language = tree_sitter_cpp::LANGUAGE;
        assert!(Query::new(&language.into(), STRUCTURE_QUERY).is_ok());
    }

    #[test]
    fn extracts_namespace_class_struct_enum_and_free_function() {
        let syms = parse_symbols(FIXTURE);

        // Top-level: namespace app + free function add.
        let ns = find(&syms, "app").expect("namespace");
        assert_eq!(ns.kind, SymbolKind::Namespace);
        let add = find(&syms, "add").expect("free function");
        assert_eq!(add.kind, SymbolKind::Function);

        // Class/struct/enum are nested under the namespace.
        let widget = find(&ns.children, "Widget").expect("class");
        assert_eq!(widget.kind, SymbolKind::Class);
        let point = find(&ns.children, "Point").expect("struct");
        assert_eq!(point.kind, SymbolKind::Struct);
        let color = find(&ns.children, "Color").expect("enum");
        assert_eq!(color.kind, SymbolKind::Enum);
    }

    #[test]
    fn class_methods_and_members_nest_with_kinds() {
        let syms = parse_symbols(FIXTURE);
        let widget = find(&syms, "Widget").expect("class");

        let area = find(&widget.children, "area").expect("method");
        assert_eq!(area.kind, SymbolKind::Method);
        let resize = find(&widget.children, "resize").expect("method");
        assert_eq!(resize.kind, SymbolKind::Method);
        let ctor = find(&widget.children, "Widget").expect("constructor");
        assert_eq!(ctor.kind, SymbolKind::Method);

        let width = find(&widget.children, "width_").expect("member");
        assert_eq!(width.kind, SymbolKind::Member);
        let data = find(&widget.children, "data_").expect("pointer member");
        assert_eq!(data.kind, SymbolKind::Member);
    }

    #[test]
    fn access_specifiers_detected() {
        let syms = parse_symbols(FIXTURE);
        let widget = find(&syms, "Widget").expect("class");
        let area = find(&widget.children, "area").expect("method");
        assert_eq!(area.access, Some(Access::Public));
        let width = find(&widget.children, "width_").expect("member");
        assert_eq!(width.access, Some(Access::Private));
        // Struct members default to public.
        let point = find(&syms, "Point").expect("struct");
        let x = find(&point.children, "x").expect("member x");
        assert_eq!(x.access, Some(Access::Public));
    }

    #[test]
    fn line_numbers_are_one_based_and_span_blocks() {
        let syms = parse_symbols(FIXTURE);
        // `int add(int a, int b) {` is on line 24 in the fixture (leading newline
        // makes line 1 empty).
        let add = find(&syms, "add").expect("add");
        let text_line = FIXTURE.lines().nth(add.line - 1).unwrap();
        assert!(text_line.contains("int add"), "line {} = {:?}", add.line, text_line);
        assert!(add.end_line > add.line, "multi-line function spans lines");

        // Widget class spans from its `class Widget {` line to its `};`.
        let widget = find(&syms, "Widget").unwrap();
        assert!(widget.end_line > widget.line);
    }

    #[test]
    fn tints_cycle_over_top_level_multiline_symbols() {
        // Two adjacent top-level multi-line functions get different tints.
        let src = "int a() {\n  return 1;\n}\nint b() {\n  return 2;\n}\n";
        let syms = parse_symbols(src);
        assert_eq!(syms.len(), 2);
        let t_a = line_tint(&syms, 1).expect("a tint");
        let t_b = line_tint(&syms, 4).expect("b tint");
        assert_eq!(t_a, 0);
        assert_eq!(t_b, 1);
        assert_ne!(t_a, t_b);
        // A line outside any symbol has no tint.
        assert_eq!(line_tint(&syms, 3), Some(0)); // still inside a()'s block
        assert_eq!(line_tint(&syms, 99), None);
    }

    #[test]
    fn empty_and_plain_sources_are_safe() {
        assert!(parse_symbols("").is_empty());
        assert!(parse_symbols("// just a comment\n").is_empty());
        // app, Widget, ctor, dtor, area, resize, width_, data_, Point, x, y,
        // Color, add.
        assert_eq!(count(&parse_symbols(FIXTURE)), 13);
    }
}
