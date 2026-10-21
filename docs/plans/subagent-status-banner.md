# Plan: "Waiting for N background agent(s) to finish" banner (CLI, websockets-chat, ACP)

## Goal

Announce background sub-agents the way the Claude TUI does: while a session has sub-agents running, show
`Waiting for N background agent(s) to finish`; remove it as soon as the last one is done (completed, failed,
partial, cancelled, or crashed). Today rust-bot starts sub-agents and says nothing, in the CLI or in
websockets-chat.

## Decisions (agreed)

- **Per-session scope.** A banner counts only sub-agents whose `session_key` matches the viewer's session/chat. Never the whole gateway.
- **Fire on every terminal path, including the safety net.** The notification is emitted from the same single place that moves a record out of `Running` (`SubagentManager::set_status`), plus the spawn site. That covers the `Failed` safety net after a run that ended without reporting.
- **No config flag.** Always on. (Deliberate exception to the "config-gated enable flag" convention: display-only, no new capability.)
- **Cancel clears the banner immediately.** The user asked for the stop; the banner must not wait for the thread to wind down. So the `ActivityGuard` is owned by the task's record/manager entry (released inside `set_status` on any terminal status, including `Cancelled`), not only by the thread. The thread holds no guard; the thread body's panic path calls `set_status(Failed)`, which releases it.
- **ACP guard is released when the reply arrives**, not when the child process exits (the user is waiting on a turn). `/subagents` keeps showing process exit separately.
- **Websocket reuses the existing `goal_status` event** (extended additively) instead of adding a new event. See M2.
- **Milestones:** M1 CLI, M2 websockets-chat, M3 ACP children. Each compiles green and is independently shippable.

## What exists today (verified)

- `SubagentManager` (`src/agent/subagent.rs:210`) keeps one `SubagentRecord` per spawn (`status`, `session_key`, `channel`, `chat_id`, `finished_at`) in `tasks`.
- Lifecycle choke points: `insert_running_record` (start, `:450`) and `set_status` (every terminal transition, one-shot, first writer wins, `:471`). The spawned thread calls `set_status(.., Failed)` as a no-op safety net at `:400`.
- **Gap:** a run that *panics* inside `block_on` unwinds the thread, so neither `set_status` nor the `running_tasks` cleanup (`:401`) runs. The record stays `Running` forever, so a counter derived from records would show a stuck banner. M1 must fix this (see S1.3).
- `set_status` runs *before* `announce_result` (`:622-643`), so the banner clears slightly before the result message reaches the bus. Acceptable; documented, not changed.
- CLI: interactive loop is `interactive_session` (`src/cli/commands.rs:2106`) using **reedline** `read_line` with `CliPrompt` (`:2070`, has `render_prompt_left/right`). While a turn runs, `message_session` owns the indicatif `ThinkingSpinner` (`src/cli/stream.rs:14`). No `ExternalPrinter` is used anywhere. Sub-agents announce via `spawn_system_message_listener` (`:1832`).
- websockets: events are `WsOutboundEvent` (`src/channels/websocket/types.rs:180`); `list_subagents`/`Subagents` is pull-based, gateway-wide. `ChatDeleted` (`runtime.rs:~2512`) is a precedent for pushing an event to every connection subscribed to a chat. Client side: `websockets-chat/src/protocol.rs` (`ServerEvent`, `parse_server_event`) and `websockets-chat/src/app.rs` (`WsContext` signals, event match at `~:1003`).
- ACP: `AcpManager::run` (`src/agent/acp/manager.rs:277`) is awaited inside the `acp_*` tool call (`src/agent/tools/acp.rs`), keyed by `RunRequest::parent_session_key`; `is_busy` is only a per-agent snapshot via lock `try_lock`.

---

## M1 — CLI only

### S1.1 Core: per-session running counter with change notifications (shared by all milestones)

New small module `src/agent/background_activity.rs` (name open), independent of the UI:

