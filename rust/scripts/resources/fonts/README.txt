Drop JetBrainsMono-*.ttf (or any .ttf/.otf) here and re-run scripts/bundle-mac.sh.
bundle-mac.sh copies every font in this dir into Jade.app/Contents/Resources/fonts,
and crates/jade/src/fonts.rs registers them at startup. When a file whose name
contains "jetbrains" is present, the app's mono family switches from Menlo to
"JetBrains Mono" automatically. Empty today: no font download is assumed, and none
was found installed on the build machine, so the app resolves Menlo.
