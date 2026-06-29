# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Native desktop app** (`desktop/`, Tauri): wraps the existing GUI by launching the `sdirstat`
  binary as a sidecar (`serve` on a loopback port) and opening a native window. Builds to `.deb`
  (verified), and — via CI — `.AppImage`, Windows `.msi`/NSIS `.exe`, and a macOS universal `.dmg`.
  The zero-dependency core is untouched; Tauri lives in a workspace-excluded crate.
- **`sdirstat gui` / `install-desktop` / `uninstall-desktop`**: the single binary self-installs a
  clickable XDG `.desktop` entry + icon (no root, no package) and opens the GUI as a standalone app
  window (chromium `--app`, falling back to the default browser).
- **O(1) navigation cache** for `serve`: revisiting a path certifies from cache by reading one
  coordinate — the directory's own mtime — so navigating away and back does not re-walk. A change at
  the coordinate (or read when you navigate into a changed subdirectory) triggers a rescan; a **↻
  Rescan** button forces a fresh walk.
- **Release pipeline** (`.github/workflows/release.yml`): on a `v*` tag, builds the CLI binaries +
  native installers for Linux/Windows/macOS, generates `SHA256SUMS` and an SPDX SBOM, signs every
  artifact with **cosign** (keyless), attests **SLSA build provenance**, and cuts a GitHub Release.
- **OpenSSF Scorecard** workflow + Dependabot + CODEOWNERS; all GitHub Actions pinned to commit SHAs.
- Governance: `CHANGELOG.md`, `CODE_OF_CONDUCT.md`, `RELEASE.md`, `docs/SIGNING.md`.

### Changed
- Dual-licensed **MIT OR Apache-2.0** (added `LICENSE-APACHE` to match `Cargo.toml`).

### Notes
- The Windows and macOS desktop installer builds are written but **not yet executed on a real
  runner** — expect to iterate on the first tagged release.
- Code signing (macOS Developer ID + notarization, Windows Authenticode) is scaffolded in CI but
  inert until the certificates are provided as repository secrets (see `docs/SIGNING.md`).

[Unreleased]: https://github.com/Ptyktos/sdirstat/commits/main
