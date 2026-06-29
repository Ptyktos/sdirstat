# sdirstat — the directory tree as a web

![sdirstat social preview](assets/social-card.png)

[![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/Ptyktos/sdirstat/badge)](https://scorecard.dev/viewer/?uri=github.com/Ptyktos/sdirstat)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Headless filesystem indexer. **Parallel scan → one reverse-pass size fold → any output.** Emits QDirStat cache files, nested JSON, or an interactive report. The zero-dep Rust replacement for Perl `qdirstat-cache-writer`. Also ships a squarified treemap + sunburst web GUI.

## Quick start

```sh
sdirstat /usr -o usr.html             # self-contained interactive HTML report
sdirstat install-desktop               # add a clickable "sdirstat" app to your menu (Linux, no root)
sdirstat gui                           # open the GUI as a standalone desktop app window
sdirstat serve                         # live web GUI at http://127.0.0.1:8080
sdirstat /var --cache -o var.cache     # QDirStat cache file (drop-in for Perl writer)
sdirstat /home --total                 # grand total, fast
sdirstat /srv --json | jq '…'         # JSON tree for your own tooling
```

From a single downloaded binary to a real desktop app: `sdirstat install-desktop` writes an
XDG `.desktop` entry + icon under `~/.local/share` (no root, no package), and clicking it runs
`sdirstat gui` — the GUI in a standalone app window (chromium `--app`, falling back to your
browser). For a fully native window instead, install the [desktop app](desktop/) (Tauri).

## Features

- **Zero runtime dependencies** — the backend is `std`-only (minus optional `io_uring`); the GUI is vanilla JS, no CDN, works offline.
- **Byte-exact with `du`** — allocated sizes via `st_blocks × 512`, hardlink dedup, or `--apparent` for logical `st_size`.
- **Three output formats** — QDirStat cache (drop-in for `qdirstat-cache-writer`), nested JSON, self-contained HTML with treemap.
- **Interactive web GUI** (`serve`) — squarified treemap, sunburst, file-type stats, sortable tree-table, breadcrumb navigation, right-click file actions (Open / Reveal / Copy path / Move to Trash).
- **Incremental trash** — after moving files to trash, only the changed subtree is re-scanned (~10 ms, not a full re-scan).
- **O(1) navigation cache** (`serve`) — revisiting a path certifies from cache by reading one coordinate (the directory's own mtime); it re-walks only when that coordinate changed, or on demand (↻ Rescan).
- **Parallel walk** — one pass to build the full tree + any output format.
- **OOM-guarded** — `--max-entries` ceiling (default 32M) prevents pathological scans.
- **io_uring backend** (`--iouring`) — batched `statx` for cold / SSD-bound scans.
- **~14× faster than the Perl tool it replaces** — `/usr` (1.25M entries) in ~0.5 s vs qdirstat-cache-writer ~7 s.

## Why / How

Filesystem analysis tools are either interactive GUI apps (Baobab, QDirStat, Filelight) or headless scripts that dump flat numbers. sdirstat is both: the **same scan** produces a GUI view, a cache file for QDirStat, a JSON tree, and a CLI total — no re-scan, no adapter. The directory tree is a **Web**: each node carries its own `st_blocks`, adjacency is dir → children, and a single reverse-pass fold (`subtree = own + Σ children`) computes every output from the same structure. QDirStat cache output means it slots into existing workflows without replacing the visualiser.

## Related projects

| project | what |
|---|---|
| [qwalk](https://gitlab.tas.twn.network/twn/RnD/qwalk) | Indexed filesystem search and code grep |
| [cerialize](https://gitlab.tas.twn.network/twn/RnD/collapse_wire) | Zero-copy, zero-dep serialization — columnar wire format |
| [pdffold](https://gitlab.tas.twn.network/twn/RnD/pdffold) | PDF → Markdown, zero dependencies |
| [webfold](https://gitlab.tas.twn.network/twn/RnD/webfold) | HTML / WARC / PDF → Markdown |
| [chunkfold](https://gitlab.tas.twn.network/twn/RnD/chunkfold) | Chunking as coordinate read |

## Releases & verification

Binaries and installers are built by CI on a `v*` tag and published to GitHub Releases. Every artifact
is checksummed (`SHA256SUMS`), ships an SPDX SBOM, is signed with [cosign](https://docs.sigstore.dev)
(keyless), and carries [SLSA build provenance](https://slsa.dev). See [RELEASE.md](RELEASE.md) and
[docs/SIGNING.md](docs/SIGNING.md) to verify a download.

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
Contributions are accepted under the same dual license.
