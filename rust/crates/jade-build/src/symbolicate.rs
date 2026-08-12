//! `atos` buffer-alias symbolication — the toolchain half of
//! `telemetry-server.ts` `resolveBufferAlias` + `extractVarName` (`:239-319`).
//!
//! [`AtosSymbolicator`] implements [`jade_telemetry::Symbolicator`]; the
//! telemetry server installs it via `set_symbolicator` and drives it when a
//! probe declares a buffer carrying allocation-site addresses. This half runs
//! `atos`, maps frames back to `file:line`, and recovers the variable name the
//! allocation was assigned to; the server owns the collision-resolution and
//! rename that turn those names into a unique alias.
//!
//! Seam note: `jade-build`'s `run()` constructs an `AtosSymbolicator` carrying
//! the executable it is about to launch, so the exe path flows from the build
//! side to the telemetry side even when the probe omits `meta.exe`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use jade_telemetry::Symbolicator;
use regex::Regex;

use crate::util::output_with_timeout;

// `atos` frame tail: `... (file.cpp:123)` (telemetry-server.ts:262).
static ATOS_FRAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(([^():]+):(\d+)\)\s*$").unwrap());

// Full `atos` frame: `Symbol (in image) (file.cpp:123)` — the symbol names the
// function containing the frame, which for constructor calls identifies the
// callee's class and lets us resolve WHICH member of the caller was being
// constructed (see `member_of`).
static ATOS_SYM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*?)\s+\(in [^)]+\)\s+\(([^():]+):(\d+)\)\s*$").unwrap());

