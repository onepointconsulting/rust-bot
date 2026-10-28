# Plan: deferred tools with `tool_search` (modelled on pi)

## Goal

Stop sending every tool definition on every model call. Tools marked deferred are registered and callable, but
not declared to the model until the model asks for them through a `tool_search` tool. Main target: MCP servers
with dozens of tools, whose schemas today are sent on every request, are counted by `snip_history`, and are part
of every cached prompt.

## Verdict

Feasible, and simpler than in pi, because all tool handling passes through a few choke points
(`Tool` trait, `ToolRegistry`, `AgentLoop::run_agent_loop`). One part of pi's design does not carry over: pi records
tool changes as mid-conversation system messages and sends them as native provider blocks (Anthropic
`tool_addition`/`tool_removal`, OpenAI `additional_tools`), which keeps the cached prefix intact. rust-bot cannot
do that cheaply (see "What does not carry over"), so each load costs one prompt-cache miss. That is acceptable for
a first version and measurable.

## What pi does (reference)

Source: `packages/coding-agent/src/extensions/tool-search/tool.ts`, `packages/ai/src/api/anthropic-messages.ts`,
`packages/ai/src/api/openai-responses-shared.ts` in the pi repo.

1. A tool has an `exposure`. `deferred` tools are registered but not declared to the model.
2. `tool_search({query, limit})` ranks the deferred tools that are not active yet with BM25 over name, description,
   schema property names and descriptions, and namespace text. A query that names a tool exactly ranks it first.
   It activates the top matches. They are declared from the next model call.
3. The activation is recorded in the transcript, so it survives resume and branch navigation.
4. Providers translate the change into native blocks so the prompt cache is not invalidated.
5. MCP servers can be configured as `deferred`; `tool_search` is activated instead of the code-mode tool.

## What exists in rust-bot today (verified by reading the code)

- `Tool` trait: `src/agent/tools/base.rs:49`. Plain `name/description/parameters/execute`, no exposure notion.
- `ToolRegistry`: `src/agent/tools/registry.rs`. `get_definitions()` returns every registered tool, built-ins
  alphabetical then `mcp_*` alphabetical. `prepare_call` resolves a call by name. `restrict_by` makes a per-call view.
- MCP tools are registered at runtime, lazily on the first message: `AgentLoop::connect_mcp` ->
  `record_mcp_batch` (`src/agent/agent_loop.rs:1000`, `:1027`) -> `registry.register(tool)`. Each is an
  `MCPToolWrapper` named `mcp_{server}_{tool}` (`src/agent/tools/mcp/mod.rs:571`).
- Per turn, `AgentLoop::tools_for_session` (`agent_loop.rs:808`) applies the session `AgentMode`
  (`src/agent/modes.rs`) to the shared registry and hands the resulting owned `ToolRegistry` to
  `AgentRunSpec.tools` (`agent_loop.rs:1276`, `:1316`).
- `AgentRunner::run` calls `spec.tools.get_definitions()` on every iteration, for the token estimate in
  `snip_history` (`runner.rs:372`) and for the request (`runner.rs:579`). Tool calls go through
  `spec.tools.prepare_call` (`runner.rs:1024`).
- Messages are OpenAI-shaped JSON (`role: tool`, `tool_call_id`, `name`, `content`). The Anthropic provider
  converts them and folds system messages into one `system` parameter (`anthropic_provider.rs:203`).
- Cache markers for Anthropic tools: `tool_cache_marker_indices` (`providers/base.rs:733`) marks the last non-`mcp_`
  tool and the tail tool. Anthropic's order is tools, system, messages.
- Per-session state lives in `session.metadata` (`SESSION_AGENT_MODE_METADATA_KEY` in `src/session/keys.rs`),
  set through `AgentLoop::set_session_mode`.
- A per-run injection precedent exists: `ContextBuilder::with_prompt_section` (used for ACP).
- Subagents build their own registry (`subagent.rs:625`) and do not receive MCP tools.
- Every agent turn goes through `AgentLoop::run_agent_loop` (`agent_loop.rs:1233`): user messages (`:2750`), system
  messages such as cron and heartbeat (`:2349`), and resumed turns (`:1604`). It builds the view at `:1276`.
