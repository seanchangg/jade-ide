# Forge IDE — Rust workspace

Rust rewrite of the Electron/TypeScript Jade IDE. The `jade` crate is the GPUI
app shell; the `forge-*` crates are the build/debug/telemetry/terminal/AI engines
it wires together.

```
cargo run -p jade                    # bare window
cargo run -p jade -- --project dir/  # open the first C++ source in a dir
cargo run -p jade -- --smoke ghost   # headless smoke check (no window)
```

## Packaging (macOS)

Phase-5b ships two idempotent scripts under `rust/scripts/` that produce a
signed `.app` and a distributable `.dmg` for Apple Silicon. Run from anywhere;
they resolve paths relative to themselves.

```bash
rust/scripts/bundle-mac.sh   # -> rust/target/bundle/Jade.app
rust/scripts/dmg-mac.sh      # -> rust/target/bundle/Jade-0.1.0-arm64.dmg
```

### `bundle-mac.sh`

1. `cargo build --release -p jade`.
2. Assembles `Jade.app` from scratch (idempotent — any prior bundle is removed):
   - `Contents/MacOS/jade` — the release binary.
   - `Contents/Info.plist` — see below.
   - `Contents/Resources/icon.icns` — reuses `build-resources/icon.icns` (the
     same icon as the Electron build).
   - `Contents/Resources/fonts/` — bundled fonts (see below).
3. Ad-hoc codesigns (`codesign --sign -`), matching the Electron build's
   `mac.identity: null`.

`Info.plist` keys:

| Key | Value |
| --- | --- |
| `CFBundleIdentifier` | `com.jade.ide` (matches the Electron `appId`) |
| `CFBundleName` / `CFBundleDisplayName` | `Jade` |
| `CFBundleExecutable` | `jade` |
| `CFBundleIconFile` | `icon.icns` |
| `CFBundleShortVersionString` / `CFBundleVersion` | `0.1.0` |
| `LSMinimumSystemVersion` | `13.0` |
| `NSHighResolutionCapable` | `true` |
| `CFBundlePackageType` | `APPL` |
| `LSApplicationCategoryType` | `public.app-category.developer-tools` |

### `dmg-mac.sh`

Requires `Jade.app` to exist (run `bundle-mac.sh` first). Stages `Jade.app` plus
an `/Applications` symlink into a temp dir and runs `hdiutil create` (UDZO,
compressed read-only). Output `rust/target/bundle/Jade-0.1.0-arm64.dmg`, volume
name `Jade 0.1.0`.

### Bundled fonts (JetBrains Mono drop-in)

No font download is assumed and none was found installed on the build machine, so
**the bundle ships no fonts today and the app resolves Menlo** (the guaranteed
macOS monospace fallback). The loading machinery is fully in place:

- Drop `JetBrainsMono-*.ttf` (or any `.ttf`/`.otf`) into
  `rust/scripts/resources/fonts/` and re-run `bundle-mac.sh`. Each file is copied
  into `Jade.app/Contents/Resources/fonts/`.
- At startup `crates/jade/src/fonts.rs` (`register_bundled_fonts`) reads that dir
  — resolved from the running executable
  (`.../Jade.app/Contents/MacOS/jade` → `../Resources/fonts`), so it works from
  the relocated bundle — and hands the TTFs to `cx.text_system().add_fonts(..)`.
- If a bundled file's name contains `jetbrains`, `mono_family()` flips from
  `Menlo` to `JetBrains Mono`. Every code/terminal `.font_family(..)` call site
  goes through `mono_family()`, so the whole app switches together.

A dev `cargo run` (no bundle dir) is a clean no-op and keeps Menlo.

### Not done / limitations

- **No notarization.** The app is not submitted to Apple's notary service. On
  another Mac, Gatekeeper quarantine (`com.apple.quarantine`, set on downloaded
  DMGs) will block first launch; the user must right-click → Open, or run
  `xattr -dr com.apple.quarantine /Applications/Jade.app`.
- **Ad-hoc signature only.** No Developer ID identity (matches the Electron
  build). `spctl -a -t exec` / `spctl --assess` reports *rejected* — this is
  expected for an ad-hoc signature and does not affect local runs.
- **Apple Silicon (`arm64`) only.** No universal/x86_64 slice.
- No custom DMG background/window layout (plain UDZO with the `/Applications`
  symlink).
