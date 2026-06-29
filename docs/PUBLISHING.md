# Publishing to package managers

Four channels, each driven by CI and **gated on a secret** — with the secret absent the job is inert
(logs a warning, exits 0), so the pipelines are safe to merge before any credentials exist.

| channel | workflow | secret | trigger |
|---|---|---|---|
| **crates.io** (`cargo install`) | `publish.yml` → `crates-io` | `CARGO_REGISTRY_TOKEN` | release published |
| **chocolatey** (`choco install`) | `publish.yml` → `chocolatey` | `CHOCO_API_KEY` | release published |
| **winget** (`winget install`) | `publish.yml` → `winget` | `WINGET_TOKEN` | release published |
| **apt** (`apt install`) | `pages.yml` → `/apt` | `APT_GPG_PRIVATE_KEY` | release published / push to main |

Set a secret with: `gh secret set NAME --repo Ptyktos/sdirstat`.

---

## crates.io — `cargo install sdirstat`

1. Create an API token at <https://crates.io/settings/tokens> (scope: publish-new + publish-update).
2. `gh secret set CARGO_REGISTRY_TOKEN`.
3. Ensure the name `sdirstat` is free (`cargo search sdirstat`). On the next published release,
   `publish.yml` runs `cargo publish`.

Users: `cargo install sdirstat`.

## chocolatey — `choco install sdirstat`

1. Register at <https://chocolatey.org>, create a push API key, `gh secret set CHOCO_API_KEY`.
2. On release, the `chocolatey` job fills `packaging/chocolatey/*.template` with the version, the
   release `.exe` URL, and its SHA256 (from `SHA256SUMS`), then `choco pack` + `choco push`.
3. **First submission is moderated** by the Chocolatey community — expect a review.

Users: `choco install sdirstat`.

## winget — `winget install Ptyktos.sdirstat`

1. Fork <https://github.com/microsoft/winget-pkgs>. Create a PAT (classic, `public_repo`) →
   `gh secret set WINGET_TOKEN`.
2. **First-ever manifest is manual** (wingetcreate can't "update" a package that doesn't exist yet):
   ```pwsh
   wingetcreate new https://github.com/Ptyktos/sdirstat/releases/download/v0.1.0/sdirstat-desktop-0.1.0-windows-x86_64.msi
   # set PackageIdentifier = Ptyktos.sdirstat, then --submit
   ```
3. Thereafter, each release's `winget` job runs `wingetcreate update Ptyktos.sdirstat …` to open the
   PR automatically. Microsoft moderates each PR.

Users: `winget install Ptyktos.sdirstat`.

## apt — `apt install sdirstat` (GitHub Pages repo)

The repo is served from the Pages site at `/apt`, rebuilt by `pages.yml` from the release `.deb`s and
signed with your key.

1. Generate a signing key (no passphrase, for CI), export the private key, and set the secret:
   ```sh
   gpg --batch --quick-gen-key 'sdirstat apt <you@example.com>' default default never
   KEYID=$(gpg --list-keys --with-colons | awk -F: '/^pub:/{print $5; exit}')
   gpg --armor --export-secret-keys "$KEYID" | gh secret set APT_GPG_PRIVATE_KEY --repo Ptyktos/sdirstat
   ```
2. Publish a release (so a `.deb` exists), then re-run `pages.yml`. It builds `/apt` and exports the
   public key to `/apt/sdirstat.gpg.key`.

Users:
```sh
curl -fsSL https://ptyktos.github.io/sdirstat/apt/sdirstat.gpg.key \
  | sudo gpg --dearmor -o /usr/share/keyrings/sdirstat.gpg
echo 'deb [signed-by=/usr/share/keyrings/sdirstat.gpg] https://ptyktos.github.io/sdirstat/apt stable main' \
  | sudo tee /etc/apt/sources.list.d/sdirstat.list
sudo apt update && sudo apt install sdirstat        # or: sdirstat-desktop
```

---

## Status

These pipelines are **scaffolded but unexercised** — each runs for the first time on the next
published release with its secret set. Expect first-run iteration (winget/choco moderation, the
cargo-wix-free crates path, apt key/index details). The `.deb`/`.exe`/`.msi`/checksum/cosign artifacts
they consume come from `release.yml`.