- `connect_mcp` (`agent_loop.rs:1000`) retries servers that failed (`mcp_retry_due`), so a server can connect, and
  its tools appear, later in a session.
- `McpServerConfig` (`config/schema.rs:1431`) has no description field.
- Read: `openai_compat_provider.rs` tool handling (sends tools verbatim, `:609-613`).
- Not read yet, to be checked in the milestone that needs them: `checkpoint_payload.rs`, `resume.rs`, the session
  clear/new path, `mcp_presets_api.rs`.

## What does not carry over from pi

- No transcript system messages. rust-bot has no message type for "tools changed", and the Anthropic provider
  collapses system messages, so a native `tool_addition` block cannot be emitted without a new message type and a
  provider change. Native blocks are also a provider beta in pi (`inline-tools-2026-09-15`); its availability must
  be verified before relying on it.
- No `/tree` or branch navigation. Loaded tools therefore follow the session, not a branch, which is simpler.
- No code-mode tool. `deferred` is the only non-default exposure we need; pi's `codemode`, `model-only` and `hidden`
  levels are not ported.

## Design

### D1. Exposure on the tool

```rust
pub enum ToolExposure { Direct, Deferred }
// Tool trait
fn exposure(&self) -> ToolExposure { ToolExposure::Direct }
```

Default `Direct` keeps every existing tool unchanged. Deferred is set by configuration, not hard-coded:

- MCP: `McpServerConfig.exposure` (`direct` default, `deferred`), applied to every tool of that server in
  `MCPToolWrapper`.
- Built-ins: `tools.deferredTools: [names]`, same shape as `tools.disabledTools`. Applied as a set of deferred names
  on the main registry, filled in `register_default_tools` (`agent_loop.rs:928`), not as a property of the tool
  itself. `ToolRegistry::exposure_of(name)` combines both sources.
- Subagents ignore `tools.deferredTools`. `build_tool_registry` (`subagent.rs:625`) registers document conversion,
  gmail, OCR and others into its own registry, which has no deferred set and no `tool_search`. If deferral were a
  property of the tool, a deferred `docx` would be unreachable in a subagent.

User configuration. Deferral is chosen per MCP server; there is no automatic rule (open question 1). The README and
`docs/` section (M3) show this example, using the same camelCase keys as the rest of `mcpServers`:

```json
"mcpServers": {
  "ouryagya-strapi": {
    "transportType": "sse",
    "url": "https://.../mcp",
    "exposure": "deferred",
    "description": "Strapi CMS: read, create and update content entries, media and users",
    "enabledTools": ["*"]
  }
},
"tools": {
  "deferredTools": ["docx"]
}
```

- `exposure` is `direct` (default) or `deferred`. Defer servers with many tools that are used now and then; keep
  servers used in most conversations `direct`.
- `description` is one line for the model: what the server offers, in the words a user would use.
- `enabledTools` is documented as filtering which tools are registered, but it is not applied when MCP tools are
  registered today (found during M1: only the wizard and the ACP config overlay read it; `record_mcp_batch`
  registers every listed tool). Separate fix; until then every tool of a deferred server is searchable.

Config validation, at load time:

- `tool_search` cannot be deferred: reject the config.
- A name in `tools.deferredTools` that is not a known built-in: warn and ignore it.
- A name in both `tools.disabledTools` and `tools.deferredTools`: warn; disabled wins.

### D2. Activation lives in the registry

`ToolRegistry` gets an activation handle (`Arc<Mutex<Vec<String>>>`, load order preserved). The handle is per
session view, never stored on the process-wide registry:

- `tools_for_session` (`agent_loop.rs:808`) creates a fresh handle on each call, seeded from that session's
  `loaded_tools` (D5). A handle on the shared registry, cloned by `restrict_tools`, would share one loaded set
  across all chats.
- Within a turn the view and the copy the runner holds share the same handle, so a load during iteration N is
  visible to `get_definitions()` in iteration N+1 with no change to the runner loop.
- Seeding in `tools_for_session` also gives `/tools`, which already calls it, the loaded state.
- `restrict_by` (`registry.rs:221`) builds the view from `tools` only and resets `disabled`. It must copy the
  deferred-name set and take the handle explicitly, or both are silently lost in every mode view.

