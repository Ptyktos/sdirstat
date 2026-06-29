# Code signing & artifact verification

Two independent layers:

1. **cosign keyless** signs *every* release artifact via GitHub OIDC — **already active, no secrets.**
2. **OS code signing** of the installers (macOS Developer ID + notarization, Windows Authenticode) —
   **scaffolded but inert** until you add the certificates as repository secrets below.

Without (2) the installers still work, but macOS Gatekeeper and Windows SmartScreen warn the user
that the publisher is unverified.

## cosign (keyless) — already on

The `release` job runs `cosign sign-blob --bundle <file>.cosign.bundle` for each artifact, using the
workflow's short-lived OIDC identity (Fulcio cert, Rekor transparency log). No keys are stored.
Verification is in [`../RELEASE.md`](../RELEASE.md).

## macOS — Developer ID signing + notarization

Tauri signs and notarizes automatically when these repo secrets are set (the `macos` job already
passes them through as `APPLE_*` env):

| secret | what |
|---|---|
| `APPLE_CERTIFICATE` | base64 of your **Developer ID Application** `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | the `.p12` export password |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: Your Name (TEAMID)` |
| `APPLE_ID` | your Apple ID email (for notarization) |
| `APPLE_PASSWORD` | an **app-specific password** for that Apple ID |
| `APPLE_TEAM_ID` | your 10-char Apple Developer Team ID |

```sh
# produce APPLE_CERTIFICATE from your exported .p12
base64 -w0 DeveloperID_Application.p12 | gh secret set APPLE_CERTIFICATE --repo Ptyktos/sdirstat
```

## Windows — Authenticode

Tauri signs the `.msi`/NSIS `.exe` when `bundle.windows.certificateThumbprint` is set in
`desktop/tauri.conf.json` and the certificate is importable on the runner. Recommended wiring:

1. Add secrets `WINDOWS_CERTIFICATE` (base64 of the `.pfx`) and `WINDOWS_CERTIFICATE_PASSWORD`.
2. In the `windows` job, before `cargo tauri build`, import the cert and set the thumbprint:
   ```bash
   echo "$WINDOWS_CERTIFICATE" | base64 -d > cert.pfx
   # import into the user store, read the thumbprint, set it in tauri.conf.json (or via CARGO env)
   ```
3. Or use an EV cert in a cloud HSM (Azure Trusted Signing / DigiCert KeyLocker) with a Tauri custom
   `signCommand` — preferred for SmartScreen reputation.

Until either is configured, the Windows installer ships unsigned.

## Verifying everything (summary)

```sh
sha256sum -c SHA256SUMS                                   # integrity
cosign verify-blob --bundle <art>.cosign.bundle ...       # signature (see RELEASE.md)
gh attestation verify <art> --repo Ptyktos/sdirstat       # SLSA build provenance
```
