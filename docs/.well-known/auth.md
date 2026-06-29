# Authentication & Authorization — sdirstat

**There is no authentication, by design.** sdirstat's only network surface is the optional local GUI
server (`sdirstat serve`), which **binds `127.0.0.1` exclusively**. Its endpoints read the filesystem
and can move files to the system trash, so the server is deliberately **never exposed to a network
interface** and ships **no auth layer** — the security boundary is the loopback interface and the OS
user the process runs as.

## Model

| aspect | value |
|---|---|
| Auth scheme | none |
| Network exposure | `127.0.0.1` (loopback) only |
| Identity | the local OS user running the process |
| Tokens / secrets | none (sdirstat handles no credentials and makes no outbound connections) |
| OAuth / OIDC | not applicable (no hosted, multi-user, or authenticated API) |

## Reaching it remotely (safely)

Do **not** change the bind address. Use an SSH tunnel:

```sh
ssh -L 8080:127.0.0.1:8080 your-server   # then open http://127.0.0.1:8080 locally
```

…or front it with a reverse proxy (nginx/Caddy) that terminates TLS and **adds** authentication,
proxying to `127.0.0.1:8080`. The `/act` endpoint acts on the server's filesystem as the service
user — gate it accordingly.

See [SECURITY.md](https://github.com/Ptyktos/sdirstat/blob/main/SECURITY.md) for the full security
model. There is intentionally no `oauth-authorization-server` / `oauth-protected-resource` document
here: sdirstat is not an OAuth-protected resource, and its absence is the correct signal.
