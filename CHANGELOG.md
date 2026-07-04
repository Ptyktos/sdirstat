# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2] - 2026-07-04

### Fixed
- **Windows `VCRUNTIME140.dll was not found`**: the CLI (and the identical binary bundled as the
  Tauri desktop app's sidecar) dynamically linked the MSVC C runtime, which a stock Windows install
  doesn't ship. Now built with `-C target-feature=+crt-static`, so it's a true no-install-required
  portable binary. `v0.1.1`'s Windows binaries hit this on a clean VM — this is the first release
  verified to actually run on Windows.
- **macOS job disabled** in the release pipeline (`if: false`) until Developer ID signing certs are
  configured — an unsigned `.dmg` just gets Gatekeeper-flagged, not worth shipping yet.

## [0.1.1] - 2026-07-03

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

### Fixed
- **macOS universal Tauri bundle**: the `.dmg` build was failing in CI because the release job only
  staged the per-arch sidecars, not the `lipo`'d universal binary Tauri's bundler expects for a
  `universal-apple-darwin` target. `v0.1.0`'s release run never published assets as a result — this
  is effectively the first release to actually ship binaries, including Windows.

### Notes
- Code signing (macOS Developer ID + notarization, Windows Authenticode) is scaffolded in CI but
  inert until the certificates are provided as repository secrets (see `docs/SIGNING.md`).

[Unreleased]: https://github.com/Ptyktos/sdirstat/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/Ptyktos/sdirstat/releases/tag/v0.1.2
[0.1.1]: https://github.com/Ptyktos/sdirstat/releases/tag/v0.1.1