- `get_definitions()` returns: direct built-ins (A-Z), direct MCP (A-Z), then loaded deferred tools in load order.
  Load order, not alphabetical, so earlier loads never move and the cached prefix only grows at the tail.
- `prepare_call` rejects a deferred tool that is not loaded:
  `Error: Tool 'x' is not loaded. Call tool_search first.` Its "Available:" list names declared tools only, so it does
  not leak the deferred catalogue.
- New `deferred_definitions()` returns the full definition of each deferred tool: name, description, parameters
  schema and, for MCP tools, the server name. `MCPToolWrapper` does not keep the server name today (it only builds
  `mcp_{server}_{tool}` from it in `new`, `mcp/mod.rs:571`), so it gains a `server_name` field; parsing it back out
  of the prefix is ambiguous because server names may contain `_`. `tool_search` needs all of it because D3 ranks on schema properties; `/tools`
  and the websocket tool list use only name and description.

### D3. `tool_search` tool

New `src/agent/tools/tool_search.rs`, input `{ query: string, limit?: integer }`.

- Ranking: BM25 in a separate pure module (`tool_ranker.rs`), no new dependency. Port pi's tokenizer: lowercase,
  split camelCase and non-alphanumerics, small stop-word list, naive plural stemming. Document text is name, name
  with `_` as spaces, description, schema property names and descriptions (recursive), server name. A query equal to
  a tool name, or to its suffix after the server prefix, ranks that tool first.
- Behaviour differences from pi, each deliberate:
  - Searches all deferred tools, loaded or not, and marks already loaded ones. Reason: crash recovery may re-run the
    call (see D5); with pi's "only inactive tools" rule the replay would answer "No matching tools" for tools that
    are in fact loaded. The same query now always gives the same answer, so the tool can be `replay_safe`.
  - `limit` defaults to 8 and is capped (20). A broad query must not reload the whole catalogue.
- Result text: `Loaded N tool(s). They are available from your next call:` then `- name: first description line`.
- Static description that does not list tools, so the `tools` array does not change when servers connect.
- The search corpus is the set of tools each server listed when it connected. rust-bot does not handle MCP
  `notifications/tools/list_changed` today (no match in `src/`), so a tool a server adds later is not searchable,
  and a removed tool stays in the corpus until reconnect. This is the existing behaviour for direct MCP tools too;
  see open question 4.
- Sequential, not concurrency-safe (default), so two calls in one turn load in a deterministic order.
- Built per turn in `run_agent_loop` (`agent_loop.rs:1276`, right after the view is built):
  `run_tools.register(Box::new(ToolSearchTool::new(run_tools.clone())))`, and only when the view contains at least
  one deferred tool. One place covers user, system and resumed turns.
- No `ToolSearchConfig` enable flag, a deliberate exception to the usual config-gated tool convention: the
  `exposure` / `deferredTools` settings already gate it, since it exists only when something is deferred. It never
  blocks on user input, so there is no Herdr wiring either. With nothing deferred (the default) nothing changes, including the
  tools array.

### D4. Finding things the model does not know exist

The model cannot search for a tool it has never heard of. Two cheap aids:

- `tool_search` description says MCP and other optional tools are reached through it.
- A prompt section (via the existing `with_prompt_section`) listing deferred server names and one line each, not
  tool names. It changes only when a server connects, so it is stable for the cache.
- The one line comes from a new optional `McpServerConfig.description` (decided). It tells the model what the
  server offers, for example "Jira: search, create and update issues and sprints". Without it the line is the
  server name alone. The server's own `instructions` from the MCP handshake are not used: that text is controlled by
  the server, can be long, and would be a prompt-injection path into the system prompt. The field is useful for
  direct servers too, but this plan only uses it here.
- Only servers that are connected are listed. `connect_mcp` retries failed servers, so a server can join the
  section later in the session (one cache write when it does). Listing a server that never connected would send
  the model searching for tools that do not exist.
