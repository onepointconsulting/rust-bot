---
name: ACP dynamic agent spawning
overview: Let rust-bot create (assemble config for), launch, and re-launch child rust-bot agents over the Agent Client Protocol (ACP v1, JSON-RPC 2.0 over stdio) — e.g. a "code review rust-bot agent" that did not exist prior to the call. A child is a full rust-bot instance with its own workspace (own SOUL.md / AGENTS.md / USER.md, own memory/ and sessions/) whose config is the parent's config plus a JSON merge-patch overlay, so it inherits everything by default and can be tweaked later. Every rust-bot is dual-role: ACP Agent (driven by a parent rust-bot or any third-party ACP client such as Zed) and ACP Client (drives child rust-bots or any ACP-speaking engine such as Claude Code / Gemini CLI). MCP is supported in both directions: MCP client (existing tools.mcpServers + ACP-provided mcpServers) and MCP server (new `rust-bot mcp` stdio subcommand).
todos:
  - id: types
    content: src/agent/acp/types.rs — thin adapter over the official agent-client-protocol crate (pinned 2.2, protocol v1 only, no unstable features — decision 18); only src/agent/acp/ touches crate types, the rest of rust-bot sees rust-bot's own types. First task is a spike that settles how the crate runs next to rust-bot's non-Send provider futures (main runtime vs dedicated thread + LocalSet) and updates or retires docs/acp/acp_peer.rs. Messages used — initialize (protocolVersion, clientCapabilities, agentCapabilities incl. loadSession / mcpCapabilities, authMethods), authenticate, session/new (cwd, mcpServers), session/load, session/prompt, session/cancel, session/update notifications, session/request_permission, fs/read_text_file, fs/write_text_file, StopReason, content blocks; session/resume only if the pinned crate exposes it as stable
    status: pending
  - id: child-store
    content: src/agent/acp/store.rs — ChildAgentStore — child workspace layout under <parent-workspace>/acp/agents/<name>/ (overlay.json, SOUL.md, AGENTS.md, USER.md, TOOLS.md, memory/, sessions/, agent.json metadata incl. preset, createdBy, depth; children discovered at startup by scanning acp/agents/*/agent.json — the directory is the registry, no separate index file) + SessionRegistry (<parent-workspace>/acp/sessions.json keyed by (agentName, parentSessionKey) → {acpSessionId, capabilitiesSeen, lastUsed})
    status: pending
  - id: config-overlay
    content: config/overlay.rs — validate_overlay against a code-level allowlist (free / narrow-only rules, effective-value comparison, unknown paths rejected) + load_config_with_overlay(parent, overlay) applying an RFC 7386 JSON merge patch before validation; overlay always forces agents.workspace = child dir; secrets stay in the parent config (never copied); order is raw parent → validate + merge overlay → resolve_config_env_vars → garde validation on the merged result (decision 19)
    status: pending
  - id: acp-cli
    content: cli/commands.rs — new `Acp(AcpArgs)` subcommand (`rust-bot acp [--config <path>] [--overlay <path>]`) — headless ACP Agent role on stdio, no channels, no ports; stdout reserved for JSON-RPC (protocol-mode logging to stderr/file + OS-level redirect of stdout to stderr, protocol writer owns a duplicate of the real stdout — decision 11); works with a plain config (no overlay) so third-party ACP clients can launch rust-bot directly
    status: pending
  - id: agent-role
    content: src/agent/acp/agent_mode.rs — ACP Agent role — initialize (advertise loadSession=true, mcpCapabilities per MCP transports supported, authMethods=[]), authenticate no-op, session/new → SessionManager key acp:<sessionId> persisted in the child workspace, connect ACP-provided mcpServers for that session, session/prompt → one AgentLoop.process_direct turn streaming session/update (text/thought chunks via callbacks, tool_call / tool_call_update via AcpSessionHook — decision 16), session/load replays history, session/cancel → abort the turn's spawned task (decision 15); outbound session/request_permission for gated tools; optional fs/* delegation to the client only when clientCapabilities.fs advertises it
    status: pending
  - id: transport
    content: src/agent/acp/transport.rs — child-process transport (client side) — spawn argv array (resolved via shared utils::process::resolve_program — which + PATHEXT, .cmd spawned directly, never cmd /C or a shell string; MCP stdio client switched to the same helper), newline-delimited UTF-8 JSON pumps, stderr ring buffer, request/response correlation by id, notification channel, Windows tree-kill (shared kill_process_tree helper — decision 15), child env built from the merged raw config's ${VAR} references + base set + RUST_BOT_ACP_DEPTH (decision 19)
    status: pending
  - id: client
    content: src/agent/acp/client.rs — AcpClient — initialize → read agentCapabilities → session/new | load | resume (capability-gated, spec MUST NOT) → session/prompt → aggregate session/update into final reply + activity tail; discard replayed updates emitted during session/load; answer agent→client requests (session/request_permission, fs/*) from policy; turn timeout + session/cancel
    status: pending
  - id: acp-config
    content: AcpConfig in config/schema.rs under tools.acp (enabled, maxDepth, allowDynamicAgents, launchPresets, defaultTimeoutSecs, keepAliveSecs, shutdownGraceSecs, permissionPolicy, sessionScope) with serde aliases + garde validation; inherited by children via the overlay mechanism so children are ACP clients too
    status: pending
  - id: tools
    content: src/agent/tools/acp.rs — acp_create_agent (assemble child workspace — copy parent bootstrap files then apply soul/agents/user overrides, write overlay.json; fails if name exists), acp_update_agent (tweak overlay / bootstrap files later; resyncFromParent re-copies listed bootstrap files and re-applies stored overrides), acp_run_agent (launch-or-reuse → prompt → final reply as tool result; throttled progress via new task-local ToolProgress; cancel-on-drop guard — decision 22), acp_list_agents (incl. bootstrap drift vs parent); register in tools/mod.rs, gate via tools_for_session on tools.acp.enabled and depth < maxDepth
    status: pending
  - id: context
    content: agent/context.rs — inject a short "Available ACP agents" section (name + purpose line from agent.json) into the supervisor system prompt so follow-up questions route to the existing agent instead of re-creating one
    status: pending
  - id: concurrency
    content: Per-child single-flight across processes — child takes an exclusive std::fs::File::lock on <child home>/.acp.lock at startup and waits for it before answering initialize; AcpManager keeps an in-process async mutex held until the child exits; sessions.json updates guarded by their own lock file; new utils::fs::write_atomic (temp + fsync + rename) used by save_config and sessions.json so children never read a half-written parent config
    status: pending
  - id: permissions
    content: Permission policy on both roles, fail-closed — child tool policy lives in its overlay (e.g. code reviewer — no shell, no writes); child never installs CliAskHook, AcpSessionHook asks via session/request_permission for edit/execute/fetch/other kinds (all kinds when confirmBeforeExecute) and denies on error/cancel/timeout; supervisor answers from tools.acp.permissionPolicy (auto-approve-read default, escalate via ToolApprovalBroker on the parent turn's channel — deny if it cannot ask, allow-all opt-in); rust-bot mcp denies gated calls when confirmBeforeExecute is on
    status: pending
  - id: lifecycle
    content: Process lifecycle — default stop-after-turn via graceful shutdown (parent closes child stdin → child drains background tasks, runs Dream, exits), with session/load (or session/resume) on relaunch; tree-kill only after shutdownGraceSecs or turn timeout; optional keep-alive pool (keepAliveSecs) whose idle reaper uses the same graceful path
    status: pending
  - id: mcp-server
    content: src/mcp_server/ + `Mcp(McpArgs)` subcommand — `rust-bot mcp [--config] [--overlay]` stdio MCP server built on rmcp ServerHandler + tool macros (enable rmcp server/macros/schemars in production deps; template tests/agent/mcp_dummy_server.rs — decision 21) (initialize, tools/list, tools/call) exposing rust_bot_chat {message, sessionKey?} backed by process_direct, plus acp_list_agents / acp_run_agent when tools.acp.enabled
    status: pending
  - id: tests
    content: Integration tests — mock ACP agent binary for client-side negatives (capability gating, permission deny, tree-kill); real `rust-bot acp` with mock provider for prompt/load/replay; overlay merge tests; end-to-end 9-step scenario test; MCP server tools/list + tools/call test
    status: pending
isProject: false
---

# ACP dynamic agent spawning

Enable the scenario: rust-bot **creates** an agent for a purpose (assembling a child workspace + config overlay — never synthesizing code), **launches** it over ACP, relays its work to the human operator, and on every later question **re-launches it with its own memory**.

## Requirements traceability

| Requirement | Guaranteed by |
|---|---|
| A rust-bot can create another rust-bot | `acp_create_agent` assembles `<ws>/acp/agents/<name>/`; default preset `rustbot` launches `rust-bot acp --config <parent> --overlay <child>/overlay.json` |
| The created rust-bot speaks ACP (Agent to its parent, Client to its own children) | Every rust-bot has both roles: `rust-bot acp` = Agent role; `tools.acp` is inherited through the overlay, so a child can create/run grandchildren (bounded by `maxDepth`) |
| Child inherits parent config, tweakable later | Parent config + RFC 7386 merge-patch overlay, applied at every child start (live inheritance; secrets never copied); `acp_update_agent` edits the overlay later |
| Parent may tweak SOUL.md / AGENTS.md / USER.md | Parent's bootstrap files copied into the child workspace at creation, then `soul` / `agents` / `user` overrides (replace or append) applied; editable later via `acp_update_agent` |
| Talk to other rust-bot instances and to other ACP clients/agents | Client role: any ACP agent via launch presets (rust-bot, Claude Code, Gemini CLI). Agent role: spec-complete enough (`authMethods: []`, `session/new` with `cwd` + `mcpServers`, `session/cancel`) for third-party ACP clients (Zed, JetBrains, …) to launch `rust-bot acp` directly |
| MCP as client and as agent | Client: existing `tools.mcpServers` (inherited by children) + per-session `mcpServers` passed in ACP `session/new`. Server: new `rust-bot mcp` stdio subcommand |
| Child memory kept separately over time | Child has its own workspace, so its own `memory/MEMORY.md`, history, dream edits, and `sessions/`. A child's file tools can never read the parent's or other children's memory and sessions (its project scope may not be inside a rust-bot home, and homes inside it are denied subtrees, decision 1; shell needs to be disabled for the same guarantee); the parent, as supervisor, can read its children's files |

## Terminology

In ACP, the process that is *launched* plays the **Agent** role and the launcher plays the **Client** role. So "the created rust-bot is an ACP client" is satisfied as: the child is an ACP **Agent** towards its parent, **and** (because it inherits `tools.acp`) an ACP **Client** towards any agents it creates itself. Both roles live in the same binary.

## Design decisions

1. **"Create" = assemble a workspace + overlay, never compile.** `acp_create_agent {name, purpose, preset?, soul?, agents?, user?, overlay?, cwd?}` creates:

   ```
   <parent-workspace>/acp/agents/code-review/
     agent.json      — {name, purpose, preset, createdAt, createdBy, depth, cwd?, overrides: {soul?, agents?, user?}, resyncedAt?}
     overlay.json    — JSON merge patch over the parent config
     SOUL.md         — parent copy, then override applied
     AGENTS.md       — parent copy, then override applied (e.g. "You are a code review specialist…")
     USER.md         — parent copy, then override applied
     TOOLS.md        — parent copy
     memory/         — empty; the child's own long-term memory
     sessions/       — the child's own session history
   ```

   **Bootstrap files are a snapshot, by design — unlike the config.** Config is inherited live (decision 2), but `SOUL.md`, `AGENTS.md`, `USER.md` and `TOOLS.md` are copied once at creation and then belong to the child. Live inheritance doesn't fit them: `MemoryStore` manages `SOUL.md` and `USER.md` in the workspace (`memory.rs:67-79`), so the child's Dream edits its own copies over time; reading them live from the parent would either lose the child's changes or overwrite the parent's.
   - **Overrides are persisted:** the `soul` / `agents` / `user` overrides given to `acp_create_agent` are stored in `agent.json` (`overrides`), so they can be re-applied later instead of being lost after creation.
   - **Explicit refresh:** `acp_update_agent {resyncFromParent: ["USER.md", …]}` re-copies the listed files from the parent, then re-applies the child's stored overrides for those files (same order as at creation), and records `resyncedAt`. Typical use: `USER.md` after the parent has learned more about the operator. The child's own Dream edits to a resynced file are replaced — that is the point of a resync, and the tool result says so.
   - **Drift is visible:** `acp_list_agents` reports, per child, whether its `USER.md` / `TOOLS.md` is older than the parent's (file mtime vs `resyncedAt` / `createdAt`), so the LLM or the operator can decide to resync.
   - `TOOLS.md` uses the same snapshot scheme for simplicity; it rarely changes.

   The LLM supplies a preset *key* — never a raw command. `acp_create_agent` fails if `name` already exists (so step 6 cannot silently re-create and wipe memory); changes go through `acp_update_agent`.

   **Agent home ≠ project directory.** The child directory above is the child's *home* (`agents.workspace`): memory, sessions and bootstrap files only. It is never the folder the child works on. The project the child works on is chosen per ACP session and reuses rust-bot's existing per-session project scope (`WorkspaceScope { project_path, access_mode }` in `src/security/workspace_access.rs`, bound per turn by `src/agent/workspace_context.rs`):
   - `session/new.cwd` becomes the child session's `WorkspaceScope.project_path` with `access_mode: restricted`, persisted in the session metadata, so `session/load` restores it. File tools resolve against and are confined to that folder via `current_tool_workspace()`; `MemoryStore` / `SessionManager` keep using the home.
   - The working directory is set per session, not per agent: the same child can work on different projects in different sessions. `acp_create_agent {cwd?}` stores an optional default `cwd` in `agent.json`, used when `acp_run_agent` does not pass one.
   - Narrowing rule: when the parent session is restricted, the `cwd` must resolve inside the parent's own current `project_path`; when the parent has full access, it may pass any existing absolute directory. Validation reuses `validate_workspace_scope_payload` (absolute, exists). A child can therefore never reach further than its parent.
   - **A child's project may never contain a rust-bot home.** Without a session scope, the project folder defaults to the workspace itself (`WorkspaceScopeResolver::default`, `workspace_access.rs:247`) — with the default setup `~/.rust-bot/workspace`. Passing that as the child's `cwd` would let the child's file tools read and edit the parent's `memory/MEMORY.md`, the parent's `sessions/` (other operators' conversations) and every sibling's home under `acp/agents/`. So:
     - **Rejected:** a `cwd` that **is, or lies inside,** a rust-bot home — the parent's workspace, any child's home (`acp/agents/<name>/`), or any directory recognised as a rust-bot workspace (has `memory/` + `sessions/`, or `acp/agents/`).
     - **Allowed, with the home blocked:** a `cwd` that **contains** a home. The onboarding default puts the workspace inside the project folder (`--workspace ./.rust-bot/workspace`, `commands.rs:242`), so rejecting such folders would block a common layout, including reviewing a repo that holds its own workspace. Instead, every rust-bot home found under the `cwd` is added to the child session's scope as a **denied subtree**: the file tools and the MCP `file://` resolver (`mcp_file_ref_resolver`, `agent_loop.rs:871-882`) refuse paths inside it. This needs a new `denied_roots: Vec<PathBuf>` on `ToolWorkspace` (`workspace_access.rs:205-219`), checked by the shared path resolution in the file tools.
     - **Limit:** shell is not path-confined without the exec sandbox, so a child *with* shell could still read a denied subtree. Children that must not see the parent's memory while working in a folder containing it should have shell disabled (as the reviewer overlay does); the tool result of `acp_run_agent` warns when a denied subtree exists and shell is enabled.
   - **No silent fallback to the parent's home:** if the parent has no separate project folder (its project defaults to its workspace) and `acp_run_agent` passes no `cwd` (and `agent.json` has no default), the call fails with a message asking for a project folder — e.g. the repository to review.
   - (The parent's `GitStore` never commits children's files: its `.gitignore` starts with `/*` and only re-includes its own tracked files, `gitstore.rs:415-435`.)
   - `restricted` still permits writes inside the project, and shell is not path-confined without the exec sandbox. Read-only children (e.g. a code reviewer) must also disable `write_file` / `edit_file` / shell in their overlay.

2. **Config inheritance is live, via overlay.** The child starts with `rust-bot acp --config <parent config path> --overlay <child>/overlay.json`. `load_config_with_overlay` validates the overlay against the allowlist (below), deep-merges it onto the parent config (RFC 7386: objects merge, `null` deletes, arrays replace), then forces `agents.workspace = <child dir>`, then runs the normal validation. Consequences:
   - The child inherits provider, model, `mcpServers`, `tools.*`, `tools.acp` — including later edits to the parent config ("might be tweaked later on").
   - Secrets are read from the parent config at runtime; nothing is duplicated to disk.
   - Per-child tweaks (e.g. `{"agents": {"model": "…"}, "tools": {"exec": {"enable": false}}}`) live only in `overlay.json`.
   - Channels are never started in `acp` mode regardless of inherited config.

   **Overlay allowlist — a child can never have more power than its parent.** The overlay is written by the LLM (`acp_create_agent` / `acp_update_agent`) and is a plain file on disk, so it is untrusted input. Without a check, an overlay could send the inherited API key to any URL (`providers.*.apiBase` — `create_provider_for` takes key, base and headers from the same entry, `factory.rs:100-156`), run arbitrary commands (`tools.mcpServers` entries; MCP children inherit the full env, `mcp/mod.rs:350-363`), or add spawn commands (`tools.acp.launchPresets`). A denylist would silently allow every config key added in future, so the check is an **allowlist**: every config path an overlay may set, each with a rule. Anything not listed is rejected with an error naming the path.

   | Rule | Meaning | Paths (final names verified against `schema.rs` at implementation) |
   |---|---|---|
   | **free** | any value; cannot grant power | `agents.model`; model preset name (must exist in the parent's presets); `agents.provider` (must name a provider already configured in the parent — it then uses the parent's key and base, never a new URL) |
   | **narrow-only** | may only be stricter than the parent's effective value | `tools.exec.enable` (true→false), `tools.restrictToWorkspace` (false→true), `tools.disabledTools` (superset of the parent's list), `tools.acp.enabled` (true→false), `tools.acp.maxDepth` (≤ parent), `tools.acp.allowDynamicAgents` (true→false), `tools.mcpServers.<name>` (`null` = drop an inherited server), `tools.mcpServers.<name>.enabledTools` (subset of the parent's) |
   | *(not listed)* | rejected | `providers.*` (apiBase, apiKey, extraHeaders), new `tools.mcpServers` entries or any of their command/args/env/url/headers, `tools.acp.launchPresets`, `channels.*`, cron, and everything else |

   - **Compare effective values, not patch text.** Narrow-only rules are checked on the merged value vs the parent's value. This also catches RFC 7386 `null` deletions: `"restrictToWorkspace": null` falls back to the default `false` and is rejected when the parent is `true`; `"disabledTools": null` drops the parent's list and is rejected.
   - **Unknown keys are rejected too.** A path missing from the config schema is not on the allowlist, so a typo such as `tools.exec.enabled` fails loudly instead of silently restricting nothing.
   - **Where it lives: in code, not in config.** A constant table in a new `src/config/overlay.rs`, next to `load_config_with_overlay`. Whatever holds the allowlist must not be editable by what it guards: in `overlay.json` the overlay could allow itself more, and in the parent `config.json` it would be one more setting that has to be kept safe. In code it is versioned, reviewed and unit-tested. (A parent-config extension such as `tools.acp.overlayExtraKeys` can be added later if needed; not in scope.)
   - **Enforced in two places, one function.** `validate_overlay(parent, overlay) -> Result<(), OverlayError>` is called by `acp_create_agent` / `acp_update_agent` (immediate, clear error for the LLM) and by `load_config_with_overlay` on every child start (the real guarantee — also catches a hand-edited or tampered `overlay.json`, which makes the child fail `initialize` with the error rather than start widened).
   - **Narrow-only keys combine as "strictest wins", not "overlay replaces".** Because inheritance is live, the parent may tighten a setting after the overlay was written (e.g. add a name to `disabledTools`, lower `maxDepth`). Plain RFC 7386 replacement would then let the older overlay value undo the parent's tightening. So for narrow-only paths the child's effective value is the stricter of parent and overlay: booleans AND/OR towards the safe side, `maxDepth` = min, `disabledTools` = union, `enabledTools` = intersection. A parent change can therefore only ever make its children stricter, never break or widen them. When the parent loosens, the child keeps its own stricter value. `validate_overlay` still rejects a widening overlay value when it is written, so the LLM gets feedback instead of a silently ignored setting.

3. **Memory isolation comes from the workspace, not from the ACP session.** `MemoryStore`, `SessionManager`, `ContextBuilder` and dream all key off the workspace path. A separate child workspace gives the child its own long-term memory that persists across ACP sessions, process restarts, and parent restarts (step 9). The ACP session (`session/load` / `session/resume`) only restores the *conversation* thread.

4. **Session scope.** `tools.acp.sessionScope`:
   - `perParentSession` (default): one ACP session per `(agentName, parentSessionKey)`, so two operators chatting with the parent do not see each other's threads in the child; long-term memory is still shared inside the child.
   - `shared`: one ACP session per agent.
   Worker side maps ACP `sessionId` → session key `acp:<sessionId>` in the child's `SessionManager`.

5. **Every rust-bot is dual-role, depth-bounded.** `tools.acp.maxDepth` (default 2) and env `RUST_BOT_ACP_DEPTH` (set by the transport on spawn, incremented per level). ACP tools are hidden from `tools_for_session` when `depth >= maxDepth`. A child's own children live under *its* workspace (`<child>/acp/agents/…`), so the tree is also visible on disk.

6. **Synchronous-first delivery (steps 3–4, 7–8).** `acp_run_agent` blocks on the turn (bounded by `timeoutSecs`) and returns the final reply + bounded activity tail as the tool result; the supervisor's AgentLoop then answers the operator via the normal channel reply path. While it blocks, child activity is shown to the operator as progress, and stopping the parent's turn stops the child (decision 22). Later: long-running turns announce via `MessageBus` (`SubagentManager::announce_result` idiom).

7. **Routing follow-ups to the same agent (steps 5–6, 9).** The supervisor system prompt gets an "Available ACP agents" section (name + purpose), so the LLM calls `acp_run_agent` for coding questions instead of answering itself or creating a duplicate. Optional later: a `/delegate <agent>` command that forwards every operator message in that chat to the agent without an LLM routing decision.

8. **Continuity is capability-gated, not assumed.** After each `initialize`: `loadSession` → `session/load`; else `sessionCapabilities.resume` → `session/resume` (only if the pinned crate exposes it as stable — decision 18; rust-bot children never depend on it); else `session/new` with a recap prepended. The client **discards `session/update` notifications emitted during `session/load` replay** so replayed history is not aggregated into the new reply. rust-bot workers always advertise `loadSession`.

9. **Correct direction for `fs/*` and permissions.** `fs/*` and `session/request_permission` are **Agent → Client** requests. The rust-bot worker uses its own local tools (confined by its overlay's tool policy) and only sends `fs/*` to the client when `clientCapabilities.fs` advertises it (useful when an editor client wants unsaved buffers). The supervisor answers these requests from `permissionPolicy`: read-only allowed, writes/terminal denied by default, `escalate` bridges to `ToolApprovalBroker`.

   **Permissions fail closed — never "allow because nobody can answer".** Today approval exists only as the global `tools.confirmBeforeExecute` switch, covering every tool call with no per-tool list (`commands.rs:650-654`). When it is on, the CLI installs `CliAskHook`, which logs a warning and **runs the tools anyway** when stdin is not a terminal (`confirm_tools.rs:102-107`); the WebSocket hook only asks for channel `"websocket"`, other channels run unconfirmed. In `rust-bot acp` stdin is the protocol pipe, never a terminal, so reusing the CLI setup would silently disable approval in every child. Therefore:
   - **Child: the ACP hook replaces the CLI hook.** `rust-bot acp` never installs `CliAskHook`. The `AcpSessionHook` (decision 16) sends `session/request_permission` from its `before_execute_tools` and returns `ToolHookDecision::DenyCalls` for refused calls. Every ACP client must support this request, so it is always available.
   - **Which calls ask** (tool kinds from decision 16): `read` and `search` never ask; `edit`, `execute`, `fetch` and `other` always ask (`other` includes MCP tools, whose side effects rust-bot cannot know). With `confirmBeforeExecute: true`, every call asks — the setting keeps its current meaning.
   - **Fail closed:** a client error, a `cancelled` outcome, or no answer within a timeout **denies** the call. The runner already injects a synthetic result for `DenyCalls`, so the model sees "denied by client" and the transcript stays well-formed.
   - **Parent: `tools.acp.permissionPolicy`:**
     - `auto-approve-read` (default) — approve `read` / `search`, deny everything else.
     - `escalate` — forward to the human through the existing `ToolApprovalBroker`, on the channel and chat the parent's own turn came from (`acp_run_agent` carries them), so the WebSocket UI or CLI prompt shows it. If that channel cannot ask (e.g. CLI without a terminal, or a channel without an approval hook), **deny**.
     - `allow-all` — explicit opt-in only, never the default.
   - **`rust-bot mcp`:** no way to ask the caller in this plan, so with `confirmBeforeExecute: true` gated calls are **denied** instead of run. With it off, tools run as today — that is the user's own setting.
   - **Follow-up, out of scope:** `CliAskHook`'s "no terminal → run anyway" in the normal CLI (e.g. rust-bot used from a script) is arguably the same bug. Changing it affects existing CLI use, so it is noted here rather than changed by this plan.

10. **Single-flight per child — across processes, not just in memory.** At most one live worker process per child home; concurrent prompts to the same child queue. Prevents two processes writing the same `sessions/` and `memory/` files, which are rewritten in place without locking. Different children run in parallel freely.
    - **Why an in-memory lock is not enough:** several processes can reach the same child — the gateway, `rust-bot mcp` (which exposes `acp_run_agent`), a `rust-bot agent` CLI session, a second gateway on the same workspace, or a third-party ACP client launching `rust-bot acp --overlay` directly. Each has its own `AcpManager`.
    - **Cross-process lock, owned by the child:** on startup, `rust-bot acp` takes an exclusive OS file lock on `<child home>/.acp.lock` using `std::fs::File::lock` (stable since Rust 1.89; toolchain is 1.97, so no new dependency). The process that writes the files is the one that holds the lock. The OS releases it when the process exits, including on a crash or tree-kill, so there are no stale locks. Without `--overlay` (plain `rust-bot acp` launched by Zed etc.), the same lock is taken on its own workspace.
    - **Queueing instead of failing:** if the lock is held, the child waits for it (blocking lock in `spawn_blocking`) *before* answering `initialize`. The parent's `initialize` simply takes longer, bounded by `timeoutSecs`; EOF on stdin while waiting makes the child exit without touching any files. The lock holder's pid and start time are written into the lock file for diagnostics only (never used to decide ownership).
    - **In-process fast path stays:** `AcpManager` keeps its per-child async mutex so one parent process doesn't spawn a second, waiting worker for a child it is already running. The in-process lock is released only after the child process has exited (decision 13).
    - **Parent-side registry:** `<parent-workspace>/acp/sessions.json` is read-modify-written by every parent process. Guard each update with an exclusive lock on `<parent-workspace>/acp/sessions.json.lock` and save it atomically (below).

    **Atomic config saves.** Children re-read the parent's `config.json` on every start (decision 2). Today `save_config` truncates and rewrites it in place (`loader.rs:85`, plain `File::create`), and it is also called at runtime by `/mcp-preset enable|disable` (`builtin.rs:504,528`). `load_config` panics on invalid JSON, so a child starting mid-write can crash. Fix:
    - Add one shared helper, `utils::fs::write_atomic(path, bytes)`: write to a temp file in the same directory, `sync_all`, then `std::fs::rename` over the target (atomic on the same volume; replaces the target on Windows too). Retry the rename a few times on transient Windows errors (antivirus or indexer holding the file).
    - `save_config` and the `sessions.json` registry use it. It is also the helper for the follow-up in decision 13 (atomic `MemoryStore` / `SessionManager` writes), so that later change is a call-site swap, not new code.
    - In `acp` mode, a config that fails to parse is reported as a JSON-RPC error on `initialize` and a non-zero exit, not a panic.

11. **stdout is the protocol.** In `acp` (and `mcp`) mode, a stray byte on stdout corrupts the JSON-RPC stream. Today stdout is polluted in three ways: logging defaults to stdout (`init_runtime_logging`, `log.rs:60-64, 118-121`, and a failed log-file open falls back to stdout, `log.rs:136`); fixed startup prints (`println!("log file: …")`, `log.rs:127`; `println!("Using config: …")`, `commands.rs:1856`); and ~58 `println!`/`print!` calls across agent, provider, tool and config modules, some on runtime paths (e.g. `filesystem.rs`, `shell.rs`, `providers/base.rs`). Auditing them one by one is fragile — the next `println!` anyone adds would break the protocol again. Three layers:
    - **Logging to stderr in protocol mode.** `init_runtime_logging` gets a mode parameter. In protocol mode a stdout destination becomes stderr, a file destination stays, the "log file:" notice goes to stderr, and a failed file open falls back to stderr. `load_runtime_config` skips the "Using config" print in these modes.
    - **OS-level stdout redirect — the actual guarantee.** First thing at startup, before any other code runs, the protocol writer takes a duplicate of the real stdout handle for itself; then the process's standard output is pointed at stderr — Unix: `dup` + `dup2` via `libc` (already a Unix dependency); Windows: `SetStdHandle(STD_OUTPUT_HANDLE, <stderr handle>)` via `windows-sys` (new direct dependency, Windows-only). Every `println!` from rust-bot or any dependency then lands on stderr; only the JSON-RPC writer can reach the real stdout.
    - **Enforced by a test with logging on** (see test plan): a full turn with tool calls under `--logs` and `RUST_LOG=debug` must produce only JSON-RPC lines on stdout.

12. **MCP in both directions.**
    - *MCP client:* existing `tools.mcpServers` (inherited through the overlay) plus per-session `mcpServers` from ACP `session/new` (stdio required by ACP; HTTP/SSE only if advertised in `mcpCapabilities`). When the parent launches a rust-bot child it passes `mcpServers: []`, because the child already inherits them through config (no double connection).
    - *MCP server:* `rust-bot mcp` exposes `rust_bot_chat {message, sessionKey?}` (one `process_direct` turn) and, when `tools.acp.enabled`, `acp_list_agents` / `acp_run_agent`. Any MCP host (Claude Desktop, Cursor, another rust-bot via `mcpServers`) can use rust-bot and its child agents as tools. Works with `--overlay`, so an individual child can be exposed as an MCP server too.

13. **Graceful stop after each turn; tree-kill only as a fallback.** A child's memory work happens *after* the reply: consolidation is scheduled in the background after every turn (`schedule_background(maybe_consolidate_by_tokens)`, `agent_loop.rs:2036-2043`) and is only awaited by `close_mcp()`. Memory and session writes are not atomic (truncate + rewrite in place), so killing the child straight after the reply can truncate `MEMORY.md` or a session file. And Dream only runs as a gateway cron job (`commands.rs:1459-1489`), so without an explicit trigger a child's memory would never be dreamed over. Therefore:
    - **Signal:** after `acp_run_agent` has the final reply, the parent closes the child's stdin. ACP has no shutdown method; EOF on stdin is the standard signal for a stdio agent.
    - **Child on EOF (`agent_mode.rs`):** stop accepting requests → `close_mcp()` (drains background consolidation and closes MCP connections) → `dream.run()` → flush and exit 0.
    - **Dream on shutdown is cheap when idle:** `Dream::run` (`memory.rs:1202-1215`) returns without an LLM call or any write when there are no unprocessed history entries since its cursor. Entries only appear when consolidation archived something, so most short turns cost nothing extra.
    - **The operator is not kept waiting:** `acp_run_agent` returns the reply to the parent's AgentLoop immediately; the shutdown wait runs in the background.
    - **Single-flight covers shutdown:** the per-child lock (decision 10) is held until the child process has exited, so the next `acp_run_agent` for that child waits instead of starting a second process on the same files.
    - **Fallback:** if the child has not exited within `tools.acp.shutdownGraceSecs` (default 180, long enough for one Dream LLM call), or the turn itself exceeds `timeoutSecs`, the parent tree-kills (`taskkill /T /F`). Log it with the stderr ring buffer tail.
    - The keep-alive pool's idle reaper uses the same graceful path.
    - Third-party ACP clients (Zed, …) that simply close stdin get the same clean shutdown.
    - **Known residual risk:** a fallback tree-kill during a write can still truncate a file. Atomic writes (temp file + rename) in `MemoryStore::write_safe` and `SessionManager::save` would close that gap; out of scope for now, candidate follow-up.

14. **ACP complements A2A.**

15. **Cancellation reuses task abort, not a new token system.** `process_direct` (`agent_loop.rs:2257-2291`) takes no cancellation token and `tokio-util` is not a dependency. The existing `abort_session` (`agent_loop.rs:958-975`) aborts `JoinHandle`s in `active_tasks`, but only `run()` registers them, so a `process_direct` call cannot be stopped today. ACP requires that on `session/cancel` the agent stops and answers the pending `session/prompt` with `stopReason: "cancelled"`.
    - **Child (`agent_mode.rs`):** each `session/prompt` turn runs in its own spawned task; the `JoinHandle` is kept per ACP session. On `session/cancel`, abort that handle and answer the pending prompt with `stopReason: "cancelled"` — the same abort path channels already use.
    - **Nothing said so far is lost:** the turn's progress is saved as it goes through the existing mid-turn runtime checkpoints (`agent_loop.rs:1041-1047, 1139-1150`); an abort loses at most the work since the last checkpoint, and the next prompt continues from there. The turn task must go through the same checkpoint path that `run()` uses.
    - **Running shell commands die with the turn:** the shell tool wraps its process in `ChildGuard` (`shell.rs:155`) and waits in a polling loop that yields at each `await` (`shell.rs:170-181`); aborting the task drops the guard, which tree-kills the command. No change needed.
    - **Parent (`client.rs`):** when `acp_run_agent`'s `timeoutSecs` expires, send `session/cancel`; if the child has not answered within a short grace period, fall back to the shutdown / tree-kill path of decision 13.
    - **No duplicated tree-kill:** the ACP transport reuses `kill_process_tree_sync` (`shell.rs:520-531`), moved to a shared helper (e.g. `utils::process`), rather than writing its own `taskkill /T /F`.

16. **Structured tool events via an `AcpSessionHook` (ACP only).** ACP clients expect tool activity as structured `session/update` notifications — `tool_call` (id, title, kind, rawInput, locations, status) followed by `tool_call_update` (status, content). Today tool activity only leaves the loop as formatted text: `LoopHook::before_execute_tools` sends strings like `read src/main.rs` via `on_progress` as `ProgressKind::ToolHint` (`agent_loop.rs:214-233`) — no id, no arguments, no result. The data already exists in the hook system: `AgentHookContext` (`hook.rs:15-30`) carries `tool_calls` (`ToolCallRequest` with id, name, arguments), `tool_results` (filled before `after_iteration`, `runner.rs:1267, 1343`), `channel` and `chat_id`; and `AgentLoop::new` accepts extra hooks (`agent_loop.rs:303, 455`), which is how the CLI / WebSocket approval hooks are installed. So no change to the loop or the callbacks:
    - **`AcpSessionHook` in `agent_mode.rs`**, registered as an extra hook when the ACP agent loop is built.
    - **Routing:** each turn calls `process_direct` with channel `"acp"` and chat id = ACP `sessionId`; the hook uses `ctx.chat_id` to address the right session.
    - **`before_execute_tools`:** one `tool_call` per call — `toolCallId` = call id; `title` from the existing `format_tool_hints` (same wording as CLI / WebUI); `kind` mapped by tool name (`read_file` → `read`, `write_file` / `edit_file` → `edit`, `list_dir` / `glob` / `grep` → `search`, `shell` → `execute`, web tools → `fetch`, everything else → `other`); `rawInput` = arguments; `locations` = the path argument when present; `status: in_progress`.
    - **`after_iteration`:** one `tool_call_update` per call, matching `tool_results` to calls by `tool_call_id` — `status: completed | failed`, result text as `content`.
    - **Text and reasoning** keep using the existing callbacks, mapped to `agent_message_chunk` / `agent_thought_chunk`.
    - **Timing:** results are reported per iteration, not per tool — all tools of one model step complete together when the step ends. Per-tool completion would need a runner change; out of scope.
    - **Same hook handles permissions:** its `before_execute_tools` also sends `session/request_permission` and returns `ToolHookDecision::DenyCalls` for refused calls (see decision 9), exactly like the CLI / WebSocket approval hooks.
    - **Parent side:** `AcpClient` aggregates these updates into the activity tail returned by `acp_run_agent` and forwards them to the operator as progress during the turn (via the tool progress channel, decision 22).
    - **Not used by `rust-bot mcp`:** MCP `tools/call` returns one final result; there is no per-tool update stream in that mode.

17. **No `spawn` tool in `acp` / `mcp` mode.** The in-process subagent tool (`SpawnTool`, registered at `agent_loop.rs:412`) is not registered when rust-bot runs as `rust-bot acp` or `rust-bot mcp`. It does respect `tools.exec.enable` and `restrictToWorkspace` (`subagent.rs:264-293`), but inside a headless child it breaks four guarantees of this plan:
    - **No permission check:** subagents run with a logging-only hook (`SubagentHook`, `subagent.rs:51`), so the fail-closed approval of decision 9 would not apply to them.
    - **Wrong folder:** their file tools use `self.workspace` (`subagent.rs:264-274`) — in a child that is the child's *home*, not the session's project. The per-turn project scope (decision 1) is task-local and does not cross to the subagent's own thread and runtime (`subagent.rs:186-191`). The subagent could not see the project, but could write the child's `MEMORY.md`, `SOUL.md` and session files.
    - **Not stoppable:** `cancel_by_session` only joins the thread (`subagent.rs:517-520`), so neither `session/cancel` (decision 15) nor graceful shutdown (decision 13) can stop it.
    - **Result lost:** `announce_result` publishes a system message on the bus (`subagent.rs:434-484`) that only `run()` or the CLI listener consumes. An ACP child runs turns via `process_direct`, and the spawning turn has already returned.

    A child that needs help creates and runs its own children over ACP (bounded by `maxDepth`), which already provides permissions, the right project folder, cancellation, separate memory and a returned reply. Making `spawn` safe for headless mode (propagate the scope, approval hook, cancellation, result delivery) is out of scope. `spawn` stays unchanged in gateway and CLI modes.

18. **Use the official `agent-client-protocol` crate, not hand-rolled types.** Hand-rolling both roles of a spec that is still moving means chasing every spec change ourselves, and variants we did not list (content blocks, update kinds) become bugs. The crate is maintained by the spec authors and covers both roles with the same types.
    - **Pin:** `agent-client-protocol = "2.2"` (2.2.0, released 2026-09-18), protocol **v1 only**, no unstable features (`unstable_protocol_v2`, `unstable_session_fork` stay off).
    - **Contain churn with a thin adapter:** only `src/agent/acp/` imports crate types; the rest of rust-bot (tools, hooks, `AcpManager` callers) sees rust-bot's own types. The crate API has already changed substantially since `docs/acp/acp_peer.rs` was sketched (the 2.x API is organised by role — `Client`, `Agent`, `Proxy`, `Conductor` — and the sketch's `ClientSideConnection` style is not in the current docs), so a future change like that must stay inside one module.
    - **Runtime spike first (Phase 1):** the docs don't state whether the crate's futures are `Send`, and rust-bot's providers use non-`Send` futures (subagents already run on their own current-thread runtime for this reason, `subagent.rs:184-191`). The spike settles whether the connection runs on the main runtime or on a dedicated thread with a `LocalSet`, as the sketch does.
    - **Dependencies it adds:** `tokio-util` and `futures` plus some non-tokio async crates (`async-io`, `async-process`, `blocking`). No `anyhow` — the crate has its own `Error` type, mapped to rust-bot errors in the adapter.
    - **`session/resume` is optional:** it is not among the crate's documented stable v1 features, so the client only uses it if the pinned version exposes it as stable. rust-bot children always advertise `loadSession`, so continuity never depends on `resume`.
    - **The sketch:** `docs/acp/acp_peer.rs` is updated to the 2.x API during the spike, or deleted once real code exists.

19. **Child environment and `${VAR}` expansion.** `resolve_config_env_vars` (`loader.rs:99-112`) rewrites **every** string value containing `${VAR}` — not only provider keys; MCP `env` / `headers` rely on it (e.g. `${MCP_HEADERS_JWT}`) — and an unset variable is an error. A fixed "PATH + provider keys" environment would therefore make children fail at startup.
    - **Order in `load_config_with_overlay`:** load the **raw** parent config → `validate_overlay` + merge (decision 2) → `resolve_config_env_vars` on the merged result. The allowlist sees placeholders, not secrets; secrets are expanded only in memory inside the child and never written to disk.
    - **Env derived from the config:** before spawning, the parent scans the merged raw config for every `${VAR}` name, reusing the same regex as `resolve_config_env_vars` (shared, not copied), and passes exactly those variables, plus a base set:
      - rust-bot: `PATH`, `RUST_BOT_ACP_DEPTH`, `RUST_LOG`, `RUST_LOG_FILE`;
      - Windows: `SystemRoot`, `COMSPEC`, `PATHEXT`, `TEMP`, `TMP`, `USERPROFILE`, `APPDATA`;
      - Unix: `HOME`, `TMPDIR`, `LANG`.
      Without the OS essentials, `npx`, the shell tool and TLS certificate lookup break in hard-to-diagnose ways.
    - **Provider key variables** (`ProviderSpec.env_key`, e.g. `ANTHROPIC_API_KEY`) are passed only for providers the child's merged config actually uses.
    - **Missing variables fail in the parent:** if a referenced variable is not set in the parent either, `acp_run_agent` returns a clear error naming it instead of spawning a child that dies at startup.
    - External presets (Claude Code, Gemini CLI) get the base set plus the variables their preset declares; they don't read rust-bot's config.

20. **Absolute paths and the child's process working directory.** Several paths resolve against the directory the process was *started in*: the default `--config` `./.rust-bot/config.json` is stored without being made absolute (`commands.rs:101, 1854`), and a missing file silently yields `Config::default()` (`loader.rs:41-58`); `BUILTIN_SKILLS_DIR` is `current_dir().join("skills")` (`skills.rs:13-17`); and any path the user wrote as relative in `config.json` resolves against the cwd. `get_data_dir()` is the config file's parent folder (`paths.rs:9-19`), holding `logs/`, `media/`, `webui/` and `pairing.json`.
    - **Parent passes absolute paths:** `--config` and `--overlay` are canonicalised before spawning.
    - **Child process cwd = the parent's own process cwd**, so every remaining relative path (including built-in skills) resolves exactly as in the parent. This is the *process* working directory; the folder the child *works on* is the separate per-session project scope from decision 1 (`session/new.cwd`).
    - **`rust-bot acp` / `rust-bot mcp` make `--config` absolute at startup** (for third-party launchers such as Zed). In these modes a config path that doesn't exist is a startup error — a JSON-RPC error on `initialize` and a non-zero exit — instead of a silent fallback to `Config::default()`. Docs tell third-party clients to use an absolute `--config`.
    - **Shared data folder is intended:** children using the parent's config write logs and media into the parent's data folder. If `RUST_LOG_FILE` names one file, several processes append to it; acceptable for line-sized log records.

21. **MCP server built on rmcp's server side.** Production code only enables rmcp's client side (`Cargo.toml:40`: `client`, `transport-child-process`, `transport-io`, `transport-streamable-http-client-reqwest`, `reqwest-native-tls`); `server`, `macros` and `schemars` exist only in `[dev-dependencies]` for tests (`Cargo.toml:103-104`), so the production build has no MCP server code today.
    - **Cargo:** add `server`, `macros` and `schemars` to the production rmcp features and drop the now-redundant `[dev-dependencies]` rmcp entry. `transport-io` (stdio) is already in production — no new transport needed.
    - **Template:** `tests/agent/mcp_dummy_server.rs` already implements `ServerHandler` with `#[tool_router]` / `#[tool]`. `src/mcp_server/` follows it: tools `rust_bot_chat {message, sessionKey?}`, plus `acp_list_agents` / `acp_run_agent` when `tools.acp.enabled`, served over rmcp's stdio transport. No hand-rolled MCP protocol code.
    - **Not in scope:** a Cargo feature (e.g. `mcp-server`) to make the server optional in the binary; add only if build size becomes a concern.

22. **Progress while `acp_run_agent` waits, and no orphaned children.** `acp_run_agent` blocks the parent's turn for up to `defaultTimeoutSecs` (600 s), but tools have no progress channel — `Tool::execute` takes only its parameters and returns a string at the end (`tools/base.rs:67`) — so the operator would see nothing. And when the operator stops the parent's turn (`/stop`, `abort_session`), the parent task is aborted and the `acp_run_agent` future is simply dropped: the child keeps working on a turn nobody will read, holds its lock (decision 10), and the next run for that child waits behind it.
    - **Tool progress channel:** a task-local `ToolProgress` context (new `src/agent/tool_progress.rs`, same pattern as `workspace_context.rs` / `cron_context.rs`). The agent loop binds it per turn to that turn's existing `on_progress` callback; `report_progress(text)` sends a `ProgressKind::ToolHint`, which the CLI and WebUI already display. With no bound context it is a no-op, so existing tools are unaffected and other tools can adopt it later.
    - **Throttled child activity:** `acp_run_agent` reports each child `tool_call` as one line (e.g. `code-review › read src/main.rs`, title from decision 16) plus a heartbeat (`code-review › still working (2m 10s)`) about every 30 s while the child only streams text. Individual text / thought chunks are not forwarded.
    - **Cancel on drop:** `acp_run_agent` holds a guard for the running child turn. If the future is dropped before the reply arrives, the guard's `Drop` hands off to a background task that sends `session/cancel`, then the graceful stop of decision 13 (tree-kill after `shutdownGraceSecs`). The operator's `/stop` therefore also stops the child, and its lock is released.
    - **Unchanged:** decision 6's later option (announce long-running turns via `MessageBus`) remains for turns too long to wait for at all. ACP (stdio) = parent launches the peer. Talking to an *already running*, networked rust-bot is A2A (`a2a_protocol.plan.md`). No shared transport code beyond the JSON-RPC envelope.

## Core architecture

```mermaid
flowchart TB
    subgraph human["Human operator"]
        OP["Operator<br/>(websocket / channel)"]
    end

    subgraph supervisor["Parent rust-bot (gateway)"]
        AL["AgentLoop"]
        CTX["ContextBuilder<br/>+ Available ACP agents section"]
        TOOLS["tools/acp.rs<br/>create · update · run · list"]
        STORE["store.rs<br/>acp/agents/&lt;name&gt;/ · sessions.json"]
        MGR["AcpManager<br/>single-flight per child · keep-alive pool"]
        CLIENT["client.rs (ACP Client role)"]
        TRANS["transport.rs<br/>argv spawn · stdio pump · tree-kill"]
        TAB["ToolApprovalBroker"]
    end

    subgraph child["Child rust-bot: rust-bot acp --config parent.json --overlay child/overlay.json"]
        AM["agent_mode.rs (ACP Agent role)"]
        CAL["Child AgentLoop"]
        CWS["Child workspace<br/>SOUL/AGENTS/USER.md · memory/ · sessions/"]
        CCL["Child ACP Client role<br/>(inherited tools.acp, depth-bounded)"]
    end

    EXT["Other ACP agents<br/>Claude Code · Gemini CLI"]
    ZED["Third-party ACP clients<br/>Zed · JetBrains"]
    MCPS["MCP servers<br/>(inherited mcpServers)"]
    MCPH["MCP hosts<br/>Claude Desktop · Cursor"]
    MCPSRV["rust-bot mcp<br/>(MCP server role)"]

    OP --> AL --> TOOLS --> STORE
    CTX --> AL
    TOOLS --> MGR --> CLIENT --> TRANS
    TRANS -- "JSON-RPC stdio" --> AM
    AM --> CAL <--> CWS
    CAL --> CCL
    CAL --> MCPS
    CLIENT -. "preset" .-> EXT
    ZED -- "launches" --> AM
    MCPH --> MCPSRV
    CLIENT -. "escalate" .-> TAB
    CLIENT -- "final reply" --> AL --> OP
```

## Scenario flow (the 9 steps)

```mermaid
sequenceDiagram
    participant OP as Operator
    participant AL as Parent AgentLoop
    participant T as acp tools
    participant S as store.rs
    participant C as AcpClient
    participant W as Child rust-bot (rust-bot acp)

    Note over OP,W: Step 1 — create (agent did not exist)
    OP->>AL: "review the rust-bot code"
    AL->>T: acp_create_agent {name:"code-review", purpose, agents:"You are a code review specialist…"}
    T->>S: create acp/agents/code-review/ — copy SOUL/AGENTS/USER.md, apply overrides, write overlay.json + agent.json
    T-->>AL: created

    Note over OP,W: Steps 2–4 — first run
    AL->>T: acp_run_agent {name:"code-review", prompt}
    T->>S: sessions.json lookup (code-review, parentSessionKey) → none
    T->>C: spawn rust-bot acp --config parent.json --overlay …/overlay.json (depth+1)
    C->>W: initialize → loadSession=true
    C->>W: session/new {cwd, mcpServers: []} → sessionId (persisted)
    C->>W: session/prompt
    W->>W: process_direct in child workspace (own memory, own tools)
    W-->>C: session/update stream + final reply (stopReason end_turn)
    C-->>T: aggregated reply + activity tail
    T-->>AL: tool result
    AL-->>OP: first answer (step 4)
    C->>W: close stdin (EOF)
    W->>W: close_mcp() drains consolidation → dream.run() → exit 0
    Note over C,W: graceful stop after turn (default); tree-kill only after shutdownGraceSecs; child memory/ + sessions/ stay on disk

    Note over OP,W: Steps 5–8 — follow-up, same agent
    OP->>AL: another coding question
    Note over AL: system prompt lists "code-review" → LLM routes to it
    AL->>T: acp_run_agent {name:"code-review", prompt}
    T->>S: sessionId found
    T->>C: spawn + initialize
    C->>W: session/load {sessionId} (replayed updates discarded by client)
    C->>W: session/prompt {follow-up}
    W-->>C: updates + final reply
    C-->>T: aggregated reply
    T-->>AL: tool result
    AL-->>OP: second answer (step 8)

    Note over OP,W: Step 9 — repeat indefinitely; child's MEMORY.md / dream evolve in its own workspace
```

## Module layout

```
src/agent/acp/
  mod.rs            — module wiring, AcpManager (single-flight, keep-alive pool, depth)
  types.rs          — thin adapter over the agent-client-protocol crate (v1); the only module that imports crate types
  agent_mode.rs     — ACP Agent role: stdio loop wrapping AgentLoop.process_direct
  transport.rs      — ChildTransport: spawn, pumps, correlation, tree-kill
  client.rs         — AcpClient: handshake, capability gate, turns, request routing
  store.rs          — ChildAgentStore + SessionRegistry
src/agent/tools/acp.rs — create / update / run / list tools
src/mcp_server/     — MCP server role (stdio): initialize, tools/list, tools/call
src/config/overlay.rs — overlay allowlist, validate_overlay, load_config_with_overlay (RFC 7386 merge patch)
src/utils/fs.rs      — write_atomic (temp + fsync + rename)
src/cli/commands.rs  — Acp(AcpArgs) and Mcp(McpArgs) subcommands (headless, stdout = protocol)
```

## Config sketch (`config.json`)

```json
{
  "tools": {
    "acp": {
      "enabled": true,
      "maxDepth": 2,
      "allowDynamicAgents": true,
      "defaultTimeoutSecs": 600,
      "keepAliveSecs": 0,
      "shutdownGraceSecs": 180,
      "permissionPolicy": "auto-approve-read",   // | "escalate" | "allow-all"
      "sessionScope": "perParentSession",
      "launchPresets": {
        "rustbot": { "command": ["C:/development/onepoint/rust-bot/target/release/rust-bot.exe", "acp"] },
        "claude":  { "command": ["npx", "-y", "@agentclientprotocol/claude-agent-acp@<pinned-version>"] },
        "gemini":  { "command": ["gemini", "--acp"] }
      }
    }
  }
}
```

For the `rustbot` preset, the transport appends `--config <parent config path> --overlay <child>/overlay.json`, both canonicalised to absolute paths, and starts the child in the parent's own process working directory (decision 20). If `command[0]` is omitted or `"self"`, it uses `std::env::current_exe()`. External presets receive the session's project folder as `cwd` (decision 1 — never the child's home) and the child's `AGENTS.md` content as the first prompt preamble (they have no overlay concept; their own memory and settings are managed by the external agent itself). Permission requests from external agents are answered by the same fail-closed `permissionPolicy` (decision 9), and their environment is the base set plus the variables the preset declares (decision 19).

**External presets are examples, verified 2026-09-29 — re-check at setup time**, since these projects rename things:
- `claude`: `@agentclientprotocol/claude-agent-acp` (renamed from `@zed-industries/claude-agent-acp`), built on the official Claude Agent SDK — the user signs in through Claude Code itself, so it is the permitted way to use a Claude subscription. Pin a version: a bare `npx -y <package>` downloads whatever is newest on every run. The project also publishes prebuilt single-file binaries (Windows, macOS, Linux) that avoid `npx` entirely.
- `gemini`: `gemini --acp` (`--experimental-acp` is deprecated).
- Codex: no preset until its ACP adapter has been verified.

Example child `overlay.json` for a read-only reviewer:

```json
{
  "tools": {
    "exec": { "enable": false },
    "restrictToWorkspace": true,
    "disabledTools": ["write_file", "edit_file"]
  }
}
```

Key paths verified against `src/config/schema.rs`: shell is `tools.exec.enable` (not `enabled`), restrict-to-workspace is `tools.restrictToWorkspace` (not under `agents`). Unknown keys are silently ignored by the config parser (no `deny_unknown_fields`); the overlay allowlist (decision 2) rejects them so a wrong key fails loudly.

`tools.disabledTools` does **not exist yet**: there is currently no config switch for `write_file` / `edit_file`. Add it as part of this plan (a name list filtered out in `register_default_tools`, also applied to the `spawn` subagent registry for gateway / CLI mode — `spawn` itself is absent in `acp` / `mcp` mode, decision 17), otherwise a "read-only" child can still write inside its project scope.

`restrictToWorkspace: true` confines file tools to the session's project scope (`session/new.cwd`, see decision 1), not to the child's home.

## Implementation phases

**Phase 1 — protocol core (types, transport, client).** Starts with the crate spike (decision 18): add `agent-client-protocol = "2.2"`, confirm the runtime model against rust-bot's non-`Send` provider futures, update or retire `docs/acp/acp_peer.rs`. Then as above; agent→client requests surface as enum events routed to handlers; stderr ring buffer for failure reports.

**Phase 2 — config overlay + child store.** Overlay allowlist + `validate_overlay`, `load_config_with_overlay`, `write_atomic`, `ChildAgentStore` (layout, bootstrap copy + overrides, `agent.json`), `SessionRegistry`. Unit tests for merge semantics (nested merge, `null` delete, array replace, forced workspace).

**Phase 3 — ACP Agent role (`rust-bot acp`).** Headless startup; stdout hygiene (decision 11: protocol-mode logging, OS-level stdout redirect); initialize/authenticate/session/*; per-session MCP connections from `mcpServers`; `session/load` replay from the child `SessionManager`; `session/cancel` via task abort (decision 15). Manually verify interop by registering `rust-bot acp` as a custom agent server in Zed.

**Phase 4 — tools, context, concurrency.** `acp_create_agent` / `acp_update_agent` / `acp_run_agent` / `acp_list_agents`; "Available ACP agents" system-prompt section; `AcpManager` single-flight; depth gating.

**Phase 5 — permissions + lifecycle.** Policy table (decision 9); `ToolApprovalBroker` escalation; graceful stop on stdin EOF with Dream on shutdown (decision 13); keep-alive pool + reaper; tree-kill as fallback only.

**Phase 6 — MCP server role (`rust-bot mcp`).** Enable rmcp server features in production (decision 21); stdio MCP server on rmcp `ServerHandler` exposing `rust_bot_chat` + ACP tools; same stdout hygiene; reuses `process_direct`.

## Security notes

- Spawn is code execution: dynamic agents only reference **preset keys**; argv arrays only.
- `cwd` (the child session's project scope, decision 1) must be an existing absolute directory inside the parent session's own `project_path` when the parent is restricted; a full-access parent may pass any existing directory. It is never required to be inside the child's home, and it may never be or lie inside a rust-bot home (parent workspace, another child's home); a home *contained* in the `cwd` becomes a denied subtree for the child's file tools (decision 1).
- Overlays are checked against a code-level allowlist (decision 2): only listed paths may be set, security-relevant ones only narrowed; providers, new MCP servers and launch presets can never be set by an overlay. Enforced in `acp_create_agent` / `acp_update_agent` and again on every child start.
- Child tool policy lives in its overlay — a code-review child can be read-only, so a compromised child cannot mutate the host.
- Agent replies are untrusted tool-result content — plain text, never executed.
- Worker env is derived from the config, not the full supervisor env (decision 19).
- `acp_create_agent` under `escalate` policy requires operator approval before first run.

## Windows notes

- `rustbot` preset spawns `rust-bot.exe` (absolute path or `current_exe()`) as an argv array — no shell, no `.cmd` shims.
- Node-based presets (`npx`/`uvx`) are `.cmd` shims. Rust's `Command` on Windows only finds `.exe` on `PATH`, so a bare `npx` fails with "program not found". The existing MCP stdio transport has **no** Windows handling either (`mcp/mod.rs:350-363`, plain `Command::new(&config.command)`), so there is nothing to copy. Fix with one shared helper (e.g. `utils::process::resolve_program` + `command_for`), used by **both** the ACP transport and the MCP stdio client:
  - Resolve the full path first with the `which` crate (already a dependency, `Cargo.toml:43`; used in `skills.rs` / `mcp_presets_api.rs`), which applies `PATHEXT` on Windows — `npx` → `C:\…\npx.cmd`.
  - Spawn the resolved file **directly**, never through `cmd /C "<string>"` (a joined command line invites shell injection and quoting bugs). Since Rust 1.77, spawning a `.cmd` / `.bat` directly quotes arguments for the batch interpreter itself and returns an error for arguments it cannot quote safely, instead of running a mangled command.
  - The argument array stays an array; no shell anywhere.
  - Side benefit: MCP servers configured as `"command": "npx"` start working on Windows too.
  - Never applies to `rustbot` (absolute `.exe` path or `current_exe()`).
- Tree-kill via `taskkill /PID <pid> /T /F`.
- UTF-8 on pipes; argv via `OsStr` lists (paths contain spaces).

## Test plan

- **Overlay merge:** nested merge, `null` delete, array replace, forced `agents.workspace`.
- **Overlay allowlist:** rejected — `providers.*.apiBase` / `apiKey` / `extraHeaders`, a new `tools.mcpServers` entry, changing an inherited server's command/env, `tools.acp.launchPresets`, `tools.exec.enable: true` under a parent with `false`, `restrictToWorkspace: null` under a parent with `true`, `disabledTools` missing a parent entry, `maxDepth` above the parent's, unknown path (`tools.exec.enabled`), `agents.provider` naming an unconfigured provider. Accepted — the read-only reviewer overlay, `agents.model`, dropping an inherited MCP server with `null`. A tampered `overlay.json` on disk makes the child fail `initialize` with the validation error. Strictest wins: the parent adds a `disabledTools` entry / lowers `maxDepth` after the overlay was written → the child's effective config has the parent's entry and the lower depth, and still starts.
- **Child store:** bootstrap files copied then overridden; overrides persisted in `agent.json`; `acp_create_agent` on an existing name fails; `acp_update_agent` edits persist. A parent edit to `USER.md` does not reach the child until `resyncFromParent: ["USER.md"]`, after which the child's file equals the parent's with the stored `user` override re-applied, and `acp_list_agents` no longer reports drift.
- **Mock ACP agent** (`tests/acp_mock_agent.rs` binary): capability gating (no `loadSession` → client MUST NOT call it), permission deny, replay updates discarded, tree-kill leaves no orphans.
- **Real `rust-bot acp` with mock provider:** prompt happy path, `session/load` replay, `session/cancel`, stdout contains only JSON-RPC frames — run with `--logs` and `RUST_LOG=debug` through a turn that calls tools, assert every stdout line parses as a JSON-RPC message; a deliberate `println!` in a test tool must appear on stderr, not stdout.
- **Permissions (fail closed):** in `rust-bot acp`, a `shell` call triggers `session/request_permission` and a `read_file` call does not; client replies of error / `cancelled` / no answer before the timeout deny the call and the model receives a "denied by client" result; `CliAskHook` is never installed in `acp` mode. Parent: `auto-approve-read` approves read and denies shell; `escalate` from a WebSocket turn reaches `ToolApprovalBroker` and honours the operator's answer; `escalate` from a CLI turn without a terminal denies. `rust-bot mcp` with `confirmBeforeExecute: true` denies gated calls instead of running them.
- **Tool events:** a turn that calls `read_file` and `shell` emits, per call, a `tool_call` (matching id, kind `read` / `execute`, `rawInput` = arguments, `locations` for the path) followed by a `tool_call_update` with `completed` and the result; a failing tool yields `failed`; updates carry the right `sessionId` when two sessions run on the same agent.
- **Progress and orphan prevention:** during `acp_run_agent`, each child tool call appears once as a `ToolHint` on the parent's progress callback and a heartbeat appears while the child only streams text; `report_progress` outside a bound context is a no-op. Aborting the parent's turn mid-run sends `session/cancel` to the child, the child exits, its `.acp.lock` is released, and the next `acp_run_agent` for that child starts without waiting.
- **Cancellation:** `session/cancel` during a long tool call → the pending `session/prompt` answers `stopReason: "cancelled"`, a running shell command's process tree is gone, the session file holds the turn up to the last checkpoint, and the next `session/prompt` in that session works. Parent timeout sends `session/cancel` first and only tree-kills after the grace period.
- **Memory isolation:** after a child turn, the child's `memory/` and `sessions/` change; the parent's do not. `acp_run_agent` with `cwd` = the parent's workspace, a folder inside it, or a sibling child's home is rejected; with no `cwd` while the parent's project defaults to its workspace, it fails asking for a project folder. A `cwd` that *contains* the parent's workspace (e.g. a repo with `./.rust-bot/workspace`) is accepted, and the child's `read_file` / `grep` / `glob` / `list_dir` and MCP `file://` arguments refuse paths inside that workspace while other repo files stay readable; with shell enabled, `acp_run_agent` warns.
- **Cross-process single-flight:** two separate processes run `acp_run_agent` for the same child at once → the second child waits on `.acp.lock` and starts its turn only after the first exits; both turns land in the session file intact. Killing the lock holder releases the lock. Two *different* children run concurrently without waiting.
- **Atomic config save:** a reader looping on `load_config` while another thread calls `save_config` repeatedly never sees invalid JSON; `write_atomic` unit tests cover replace-existing and same-directory temp file.
- **Graceful shutdown:** closing stdin after a turn that triggered consolidation → child exits 0 only after consolidation finished and Dream advanced its cursor; `MEMORY.md` and the session file parse cleanly. With no unprocessed history, shutdown makes no LLM call. A child that ignores EOF is tree-killed after `shutdownGraceSecs`. A second `acp_run_agent` issued during shutdown waits for the exit instead of spawning a second process.
- **Depth:** at `maxDepth` the ACP tools are absent from the child's tool list.
- **Paths and process cwd:** a parent started with a relative `--config` spawns a child that receives absolute `--config` / `--overlay` and runs in the parent's cwd, so it loads the same config and finds the built-in skills; `rust-bot acp --config does-not-exist.json` fails `initialize` with a clear error instead of starting on defaults.
- **Child environment:** a parent config with `${MCP_HEADERS_JWT}` in an MCP header → the child receives that variable and starts; a variable not in the config is absent from the child's env; a referenced variable unset in the parent → `acp_run_agent` errors with its name and spawns nothing; the merged config seen by `validate_overlay` still contains `${…}` placeholders.
- **Program resolution (Windows):** `resolve_program("npx")` returns the `npx.cmd` path; a preset and an MCP stdio server configured with a bare `.cmd` shim both start; an argument containing a space arrives intact; an argument Rust refuses to quote for a batch file yields a clear spawn error, not a run.
- **No `spawn` in headless modes:** the tool list of `rust-bot acp` and `rust-bot mcp` does not contain `spawn`; gateway and CLI tool lists still do.
- **End-to-end 9-step scenario:** create → run → operator reply → follow-up routed to the same agent → relaunch with `session/load` → second reply references first-turn context → third question also succeeds; restart the parent between turns 2 and 3 and assert continuity survives.
- **MCP server:** `tools/list` includes `rust_bot_chat`; `tools/call` returns a reply; stdout hygiene (same logging-on test as `acp`).

## Out of scope

- ACP over HTTP/SSE remote transport (spec WIP; A2A covers networked peers).
- Supervision of parallel fan-out across many children (concurrency across *different* children works by design; orchestration logic is a later plan).
- Third-party engine presets ship as config examples only; rust-bot never installs engines.
