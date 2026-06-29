# AUR packaging

Two AUR packages:

| package | what | source |
|---|---|---|
| [`sdirstat`](sdirstat/) | builds from source with `cargo` | the release tag's source tarball |
| [`sdirstat-bin`](sdirstat-bin/) | installs the prebuilt release binary | the release's `…-linux-x86_64` asset |

Both install the `sdirstat` CLI, the `.desktop` entry (which launches `sdirstat gui`), the icon, and
the licenses. `sdirstat-bin` `provides`/`conflicts` `sdirstat` so only one is installed at a time.

## Install (users)

```sh
yay -S sdirstat        # build from source
yay -S sdirstat-bin    # prebuilt binary (faster install)
# (or paru, or: makepkg -si in the package dir)
```

## Publishing to the AUR (maintainers)

The AUR hosts each package as its own git repo (`ssh://aur@aur.archlinux.org/<pkg>.git`). The
PKGBUILDs here are the source of truth; mirror them to the AUR.

**First-time submission** (per package), on an Arch box with `base-devel` + an
[AUR SSH key](https://wiki.archlinux.org/title/AUR_submission_guidelines):

```sh
git clone ssh://aur@aur.archlinux.org/sdirstat.git aur-sdirstat
cp packaging/aur/sdirstat/PKGBUILD aur-sdirstat/
cd aur-sdirstat
updpkgsums                      # fill real sha256sums (needs the release tag to exist)
makepkg --printsrcinfo > .SRCINFO
makepkg -si                     # sanity-build + install locally
git add PKGBUILD .SRCINFO && git commit -m "sdirstat 0.1.1" && git push
```

**Each new release:** bump `pkgver` (and reset `pkgrel=1`), then `updpkgsums`,
`makepkg --printsrcinfo > .SRCINFO`, commit, push. The `aur.yml` workflow automates this when the
`AUR_SSH_KEY` secret is set (see below).

> The committed `sha256sums` are `SKIP` here because the release they reference may not exist yet;
> `updpkgsums` (or the workflow) replaces them with real checksums at publish time. `sdirstat-bin`'s
> binary checksum also appears in the release `SHA256SUMS`.

## Notes

- `depends = gcc-libs glibc` — what a dynamically-linked Rust binary needs at runtime.
- `chromium` is an optdepend: `sdirstat gui` opens a standalone app window via any chromium-based
  browser, falling back to the default browser otherwise.
- The `.desktop` runs `sdirstat gui`; no separate launcher script is installed.