- The same section lists deferred built-ins (`tools.deferredTools`) by name with the first line of their
  description. A built-in has no server, so without this line the model has no hint it exists (for example `docx`
  when deferred). There are few of them and the list changes only with config, so the cache cost is the same as
  the server lines. Example:

  ```
  Optional tools, not loaded yet. If a request needs a capability you do not have,
  call tool_search before saying you cannot do it:
  - Servers: jira (Jira: search, create and update issues and sprints), github (repos, PRs)
  - Tools: docx (convert Word documents to PDF and other formats)
  ```

  The header line is an instruction, not only a label: it tells the model when to search, which is the main
  mitigation for the "Model does not search" risk. How loading works stays in the `tool_search` description.

  This guidance lives here, not in `TOOLS.md`. `TOOLS.md` is copied into each workspace once and then edited by
  users, so template changes do not reach existing installs. It is static and cannot know what is deferred, it is
  only loaded in standard mode (`modes.rs:18`), and it would cost tokens even when nothing is deferred.

  Only names in the session's mode view are listed, matching what `tool_search` can find. The section is omitted
  when the view has nothing deferred.

### D5. Persistence and crash recovery

Store the loaded names in `session.metadata["loaded_tools"]` (new key in `src/session/keys.rs`), the same mechanism as
the session mode override.

Why not derive it from history like pi's earlier `splitDeferredTools`: the consolidator archives old messages out of
the session, so the `tool_search` result that justified a load can disappear while later messages still call that
tool. Metadata does not have that problem.

- Seed the activation handle from metadata at the start of a turn (in `tools_for_session`, D2). A name is seeded
  only if it is registered, deferred, and in the session's mode view. Other names are ignored, not deleted:
  - hidden by the mode (for example `no_mcp`): switching modes back restores them;
  - not registered (server removed from config, or not connected yet): restored when it connects;
  - now direct (config changed): already declared, so seeding it would declare it twice in `get_definitions`.
- Save it the moment `tool_search` loads something new: `ToolSearchTool::on_load` calls
  `AgentLoop::record_session_loaded_tools`, which adds the names to the live session and saves it. (The plan first
  said "at each `tools_completed` checkpoint", but with a journal, the default, the runner writes checkpoints to the
  journal and never calls the metadata callback.) A crash after a load therefore cannot lose it, and the session
  file is only rewritten when a load actually happens.
- At turn end, `persist_finished_turn` saves the snapshot taken when the turn started. It first copies
  `loaded_tools` over from the live session (`carry_loaded_tools_from_live_session`), so the save does not erase the
  turn's loads. Merging is a union: names the turn could not see stay.
- Crash replay: `replay_interrupted_calls` looks tools up in the process-wide registry, where the per-turn
  `tool_search` does not exist, so an interrupted `tool_search` call is not replayed; the model gets the usual
  "interrupted" result and can search again. Its loads were already saved by `on_load`.
- Clear it wherever session history is cleared: `Session::clear`, used by `reset_session` (`/new`, websocket reset)
  and idle compaction, since the history that used the tools is gone.
- The set only grows within a session. There is no unload, which also avoids history containing calls to tools that
  are no longer declared.

### D6. Prompt cache cost

Order on the wire is tools, system, messages. A load appends to the tail of `tools`, so the system and message
breakpoints miss once on the next request; the built-in tools prefix stays cached because the first marker sits on
the last built-in. Later requests cache normally. Expected cost: one cache write per `tool_search` load, against
re-sending all MCP schemas on every request today.

#### Cache boundary hint (decided)

Problem: `tool_cache_marker_indices` (`providers/base.rs:733`) puts the first tool marker on the last tool whose name
does not start with `mcp_`. A loaded deferred built-in (for example `docx`) sits at the tail, so it becomes the
"last non-MCP tool": the boundary marker merges with the tail marker and the stable built-in prefix loses its
breakpoint. A later MCP load leaves the boundary on `docx`, inside the growing part.

Decision: the registry tells the provider where the boundary is, instead of the provider guessing from names.

- `get_definitions()` sets `"cache_boundary": true` at the top level of the last direct built-in definition (the
  same level as `cache_control`, outside `function`). This keeps today's meaning: built-ins are stable, direct MCP
  tools arrive lazily after them. It is set only when the registry has deferred tools, so the definitions of a
  setup without deferral are byte-for-byte what they were.
- `tool_cache_marker_indices` uses the flagged index as the boundary when present and falls back to the `mcp_` rule
  otherwise, so callers that do not set it (subagents, existing tests) are unchanged.
