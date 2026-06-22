# sdirstat

A fast, zero-dependency disk-usage analyzer with an explorable web GUI — the headless
indexer and the visualization in one Rust binary. It subsumes **Baobab** and **QDirStat**:
the same scan, two of their signature views, and a drop-in for QDirStat's cache format.

- **Parallel scan**, allocated sizes **byte-exact with `du`** (`st_blocks×512` + hardlink dedup).
- **Web GUI** (`sdirstat serve`): a squarified **treemap** (QDirStat/WinDirStat), a **sunburst**
  (Baobab/Filelight), **file-type statistics** (QDirStat), a sortable tree-table, scan-any-folder,
  breadcrumb, and right-click **file actions** (Open · Reveal · Copy path · Move to Trash).
- **Outputs**: a self-contained interactive **HTML** report, a nested **JSON** tree, or a
  **QDirStat v2.0 cache file** — a drop-in replacement for the Perl `qdirstat-cache-writer`.
- **Zero runtime dependencies.** The backend is `std`-only; the GUI is vanilla JS (no CDN, offline).
- Optional **io_uring** batched-`statx` backend (`--iouring`) for cold/SSD-bound scans.

> Platform: Linux is the primary target (`statx`, optional `io_uring`, `gio` for trash). macOS and
> Windows are on the roadmap — see [docs/CROSS_PLATFORM_GUI.md](docs/CROSS_PLATFORM_GUI.md).

## Install

```sh
cargo build --release
# binary at target/release/sdirstat
```

## Quick start

```sh
sdirstat serve                       # open http://127.0.0.1:8080 — the full interactive GUI
sdirstat /usr -o usr.html            # one self-contained report you can open in any browser
sdirstat /var --cache -o var.cache   # a QDirStat cache file — open it in QDirStat
sdirstat /home --total               # just the grand total, fast
sdirstat --help                      # the full CLI
```

## CLI

```
sdirstat <path> [options]      scan a directory (default: writes report.html)
sdirstat serve [-p PORT]       live web GUI at http://127.0.0.1:PORT (default 8080)
```

| flag | meaning |
|---|---|
| `--json` | emit a nested JSON tree instead of HTML |
| `--cache` | emit a QDirStat v2.0 cache file (drop-in for `qdirstat-cache-writer`) |
| `--total` | print only the grand total (scan + fold, no serialization) |
| `-o FILE` | output path (default: `report.html` / `tree.json` / `out.qdirstat.cache`) |
| `--threads N` | worker threads (default: CPU count; `1` = single-threaded) |
| `--max-depth N` | maximum recursion depth (default 40) |
| `--max-entries N` | OOM-guard entry ceiling (default 32M; `0` = unlimited). Hitting it warns and leaves the scan **incomplete** — raise it to scan a whole large `/`. |
| `--top K` | children kept per directory in the pruned tree/HTML/JSON (default 80) |
| `--apparent` | count apparent size (`st_size`) instead of allocated blocks |
| `--iouring` | io_uring batched-`statx` backend (Linux x86_64; for cold/SSD scans) |
| `-p, --port N` | port for `serve` (default 8080) |
| `-h, --help` | show help |

By default sizes are **allocated** (`st_blocks × 512`, what `du`/baobab/qdirstat report) with
hardlink dedup. `--apparent` switches to logical `st_size`.

## Output formats

- **HTML** (default) — a single self-contained file: a zoomable squarified treemap with the data
  embedded. Open it in any browser, no server. Good for reports and sharing.
- **JSON** (`--json`) — `{"n":name,"v":bytes,"d":1|0,"c":[…]}`, the pruned tree (top `--top`
  children per directory, the rest bucketed). For your own tooling / charts.
- **QDirStat cache** (`--cache`) — the `[qdirstat 2.0 cache file]` format with `D`/`F`/`L` records
  (path, size, uid, gid, perm, mtime, `blocks:`/`links:`). Open it directly in QDirStat, or feed it
  to any tool that reads that format. This is the headless-indexer replacement for the slow Perl
  `qdirstat-cache-writer`.

## The web GUI (`sdirstat serve`)

A local app at `http://127.0.0.1:PORT`. **Bound to `127.0.0.1` only** — it reads (and trashes)
files, so it is never exposed to the network (see [SECURITY.md](SECURITY.md)).

- **Three views**, synchronized with a sortable tree-table: **treemap** (`▦ map`), **sunburst**
  (`◎ rings`), and **file-type statistics** (`≡ types`).
- **Scan any folder** from the path bar; **breadcrumb** + double-click to zoom; **lazy-load** so
  deep trees stay explorable.
- **Right-click** any row or tile: Open, Reveal in folder, Copy path, **Move to Trash** (reversible
  via the system trash; confirmation modal; no hard-delete by design).
- **Incremental updates** — after a trash, only the changed subtree is re-scanned and the size delta
  folded to the root (≈10 ms, not a full re-scan).

HTTP API (for embedding / a future native GUI):

```
GET  /                                  the GUI (self-contained HTML)
GET  /scan?path=<urlenc>&top=N&depth=N  → {scan_ms, entries, tree, types}
POST /act?op=trash|open|reveal&path=<urlenc>
```

## Running headless (service / cron)

See **[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md)** for the full systemd unit + cron examples. In short:

- **Nightly cache via cron** (the `qdirstat-cache-writer` use case) — index a tree and drop a cache
  file a GUI can open later, off the user's clock:

  ```cron
  0 3 * * *  /usr/local/bin/sdirstat /srv --cache -o /var/cache/sdirstat/srv.cache
  ```

- **A persistent local GUI service** (`systemd`):

  ```ini
  [Service]
  ExecStart=/usr/local/bin/sdirstat serve --port 8080
  ```

  Bound to localhost; reach it over an **SSH tunnel** (`ssh -L 8080:127.0.0.1:8080 host`) or a
  reverse proxy that adds authentication. Never bind it to a public interface.

## Performance

Scanning `/usr` (≈1.25 M entries), warm cache, allocated sizes byte-exact with `du`:

| tool | wall | notes |
|---|---|---|
| `du -s` | ~2.3 s | total only |
| `qdirstat-cache-writer` (Perl) | ~7 s | the cache writer this replaces |
| **sdirstat** | **~0.5 s** | full tree + JSON/cache/GUI in one pass |

Faster than every timeable tool on the scan *and* the only one that also produces the explorable
tree. The walk is I/O-bound; `--iouring` adds a deep-queue batched-`statx` backend whose win is on
**cold** caches (overlapping random metadata reads). See [docs/CROSS_PLATFORM_GUI.md](docs/CROSS_PLATFORM_GUI.md)
for the architecture.

## Architecture (one line)

The directory tree is a **Web**: each node's own size + a parallel walk (schedule-free) + a single
reverse-pass **size fold** (`subtree = own + Σ children`). The cache, JSON, HTML, and GUI are all
*output adapters* over that one scanned tree.

## Project

- **Maintainers**: [MAINTAINERS.md](MAINTAINERS.md)
- **Contributing**: [CONTRIBUTING.md](CONTRIBUTING.md)
- **Security**: [SECURITY.md](SECURITY.md)
- Part of TWN Systems R&D, alongside `qwalk`, `cerialize`, and the rest of the search/data-plane line.