// Assignment LHS: `[type] lhs = ...` (telemetry-server.ts:310-312).
static ASSIGN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:[\w:<>,~ \t*&\[\]]+?\s+)?((?:this->)?[A-Za-z_]\w*(?:(?:\.|->)[A-Za-z_]\w*)*)\s*=[^=]").unwrap()
});
// Declaration with constructor: `Type name(...)` / `Type name{...}` (`:315`).
static CTOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[\w:<>,~ \t*&]+\s+([A-Za-z_]\w*)\s*[({]").unwrap());

const KEYWORDS: &[&str] = &["if", "for", "while", "switch", "return", "else", "do", "case"];

/// Resolves probe buffer names to source variable names via `atos`.
pub struct AtosSymbolicator {
    /// Executable to symbolicate against when the probe omits `meta.exe`.
    fallback_exe: PathBuf,
    /// `path → lines` cache mirroring the TS `sourceCache` (`None` = unreadable).
    source_cache: Mutex<HashMap<PathBuf, Option<Vec<String>>>>,
}

impl AtosSymbolicator {
    pub fn new(fallback_exe: PathBuf) -> Self {
        AtosSymbolicator {
            fallback_exe,
            source_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Cached source lines of `base_name` under the first root that has it.
    fn source_lines(&self, roots: &[PathBuf], base_name: &str) -> Option<Vec<String>> {
        let mut cache = self.source_cache.lock().unwrap();
        for root in roots {
            let p = root.join(base_name);
            let content = cache.entry(p.clone()).or_insert_with(|| {
                std::fs::read_to_string(&p)
                    .ok()
                    .map(|s| s.split('\n').map(str::to_string).collect())
            });
            if let Some(lines) = content {
                return Some(lines.clone());
            }
        }
        None
    }

    /// Resolve which member of `caller_class` the frame was constructing, by
    /// the callee's type. Line-based extraction alone cannot split an
    /// initializer list that packs several member inits onto one line
    /// (`MetalBlock() : ln1(...), attn(...),`) — it recovers one arbitrary
    /// identifier and files every buffer under it. The callee class from the
    /// inner frame's symbol names the member's TYPE: a unique member of that
    /// type wins outright; duplicated types (ln1/ln2) fall back to whichever
    /// candidate appears on the call-site-adjusted source line.
    fn member_of(
        &self,
        roots: &[PathBuf],
        base_name: &str,
        caller_class: &str,
        callee_class: &str,
        line_no: usize,
    ) -> Option<String> {
        let lines = self.source_lines(roots, base_name)?;
        let def_re = Regex::new(&format!(
            r"^\s*(?:struct|class)\s+{}\b",
            regex::escape(caller_class)
        ))
        .ok()?;
        let start = lines.iter().position(|l| def_re.is_match(l))?;
        let mem_re = Regex::new(&format!(
            r"^\s*{}\s+([A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*)\s*;",
            regex::escape(callee_class)
        ))
        .ok()?;
        let mut cands: Vec<String> = Vec::new();
        for l in lines.iter().skip(start + 1).take(500) {
            if l.trim_start().starts_with("};") {
                break;
            }
            if let Some(c) = mem_re.captures(l) {
                cands.extend(c[1].split(',').map(|s| s.trim().to_string()));
            }
        }
        if cands.len() == 1 {
            return cands.pop();
        }
        if cands.len() > 1 {
            let line = lines.get(line_no.wrapping_sub(1))?;
            let mut on_line = cands.into_iter().filter(|c| {
                Regex::new(&format!(r"\b{}\s*\(", regex::escape(c)))
                    .map(|r| r.is_match(line))
                    .unwrap_or(false)
            });
            let first = on_line.next();
            if first.is_some() && on_line.next().is_none() {
                return first;
            }
        }
        None
    }

    /// Read `root/base_name`:`line_no` and extract the variable the allocation
    /// was assigned to (telemetry-server.ts `extractVarName`).
    fn extract_var_name(
        &self,
        roots: &[PathBuf],
        base_name: &str,
        line_no: usize,
    ) -> Option<String> {
        for root in roots {
            let p = root.join(base_name);
            let line = {
                let mut cache = self.source_cache.lock().unwrap();
                let content = cache.entry(p.clone()).or_insert_with(|| {
                    std::fs::read_to_string(&p)
                        .ok()
                        .map(|s| s.split('\n').map(str::to_string).collect())
                });
                match content {
                    Some(lines) => lines.get(line_no.wrapping_sub(1)).cloned(),
                    None => None,
                }
            };
            let Some(line) = line else { continue };
            if line.is_empty() {
                continue;
            }

            // Assignment: `[type] lhs = ...` → dotted identifier chain.
            if let Some(c) = ASSIGN_RE.captures(&line) {
                let name = &c[1];
                if !KEYWORDS.contains(&name) {
                    let stripped = name.strip_prefix("this->").unwrap_or(name);
                    return Some(stripped.replace("->", "."));
                }
            }
            // Declaration with constructor: `Type name(...)` / `Type name{...}`.
            if let Some(c) = CTOR_RE.captures(&line) {
                let name = &c[1];
                if !KEYWORDS.contains(&name) {
                    return Some(name.to_string());
                }
            }
        }
        None
    }
}

/// `Cls::Cls(args)` → `Cls`: a demangled symbol is a constructor when its last
/// two `::` segments match. Frames inside a constructor mean the caller was
/// constructing a member — the class name keys the `member_of` lookup.
fn ctor_class(sym: &str) -> Option<&str> {
    let head = sym.split('(').next().unwrap_or("").trim();
    let segs: Vec<&str> = head.split("::").collect();
    match segs.as_slice() {
        [.., cls, last] if cls == last => Some(cls),
        _ => None,
    }
}

/// `atos` against the bare executable reads line info through the stab debug
/// map (macOS keeps DWARF in the .o files), and CoreSymbolication mis-reads
/// DWARF-5 line tables through that map for optimized builds — file:line
/// comes back shifted into the wrong FILE (observed: `-g -O3` metal-cpp
/// project, every allocation frame reported in main.cpp at pre-refactor line
/// numbers, so no variable name ever extracted and every buffer kept its
/// `Ctor::Ctor #N` fallback). A linked dSYM bypasses the debug map entirely
/// and symbolicates correctly, so build one (dsymutil, sub-second on debug
/// binaries) whenever the exe is newer than its dSYM, and hand THAT to atos.
fn ensure_dsym(exe: &PathBuf) -> Option<PathBuf> {
    let name = exe.file_name()?;
    let mut dsym = exe.clone().into_os_string();
    dsym.push(".dSYM");
    let dwarf = PathBuf::from(dsym).join("Contents/Resources/DWARF").join(name);
    let stale = match (dwarf.metadata().and_then(|m| m.modified()), exe.metadata().and_then(|m| m.modified())) {
        (Ok(d), Ok(e)) => d < e,
        _ => true,
    };
    if stale {
        let mut cmd = Command::new("dsymutil");
        cmd.arg(exe);
        let ok = output_with_timeout(cmd, Duration::from_secs(30))
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }
    }
    dwarf.exists().then_some(dwarf)
}

/// Probe frames are `backtrace()` return addresses, which point at the
/// instruction AFTER the call. Symbolicate one byte back so the lookup lands
/// inside the call instruction itself: when a call is the last thing on its
/// source line (constructor initializer lists spanning lines — `attn(...),\n
/// ln2(...)` — hit this every time), the return address otherwise resolves to
/// the NEXT line and the buffer gets filed under the wrong member.
fn call_site(addr: &str) -> String {
    addr.strip_prefix("0x")
        .and_then(|h| u64::from_str_radix(h, 16).ok())
        .filter(|a| *a > 0)
        .map(|a| format!("0x{:x}", a - 1))
        .unwrap_or_else(|| addr.to_string())
}

impl AtosSymbolicator {
    /// One frame set (innermost first) → dotted-name parts, applying the
    /// constructor-member disambiguation before the line regexes.
    fn parts_from_frames(&self, roots: &[PathBuf], frames: &[(String, String, usize)]) -> Vec<String> {
        let mut parts = Vec::new();
        for (i, (sym, base, line_no)) in frames.iter().enumerate() {
            // Constructor-in-constructor: name the member by the callee's type
            // first; the line regexes can't split multi-init lines.
            let mut name = None;
            if i > 0 {
                if let (Some(callee), Some(caller)) = (ctor_class(&frames[i - 1].0), ctor_class(sym))
                {
                    name = self.member_of(roots, base, caller, callee, *line_no);
                }
            }
            let name = name.or_else(|| self.extract_var_name(roots, base, *line_no));
            if let Some(name) = name {
                parts.push(name);
            }
        }
        parts
    }
}

impl Symbolicator for AtosSymbolicator {
    fn variable_names(&self, addrs: &[String], exe: Option<&str>, load: &str) -> Vec<String> {
        self.variable_names_batch(std::slice::from_ref(&addrs.to_vec()), exe, load)
            .pop()
            .unwrap_or_default()
    }

    /// Whole batch through ONE `atos` process — the per-decl process storm at
    /// app startup (hundreds of buffers declared at once) is what used to blow
    /// timeouts and strand fallback names. Output lines map back to input
    /// order, so the flat result is re-split by each set's address count.
    fn variable_names_batch(
        &self,
        addr_sets: &[Vec<String>],
        exe: Option<&str>,
        load: &str,
    ) -> Vec<Vec<String>> {
        let flat: Vec<&String> = addr_sets.iter().flatten().collect();
        if flat.is_empty() {
            return addr_sets.iter().map(|_| Vec::new()).collect();
        }
        let exe: PathBuf = exe.map(PathBuf::from).unwrap_or_else(|| self.fallback_exe.clone());

        // `atos -o <dSYM|exe> -l <load> <addrs...>`, 8s timeout
        // (telemetry-server.ts:246-253). Prefer the dSYM (see `ensure_dsym`).
        let sym_target = ensure_dsym(&exe).unwrap_or_else(|| exe.clone());
        let mut cmd = Command::new("atos");
        cmd.arg("-o").arg(&sym_target).arg("-l").arg(load);
        cmd.args(flat.iter().map(|a| call_site(a)));
        let stdout = match output_with_timeout(cmd, Duration::from_secs(8)) {
            Some(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
            _ => return addr_sets.iter().map(|_| Vec::new()).collect(),
        };
        let lines: Vec<&str> = stdout.trim().split('\n').collect();
        if lines.len() != flat.len() {
            return addr_sets.iter().map(|_| Vec::new()).collect();
        }

        // Search roots: the exe's directory and its parent (CMake build dirs
        // live inside the project root) (telemetry-server.ts:257).
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(d) = exe.parent() {
            roots.push(d.to_path_buf());
            if let Some(dd) = d.parent() {
                roots.push(dd.to_path_buf());
            }
        }

        let mut results = Vec::with_capacity(addr_sets.len());
        let mut cursor = 0usize;
        for set in addr_sets {
            let chunk = &lines[cursor..cursor + set.len()];
            cursor += set.len();
            // Innermost-first frames: (symbol, file base name, line).
            let frames: Vec<(String, String, usize)> = chunk
                .iter()
                .filter_map(|line| {
                    if let Some(c) = ATOS_SYM_RE.captures(line) {
                        Some((c[1].to_string(), c[2].to_string(), c[3].parse().unwrap_or(0)))
                    } else {
                        // No symbol (stripped frame) but source info still usable.
                        ATOS_FRAME_RE.captures(line).map(|c| {
                            (String::new(), c[1].to_string(), c[2].parse().unwrap_or(0))
                        })
                    }
                })
                .collect();
            results.push(self.parts_from_frames(&roots, &frames));
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn sym(root: &Path) -> AtosSymbolicator {
        AtosSymbolicator::new(root.join("app"))
    }

    #[test]
    fn extract_assignment_variable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("model.cpp"),
            "int a;\nfloat* weights = allocate(1024);\nreturn weights;\n",
        )
        .unwrap();
        let s = sym(dir.path());
        let roots = vec![dir.path().to_path_buf()];
        assert_eq!(
            s.extract_var_name(&roots, "model.cpp", 2).as_deref(),
            Some("weights")
        );
    }

    #[test]
    fn extract_constructor_variable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("m.cpp"),
            "// header\nMatrix attn(rows, cols);\n",
        )
        .unwrap();
        let s = sym(dir.path());
        let roots = vec![dir.path().to_path_buf()];
        assert_eq!(
            s.extract_var_name(&roots, "m.cpp", 2).as_deref(),
            Some("attn")
        );
    }

    #[test]
    fn member_assignment_flattens_arrow_and_this() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.cpp"), "x;\nthis->cache->buf = make();\n").unwrap();
        let s = sym(dir.path());
        let roots = vec![dir.path().to_path_buf()];
        assert_eq!(
            s.extract_var_name(&roots, "s.cpp", 2).as_deref(),
            Some("cache.buf")
        );
    }

    #[test]
    fn control_flow_and_keyword_lines_yield_no_name() {
        let dir = tempfile::tempdir().unwrap();
        // `for (...)`: the `(` blocks the type prefix, so no variable is
        // recovered — control-flow lines don't name allocations.
        std::fs::write(dir.path().join("k.cpp"), "x;\nfor (int i = 0; i < n; i++) {\n").unwrap();
        // `do = ...`: assignment form captures the keyword `do`, which the
        // keyword filter rejects (telemetry-server.ts:308,313).
        std::fs::write(dir.path().join("d.cpp"), "x;\ndo = spin();\n").unwrap();
        let s = sym(dir.path());
        let roots = vec![dir.path().to_path_buf()];
        assert_eq!(s.extract_var_name(&roots, "k.cpp", 2), None);
        assert_eq!(s.extract_var_name(&roots, "d.cpp", 2), None);
    }

    #[test]
    fn ctor_class_detects_constructors() {
        assert_eq!(ctor_class("MetalAttention::MetalAttention(int, int, int)"), Some("MetalAttention"));
        assert_eq!(ctor_class("Outer::Inner::Inner()"), Some("Inner"));
        assert_eq!(ctor_class("main"), None);
        assert_eq!(ctor_class("AdamOptimizer::build(int)"), None);
    }

    #[test]
    fn initializer_list_members_resolve_by_callee_type() {
        // The metalLLM shape: four member inits on two lines; ln1/attn share
        // line 6, ln2/mlp share line 7. Line-based extraction alone filed
        // attn's buffers under whatever identifier the line regex grabbed.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("model.cpp"),
            "struct Block {\n    Norm ln1;\n    Attn attn;\n    Norm ln2;\n    Mlp mlp;\n    Block() : ln1(1), attn(2),\n        ln2(3), mlp(4) {}\n};\n",
        )
        .unwrap();
        let s = sym(dir.path());
        let roots = vec![dir.path().to_path_buf()];
        // Unique member type: resolved regardless of which line the frame hit.
        assert_eq!(s.member_of(&roots, "model.cpp", "Block", "Attn", 6).as_deref(), Some("attn"));
        assert_eq!(s.member_of(&roots, "model.cpp", "Block", "Mlp", 7).as_deref(), Some("mlp"));
        // Duplicated member type: the call-site line picks the sibling.
        assert_eq!(s.member_of(&roots, "model.cpp", "Block", "Norm", 6).as_deref(), Some("ln1"));
        assert_eq!(s.member_of(&roots, "model.cpp", "Block", "Norm", 7).as_deref(), Some("ln2"));
        // Unknown type or class: no fabricated name.
        assert_eq!(s.member_of(&roots, "model.cpp", "Block", "Loss", 6), None);
        assert_eq!(s.member_of(&roots, "model.cpp", "Nope", "Attn", 6), None);
    }

    #[test]
    fn call_site_backs_up_one_byte() {
        // Return addr → call-site addr; malformed input passes through.
        assert_eq!(call_site("0x1020f04fc"), "0x1020f04fb");
        assert_eq!(call_site("0x1"), "0x0");
        assert_eq!(call_site("0x0"), "0x0");
        assert_eq!(call_site("garbage"), "garbage");
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let s = sym(dir.path());
        let roots = vec![PathBuf::from("/nonexistent")];
        assert_eq!(s.extract_var_name(&roots, "nope.cpp", 1), None);
    }
}