- The flag is internal and must never reach an API:
  - Anthropic: `convert_tools` (`anthropic_provider.rs:206`) whitelists fields and runs before
    `apply_cache_control`, so it copies `cache_boundary` across like `cache_control`; `apply_cache_control` removes
    it after placing the markers.
  - OpenAI-compatible: tools are sent verbatim (`openai_compat_provider.rs:609-613`), so the flag is removed before
    `request["tools"]` is set. That path's own `cache_control.rs` marks only the tail and ignores the flag.
  - One shared helper, `strip_internal_tool_fields`, in `cache_control.rs` for both.
- Why not a new `chat()` parameter: it would change `LLMProviderDyn` and `LLMProvider` (`chat`, `safe_chat`,
  `chat_with_retry`, `chat_stream_with_retry_boxed` and the streaming variants) and about 20 test providers, for a
  value only the Anthropic path reads.

### D7. Token accounting

`snip_history` and the consolidator (`memory.rs:769`, through `ContextBuilder::get_definitions`) estimate prompt size
from `get_definitions()`. After D2 that is automatically the declared set, which is what the model actually pays for.
Check that the consolidator's registry is the full process registry: it must count only direct plus loaded tools.

## Milestones

Each compiles green and is shippable on its own. Default behaviour is unchanged until a server or tool is configured
as deferred.

### M1. Core, in memory

Status (2026-10-28): implemented, unit tests green; baseline and deferred measurement with `ouryagya-strapi` not
run yet. Loads last for one turn until M2 persists them.

First manual test (Xiaomi model, `ouryagya-strapi` deferred, no description): asked for "the latest news from the
strapi server", the model grepped the workspace and asked for a URL instead of searching; only when told to search
for tools did it call `tool_search`. It then needed 7 searches, loading about 50 tools, before
`mcp_ouryagya-strapi_list_new` surfaced (found by an exact-name query). Consequences:

- The D4 prompt section was moved from M3 into M1 and is implemented (`deferred_tools_prompt_section` in
  `tool_search.rs`). `PromptSection` closures now receive the session's `AgentMode`, so `no_mcp` lists no MCP
  server. Format: one line per server, ``- MCP server `name` (N tools): description``, then one line per deferred
  built-in.
- Search noise is open: see "Ranking on CRUD-style servers" under Risks.

