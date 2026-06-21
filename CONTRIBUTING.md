# Contributing

Thanks for helping. sdirstat is a small, focused tool; contributions that keep it that way are the
easiest to merge.

## Build & run

```sh
cargo build --release
./target/release/sdirstat --help
./target/release/sdirstat serve            # the GUI at http://127.0.0.1:8080
cargo build && cargo clippy && cargo fmt   # before sending an MR
```

There are no required external services. Linux is the primary platform.

## Project layout

```
src/main.rs        the CLI: arg parsing, the parallel scan, the size fold, the output adapters
src/uring.rs       raw io_uring + statx primitives (the --iouring backend), zero-dep
src/lib.rs         the library surface (currently the uring module)
src/app.html       the full web GUI (served by `sdirstat serve`) — vanilla JS, no CDN
src/viewer.html    the self-contained static treemap (the default HTML output)
src/bin/           standalone experiments (e.g. the io_uring scanner)
docs/              deployment + the cross-platform GUI plan
```

The core model: the directory tree is a **Web** (node = own size, edges = dir→children); the scan
is a parallel walk; the size fold is one reverse pass (`subtree = own + Σ children`); the cache,
JSON, HTML, and GUI are output adapters over that one scanned tree. Keep that shape — add an output
adapter, not a second scanner.

## The rules that matter

1. **Zero runtime dependencies in the core scanner.** The backend is `std`-only on purpose. Adding
   a crate to the scan/serve path is a maintainer decision (see [MAINTAINERS.md](MAINTAINERS.md)),
   not a routine change. The GUI is vanilla JS with **no CDN** (it must work offline).
2. **Sizes stay byte-exact with `du`.** Allocated (`st_blocks×512`) + hardlink dedup by default; if
   you touch the size path, verify against `du -s --block-size=1` (and `du -l` for the no-dedup
   case) on a real tree.
3. **The GUI server is localhost-only.** Don't change the bind address; see [SECURITY.md](SECURITY.md).
4. **No hard-delete.** File actions are reversible (trash only). Don't add `rm -rf`.
5. **Benchmark on real filesystems**, not synthetic trees, and report warm vs cold and RSS — the
   workload is I/O-bound and synthetic numbers mislead.

## Style & commits

- `cargo fmt` + `cargo clippy` clean.
- Match the surrounding code: terse comments that say *why*, the existing naming.
- Commit messages describe **what** and **why** in terms a future reader sees in the diff. Conventional
  Commits (`feat:`, `fix:`, `docs:`, `perf:`) are welcome but not enforced.

## Submitting

Open a merge request against `main` with a short description of the change and how you verified it
(the command + the number). For anything that adds a dependency, a frontend, or changes the cache
format or size semantics, include a one-paragraph rationale.
