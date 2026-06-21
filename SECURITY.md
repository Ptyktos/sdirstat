# Security Policy

## Reporting a vulnerability

Email **clay@twn.systems** with a description and reproduction steps. Please do **not** open a
public issue for security-sensitive reports. We aim to acknowledge within a few business days.

## Supported versions

This project is pre-1.0; only the latest `main` is supported. Fixes land on `main`.

## Security model

sdirstat reads filesystem metadata and, in the GUI, can move files to the system trash. Its design
keeps that surface small and local.

### The web GUI is localhost-only

`sdirstat serve` binds to **`127.0.0.1`** exclusively. The `/scan` endpoint reads arbitrary paths
on the host and `/act` can move files to trash, so the server is **never** exposed to a network
interface. To reach it from another machine, use an **SSH tunnel**
(`ssh -L 8080:127.0.0.1:8080 host`) or a reverse proxy that adds authentication and TLS in front of
it. Do not run it behind `0.0.0.0`.

### File actions are reversible by design

- **Move to Trash** uses `gio trash` — files go to the **system trash and are recoverable**. There
  is **no hard-delete** (`rm -rf`) in the tool, deliberately; empty the trash yourself.
- Actions require an explicit `POST /act` and a confirmation modal in the UI.
- Path guards: the action endpoint refuses `/`, `$HOME`, and non-existent paths.

### Scanning

- The scanner uses `lstat`-equivalent metadata (no symlink following) and does not read file
  contents — only directory entries and inode metadata.
- It does not follow symlinks into directories (no symlink loops), and bounds depth (`--max-depth`)
  and total entries.

### The io_uring backend (`--iouring`)

Uses **unprivileged** io_uring via the raw syscall ABI (no `liburing`), `IORING_OP_STATX` only
(metadata reads, no writes), with a sequential `statx` fallback if io_uring is unavailable. It
requires no elevated privileges. It is x86_64-Linux-only and **off by default**.

### No secrets

sdirstat handles no credentials, tokens, or secrets, and makes no outbound network connections (the
only network surface is the optional localhost GUI server).

## Hardening checklist for deployments

- Keep `serve` on `127.0.0.1`; front it with auth if remote access is needed.
- Run the headless `--cache` indexer as an unprivileged user with read-only access to the target.
- Prefer cron/`systemd` with a dedicated low-privilege account over running as root.
