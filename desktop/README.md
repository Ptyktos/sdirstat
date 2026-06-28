# sdirstat-desktop — native desktop app

A [Tauri](https://tauri.app) shell around the existing sdirstat GUI. It launches the bundled
`sdirstat` binary as a **sidecar** (`sdirstat serve` on a free `127.0.0.1` port) and opens a native
window pointed at it — so the same `app.html` (treemap / sunburst / type-stats / Open·Reveal·Trash)
runs unchanged, with **no frontend rewrite**.

This crate is deliberately **not** a member of the root workspace (`exclude = ["desktop"]` in the
top-level `Cargo.toml`): it carries the Tauri dependency tree, while the core `sdirstat` crate stays
zero-dependency (std-only). The core is reused **as-is**, via the sidecar — the desktop app adds no
requirements to it.

## Prerequisites

- Rust (stable) and the Tauri CLI: `cargo install tauri-cli --version "^2" --locked`
- **Linux** system libs: `libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev`
- **Windows**: WebView2 (preinstalled on Windows 10/11). **macOS**: Xcode command-line tools.

## Build & run

The sidecar is the core binary, staged under `binaries/` with the target-triple suffix Tauri
expects. Build the core first, stage it, then build/run the app:

```sh
# from the repo root — build the zero-dep core
cargo build --release

# stage it as the sidecar for this host (adjust the triple per platform)
mkdir -p desktop/binaries
cp target/release/sdirstat desktop/binaries/sdirstat-$(rustc -vV | sed -n 's/host: //p')

# run the app (dev), or build native installers
cd desktop
cargo tauri dev
cargo tauri build --bundles deb      # Linux .deb   (AppImage: add `appimage`, best-effort)
cargo tauri build                    # Windows .msi + NSIS .exe
cargo tauri build --target universal-apple-darwin --bundles dmg   # macOS universal .dmg
```

CI builds all of these on tag push — see [`../.github/workflows/release.yml`](../.github/workflows/release.yml).

> **Tested status:** only the Linux path (`.deb`) has been built and run. The Windows
> (`.msi`/NSIS) and macOS (universal `.dmg`) bundles are **untested** — no Win/Mac host was
> available — and may need iteration on their first tagged CI run.

## Packaging notes

- The Linux `.deb` package is `sdirstat-desktop` and declares `Provides`/`Conflicts`/`Replaces:
  sdirstat`, so it cleanly supersedes the CLI-only `sdirstat` package (it ships the CLI too, at
  `/usr/bin/sdirstat`, alongside `/usr/bin/sdirstat-desktop`).
- The sidecar is terminated when the app exits (window close / quit) via `RunEvent::Exit`. A
  `SIGKILL` of the app is the one path Tauri can't intercept and could orphan the child.
