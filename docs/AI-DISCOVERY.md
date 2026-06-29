# AI & agent discoverability

Files served from the Pages site (`https://ptyktos.github.io/sdirstat/`) so crawlers, LLMs, and
agents can find and understand sdirstat. Everything here is **real** — it describes what sdirstat
actually is. We deliberately do **not** publish cards for services sdirstat doesn't run (OAuth
server, hosted MCP server, A2A agent, WebMCP, Web Bot Auth); their absence is the correct signal, and
the no-auth model is documented in [`.well-known/auth.md`](.well-known/auth.md).

| file | standard | describes |
|---|---|---|
| `/robots.txt` | Robots + **Content Signals** + AI-bot rules (Bot Access Control) | crawl + AI-use policy |
| `/sitemap.xml` | Sitemaps 0.9 | the site URLs |
| `/llms.txt` | [llmstxt.org](https://llmstxt.org) | LLM-oriented project summary |
| `/index.md` | Markdown content negotiation / accessibility | the homepage as Markdown |
| `/.well-known/api-catalog` | RFC 9727 (linkset) | the `serve` API + the library |
| `/.well-known/openapi.json` | OpenAPI 3.1 | the `sdirstat serve` HTTP API |
| `/.well-known/auth.md` | — | the **no-auth, loopback-only** model |
| `/.well-known/agent-skills.json` | — | sdirstat's skills → real CLI/library commands |

`index.html` also carries `<link>` discovery elements (Link-header equivalents).

## Things GitHub Pages can't serve — apply on a custom domain / real server

Pages serves static files only: it **can't set HTTP response headers**, and you don't control
`github.io` DNS. So these two live here as instructions, with `<your-domain>` as the placeholder.

### Link headers
On a server you control, advertise discovery via HTTP `Link:` headers (RFC 8288):

```
Link: </.well-known/api-catalog>; rel="api-catalog"
Link: </llms.txt>; rel="alternate"; type="text/plain"
Link: </index.md>; rel="alternate"; type="text/markdown"
```

On Pages we approximate these with `<link>` elements in the page `<head>` (already in `index.html`).

### DNS for AI Discovery (DNS-AID)
With a custom domain, publish a TXT record pointing agents at the LLM summary (draft *DNS-based AI
agent discovery*):

```
<your-domain>.   IN TXT "llms=https://<your-domain>/llms.txt"
```

If you ever stand up an A2A agent or MCP server for sdirstat, add its card under `/.well-known/` and a
matching `_agent` / `_mcp` TXT record then — not before.

## Caveats

- Extensionless `.well-known` files (`api-catalog`) are served by Pages as `text/plain`, not their
  spec media type (`application/linkset+json`). JSON-sniffing consumers still parse them; a custom
  server can set the correct type.
- Update `version`/URLs here when the project version or domain changes.
