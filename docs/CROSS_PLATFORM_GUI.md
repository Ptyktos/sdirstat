# Cross-platform GUI — plan & preparation

Today sdirstat ships a **web GUI** (`sdirstat serve`) that is already cross-platform *at the view
layer* — it runs in any browser on any OS. What is **not** yet portable is the **backend**: the
scanner and the file actions use Linux-specific facilities. This document is the plan to reach a
native, cross-platform desktop app, and the preparation that makes it cheap.

## Goal

A native desktop app on **Linux, macOS, and Windows** that wraps the same scan + the same views
(treemap / sunburst / type-stats / actions), installable as a normal app, no browser or terminal.

## Recommended approach: Tauri (reuse `app.html`)

[Tauri](https://tauri.app) wraps the OS's native webview around a web frontend and a Rust backend.
This is the lowest-effort path because **`src/app.html` already is the frontend** and the Rust core
already does the work:

- **Frontend**: `app.html`, unchanged, loaded into the webview.
- **Backend**: the scan/fold core, called either as Tauri `invoke` commands (no HTTP) or by keeping
  the existing `serve` loopback server and pointing the webview at it.
- **Result**: one native app per OS, reusing ~all existing code.

Alternatives, if a non-webview/pure-Rust UI is preferred later: **egui** (immediate-mode, draws the
treemap directly — heaviest rewrite, lightest binary), **Slint**, or **Dioxus**. Tauri is the
recommended first target precisely because it reuses `app.html`.

## Preparation (do these first, in order)

### 1. Lift the core into the library

Move the scan + fold + emit out of `src/main.rs` into `src/lib.rs` so **all** frontends — the CLI,
`serve`, and a Tauri/egui app — share one implementation:

```
sdirstat::scan(path, cfg) -> Tree          // the parallel Web walk + B/U fold
sdirstat::Tree::to_json() / to_cache() / to_html()
sdirstat::action::{open, reveal, trash}(path)
```

`main.rs` becomes a thin CLI; `serve` and the native app call the library. This is the single
biggest enabling step and has no platform concerns of its own.

### 2. Abstract the OS-specific bits behind `cfg`

The core scanner today assumes Linux. Each item below needs a portable seam; the **zero-dependency**
rule for the core still holds — platform code uses `std` + each OS's own APIs, and any cross-platform
crate (e.g. `trash`) is isolated to the **GUI binary**, never the core scanner.

| concern | Linux (today) | macOS | Windows |
|---|---|---|---|
| enumerate | `getdents`/`std::fs::read_dir` | `read_dir` | `read_dir` |
| metadata | `statx` / `MetadataExt::blocks()` | `MetadataExt::blocks()` | no `st_blocks` — use `len()` or the allocation-size API; treat "allocated" as cluster-rounded |
| allocated size | `st_blocks×512` | `st_blocks×512` | `GetCompressedFileSizeW` / round `len` up to cluster size |
| trash | `gio trash` | `NSFileManager trashItem` | `SHFileOperation`/`IFileOperation` (Recycle Bin) |
| io_uring (`--iouring`) | raw syscalls (x86_64) | n/a — std walk | n/a — std walk |

The std parallel walk already works on all three OSes — only the **size metric** and the **trash**
action are genuinely platform-divergent, and both are small, well-isolated functions. Put them
behind `#[cfg(target_os = ...)]` with a common trait/signature.

### 3. Keep the API contract stable

The view layer already talks to the backend through a stable contract — the JSON tree
(`{n,v,d,c}` + `types`) and the `/scan` · `/act` endpoints. A Tauri app reuses **exactly** this,
either over the loopback server or via `invoke` returning the same JSON. No frontend rewrite.

## Staged roadmap

1. **Core-in-library** refactor (step 1) — no behavior change, all tests/benches still pass.
2. **Platform seam** for size + trash (step 2), gated by `cfg`; CI builds on macOS + Windows with
   the std backend (no io_uring) and verifies sizes against the OS's own `du`/`Get-ChildItem`.
3. **Tauri shell** wrapping `app.html` + the library; one installable artifact per OS.
4. (Optional) native trash + allocated-size parity per OS; packaging/signing.

## What is already portable

- The entire **GUI** (`app.html`) — vanilla JS, no CDN.
- The **std parallel scanner** and the size fold.
- **JSON** / **HTML** outputs.
- The cache format (text); `--cache` semantics are Unix-flavored (uid/gid/perm) but emit fine
  anywhere.

The honest summary: the **web GUI is cross-platform now**; making a **native** cross-platform app is
mostly (1) the library refactor and (2) two small platform seams — after which Tauri-over-`app.html`
is a thin shell.
