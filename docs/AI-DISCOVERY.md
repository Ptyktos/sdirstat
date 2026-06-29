# AI & agent discoverability

Files served from the Pages site (`https://ptyktos.github.io/sdirstat/`) so crawlers, LLMs, and
agents can find and understand sdirstat. **Real** = accurate today; **Template** = scaffold for a
service sdirstat doesn't host yet (fill in + remove the `_template`/`_comment` key to activate).

| file | standard | status |
|---|---|---|
| `/robots.txt` | Robots + **Content Signals** + AI-bot rules (Bot Access Control) | ✅ real |
| `/sitemap.xml` | Sitemaps 0.9 | ✅ real |
| `/llms.txt` | [llmstxt.org](https://llmstxt.org) | ✅ real |
| `/index.md` | Markdown content negotiation / accessibility | ✅ real |
| `/.well-known/api-catalog` | RFC 9727 (linkset) | ✅ real (the `serve` API + library) |
| `/.well-known/openapi.json` | OpenAPI 3.1 of `sdirstat serve` | ✅ real |
| `/.well-known/auth.md` | Auth model | ✅ real (documents **no-auth, loopback-only**) |
| `/.well-known/agent-skills.json` | Agent skills → real CLI/lib commands | ✅ real |
| `/.well-known/oauth-authorization-server` | RFC 8414 | 📝 template (no OAuth server) |
| `/.well-known/oauth-protected-resource` | RFC 9728 | 📝 template |
| `/.well-known/mcp.json` | MCP server card | 📝 template (no hosted MCP server) |
| `/.well-known/agent.json` | A2A Agent Card | 📝 template (no hosted agent) |
| `/.well-known/webmcp.json` | WebMCP | 📝 template |
| `/.well-known/http-message-signatures-directory` | **Web Bot Auth** (JWKS) | 📝 template (no signing key) |

## Things GitHub Pages can't serve — apply these on a custom domain / real server

GitHub Pages serves static files only: **it can't set HTTP response headers**, and you don't control
`github.io` DNS. Two items therefore live here as instructions:

### Link headers
On a server you control, advertise discovery via HTTP `Link:` headers (RFC 8288), e.g.:

```
Link: </.well-known/api-catalog>; rel="api-catalog"
Link: </llms.txt>; rel="alternate"; type="text/plain"
Link: </index.md>; rel="alternate"; type="text/markdown"
```

On Pages we approximate these with `<link>` elements in the page `<head>` (already added to
`index.html`).

### DNS for AI Discovery (DNS-AID)
With a custom domain, publish TXT records pointing agents at the cards (draft *DNS-based AI agent
discovery*):

```
_agent.example.com.   IN TXT "v=aid1; uri=https://example.com/.well-known/agent.json; proto=a2a"
_mcp.example.com.     IN TXT "v=aid1; uri=https://example.com/.well-known/mcp.json; proto=mcp"
example.com.          IN TXT "llms=https://example.com/llms.txt"
```

## Caveats

- Extensionless `.well-known` files (`api-catalog`, `http-message-signatures-directory`) are served
  by Pages as `text/plain`/`octet-stream`, not their spec media types (`application/linkset+json`
  etc.). Consumers that sniff JSON still parse them; a custom server can set the correct type.
- Update `version`/URLs here when the project version or domain changes.
