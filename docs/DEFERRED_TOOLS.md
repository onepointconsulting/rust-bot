# Deferred tools (loading tools on demand)

Every tool the agent can call is normally described to the model on every request. An MCP server with many
tools makes every request large: a Strapi server with about 100 tools added roughly 460,000 prompt tokens to
each request, more than many models can take at all.

A **deferred** tool is not described to the model up front. The model finds it with the built-in
`tool_search` tool, which loads the best matches; they are described from the next model call on and stay
loaded for the rest of the chat. With the same Strapi server deferred, requests dropped to about 14,000
prompt tokens.

## When to use it

Defer an MCP server when it has many tools and most conversations do not need them. Keep a server **direct**
(the default) when it is used in most conversations or has only a few tools: deferral costs the model one
extra step (the search) before the first call.

## Configuration

Deferral is set per MCP server with `exposure`; there is no automatic rule. Built-in tools can be deferred by
name with `tools.deferredTools`.

```json
"tools": {
  "mcpServers": {
    "ouryagya-strapi": {
      "transportType": "sse",
      "url": "https://example.org/mcp",
      "exposure": "deferred",
      "description": "Strapi CMS: list, read, create and update news, events, authors and other content"
    }
  },
  "deferredTools": ["docx"]
}
```

| Key | Values | Meaning |
|---|---|---|
| `mcpServers.<name>.exposure` | `"direct"` (default), `"deferred"` | Whether the server's tools are described on every request or loaded on demand. |
| `mcpServers.<name>.description` | text | One line telling the model what the server offers, in the words a user would use. Strongly recommended for deferred servers. |
| `tools.deferredTools` | list of built-in tool names | Built-in tools to load on demand. `tool_search` cannot be listed; a name that is also in `disabledTools` stays disabled. |

`exposure` can also be switched in the web UI: **Settings → MCP servers → Load tools on demand**. Like the
other settings there, it takes effect after a restart. A server enabled from an MCP preset gets the preset's
description when it has none.

Subagents ignore deferral: they always get their tools directly.

## What the model sees

When anything is deferred, the system prompt gets an "Optional tools" section that names each deferred server
(with its tool count, a few example tool names and its `description`) and each deferred built-in, and tells
the model to call `tool_search` before saying it cannot do something. `tool_search` ranks the deferred tools
with BM25 over their names, descriptions, parameter names and server name:

- a query with a tool's exact name (or the name without the `mcp_<server>_` prefix) always loads that tool;
- other matches are loaded only if they score at least half as well as the best match; weaker matches are
  listed as "not loaded" so the model can ask for them by name;
- at most 8 tools are loaded per search by default (the model can ask for up to 20).

Under the `no_mcp` agent mode, deferred MCP tools are neither listed nor searchable.

## Where to see what is loaded

- The tools popup in the web UI counts the tools the model can call now, marks tools loaded in this chat as
  **loaded**, and lists the rest under **Available on demand**.
- `/tools` shows the same split.
- The chat's session file stores the loaded tools under `loaded_tools`. `/new` clears them with the history.
