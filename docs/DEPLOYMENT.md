# Deployment — headless indexing & the GUI service

sdirstat runs in two operational modes: a **headless indexer** (emit a cache/JSON on a schedule,
the `qdirstat-cache-writer` role) and a **local GUI service** (`serve`). Both are below.

## 1. Headless cache indexer (the cron / scheduled role)

The classic server workflow: scan a tree in the background, off the user's clock, and drop a cache
file that a GUI (QDirStat, or `sdirstat serve` reading the JSON) opens instantly.

### cron

```cron
# /etc/cron.d/sdirstat — nightly index of /srv, as an unprivileged user
0 3 * * *  indexer  /usr/local/bin/sdirstat /srv --cache -o /var/cache/sdirstat/srv.cache
```

QDirStat reads gzipped caches too; gzip if you want to save space:

```sh
sdirstat /srv --cache -o /var/cache/sdirstat/srv.cache && gzip -f /var/cache/sdirstat/srv.cache
# → /var/cache/sdirstat/srv.cache.gz, openable in QDirStat
```

### systemd timer (the modern equivalent)

`/etc/systemd/system/sdirstat-index.service`:

```ini
[Unit]
Description=sdirstat — index /srv to a cache file

[Service]
Type=oneshot
User=indexer
Nice=10
IOSchedulingClass=idle
ExecStart=/usr/local/bin/sdirstat /srv --cache -o /var/cache/sdirstat/srv.cache
```

`/etc/systemd/system/sdirstat-index.timer`:

```ini
[Unit]
Description=Run sdirstat index nightly

[Timer]
OnCalendar=*-*-* 03:00:00
Persistent=true

[Install]
WantedBy=timers.target
```

```sh
systemctl enable --now sdirstat-index.timer
```

Notes:
- Run as a **dedicated unprivileged user** with read access to the target; never as root.
- `--cache` writes every entry (one pass, memory holds the tree). For a grand total only, use
  `--total`; for your own dashboards, `--json`.
- The scan is I/O-bound — `Nice`/`IOSchedulingClass=idle` keeps it out of the way on busy hosts.

## 2. The GUI as a service

`sdirstat serve` is a long-running local web app. It **binds `127.0.0.1` only** because its
endpoints read the filesystem and can move files to trash (see [../SECURITY.md](../SECURITY.md)).

`/etc/systemd/system/sdirstat-gui.service`:

```ini
[Unit]
Description=sdirstat GUI (localhost only)
After=network.target

[Service]
ExecStart=/usr/local/bin/sdirstat serve --port 8080
Restart=on-failure
User=youruser

[Install]
WantedBy=default.target
```

### Reaching it remotely (safely)

The server has **no authentication** and is localhost-bound by design. To use it from another
machine, do **not** change the bind address — tunnel instead:

```sh
# from your laptop:
ssh -L 8080:127.0.0.1:8080 server
# then open http://127.0.0.1:8080 locally
```

Or put a reverse proxy (nginx/Caddy) in front that terminates TLS and enforces auth, proxying to
`127.0.0.1:8080`. The file-action endpoints (`/act`) act on the server's filesystem as the service
user — gate them accordingly.

## Output cheatsheet

| you want | command |
|---|---|
| a QDirStat-openable cache | `sdirstat <dir> --cache -o out.cache` |
| JSON for your own tooling | `sdirstat <dir> --json -o tree.json` |
| a shareable HTML report | `sdirstat <dir> -o report.html` |
| just the number | `sdirstat <dir> --total` |
| the live explorer | `sdirstat serve` |
