# Icons

This directory holds the macOS app icon set referenced by `tauri.conf.json`.
Bundle icons are generated with `tauri icon path/to/source.png` and committed
in the PR that introduces the final art. The placeholder `icon.png` here is a
1×1 transparent PNG used only to satisfy the Tauri proc-macro at build time;
it is not bundled because `tauri.conf.json` sets `bundle.active = false` for
this scaffold-only PR.
