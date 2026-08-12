//! Compile-on-demand of the injected dylibs (build-runner.ts:29-48, 536-559).
//!
//! The `/tmp` paths are fixed and shared across app instances, exactly as the
//! inventory (§9) mandates: "Fixed dylib paths in `/tmp` … known wart". Each is
//! recompiled only when missing or older than its source; any failure (missing
//! source, clang error, timeout) silently disables the feature.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::util::output_with_timeout;

/// Malloc interposer output path (build-runner.ts:20).
pub const INTERPOSE_DYLIB: &str = "/tmp/jade_interpose.dylib";
/// Metal telemetry probe output path (build-runner.ts:27).
pub const PROBE_DYLIB: &str = "/tmp/jade_probe.dylib";

fn needs_compile(src: &Path, dylib: &str) -> Option<bool> {
    if !src.exists() {
        return None; // no source: feature unavailable
    }
    let dylib = Path::new(dylib);
    if !dylib.exists() {
        return Some(true);
    }
    // Recompile if the source is newer than the built dylib.
    let src_m = std::fs::metadata(src).and_then(|m| m.modified()).ok();
    let dl_m = std::fs::metadata(dylib).and_then(|m| m.modified()).ok();
    match (src_m, dl_m) {
        (Some(s), Some(d)) => Some(s > d),
        _ => Some(true),
    }
}

/// Ensure `/tmp/jade_probe.dylib` is built and current. Returns `false` on any
/// failure (feature off). `clang++ -dynamiclib -fobjc-arc -O2 -framework Metal
/// -framework Foundation`, 30s timeout (build-runner.ts:29-48).
pub fn ensure_probe_dylib(probe_mm: &Path) -> bool {
    match needs_compile(probe_mm, PROBE_DYLIB) {
        None => false,
        Some(false) => true,
        Some(true) => {
            let mut cmd = Command::new("clang++");
            cmd.args([
                "-dynamiclib",
                "-fobjc-arc",
                "-O2",
                "-framework",
                "Metal",
                "-framework",
                "Foundation",
                "-o",
                PROBE_DYLIB,
            ])
            .arg(probe_mm);
            output_with_timeout(cmd, Duration::from_secs(30))
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
    }
}

/// Ensure `/tmp/jade_interpose.dylib` is built and current. Returns `false` on
/// any failure. `clang -shared -ldl`, 15s timeout (build-runner.ts:536-559).
pub fn ensure_interpose_dylib(interpose_c: &Path) -> bool {
    match needs_compile(interpose_c, INTERPOSE_DYLIB) {
        None => false,
        Some(false) => true,
        Some(true) => {
            let mut cmd = Command::new("clang");
            cmd.args(["-shared", "-o", INTERPOSE_DYLIB])
                .arg(interpose_c)
                .arg("-ldl");
            output_with_timeout(cmd, Duration::from_secs(15))
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
    }
}