- `BackgroundActivity` tracker: per-session list of entries `ActivityKind::Spawn | ActivityKind::Acp { agent: String }` behind a `Mutex`, plus a `tokio::sync::watch`/broadcast per session or one broadcast of `ActivityChange { session_key, spawn_count: usize, acp_agents: Vec<String> }`. API: `begin(session_key, kind) -> ActivityGuard`, `snapshot(session_key) -> ActivitySnapshot`, `subscribe() -> Receiver<ActivityChange>`. Only `Spawn` is used in M1; `Acp` is exercised by tests until M3.
- **`ActivityGuard` is RAII**: dropping it decrements and notifies. This is what makes the "stuck banner" case structurally impossible — a panic unwinds and drops the guard. It is the answer to the open point "must fire from the safety net too", and it holds for every exit path, not only the ones we remembered to instrument.
- Guard ownership (decided): spawn-task guards are stored with the task entry in `SubagentManager` and dropped by `set_status` on any terminal status. Dropping on `Cancelled` clears the banner immediately even if the thread is still unwinding. `set_status` is already one-shot (first writer wins), so a late `Completed` after `Cancelled` cannot double-decrement.
- Sources of truth: the tracker is the display counter; `SubagentRecord` stays the listing source. They must agree; a test asserts it.
- Sessions with no `session_key` (spawned from contexts without one) are not tracked; no banner for them (documented).

### S1.2 Wire into `SubagentManager`

- Hold an `Arc<BackgroundActivity>` (constructor param with a default in `new_simple`, so existing tests keep compiling).
- In `spawn_with_task_id`: `begin(session_key)` next to `insert_running_record`; store the guard in a manager-side map keyed by task id (not moved into the thread).
- `set_status` removes and drops that guard when it moves a record out of `Running`. The safety-net line at `:400` and cancellation (`cancel_by_session`, `:770`, which calls `set_status(Cancelled)` at `:799`) then both clear the count; test each.
- Idempotent replay (`existing_task_reply`) must NOT call `begin` again.

### S1.3 Panic-safety

Wrap the thread body so the guard, `running_tasks` removal and `session_tasks` removal happen in a drop guard / `catch_unwind` path. Today a panic leaves the record `Running` and the thread handle in `running_tasks`. Fix: on unwind, `set_status(Failed)` and clean maps. Test with a provider that panics.

### S1.4 CLI rendering

The hard part is terminal mechanics, not data. Two states:

