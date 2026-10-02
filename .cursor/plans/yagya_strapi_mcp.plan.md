---
name: Wire rust-bot to our-yagya Strapi 5 built-in MCP
overview: Attach the our-yagya-strapi-5 built-in MCP server (Strapi 5.51.2, endpoint http://localhost:1337/mcp) to the temp-2 rust-bot install as a streamableHttp MCP client with Bearer Admin-token auth. Config-only — rust-bot already supports the transport natively (McpTransportType::StreamableHttp, src/agent/tools/mcp/mod.rs:346). Main risk: the project has ~40 content types, so a full-permission token would expose up to ~300 tools into a 131072-token context window that already carries 77 sisterjayanti tools. Mitigation: scoped Admin token + enabledTools allowlist.
todos:
  - id: scope-token
    content: Decide Admin token scope — recommended start read-only (`read`) on a starter set of content types (e.g. page, event, speaker, video); Strapi only exposes MCP tools the token permits, so the token IS the tool-menu control
    status: pending
  - id: create-token
    content: Create the Admin token in the yagya admin panel (Admin tokens feature); copy value; do not commit it anywhere in a repo
    status: pending
  - id: start-strapi
    content: Start yagya Strapi and sanity-check the endpoint — POST http://localhost:1337/mcp with no token should get 401, with the Bearer token a valid initialize. Port 1337 was free on 2026-09-30 19:30 UTC (netstat verified) but is also the strapiSso pointer in temp-2 config — confirm nothing else binds it first
    status: pending
  - id: config-edit
    content: Add `yagya-strapi` entry to tools.mcpServers in C:\temp\rust-bot-temp-2\.rust-bot\config.json — transportType "streamableHttp", url "http://localhost:1337/mcp", headers { "Authorization": "Bearer <token>" } (note the Bearer prefix — sisterjayanti's raw-token header is a different auth scheme), toolTimeout 30, enabledTools ["*"] initially just to capture real tool names
    status: pending
  - id: verify-tools
    content: Restart the gateway (rust-bot.exe), confirm `yagya-strapi` connects cleanly, capture tools/list — real tool names + count (naming convention for the ~40 content types is not documented; observe it)
    status: pending
  - id: narrow-tools
    content: Second config edit — replace enabledTools ["*"] with the allowlist chosen from observed names (per content type: list/get, plus create/update/publish/unpublish only if the token grants them anyway); restart and confirm the final menu size is sane
    status: pending
  - id: smoke-test
    content: Natural-language smoke test through the agent — "list 5 most recent pages", "get page <documentId>" — verify results match the admin panel
    status: pending
  - id: optional-main-install
    content: Optional — repeat the wiring on the main rust-bot install if this is meant to be permanent rather than test-only
    status: pending
isProject: false
---

## Flow

```mermaid
flowchart TD
    A[Decide token scope\nread-only starter set] --> B[Create Admin token\nin yagya admin panel]
    B --> C[Start yagya Strapi\nverify :1337 free first]
    C --> D[Sanity-check /mcp\n401 without token]
    D --> E[Add mcpServers entry\ntemp-2 config.json]
    E --> F[Restart gateway\nobserve tools/list]
    F --> G[Narrow enabledTools\nallowlist]
    G --> H[Smoke test\nvia agent chat]
    H --> I{Keep?}
    I -->|yes| J[Optional: wire main install]
    I -->|no| K[Remove entry, revoke token]
```

## Key facts (verified)

- **Transport**: Strapi's built-in MCP is streamable HTTP at `/mcp`; rust-bot maps `McpTransportType::StreamableHttp` → rmcp `StreamableHttpClientTransport` (src/agent/tools/mcp/mod.rs:346, 369). No code changes needed anywhere.
- **Auth**: Admin token created in the admin panel; sent as `Authorization: Bearer <token>`. Tool visibility is filtered by token permissions at connection time — the token is both auth and scope control.
- **Config target**: `C:\temp\rust-bot-temp-2\.rust-bot\config.json` → `tools.mcpServers` (sisterjayanti-strapi entry is the shape reference: transportType / url / headers / toolTimeout / enabledTools).
- **Tool surface at 5.51.2**: content-management tools only — collection types: list/get/create/update/delete/publish/unpublish/discard_draft; single types: get/write/delete/publish/unpublish/discard_draft. Media Library tools (10) require 5.54.0 — absent here. Dev-mode `log` utility tool is available in development mode only.
- **Scale**: ~40 content types in src/api → at full token permissions, up to ~280 content tools on top of the existing 77 sisterjayanti tools. Current model context window is 131072 tokens — do not connect with an unscoped token + enabledTools ["*"] left in place.

## Decision points (to confirm before execution)

1. **Token scope** — read-only starter set, or full CRUD from day one? Recommendation: read-only first; widen after the plumbing is proven.
2. **Target install** — temp-2 assumed (this session's install). Say so if the main install is the real target.
3. **Content-type starter set** — which ones does rust-bot actually need? Proposal: page, event, speaker, video unless there's a specific use case.
