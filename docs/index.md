<!--
  Markdown version of the homepage, for Markdown content negotiation and AI/accessibility readers.
  Linked from the HTML page via <link rel="alternate" type="text/markdown">. Canonical HTML:
  https://ptyktos.github.io/sdirstat/
-->
# sdirstat

A fast, parallel disk-usage analyzer with an interactive treemap & sunburst GUI — and a
zero-dependency QDirStat cache writer. Point it at a folder (or your whole disk); it scans in
parallel and shows where the space went.

## Install

- **Cargo:** `cargo install sdirstat`
- **Arch (AUR):** `yay -S sdirstat` (source) or `yay -S sdirstat-bin` (prebuilt)
- **Prebuilt:** <https://github.com/Ptyktos/sdirstat/releases/latest> (Linux glibc/musl, Windows `.exe`/`.msi`, macOS universal)
- **From source:** `git clone https://github.com/Ptyktos/sdirstat && cd sdirstat && cargo build --release`

## Usage

```sh
sdirstat ~/Downloads        # scan → a shareable report.html (treemap)
sdirstat / --total          # grand total, fast
sdirstat serve              # live web GUI at http://127.0.0.1:8080
sdirstat gui                # GUI as a standalone desktop window
sdirstat /var --cache -o var.cache   # QDirStat-openable cache file
sdirstat /srv --json | jq   # JSON tree for tooling
```

## What it is

- **Four outputs from one scan:** interactive HTML, QDirStat cache, JSON tree, or a total.
- **Byte-exact with `du`**, hardlink-dedup, optional apparent size.
- **Zero runtime dependencies** (scanner is std-only; GUI is vanilla JS, works offline).
- **Safe by design:** the GUI binds `127.0.0.1` only; "Move to Trash" is reversible.

## Links

- Repository: <https://github.com/Ptyktos/sdirstat>
- Library docs: <https://docs.rs/sdirstat>
- Machine summary: [llms.txt](https://ptyktos.github.io/sdirstat/llms.txt)
- License: MIT OR Apache-2.0