1. **During a turn** (`message_session` owns the spinner): change the spinner template to include the count when `> 0`, e.g. `⠋ rust-bot is thinking... · 2 background agents`. Cheapest path, no new line, no redraw conflict. indicatif supports `set_message`/template message.
2. **Idle at the prompt** (reedline owns the terminal; this is the common case since `spawn` returns immediately and the agent often ends its turn): reedline cannot be redrawn from another task safely. Options, in order of preference:
   - a. Show the banner in `CliPrompt::render_prompt_right` (reedline re-renders the prompt on repaint; use the tracker's `running()` read at render time) — cheap but only updates when reedline repaints (keypress). Pair with b.
   - b. Use reedline's `ExternalPrinter` to print a one-shot dim line `⏳ Waiting for N background agent(s)…` at the first spawn and `✓ background agents finished` at 0. Append-only, safe, no cursor tricks (matches the "append-only, never redrawn" rule documented on `StreamRenderer`).
   - Step 0 spike decides a vs b vs both; the requirement "message disappears when done" is met by (a) clearing, and by (b) printing a completion line instead of erasing scrollback.
3. A single subscriber task `spawn_background_activity_listener` (next to `spawn_system_message_listener`, `commands.rs:1832`) turns `ActivityChange` for the CLI session id into spinner/prompt updates. Non-TTY or `--markdown`-off runs: print nothing (no spinner either), matching `ThinkingSpinner`'s TTY behavior.
4. Pure formatting function `background_agents_banner(snapshot: &ActivitySnapshot) -> Option<String>` (`None` when nothing runs; singular/plural "1 background agent" / "n background agents"; ACP entries appended by name as `acp::<agent>`, joined with commas; ACP-only gives `Waiting for acp::reviewer to finish`) — unit-tested for 0, spawn-only, ACP-only, mixed, and several ACP agents.

### M1 tests

- Tracker: begin/drop counts, multiple sessions isolated, notification on each change, guard drop on panic.
- Manager: spawn → count 1; completion, failure, partial, cancel, provider panic, safety-net → count 0; replay of same task id does not double count; two sessions don't see each other.
- Banner formatter (0/1/n).
- Spinner message update logic (pure part).
- Manual check: run CLI, spawn two sub-agents, see banner, see it clear; kill one with `/stop`-style cancel; confirm no stuck line. (Record result in the PR.)

### M1 status (implemented 2026-10-19, uncommitted on `feature/background-agents-banner`)

Built: `src/agent/background_activity.rs` (tracker, guard, `ActivitySnapshot::banner`), `SubagentManager` wiring (guard map, `register_activity`, `set_status` releases it, `activity()` accessor, panic-safe thread body), `src/cli/background_banner.rs` (spinner updates, prompt notices via `ExternalPrinter`, pre-prompt line), spinner/`StreamRenderer` banner support, `reedline` `external_printer` feature in `Cargo.toml`. `cargo clippy --all-targets` clean; `cargo test` green.

Deviations from the plan above:
- The idle prompt uses printed notice lines only (`⏳ Waiting for N ...` on each change, `✓ Background agents finished` at 0); the optional right-hand prompt segment was **not** built. Append-only lines cannot be erased from scrollback, so "disappears" at the prompt means "replaced by the finished line". Say if you want the right-prompt segment as well.
- The CLI keys its banner on `cli:direct`, because the `spawn` tool keys every CLI task that way regardless of `--session`.
- A task that panics is marked `Failed` and cleared from the banner, but (as before) no failure announcement is sent to the agent.

Still to do for M1: the manual terminal check below (needs a real provider and a TTY).

### M1 done when

CLI shows and clears the banner for both the in-turn and idle states; panic case proven to clear; all tests green (`cargo test`, `cargo clippy`).

---

## M2 — websockets-chat

### Reuse finding: `goal_status` is the only candidate, and cannot be reused as-is

The websocket path has exactly one lifecycle event, `goal_status` (`status: "running" | "idle"`, `started_at`, `turn_id`), sent by `send_goal_status` (`runtime.rs:4162`, fan-out to a chat's subscribers with dead-connection cleanup), replayed on attach by `maybe_push_turn_run_wall_clock` (`:4142`), and parsed client-side as `ServerEvent::GoalStatus` (`protocol.rs:930`). There is no typing/busy/working event. Problems with using it unchanged:

- It means "a **turn** is running". `idle` is sent when the turn completes (`:4712`), which is exactly when background sub-agents usually still run. Mapping "agents running" onto `running` would be wrong in both directions.
- It is derived from `turn_registry`, not from background work, and is only replayed on attach while a turn is active.
- The client currently only logs it (`app.rs:796`); nothing consumes it yet, so extending it is cheap.

**Decision: reuse the event, the fan-out helper and the attach replay, and extend the payload additively** with an optional `background_agents: n` field. Wire-compatible for the nanobot-mirrored shape (unknown field, ignored by others; field absent means "no information", never "zero").

### S2.1 Server: extend `goal_status`

- `send_goal_status` gains an optional `background_agents: Option<usize>` parameter; when `Some`, the body carries `"background_agents": n`. Existing call sites pass `None`, so their output is unchanged.
- New gateway task subscribes to `BackgroundActivity` changes, maps `session_key → chat_id` (reuse the existing helpers in `runtime.rs`), reads the chat's current turn state from `turn_registry` to fill `status` (`running` if a turn is active, else `idle`), and calls `send_goal_status(.., Some(n))`. Same fan-out and cleanup as today; no new event type, no new `WsOutboundEvent` variant.
- **Attach/reconnect:** extend `maybe_push_turn_run_wall_clock` (or add a sibling called next to it) so attach always sends one `goal_status` with the current `background_agents` when `> 0`; a client that missed a `0` is corrected on its next attach, and the client also resets the count on disconnect.
- Race to test: count change and turn end arriving close together must not make the client show a stale turn state. The server reads turn state at send time; the client only takes `background_agents` from the field when present and ignores `status` for the banner.
- Non-chat-scoped sessions (`api`, telegram, etc.) are untouched.

### S2.2 Client

- `websockets-chat/src/protocol.rs`: `ServerEvent::GoalStatus` gains `background_agents: Option<usize>`; update `GoalStatusWire` (`:1386`), the `"goal_status"` parse arm (`:1699`) and the existing tests. The event is already chat-scoped (`:1138`), so no scoping change.
- `app.rs`: replace the log-only `GoalStatus` arm (`:796`) so that `Some(n)` writes `n` into `subagents_running: RwSignal<HashMap<chat_id, usize>>` (or only the current chat); `None` leaves it untouched. Cleared on disconnect, chat delete, chat switch (`clear_*` functions near `:1347/:1468`).
- UI: a slim status row above the composer in `chat_shell.rs`, text `Waiting for N background agent(s) to finish`, hidden at 0. Reuse the pure formatter from M1 if the crate can share it; otherwise duplicate the 5-line function with a test in each (note it — `chat-ui` is a separate crate).
- Sidebar optional nicety (out of scope unless trivial): a dot on chats with running agents.

### M2 tests

- `protocol.rs`: parse `goal_status` with `background_agents` present (n, 0) and absent (`None`, existing tests unchanged).
- Runtime: `send_goal_status` includes the field only when `Some` (existing assertions on the body still pass); fan-out reaches only connections subscribed to that chat; attach sends current count; dead connection cleaned up; two chats isolated; count change during an active turn and after turn end both carry the right `status`.
- Client logic: pure reducer for the signal map (set, clear on 0, clear on disconnect).
- Manual check in browser: spawn agents from chat A, confirm chat B shows nothing; reload mid-run; cancel; confirm banner clears.

### M2 status (implemented 2026-10-19, uncommitted on `feature/background-agents-banner`)

Built: `send_goal_status_with_background_agents` (additive `background_agents` field; `send_goal_status` unchanged for existing callers), `forward_background_agent_changes` spawned from `WebSocketChannel::start()` (fans out per chat, reading turn state from the registry at send time), attach replay in `hydrate_after_subscribe`, `ConnectionRegistry::subscribed_chat_ids` for resync after a lagged receiver. Client: `ServerEvent::GoalStatus.background_agents`, `state::background_agents_after` / `background_agents_banner`, `WsContext.background_agents` (reset on attach and on disconnect), `BackgroundAgentsBanner` row above the approval/question cards and composer. No other client parses `goal_status`, so the Step 0 unknown-field check is closed.

Notes: `background_agents` counts spawn tasks only (ACP names arrive in M3). The wording is duplicated in `websockets-chat/src/state.rs` because it is a separate wasm crate. Still to do: the manual browser check below, and rebuilding the UI bundle (`websockets-chat/dist` is git-ignored; `trunk build`).

### M2 done when

Banner appears and disappears in websockets-chat per chat, survives reload/reconnect, no cross-chat leakage; docs updated (`docs/WEBSOCKETS_CHAT.md` protocol section).

---

## M3 — ACP children

Spawn tasks and ACP children are different mechanisms, so this gets its own milestone.

- **Where to hook:** `AcpManager::run` (`manager.rs:277`); `RunRequest::parent_session_key` is the session scope. Take an `ActivityGuard` after the agent is validated and release it when the reply arrives (decided), i.e. when `run` returns its `RunResult`, not when `RunResult::exit` resolves. `/subagents` keeps showing process exit via `is_busy`.
- **Foreground vs background:** `acp_*` tools are awaited inside a tool call, so the parent turn is already blocked and the spinner/"working" state exists. ACP runs add to the same count ("Waiting for 3 background agents"); it is the same user-visible "something is running for me". **Wording (decided):** ACP runs are worded differently so it is clear an external agent is being called. Each ACP entry is shown by name with an `acp::` prefix, e.g. `Waiting for 2 background agents, acp::reviewer to finish`. Spawn tasks stay a plain count. This needs the tracker to know the kind of each entry (see S1.1), which is why the kind is added in M1 even though ACP only arrives in M3.
- **Websocket wire for ACP:** `background_agents` stays the spawn-task count; M3 adds an optional `acp_agents: ["reviewer", ...]` (names, no prefix; the client adds `acp::`) to the same `goal_status` event. Absent means "no information", as before.
- Wire `Arc<BackgroundActivity>` into `AcpManagerSettings` (default for tests).
- Error paths (`RunError` variants, timeouts, permission denial, panics) are covered by the RAII guard; add tests for each early-return branch of `run` (`UnknownAgent`, `Launch`, `Spawn`, `Child`) to prove the count never leaks, including `UnknownAgent` which fails before anything starts (must not count, so take the guard after validation).
- Parent-process vs child-process: a child rust-bot launched via ACP has its own tracker (separate process); its spawned sub-agents count toward the *child's* session, not the parent's. Not bridged in M3; note as future work (would need an ACP notification).
- **Tests:** guard held across a successful run and released after; released on each error branch; two parents isolated; concurrent runs of different agents in one session count 2.
- **Done when:** ACP runs show in both CLI and websockets-chat banners using the M1/M2 plumbing with no UI changes beyond tests/docs.

---

## Step 0 (before M1 code)

1. Green baseline: `cargo test` and `cargo clippy`; record counts.
2. Read `cancel_by_session` (`subagent.rs:770`) to confirm it reaches `set_status(Cancelled)` for every task of the session, so releasing the guard there clears the banner immediately (S1.2).
3. Spike the idle-prompt rendering in reedline (S1.4): confirm `ExternalPrinter` works with the current reedline config incl. `use_kitty_keyboard_enhancement`, and that the prompt-right repaint behaves on Windows Terminal.
4. ~~Check for an existing "working" event~~ Done: only `goal_status` exists; reuse it with an additive `background_agents` field (see M2). Remaining check: that no other client (`chat-ui` crate, the legacy web UI under `src/channels/websocket/webui`) rejects unknown fields on `goal_status`.

## Risks

| Risk | Mitigation |
|---|---|
| Terminal flicker/garbled prompt in reedline | Append-only `ExternalPrinter` lines; no manual cursor control; spike in Step 0 |
| Stuck banner after panic/crash | RAII guard + panic-safe thread body; websocket client resets on disconnect/attach |
| Counter and records drift | Single begin point; test asserting `running()` equals `Running` records |
| Cross-session leakage | Counter keyed by `session_key`; tests with two sessions in every milestone |
| Event spam on churny spawns | Notify only on count change; websocket may coalesce within a short window if needed (measure first) |
| Banner clears before the announcement message arrives | Accepted; documented (`set_status` precedes `announce_result`) |

## Out of scope

Config flag, per-agent names/progress in the banner, a sidebar indicator, bridging child-process activity into the parent, persisting activity across restarts (sub-agents are in-memory and die with the process).

---

## Follow-up (2026-10-21): the main agent no longer goes quiet after the last sub-agent

Found in `rust-bot-ems` logs: after the last sub-agent ended, the main agent spent 10 minutes on its follow-up turn (writing the report) while a user message waited unread, because system-message turns ran inline on the agent loop and showed no progress.

- **Loop unblocked:** system messages are spawned as per-session tasks like user messages (`dispatch_system`, shared `acquire_turn_slot`), registered in `active_tasks`, so `/stop` and Stop work during a follow-up. Turns of one session still serialize on the session lock, so a user message waits for the running follow-up, but is no longer unreadable.
- **Visible work:** new `ActivityKind::FollowUp` (held for the length of `process_system_message`) feeds the banner text ("Working on the background agents' results", or "Waiting for … · working on the results so far"); `goal_status` gained `working_on_results`. For WebUI chats the follow-up turn also publishes its tool calls and notes (never the reasoning stream) as progress; the client shows the latest as a hint under the banner, with a Stop button.
- **Replies of a follow-up** are flagged `from_background` on the wire so the client shows them as their own entries instead of attaching them to a turn the user has pending.
- Not done: letting a user message interrupt a follow-up (the earlier "option C").
