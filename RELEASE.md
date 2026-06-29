# Releasing sdirstat

Releases are cut by pushing a `v*` tag; `.github/workflows/release.yml` does the rest (build all
OSes → checksums → SBOM → cosign signing → SLSA provenance → GitHub Release).

## Cut a release

1. Bump the version in `Cargo.toml`, `desktop/Cargo.toml`, and `desktop/tauri.conf.json`, and move
   the `## [Unreleased]` section of `CHANGELOG.md` under a new `## [X.Y.Z] - YYYY-MM-DD` heading.
2. Commit on a branch, open a PR, merge to `main`.
3. Tag and push:
   ```sh
   git tag vX.Y.Z
   git push origin vX.Y.Z        # or: git push github vX.Y.Z
   ```
4. Watch the `release` workflow. On success the GitHub Release holds, per OS:
   - CLI binaries (portable, static musl, `.exe`, macOS universal) + the `.deb`
   - desktop installers (`.deb`/`.AppImage`, `.msi`/NSIS `.exe`, `.dmg`)
   - `SHA256SUMS`, `sdirstat.spdx.json` (SBOM), and a `*.cosign.bundle` per artifact

## Verify a release (what users can run)

```sh
# integrity
sha256sum -c SHA256SUMS

# cosign keyless signature (identity = the release workflow's OIDC identity)
cosign verify-blob \
  --bundle sdirstat-X.Y.Z-linux-x86_64.cosign.bundle \
  --certificate-identity-regexp 'https://github.com/Ptyktos/sdirstat/.github/workflows/release.yml@.*' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  sdirstat-X.Y.Z-linux-x86_64

# SLSA build provenance
gh attestation verify sdirstat-X.Y.Z-linux-x86_64 --repo Ptyktos/sdirstat
```

## Notes

- The version fallback in CI is `0.1.0` when not building from a tag (e.g. a `workflow_dispatch` dry
  run), so manual runs still produce named artifacts.
- Windows/macOS desktop bundling is **unverified** until the first real tagged run — budget time to
  fix paths/toolchain quirks on those runners.
- Code signing of the installers requires the secrets in [`docs/SIGNING.md`](docs/SIGNING.md);
  without them the installers are unsigned (functional, but Gatekeeper/SmartScreen will warn).