Measurement (2026-10-28, Xiaomi model via the OpenAI-compatible provider, `ouryagya-strapi`, similar "list the 10
latest X" prompts, from `token_usage` in session metadata):

| Exposure | Model requests | Input | Cache read | Prompt tokens per request |
|---|---|---|---|---|
| `direct` | 6 | 406,517 | 2,439,680 | about 474,000 |
| `deferred` | 8 | 40,620 | 74,240 | about 14,000 |

About 33 times fewer prompt tokens per request. The prompts differed (avyakt signals vs articles), so this is
indicative, not a controlled comparison. With `direct`, each request is larger than a 200k-token context window,
so the server is only usable at all with very large-context models. The deferred session's first search was
`"list_article"`, an exact-name query following the example names, and loaded the right tool at once.

- Baseline first: the `ouryagya-strapi` server (SSE, more than 100 tools, `enabledTools: ["*"]`), one session with
  `exposure: direct`, recording input tokens and `cache_creation` / `cache_read` per request from the usage logging.
  Use the same prompts again with `deferred` after M1. Open question 3 depends on these numbers.
- `ToolExposure` and `Tool::exposure` (base.rs); deferred-name set and `exposure_of` on the registry (D1).
- Registry activation handle, `get_definitions` ordering, `prepare_call` rejection, `deferred_definitions` (registry.rs).
- `tool_ranker.rs` (BM25) and `tool_search.rs`.
- `McpServerConfig.exposure` and `McpServerConfig.description`, `ToolsConfig.deferred_tools`, `MCPToolWrapper`
  exposure; `deny_unknown_fields` and schema/serde defaults checked in `config/schema.rs`; config validation (D1).
- Per-session activation handle created in `tools_for_session` (D2).
- Wire into `run_agent_loop`: register `ToolSearchTool` per turn when deferred tools exist.
- Cache boundary hint (D6): flag in `get_definitions`, `tool_cache_marker_indices` reads it, both providers strip it.

### M2. Persistence and lifecycle

Status (2026-10-28): implemented as described in D5, unit tests green, not yet tried on a real installation.

- `loaded_tools` metadata key, seeded in `tools_for_session`, saved by `tool_search` on each new load, carried over
  at turn end.
- Reset with the history (`Session::clear`); `AgentMode` filtering respected (D5).
- Crash replay of `tool_search` checked: not replayed (per-turn tool), harmless because loads are saved at once.

### M3. Surfaces

- `/tools` and the websocket tool list (`visible_tool_summaries`, `runtime.rs:2612`) show deferred tools and whether
  they are loaded. Today both list every registered tool as `{name, description}` (`ToolRegistry::tool_summaries`,
  `registry.rs:173`; `CmdTools`, `command/builtin.rs:923`). Deferred tools stay registered (D1), so without this
  change every deferred tool would appear as if the model could call it.
  - **Status per tool.** `ToolSummary` gains `exposure: direct | deferred | loaded`. One registry function,
    `tool_summaries_with_status(loaded_names)`, builds it for both surfaces.
  - **Source of "loaded".** `session.metadata["loaded_tools"]` (D5), filtered to the mode view. Not the activation
    handle: that belongs to a per-turn view, and the websocket path reads the process-wide registry, where a handle
    would mix loaded tools across chats. Both callers already have the `Session`.
  - **Frontend** (`websockets-chat`). `ToolsListWire` (`protocol.rs:2065`) decodes `exposure`, defaulting to
    `direct` so an older server still works. The tools popup shows direct and loaded tools as today, with a "loaded"
    badge on loaded ones, then unloaded deferred tools in a collapsed "Available on demand (N)" group. The header
    count is the tools the model can call now (direct plus loaded).
  - **`/tools`.** Same split in text: the callable list first, then an "Available on demand" list of deferred
    names.
  - **Refresh.** The popup requests the list when opened (`app.rs:1749`) and nothing is pushed. A tool loaded
    mid-turn shows up on the next open after `loaded_tools` is saved at `tools_completed` (D5). Accepted; a push
    event is not needed for M3.
- Prompt section for deferred servers and deferred built-ins (D4): done in M1, see the M1 status.
- `tool_search` is registered per turn, so the tools popup and `/tools` (which read the process-wide registry) do
  not list it. Show it there when anything is deferred.
- Settings schema and `mcp_presets_api.rs` expose `exposure` and `description`; README and `docs/` section.

### M4. Optional spike: native tool changes on Anthropic

Only if M1 measurements show the per-load cache miss matters. First confirm the provider feature exists and is
available to our keys; then it needs a transcript system-message type and an Anthropic provider change, which is
larger than M1 to M3 combined. Not committed to here.

## Tests

Unit tests next to the code, as in the rest of the repo.

- Ranker: tokenizer (camelCase, underscores, plurals, stop words); ranking order; exact-name query wins over a
  shorter description; ties keep corpus order (A-Z, since the registry map has no registration order); empty query, zero limit, empty corpus; a query matching only a
  schema property name still finds the tool.
- Registry: deferred tool absent from `get_definitions` until loaded; loaded tools appended in load order after
  direct MCP; `prepare_call` on an unloaded deferred tool errors without listing deferred names; `restrict_by` keeps
  the turn's activation handle; default tools unaffected.
- `ToolSearchTool`: loads matches, marks already loaded, caps limit, rejects empty query and non-positive limit,
  idempotent for the same query, not registered when nothing is deferred.
- Mode filtering: under `no_mcp`, `tool_search` is not registered when the mode view leaves nothing deferred, and
  otherwise returns no MCP matches even for an exact MCP tool name; a call to an MCP tool name already in
  `loaded_tools` fails as an unknown tool, not with "Call tool_search first".
- Runner: a run where iteration 1 calls `tool_search` and iteration 2 declares and calls the loaded tool (fake
  provider recording the `tools` it receives per request).
- Cache boundary (M1):
  - `get_definitions` flags only the last direct built-in, and nothing when there are no built-ins or nothing is
    deferred.
  - `tool_cache_marker_indices` with `[read, write*, mcp_a, docx]` (`*` = flagged) gives `[1, 3]`, not `[3]`.
  - Without a flag the existing `mcp_` cases still pass.
  - Anthropic and OpenAI-compatible request bodies contain no `cache_boundary` key.
- Activation scope (M1): two sessions built from the same registry do not see each other's loads.
- Subagents (M1): with `docx` in `tools.deferredTools`, the subagent registry declares `docx` directly.
- Config validation (M1): deferring `tool_search` is rejected; an unknown deferred name and a name both disabled
  and deferred produce a warning, and disabled wins.
- Persistence (M2): seed, save, ignore names outside the mode view, reset on `/new`. Seeding skips a name that is
  not registered and a name that is now direct (no duplicate in `get_definitions`). End to end: a tool loaded in one
  turn is callable without a new search in the next (`a_tool_loaded_in_one_turn_can_be_called_in_the_next`).
- Prompt section (M3): lists deferred servers and deferred built-ins; leaves out names outside the mode view;
  absent when nothing is deferred; starts with the "call tool_search before saying you cannot" instruction; uses
  `description` when set and the bare server name otherwise; leaves out a
  deferred server that is not connected.
- Tool list (M3):
  - `tool_summaries_with_status` marks direct, deferred and loaded tools correctly.
  - A name in `loaded_tools` that the mode hides (`no_mcp`) is not listed.
  - A name in `loaded_tools` that is no longer registered is ignored.
  - `visible_tool_summaries` reads `loaded_tools` from the session it is given, not from another chat.
  - `/tools` output separates callable and on-demand tools.
  - Frontend: `ToolsListWire` decodes `exposure` and defaults it to `direct` when missing; the popup count leaves
    out unloaded deferred tools.

## Risks

- **Model does not search.** Weaker models may never call `tool_search`. Mitigation: the D4 prompt section, whose
  header tells the model to search before saying it cannot do something; keep frequently used servers `direct`.
- **Load happens mid-turn.** A tool becomes callable one iteration later than its load. The result text says so.
- **Cache miss per load.** Accepted for M1, quantified through the existing usage logging (`cache_creation` tokens).
- **Config surface.** `exposure` and `description` need to appear in the settings schema and presets API or users
  will hand-edit JSON.
- **Ranking on CRUD-style servers.** Strapi exposes about 100 tools named `{verb}_{collection}` with near-identical
  descriptions ("Content-manager list for api::new.new.") and large, similar schemas. BM25 then ranks
  `list_news-occasion` above `list_new` for "list news", and each search loads 8 tools, most of them noise.
  Options: a relative score cut-off (load only matches within X% of the best), a lower default limit, a smaller
  length penalty (`b`), or a few example tool names per server in the D4 section so the model can query by name.
  Done (2026-10-28): the cut-off (`LOAD_SCORE_RATIO` 0.5; exact-name matches always load; weaker matches are
  listed as "not loaded") and up to 5 example names per server, one per leading word (`create_x`, `list_x`, ...).
  Limit and `b` unchanged until the next manual test shows whether they still matter.
- **Loaded set only grows.** Nothing is unloaded (D5), so a long session that searches widely drifts back toward
  declaring every schema, the cost this feature removes. Remedy for now: `/new`. If the M1 numbers show long
  sessions reaching that point, add a cap on loaded tools; dropping the oldest load would cost one cache write.

## Open questions

1. ~~Per-server `exposure` only, or also an automatic rule?~~ Decided (2026-10-28): per-server only, no automatic
   rule for now. The user chooses in config, so the docs must show how (see "User configuration" under D1).
2. Should deferred apply to subagents? Today they get no MCP tools, and D1 makes them ignore
   `tools.deferredTools`, so they get every built-in directly. Recommendation: keep it that way.
3. Is M4 worth pursuing after M1 numbers are in? Recommendation after the measurement: no. A load costs one cache
   write of a ~14k-token prompt; the saving is about 460k tokens on every request.
4. Should rust-bot handle `notifications/tools/list_changed`? For deferred servers it would be cheap: re-register the
   server's tools and the corpus follows, with no change to the `tools` array. A loaded tool the server removes
   would need the same treatment as a name outside the mode view (D5: skipped, not deleted). Recommendation: out of
   scope here; it applies to direct servers as well and belongs in its own change.
