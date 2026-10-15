# Plan: P0 — Durability Core (Pi Durable–style) for rust-bot

## High-level view

Make rust-bot crash-resilient with three interlocking guarantees, built on the persistence machinery that already exists:

1. **Write-ahead tool intent + replay safety** — every tool call is journaled (`intent` → `done/error/blocked`) before/after `Tool::execute` (bracketing `src/agent/runner.rs:987–992`); after a crash, an unfinished call is re-executed only if the tool declares `replay_safe() == true`, otherwise the model gets a synthetic "interrupted" tool result and the call is never repeated. `execute(&Value) -> String` is unchanged.
2. **Step-level checkpoints + auto-resume** — every step (each model request, each tool batch, compaction) persists a full turn snapshot before the next step begins; on startup, unfinished runs are resumed from their last checkpoint (gateway and api automatically, interactive CLI by confirmation, ACP never — see D7): a cut-off model request is re-sent, the partial answer stays in the transcript marked aborted, queued messages are re-driven. This upgrades today's coarse turn markers (`{"type": "awaiting_tools"|"tools_completed"|"final_response"}`, `src/agent/runner.rs:1246/1361/1472`) to the rich payload shape `restore_runtime_checkpoint` (`src/agent/agent_loop.rs:2227–2317`) already understands.
3. **Exactly-once inbound submissions** — `InboundMessage` (`src/bus/events.rs:9`) gains `request_id: Option<String>` (adapters map platform message ids); a durable ledger maps `request_id → outcome`; a retried delivery gets the original outcome back instead of triggering a second turn.

```
                        ┌──────────────────────────────┐
  channel adapter ──▶   │ InboundMessage(+request_id)  │
                        └──────────────┬───────────────┘
                                       ▼
                        inbox ledger dedupe (journal)      [WS3]
                                       ▼
                     AgentLoop::run / process_message(_inner)
                                       ▼
                AgentRunner::run  (the step loop, runner.rs:1131)
                 │ step: PreModel ──▶ checkpoint snapshot   [WS2]
                 │ model request  (cut off ⇒ re-send)
                 │ step: PreTool  ──▶ checkpoint snapshot   [WS2]
                 │ per call:  journal intent  ─▶ tool.execute ─▶ journal terminal [WS1]
                 │ step: Final    ──▶ terminal run record
                        ┌────────────┴──────────────┐
                        ▼                            ▼
   <key>.journal (append-only,           <key>.jsonl (SessionManager,
    tool_intent | step_checkpoint |        write_atomic already, manager.rs:395)
    inbox_ledger records)
                        ▲
   startup scan ────────┘: unfinished runs → resume driver  [WS2]
                           intent w/o terminal → replay/synthesize [WS1]
                           ledger Queued/Processing → re-drive    [WS3]
```

"Done" = kill the process at any point during a turn and, on restart, the turn finishes correctly: safe tools re-run idempotently, unsafe tools surface as interrupted (never repeated), no message is lost, no inbound message gets a second **model turn**, no duplicate spawns. **Honest scope of the claim (review fix):** this is *effectively-once model turns*, not exactly-once delivery. A crash between sending the reply and writing the ledger `Done` record can re-send the reply once on resume (at-least-once outbound). Durability is against **process kill** (data in OS buffers survives); power-loss durability needs `fsync` and is opt-in (see D1).

## Main ideas

- **One durable per-session run journal, not three parallel stores.** `<sessions_dir>/<safe key>.journal` beside the existing `<safe key>.jsonl` (`JSONLSessionStore`, `src/session/manager.rs:108–143`), reusing `safe_filename`/path conventions. **The extension must NOT be `.jsonl`**: `SessionStore::list_sessions` scans every `*.jsonl` in the directory (`manager.rs:455–468`) and would list `<key>.journal.jsonl` as a bogus session — the existing `.archive` file (`manager.rs:132–136`) is named non-`jsonl` for exactly this reason. Content is still JSON lines. The journal must also follow the session through `delete_session` (`manager.rs:405/1112`, add `delete_journal` next to `delete_archive`), `clear` (`652`), `rename_session` (`1365`) and `fork_session_before_user_index` (`1131`; a fork gets **no** journal — it starts clean). It hosts three record types: `tool_intent`, `step_checkpoint`, `inbox_ledger`. Append-only + torn-trailing-line tolerance + monotonic `seq` per journal.
  *Rejected:* stuffing step checkpoints/tool intents into `session.metadata` + `RUNTIME_CHECKPOINT_KEY` — every `set_runtime_checkpoint` (`agent_loop.rs:1254–1267`) triggers a full atomic session rewrite; once per tool call that is brutal write amplification and multiplies the clobber hazard the comment at `agent_loop.rs:1249–1253` warns about. *Rejected:* three separate durable stores — three failure modes, three eviction stories, three recoveries to write.
- **Extend, don't replace, the restore path.** `restore_runtime_checkpoint` already materializes `assistant_message`, `completed_tool_results`, and synthetic *"interrupted"* results for `pending_tool_calls` (keys read at `agent_loop.rs:2239/2250/2265`), with suffix/prefix overlap detection making re-materialization idempotent. **Verified gap: nothing in-tree writes those keys today** — the runner only emits the three coarse `{type}` markers. WS2 is therefore mostly a *writer-side* job: emit full snapshots in exactly that payload shape via one shared `checkpoint_payload` builder module used by both writer and restore, so the contract cannot drift silently (restore returning `false` is a silent no-op today).
- **Recovery policy = the Pi replay semantics, decided per call id:** intent without terminal state ⇒ `replay_safe()` true → re-execute with identical args (a subagent-style tool finds its existing run and waits for it); false → synthesize the interrupted tool result and never repeat. Terminal record present → never re-execute (at-most-once for everything completed).
- **`Tool::replay_safe(&self) -> bool` with default `false`** — same conservative default pattern as `exclusive/read_only/concurrency_safe` (`src/agent/tools/base.rs:72–88`). A follow-up one-line opt-in sweep marks provably idempotent read-only tools (filesystem reads). `spawn` opts in only after its idempotency mechanism (deterministic task id) exists.
- **Unsafe tools are never auto-re-executed** — which is also what keeps the in-memory approval broker (`src/agent/tool_approval.rs`) out of the recovery path: no replay ⇒ no unexpected re-prompt. Durable approvals are explicitly P1.
- **"Queued messages stay queued" needs the ledger, not the bus.** `MessageBus` is in-memory `tokio::mpsc` (`src/bus/queue.rs:127–136`); queued-but-unprocessed inbound dies with the process. The inbox ledger records submissions at enqueue time (`Queued → Processing → Done{outcome}`), and startup re-drives `Queued/Processing` entries — with `request_id` making channel-side redelivery safe. This is why WS3 is load-bearing for WS2's promise, not an optional add-on.
- **Model request cut off ⇒ re-send; partial answer stays marked aborted.** Restore already re-inserts `assistant_message`; we add an `_aborted: true` marker (new content marker alongside `_hidden_history`-style keys in `src/session/keys.rs`) so the partial text is visibly not a completed reply.
- **Single shared `turn_id`** (per turn) + provider `call_id` (per tool call, `ToolCallRequest.id`, `src/providers/base.rs:13–21`) + `request_id` (per inbound) form the identity spine of all three workstreams; journals key tool records by `(turn_id, call_id)` so provider call-id reuse across turns cannot collide. Batches (`partition_tool_batches`, `runner.rs:822`) are safe because intents are per call id.
- **ACP mode gets no auto-resume** (client-driven `session/load` + the existing lazy restore at `agent_loop.rs:1622/1980`) — auto-resuming under a client that replays transcripts (`src/agent/acp/replay.rs` — note the name collision: that "replay" is transcript replay, ours is tool replay; new module is named `recovery`) would double-feed the client.

## Strategy

- **Sequencing: foundations → WS1 → WS2 → WS3 → subagent/edge hardening.** Each step compiles green and is independently testable. Recovery logic is built once (`recovery` module) and shared: tool replay (WS1) and resume (WS2) both consume journal scans.
- **Behavior is additive and inert until data exists**: with no journal/checkpoint records, startup behaves exactly as today. Auto-resume activates only for runs whose journal shows no terminal record, no cancel record, and that are within the resume age limit (D7). One kill switch is kept (`auto_resume`, default on; see D7) because resuming a run on startup is the only behavior change an operator can be surprised by.
- **Rollout safety**: re-execution on resume is safe-by-construction (declared `replay_safe` only) but is still surprising — every replay/synthesis logs loudly with turn/call ids. Journal write failures degrade to `log::error!` and let the tool call proceed (availability over durability), mark the run `degraded` and tell the user — see S2 "Journal degradation".
- **Step 0 comes first (D0)**: record a green baseline and confirm the facts this plan assumes about the code before writing anything (several assumptions below were found wrong or unproven in review).
- **Every step ends with the verification gate** in "Verification protocol" below; a step is not done until its gate output is recorded in the PR/commit notes.
- **Migration/compat**: `RUNTIME_CHECKPOINT_KEY` metadata checkpoints remain readable as a fallback for pre-feature sessions but are no longer written per step (see D6); `restore_runtime_checkpoint` is extended, not forked; old sessions restore as today.
- **Single-writer assumption** for `sessions_dir` (one rust-bot process per workspace) — same as current `SessionManager`; enforced via an advisory lock file (S2).
- **Rollback**: revert the runner emit-sites and the resume-on-startup hook; journals are append-only sidecars and can be ignored or deleted safely.

## Step plan

Steps are sized **S** (≤½ day), **M** (1–2 days), **L** (3–5 days).

### Phase 0 — Foundations

**D0. Baseline + assumption audit (no production code)** *(S)* — added in review
- Do: (1) record a green baseline on a clean tree: `cargo build`, `cargo test`, and `cargo clippy --all-targets` / `cargo fmt --check` if the repo already runs them (note the pre-existing failures so they are not blamed on this work); `default-members = ["."]` so root-crate `cargo test` does not cover `web-chat`/`chat-ui`/`websockets-chat`; (2) re-verify every `file:line` reference in this plan with `grep` — line numbers drift (known stale: subagent task id is at `subagent.rs:319`, not ~172; `process_system_message_inner` is at `agent_loop.rs:1602`; `write_atomic` lives in `src/utils/fs.rs:39`, not `session/manager.rs`); (3) answer in writing, from the code, the **blocking questions**: (a) *when is the user message of the in-flight turn written to `<key>.jsonl`?* (current reading: only at turn end by `persist_finished_turn` → `save_turn`, `agent_loop.rs:1756–1778`, so a mid-turn crash leaves it only in memory); (b) *does `write_temp_file` in `utils/fs.rs` fsync?* (`sync_all`/`sync_data` were not found under `src/session/`); (c) *which concrete tool names exist* for the D2 opt-in list (the names `read_file`/`grep`/`glob`/`list_dir` are assumed, not verified); (d) *do `process_direct` callers in `run_agent`/`run_api` (`cli/commands.rs:715/963`) ever call `AgentLoop::run`?* (current reading: only `run_gateway` does, `commands.rs:1710`).
- Acceptance: baseline numbers + the four answers written into the top of this file under "D0 findings"; any answer that contradicts the plan triggers a plan edit **before** D1 starts.

**D1. Run-journal module + record schema** *(M)*
- Files: new `src/session/journal.rs` (+ export in `src/session/mod.rs`); reuse `safe_filename` and path conventions from `src/session/manager.rs:108–143` and `write_atomic` from `src/utils/fs.rs:39`. File name `<safe key>.journal` (**not** `.jsonl`, see Main ideas). Session lifecycle hooks: `delete_journal` beside `delete_archive` (`manager.rs:103/451`), `clear` and `rename_session` move/clear the journal, `fork_session_before_user_index` creates none.
- Durability level: appends are `write` + `flush` (survives process kill); `sync_data` on `Intent` records of non-`replay_safe` tools and on `Done` ledger records is a config option (`durability.fsync`, default **on for those two record kinds only**, measured cost in D14). Per-journal in-process mutex; one record = one `write_all` of a single line (no interleaving between concurrent tool tasks).
- Changes: `RunJournal` (open/append/scan per session key) with serde records: `ToolIntent {seq, ts, turn_id, call_id, tool_name, args, replay_safe, status: Intent|Done|Error|Blocked, result: Option<ResultRef>}` (large results via `maybe_persist_tool_result` spill refs, `runner.rs:20`); `StepCheckpoint {seq, turn_id, kind: PreModel|PreTool|PreCompaction|Final, payload}`; `InboxRecord {seq, request_id, channel, chat_id, session_key, state: Queued|Processing|Done, outcome}`. Schema carries `"v": 1`. Torn trailing line tolerated (skip + warn); `seq` gaps reported.
- Extra record kinds added in review: `TurnStart {seq, turn_id, session_key, request_id?, user_message, attempt}` (see D5: the in-flight user message is otherwise lost), `TurnEnd {turn_id, outcome: Completed|Cancelled|Failed|Abandoned}` (the terminal record; `/stop` writes `Cancelled`), `Degraded {turn_id, reason}`.
- Journal dir is created lazily; read-only/full disk ⇒ `Degraded` path, never a panic (journal code must not `unwrap` on I/O).
- Acceptance: append→scan round-trip; torn-tail tolerated; spill refs resolve; eviction API compacts cleanly; **`list_sessions` does not list journals**; `delete_session`/`clear`/`rename_session`/fork leave no orphan or stale journal; concurrent appends from N threads yield N intact lines.
- Verify: `cargo test session::journal` and `cargo test session::manager` (existing manager tests must stay green).

**D2. `Tool::replay_safe()`** *(S)*
- Files: `src/agent/tools/base.rs` (new default-false method next to `concurrency_safe` at line 86, docs state the contract: re-execution with identical args must not produce user-visible duplicate effects **and must not depend on state the first run changed**), opt-in overrides in read-only tools. **The tool names/files are unverified** (`fn name()` returns `String`, `base.rs:51`; filesystem/search tools live in `tools/filesystem.rs`, `search.rs`, `shell.rs`, `spawn.rs`, `message.rs`, plus MCP and ACP tools) — D0 produces the real list. Classification rule written in the doc comment: `read_only() == true` is necessary but **not sufficient** (e.g. a read-only tool that reads a file the interrupted turn was writing, or `web` fetches with side effects, must stay `false`); MCP and `acp` tools are always `false` (external, opaque effects).
- Acceptance: default `false` everywhere else (assert for a sample incl. `message`, `spawn`, `exec`); schema/registry unchanged; an exhaustive test iterates the real `ToolRegistry` and asserts the exact set of `replay_safe() == true` tool names against an expected list, so any future opt-in is a deliberate diff.
- Verify: `cargo test tools::` ; `cargo build` (all `impl Tool` sites still compile — default method means no mass edit).

### Phase 1 — Tool intent WAL (WS1)

**D3. Bracket `tool.execute` with journal writes** *(M)*
- Files: `src/agent/runner.rs` (`run_tool` 871–1030, execute at 987–992; hook-blocked path `execute_tools_honoring_decision` 746), `AgentRunSpec` (50) gains `journal: Option<Arc<RunJournal>>` + `turn_id`; `src/agent/agent_loop.rs` (spec construction at 1188–1213 passes them). **`AgentRunSpec { .. }` is constructed in 34 places** (runner.rs 31 — mostly tests, subagent.rs 1, agent_loop.rs 1, memory.rs 1): the new fields must default to `None` via a constructor/`Default` so existing literals do not all need editing; subagent and memory runs pass `None` (no journal).
- Changes: write `Intent` before execute, `Done|Error` after (result stored by ref); `Blocked` for hook denials. Concurrent batches: one record per call id. The `Intent` is appended **before** `tool.execute` and must reach the OS before execute starts (`write_all` + `flush` completed, not queued on a background task). **Cancellation:** `/stop` aborts the task (`agent_loop.rs:1062–1083`) — an aborted run must write `Cancelled` terminals for in-flight intents (best effort in a drop guard) and a `TurnEnd{Cancelled}`; otherwise startup would "resume" something the user deliberately stopped.
- Acceptance: instrumented tool + journal scan proves Intent-precedes-execute and terminal-follows; crash between = intent without terminal; `/stop` mid-tool leaves no resumable run; the 31 existing runner tests pass untouched.
- Verify: `cargo test agent::runner` (full existing module) + new `runner_journal_*` tests.

**D4. Recovery policy + interrupted synthesis** *(M)*
- Files: new `src/agent/recovery.rs`; `src/agent/agent_loop.rs` `restore_runtime_checkpoint` `pending_tool_calls` path (2264–2282) delegates per call id: terminal present → skip; `replay_safe` → re-execute once (same args) and append its result; else → synthetic "interrupted" result (today's behavior, kept verbatim as the unsafe fallback). `execute` semantics unchanged.
- Policy additions from review: (a) a per-turn `attempt` counter in `TurnStart`; a turn whose resume attempts reach `max_resume_attempts` (default 2) is closed with `TurnEnd{Abandoned}` and an "interrupted, not resumed" assistant note — this stops a poison turn that crashes the process from crash-looping forever on every restart; (b) a safe re-execution that fails or itself crashes is recorded as `Error` and never retried a second time; (c) the resumed args are taken from the `Intent` record, never re-derived from the model.
- Acceptance: policy matrix unit tests (see §Testing); mock execution counters prove unsafe-at-most-once and safe-exactly-once; `spawn` re-execution defers to D12 semantics; poison-turn test: a tool that kills the process every time is resumed at most `max_resume_attempts` times.
- Verify: `cargo test agent::recovery`; `cargo test agent::agent_loop` (existing restore tests unchanged and green).

### Phase 2 — Step checkpoints (WS2)

**D5. Full checkpoint writer in the restore-compatible payload shape** *(M)*
- Files: new `src/agent/checkpoint_payload.rs` (the single builder: `assistant_message` / `completed_tool_results` / `pending_tool_calls`); `src/agent/runner.rs` emit sites (1246 PreTool, 1361, 1472 Final, + new PreModel at each model request; partial assistant at 1417 marked `_aborted: true`, marker const in `src/session/keys.rs`).
- **Blocking design point found in review — the user message of the in-flight turn is not durable.** `persist_finished_turn` → `save_turn` (`agent_loop.rs:1756–1778`) writes the turn's messages only when the turn ends; the checkpoint payload shape that `restore_runtime_checkpoint` reads (`2227+`) holds `assistant_message`, `completed_tool_results`, `pending_tool_calls` but **no user message and no earlier tool-loop messages** (only the *last* assistant message and its results). So after a crash, restore would append an assistant/tool fragment to a session that never saw the user's prompt, and a multi-iteration turn would lose every iteration but the last. Resolution: `TurnStart` (D1) stores the user message + `initial_messages` delta, and each `PreModel`/`PreTool` checkpoint stores the **full list of messages produced so far in this turn** (turn-local, not whole session; spill large tool results by ref). Restore rebuilds `session.messages += turn_messages` from the journal, with the existing suffix/prefix overlap detection as the idempotency guard. If D0 (a) shows the user message *is* saved earlier than assumed, simplify accordingly.
- Acceptance: builder ↔ `restore_runtime_checkpoint` round-trip tests green (existing overlap-detection tests around `agent_loop.rs:2208+` still pass); aborted marking visible; **multi-iteration turn** (model→tools→model→tools→crash) restores all iterations plus the user message, not only the last step.
- Verify: `cargo test agent::checkpoint_payload`; `cargo test agent::agent_loop`.

**D6. Journal-backed step persistence + metadata compatibility** *(M)*
- Files: `src/agent/agent_loop.rs` (`set_runtime_checkpoint` 1254: also append `StepCheckpoint`; `restore_runtime_checkpoint` 2227: journal-first, `RUNTIME_CHECKPOINT_KEY` fallback; `clear_runtime_checkpoint` 2315: write `Final` run record + clear metadata).
- Compatibility rule (was ambiguous in the first draft): the journal is the source of truth for new runs; the `RUNTIME_CHECKPOINT_KEY` metadata checkpoint is **read-only fallback** for sessions written before this feature and is no longer written per step. The per-step full-session rewrite that exists today (`checkpoint_callback` at `agent_loop.rs:1154–1158` → `set_runtime_checkpoint` 1254–1267 → `manager.save`) is removed, which is the write-amplification win. If both exist for one turn, the journal wins and the metadata key is cleared.
- Acceptance: crash before tool N resumes with calls 1..N-1 done and N pending (step granularity proven, not whole-turn reset); pre-feature sessions still restore; a session with both journal and metadata checkpoint restores once, not twice (no duplicated messages).
- Verify: `cargo test agent::agent_loop`; manual timing note: save count per tool call drops from 1 full rewrite to 0 (assert via a counting store in a test).

### Phase 3 — Auto-resume + compaction

**D7. Startup scan + resume driver** *(L)*
- Files: new `src/agent/resume.rs`; `src/agent/agent_loop.rs` (`run()` 1303: before the consume loop, scan journals/sessions for runs without terminal record, enqueue one resume task per session reusing the turn entry (`process_message_inner` 1945) with a `resume` flag — no fabricated user message); wire into startup paths in `src/cli/` (agent interactive + `--message` one-shot, api, gateway, channels). ACP mode: skip auto-resume (scope note in docs).
- **Per-mode wiring (verified in review, `cli/commands.rs`):** only `run_gateway` (1166) drives `AgentLoop::run()` (call at 1710, inside `tokio::join!`); `run_agent` (715) and `run_api` (963) go through `process_direct` (1246/1431/1901) and never run the consume loop; `acp` has its own runtime (`cli/acp.rs`). So: **gateway** = resume driver before `run()`'s consume loop; **api** = resume driver at server start, driven through `process_direct`; **interactive `agent`** = *offer* (print "turn X was interrupted — resume? [y/N]", default no) because a human is present and auto-running tools on launch is surprising; **`agent --message` one-shot** = no auto-resume (it is a different invocation than the interrupted one) but prints a one-line notice that unfinished runs exist; **acp** = none. Resume uses `process_direct`-style entry with a `resume` flag, not a fabricated user message.
- Resume guards (review): skip runs whose `TurnEnd` exists (incl. `Cancelled`); skip runs older than `resume_max_age` (default 24h; stale tool calls on old state are dangerous) and report them instead; honor `auto_resume=false`; cap concurrent resumes (default 4) so a restart after a busy gateway does not stampede the LLM provider; resume only sessions whose key/channel is still configured.
- Acceptance: kill-during-run integration tests pass (below); re-sent model request observable exactly once via `MockLlm::requests()`; no duplicate final response; resume logged with turn id; stale/cancelled/abandoned runs are not resumed; interactive mode prompts instead of auto-running.
- Verify: `cargo test durability::resume`; manual: kill gateway mid-tool, restart, confirm single completion and log lines `resume turn=… attempt=…`.

**D8. Compaction as a journaled step** *(S)*
- Files: `src/agent/agent_loop.rs` (auto-compaction `prepare_session` sites 1653/2044): `PreCompaction` record before, checkpoint after; interleave with `SessionManager::save` archive logic (`consolidated_len`/`take_consolidated_prefix`, `manager.rs:1060–1071`) always resolving the canonical session (the 1253 clobber comment).
- Acceptance: crash at `PreCompaction` resumes with history intact (no loss/duplication); compaction/archive state after resume equals state after an uninterrupted run (compare message-key sets and archive length).
- Verify: `cargo test session::manager` (archive/consolidation tests) + `durability::compaction`.

### Phase 4 — Exactly-once inbound (WS3)

**D9. `request_id` plumbing** *(S)*
- Files: `src/bus/events.rs:9` (`pub request_id: Option<String>`, `#[serde(default)]`); adapters (`src/channels/*`, websockets ingest, api, gateway; `process_direct` 2366 caller) map platform message ids; **no synthetic ids**.
- **`InboundMessage { .. }` is constructed in 23 places across 8 `src` files** (`command/builtin.rs` 13, `command/router.rs` 2, `subagent.rs` 2, `bus/events.rs` 2, `agent_loop.rs`, `channels/base.rs`, `gateway_services.rs`, `bus/queue.rs`; none in `tests/`). Add `request_id` to every literal (or add a `new(..)` constructor and migrate) — budget for this in the size; compile errors enumerate the sites. Internally generated messages (subagent announcements, command replies) get `None` and are never deduped.
- The id must be namespaced `<channel>:<chat_id>:<platform id>` to avoid cross-channel collisions; the ledger is per session key.
- Acceptance: adapters with stable ids fill it; absent ⇒ behavior identical to today; every existing channel/command test passes unchanged.
- Verify: `cargo build` (all 23 sites), `cargo test command::`, `cargo test channels::`.

**D10. Inbox ledger: dedupe + outcome replay + eviction** *(M)*
- Files: `src/session/journal.rs` (`InboxRecord` + index); `src/agent/agent_loop.rs` consume loop (1315–1330): `ledger.begin(request_id)` before processing — `Done` → re-emit stored outcome (truncated) as outbound and drop; `Queued|Processing` → drop duplicate; new → `Processing` → proceed → `Done{outcome}` after the turn finalizes. Eviction: TTL (default 14d) + cap (default 10k/session), applied at startup and on write, journal-compacted on evict.
- Clarifications (review): (a) **the consume loop is the wrong place for the `Queued` write** — `run()` only sees a message after `consume_inbound` (`agent_loop.rs:1315`), when it has already left the in-memory bus; a crash between `publish_inbound` and consume loses it. Write `Queued` in the publisher path (`MessageBus::publish_inbound`, `bus/queue.rs`) or the channel adapter, before the message enters the mpsc; (b) the ledger lives in the **session** journal, but the session key of an inbound message is only known after routing (`session_key_override`, `bus/events.rs`) — define one function `resolve_session_key(&InboundMessage)` used by both ledger and loop; (c) outcome stored = the reply text (truncated, default 4 KB) plus `turn_id`, not the whole transcript; (d) `Done` is written *after* the outbound send, which gives the at-least-once-outbound window stated in the High-level view.
- Acceptance: duplicate delivery ⇒ exactly one model turn; retry gets the original outcome back; absent key passes through; evicted key reprocesses (documented); crash between `publish` and `consume` re-drives the message on restart.
- Verify: `cargo test session::journal` (ledger), `cargo test bus::`, `durability::inbound_idempotency`.

**D11. Startup reconciliation of ledger vs runs** *(M)*
- Files: `src/agent/resume.rs` + `agent_loop.rs`: `Queued|Processing` records whose turn shows completion → mark `Done`; whose run is unfinished → re-drive after resume; ordering preserved per session.
- Ordering rule: per session, resumed run first, then `Queued` records in `seq` order; `Queued` records for a session whose resume was `Abandoned` are still delivered (the abandoned turn must not block the queue forever).
- Acceptance: message queued behind an active turn survives a crash and is processed exactly once after restart; ordering preserved.
- Verify: `durability::queued_stays_queued` (I4).

### Phase 5 — Idempotent spawn + hardening

**D12. Replay-safe `spawn` (find existing work, wait)** *(M)*
- Files: `src/agent/tools/spawn.rs`, `src/agent/subagent.rs` (random 8-char task id at `subagent.rs:319` (`uuid::Uuid::new_v4()[..8]`; `running_tasks` map at 229 holds `std::thread::JoinHandle`s) replaced/overridden by caller-supplied deterministic id = `(turn_id, call_id)` hash; `SubagentManager` lookup by id): running → return "already running (id: …)" ack (no second thread, waits for completion notification); finished → return stored result; crash-orphaned (`Running`, no live handle) → consult the subagent's own session for existing work, else respawn once. Only then does `spawn` override `replay_safe() -> true`.
- Scope check (review): the existing persisted subagent records (`subagent.rs:1494`-area, `ChildAgentStore` is the ACP half) must be reloadable at startup for the "crash-orphaned" lookup; if they are not persisted today, D12 needs a write path first — confirm in D0 and resize. Whether a respawned subagent can truly resume from its own session or only restart is **unverified**; the documented fallback is "respawn once and note it".
- Acceptance: replaying the same call id never starts two subagents; ack/result idempotent; unsafe-flag invariant test passes (the D2 exhaustive test now lists `spawn` as safe).
- Verify: `cargo test agent::subagent` (existing tests incl. `cancel_by_session_*` green) + `durability::subagent_replay` (I7).

**D13. Approval non-interaction + failure-mode policy + docs** *(S)*
- Files: `src/agent/tool_approval.rs` (comment: recovery never touches the broker; interrupted unsafe ⇒ synthesized result only), module docs for `recovery`/`journal`, README durability section; journal-write-failure policy (log + proceed) implemented in D3/D6 sites.
- Also (review): document the config keys introduced (`auto_resume`, `resume_max_age`, `max_resume_attempts`, `durability.fsync`, ledger TTL/cap) in one table with defaults; add a `rust-bot` log line summary at startup ("journal: N unfinished runs, M queued").
- Acceptance: test proves unsafe recovery executes zero tools; doc review; every config key has a default and a test that the default applies when the key is absent.
- Verify: `cargo test tool_approval`; grep the repo that `recovery`/`resume` modules never reference `ToolApprovalBroker` (`grep -r ToolApprovalBroker src/agent/recovery.rs src/agent/resume.rs` returns nothing).

**D14. Crash/kill integration harness** *(M)*
- **Layout fix (review):** Cargo only auto-discovers `tests/*.rs` and `tests/*/main.rs`; a bare `tests/durability/*.rs` directory is **not** built. Use a single `tests/durability_test.rs` with `mod durability;` / `mod support;` (pattern of `tests/acp_agent_test.rs:28`, which declares `mod support;`), plus `tests/durability/mod.rs` and submodules. (`tests/mod.rs` exists but is not a test target by itself.)
- Harness APIs actually available: `tests/support/mock_llm.rs` provides `MockReply::text/tool_call`, `MockLlm::start/api_base/push/requests/everything_seen` (an HTTP openai-compat mock — suited to real-binary tests); `tests/support/mod.rs` provides `ScriptedProvider`, `fixture`, `restarted_runtime` (in-process, ACP-runtime based), `wait_until`, `long_running_command`. In-process crash tests must use these or a new `AgentLoop`-level fixture (D14 builds it if absent — verify first that a non-ACP `AgentLoop` can be assembled from a tempdir workspace). **There is no "kill the runner" seam today**: add a test-only crash hook (`#[cfg(test)]`/feature `crash-points`) that panics or aborts at named points (`after_intent`, `before_terminal`, `after_checkpoint`) rather than relying on dropping a runtime.
- Real-kill tests: build with `env!("CARGO_BIN_EXE_rust-bot")`, kill via `Child::kill()` (cross-platform; `taskkill /F` only as Windows fallback), and assert on files in the temp workspace + `MockLlm::requests()`; the binary must be runnable without network/keys against the mock's `api_base`.
- Acceptance: suite green locally (`cargo test`); see §Testing; each integration test runs in < 60 s and does not leave `rust-bot`/`PING` processes behind (assert with `running_process_count`). CI: workflows are release-build-only today — adding a `cargo test` job is **part of this plan's exit criteria**, not a nice-to-have (see S2 "No CI gate").
- Verify: `cargo test --test durability_test` ; `cargo test` (whole root crate) ; repeat the kill tests 20× in a loop to expose flakiness before merge.

## Gaps and risks

### S1 — Critical
- **In-flight user message and earlier tool-loop messages are not in the checkpoint (added in review).** The current payload shape carries only the last assistant message and its tool results, and the user's prompt reaches `<key>.jsonl` only when the turn ends (`persist_finished_turn`, `agent_loop.rs:1756`). A resume built on the existing shape would produce a transcript with no prompt and only the final step. *Impact:* resume "succeeds" with a corrupt, incoherent history; the model answers a question it can no longer see. *Resolution:* `TurnStart` + full turn-local message list in every step checkpoint (D1/D5); acceptance test with a multi-iteration turn; D0 confirms the premise first.
- **Queued inbound messages are not durable in the bus.** `MessageBus` is in-memory mpsc (`bus/queue.rs:127–136`); a crash loses queued-but-unprocessed messages, so WS2's "queued messages stay queued" is false without WS3's ledger + re-drive (D10/D11). *Impact:* silent message loss while claiming durability. *Resolution:* journal at enqueue; D10/D11 are release-blocking for the durability claim, not follow-ups; acceptance test "queued-stays-queued".
- **Checkpoint writer/restore payload contract drift is a silent failure.** Nothing writes `assistant_message`/`completed_tool_results`/`pending_tool_calls` today (reads only, `agent_loop.rs:2239/2250/2265`); `restore_runtime_checkpoint` returns `false` silently on mismatch. *Impact:* the entire WS2 resume path no-ops with zero signal. *Resolution:* one shared builder module (D5) + builder↔restore round-trip tests as the merge gate.

### S2 — High
- **Crash loops / poison turns (added in review).** A turn that itself kills the process (OOM, panic in a tool, a segfaulting MCP server) is resumed on every restart, forever. *Impact:* service never comes up healthy. *Resolution:* `attempt` counter + `max_resume_attempts` + `Abandoned` terminal (D4/D7); test with a self-killing tool.
- **Resuming what the user cancelled or what went stale (added in review).** `/stop` aborts the task (`agent_loop.rs:1062–1083`); without a `Cancelled` terminal a restart would revive it. Days-old runs would act on outdated state. *Resolution:* `TurnEnd{Cancelled}` (D3), `resume_max_age` (D7), `auto_resume` kill switch; interactive mode asks (D7).
- **No CI gate for the durability claim (promoted from S3 in review).** Workflows are release-only (`.github/workflows/release-*.yml`), so none of the I1–I16 tests would ever run automatically; a durability feature with no continuous test is a regression waiting to happen. *Resolution:* add a `cargo test` workflow (Windows + Linux) as part of D14's exit criteria.
- **Journal degradation must be visible, not silent (added in review).** "Log and proceed" on journal write failure means the user believes they have crash safety when they do not. *Resolution:* mark the run `Degraded`, emit one user-visible notice per session, expose a counter; test with a read-only journal dir.
- **Journal lifecycle bugs (added in review).** Wrong extension would make `list_sessions` list journals; missing hooks would leak journals on `delete_session`/`clear`/`rename_session`/fork or attach a stale journal to a new session of the same key. *Resolution:* D1 acceptance tests for each lifecycle operation.
- **`replay_safe` misclassification ⇒ duplicate side effects** (double chat sends, double spawns, double file writes). *Impact:* the exact harm replay safety exists to prevent. *Resolution:* conservative default `false` (D2); opt-in only after review; `spawn` idempotency (D12) lands before its flag flips; mock-counter tests assert declared-safe tools execute exactly once across recovery.
- **Recovery must not re-trigger approval prompts.** The broker is in-memory and empty after restart; re-proposing an interrupted unsafe tool would surprise users or deadlock unattended resumes. *Impact:* prompt storms / stuck resume. *Resolution:* hard policy — unsafe interrupted calls are never re-executed (synthesized result only), asserted by test (D13). Durable approvals = P1, out of scope.
- **Crash-orphaned subagents duplicate work on naive replay.** Subagent threads die with the process (`running_tasks` is in-memory); re-running `spawn` blind would fork parallel work. *Impact:* duplicated side effects in subagent tasks. *Resolution:* D12 (deterministic id, find-and-wait, session-based "existing work" check, documented one-respawn fallback).
- **Multi-process `sessions_dir` writers** (e.g. gateway + interactive on one workspace) interleave journal appends and session saves. *Impact:* corrupt/ambiguous recovery state. *Resolution:* advisory lock file at startup + explicit single-writer documentation; detect-and-warn at minimum before release.

### S3 — Medium
- **Step-checkpoint write amplification.** `set_runtime_checkpoint` rewrites the whole session file atomically per call (1254–1267 + `manager.rs:395`); per-step that's heavy on long sessions. *Impact:* latency/CPU in gateway load. *Resolution:* step payloads live in the journal (small appends); metadata checkpoint throttled to turn boundaries + crash-critical points; measure in D14.
- **`request_id` adoption gaps.** Platforms/adapters without stable message ids (or redelivery) get at-least-once only; synthesizing ids would break retry semantics. *Impact:* per-channel guarantee variance. *Resolution:* document per-adapter guarantees; never synthesize.
- **Ledger eviction vs redelivery window.** TTL too short ⇒ a slow retry reprocesses; too long ⇒ journal bloat. *Impact:* duplicate turns or disk growth. *Resolution:* 14d/10k defaults + config knobs + compaction on evict; tune during rollout.
- **Compaction crash consistency** (summary vs archive overlap mid-`prepare_session`, sites 1653/2044). *Impact:* lost or duplicated history segment. *Resolution:* `PreCompaction`/post records (D8) + existing overlap detection; integration test.
- **Journal torn tails / partial appends on Windows.** *Impact:* unreadable last record. *Resolution:* line-granular parse, skip-and-warn trailing garbage, seq-gap reporting (D1).
- **Huge tool results in the WAL.** *Impact:* journal bloat kills append locality. *Resolution:* spill refs via `maybe_persist_tool_result` (`runner.rs:20/257`), WAL stores the ref.
- **Plan line numbers and tool names drift (added in review).** Several references in the first draft were already wrong (subagent id site, `write_atomic` location, test directory layout, tool names). *Resolution:* D0 re-verification; line refs are hints, symbols are the contract.
- **Unbounded journal growth for sessions with no ledger traffic.** `tool_intent`/checkpoint records of completed turns are dead weight. *Resolution:* on `TurnEnd`, compact the journal to keep only `TurnEnd` markers and ledger records (D1 eviction API; run at turn end, not per record).

### S4 — Low
- **"Replay" name collision** with `src/agent/acp/replay.rs` (client transcript replay) — new code lives in `recovery`/`journal`, documented distinction.
- **`execute` has no streaming** ⇒ "partial output if any" is almost always empty (only terminal-write race artifacts). *Resolution:* document; optional progress hook later.
- **Provider call-id reuse across turns** — mitigated by `(turn_id, call_id)` keys; assert in tests.
- **Schema versioning** (`"v": 1` from day one) and observability counters (replays, synthesized, dedupe hits) — nice-to-haves; counters are cheap, include them.

## Verification protocol (applies to every step D0–D14)

A step is "done" only when all of these were run and the output was recorded in the commit/PR notes:

1. `cargo build` — clean, no new warnings from touched files.
2. `cargo test <step-specific filter>` (see each step's **Verify** line) — new tests fail before the change and pass after (write the test first, watch it fail, for D3/D4/D5/D10).
3. `cargo test` for the whole root crate — compare against the D0 baseline; only the D0-listed pre-existing failures may remain. Remember `default-members = ["."]`: run `cargo test --workspace` once per phase too, because `websockets-chat`/`chat-ui` may share types.
4. `cargo clippy --all-targets` and `cargo fmt --check` if the repo uses them (decide in D0; do not introduce a new lint regime here).
5. **Inertness check** (Phases 1–3): run one normal scripted turn with the journal feature present and no crash; the session file, the outbound reply and the `MockLlm::requests()` count must be byte-identical to a run on the D0 baseline commit (the "additive and inert" claim is tested, not asserted).
6. **Mutation check** for the recovery policy (D4) and the ledger (D10): temporarily invert the key condition (`replay_safe` check, `request_id` lookup) and confirm at least one test fails; revert. This proves the tests actually exercise the logic.
7. **Windows check**: tests that touch files/processes run on Windows (the author's platform) — path length, `safe_filename` collisions, file-in-use errors on rename, `Child::kill` semantics.
8. Update the "D0 findings"/status table in this file (what was verified, date, commit).

Phase gates: after Phase 1 run I1; after Phase 3 run I1, I2, I4, I8 and the 20× loop; after Phase 4 run I3, I4, I5; release gate = acceptance criteria below.

## Testing scenarios

### Automated — unit

| # | Area (file) | Key scenarios |
|---|---|---|
| U1 | `session/journal.rs` | append/scan round-trip; torn trailing line skipped; seq monotonic + gap detection; eviction (TTL/cap) leaves index consistent; spill-ref resolution |
| U2 | `tools/base.rs` + tool crates | `replay_safe()` default false for sample tools (`message`, `spawn`, `exec`, `write_file`); opt-ins true (`read_file`, `grep`, `glob`, `list_dir`); registry/schema unchanged |
| U3 | `runner.rs` WAL bracket | Intent written before execute (instrumented mock tool asserts ordering via journal scan); `Done`/`Error` terminal after; hook-blocked ⇒ `Blocked` terminal; concurrent batch (`partition_tool_batches`) ⇒ one intent per call id |
| U4 | `recovery.rs` policy matrix | intent-only + safe ⇒ re-execute exactly once (mock execution counter); intent-only + unsafe ⇒ zero executions + synthetic "interrupted" result; terminal ⇒ skip regardless; args identical on re-execution; re-execution result appended as normal `role: tool` message |
| U5 | `checkpoint_payload.rs` ↔ restore | builder→`restore_runtime_checkpoint` round-trip; overlap detection idempotence (double restore, cf. `agent_loop.rs:2208+`); partial assistant kept with `_aborted: true`; unknown-tool pending calls still get today's synthetic interrupted result |
| U6 | inbox ledger | duplicate `request_id` ⇒ one processing + outcome replay; dup while `Processing`/`Queued` ⇒ dropped; `Done` dup ⇒ original outcome re-emitted (truncated); missing `request_id` ⇒ passthrough; eviction boundary ⇒ evicted key reprocesses |
| U7 | `spawn` idempotency | same `(turn_id, call_id)` twice ⇒ one subagent thread, second call returns "already running"/stored result; crash-orphaned `Running` record ⇒ find-existing-work path; flag only set after mechanism verified |
| U8 | `agent_loop.rs` compat | `RUNTIME_CHECKPOINT_KEY` fallback restore for pre-feature sessions; `clear_runtime_checkpoint` emits `Final`; checkpoint save never clobbers live history (1253 invariant); journal + metadata both present ⇒ restored once |
| U9 | `session/manager.rs` lifecycle | `list_sessions` ignores `*.journal`; `delete_session`/`clear`/`rename_session` handle the journal; fork starts without one; same-key re-create does not inherit a stale journal |
| U10 | resume guards | `Cancelled`/`Abandoned`/stale (`resume_max_age`) runs not resumed; `auto_resume=false` honored; `attempt` limit enforced; concurrency cap respected |
| U11 | degradation | read-only/full journal dir ⇒ tool still runs, `Degraded` recorded, user notice emitted once, no panic |
| U12 | `/stop` | abort mid-tool writes `Cancelled` terminals + `TurnEnd{Cancelled}`; restart does not resume |
| U13 | identity | same provider `call_id` in two turns does not collide; `request_id` namespacing prevents cross-channel collisions |
| U14 | torn / corrupt journal | truncated last line, garbage middle line, duplicate `seq`, unknown `"v"` (future version ⇒ skip + warn, never crash), journal larger than expected |

### Automated — integration (incl. crash/kill simulation), `tests/durability/*` with `tests/support/mock_llm.rs`

| # | Scenario | Mechanics / assertions |
|---|---|---|
| I1 | **Deterministic in-process crash mid-tool** | Script `[tool_call A (safe), tool_call B (unsafe), text]` with `ScriptedProvider`/`MockLlm` + instrumented tools; crash via the **test-only crash hook** (`after_intent`) — there is no existing kill seam, D14 adds it; rebuild the loop over the same workspace → resume: A executed once more (total 2, same args), B zero more times + interrupted result in transcript, **user prompt and all earlier iterations present**, one final response |
| I2 | **Real kill-during-run** | Spawn built binary (`env!("CARGO_BIN_EXE_rust-bot")`, pattern of `tests/acp_binary_test.rs`) on scratch workspace, `MockLlm::api_base()` as provider; script a long-running tool call; `Child::kill()` mid-call; relaunch (gateway or api mode, per D7) → `MockLlm::requests()` shows the model request re-sent exactly once; transcript: partial answer `_aborted`, unsafe tool interrupted, safe tool re-run; one final response. **Preconditions to verify first:** a gateway/api start that needs no real channel credentials, and a tool that blocks long enough to be killed deterministically (use `long_running_command`) |
| I3 | **Idempotency replay** | Same `InboundMessage{request_id}` delivered twice (before crash and as retry after crash) ⇒ exactly one model turn total; retry receives the original outcome content; third delivery after ledger eviction reprocesses (documented) |
| I4 | **Queued stays queued** | Enqueue message 2 while turn 1 is active → crash → restart ⇒ message 2 processed exactly once after resume, in order |
| I5 | **Auto-resume per mode** | gateway + agent-interactive resume unfinished runs at startup; `acp` mode does **not** auto-resume (lazy restore only) |
| I6 | **Compaction crash** | Trigger compaction, crash at `PreCompaction` record, resume ⇒ history intact, no loss/duplication (compare message-key sets) |
| I7 | **Subagent replay** | Replay a `spawn` call after crash ⇒ no duplicate subagent; ack idempotent |
| I8 | **Batch partial crash** | Crash mid-batch (2 concurrent safe calls, one terminal, one intent-only) ⇒ completed call not re-run, pending call re-run once |
| I9 | **Crash at every boundary (sweep)** | Parametrised over all crash-hook points (`after_turn_start`, `before_model`, `mid_model_stream`, `after_intent`, `before_terminal`, `after_terminal_before_checkpoint`, `after_final_before_turn_end`, `before_ledger_done`): for each, restart and assert the invariants — one final assistant message, no lost/duplicated messages, safe ≤ +1 run, unsafe 0 re-runs. This is the test that backs acceptance criterion 2 ("kill at any step") — I1–I8 alone only sample it |
| I10 | **Double crash** | Crash during recovery itself (after resume start, before it finishes) ⇒ second restart still converges, `attempt` increments, poison turn ends `Abandoned` |
| I11 | **`/stop` then restart** | Stop mid-tool, kill, restart ⇒ nothing resumed |
| I12 | **Inertness / regression** | Normal turn on the feature build vs D0 baseline commit: identical session file contents and request count; existing suites unchanged |
| I13 | **Pre-feature session upgrade** | Session file + `RUNTIME_CHECKPOINT_KEY` metadata produced by the D0 baseline binary (commit it as a fixture) restores on the new build without duplication |
| I14 | **Crash-loop guard** | Tool that exits the process every time ⇒ at most `max_resume_attempts` resumes, then service starts and the user sees an "abandoned" note |
| I15 | **Concurrency / load** | 50 sessions with interleaved turns, kill, restart ⇒ all resolve, resume cap respected, journal appends never interleave mid-line |
| I16 | **Journal on disk-full / read-only** | Turn completes, `Degraded` recorded and surfaced |

### Manual

- Kill the interactive CLI mid-tool (`Ctrl+C` / `taskkill`), restart: resume messages, interrupted-tool note, no duplicate sends.
- Re-send an API/webhook request with the same id → single turn, original outcome returned.
- Long-session latency with per-step journal writes (timings in logs).
- `/stop` mid-turn then restart — interplay with lazy restore (`agent_loop.rs:1622`).
- Channel with no platform ids: verify at-least-once behavior matches documentation.
- Interactive CLI: after a kill, the next launch shows the "interrupted turn — resume?" prompt and answering `n` leaves the session usable and the run marked closed (does not re-prompt forever).
- Journal inspection: open a `.journal` file by hand after a real turn and confirm the records are readable, ordered by `seq`, and contain no secrets beyond what the session file already holds (tool args can contain tokens — decide in D1 whether to redact; default: same retention as the session file, documented).
- Disk usage: after 100 turns in one session, journal size is bounded (compaction at `TurnEnd` works).

### Edge cases & failure modes

Empty journal; journal with only terminal records; two interleaved turns in one session (turn-id scoping); provider-reused call ids; `session_key_override` (`bus/events.rs:27`) ledger→session mapping; system-message path (`process_system_message_inner` 1595) vs user path (1945) both resumable; media-heavy/spilled results; non-UTF-8-adjacent args serialization (`ToolCallRequest.arguments: HashMap<String, Value>`, `providers/base.rs:17`); Windows `safe_filename` collisions; journal write I/O error mid-turn (degrade, don't block); crash exactly between terminal write and `Final` run record (recovery must not re-run a completed call — terminal wins).

### Acceptance criteria (release gate)

1. I1–I16 green locally **and in the new CI job**; U1–U14 green; existing suites (runner tests, `agent_loop` tests, `session/manager.rs` tests, `tests/agent/*`, `tests/acp_*`) unbroken versus the D0 baseline; the 20× kill-loop passes with zero flakes.
2. Kill at **any** step boundary (backed by the I9 sweep, not by sampling) ⇒ restart completes the turn: safe tools re-run ≤1×, unsafe tools 0× with visible "interrupted" result, one final assistant message, no duplicate spawns; outbound send is documented at-least-once in the single window described in the High-level view.
3. Duplicate `request_id` delivery ⇒ one model turn; retry returns the original outcome.
3a. A turn that kills the process every time stops being resumed after `max_resume_attempts`; `/stop`ped and stale runs are never resumed.
4. A crash never corrupts or loses `<key>.jsonl` (existing `write_atomic`) and never silently no-ops restore (round-trip tests as gate).
5. Pre-feature sessions restore unchanged; `execute` signature and tool schemas unchanged.

### Test matrix (scenario × level)

| Scenario | Unit | Integration | Manual |
|---|:---:|:---:|:---:|
| WAL intent/terminal ordering & torn tail | U1, U3 | I1 | — |
| Replay policy (safe / unsafe / done) | U4 | I1, I2 | ✓ |
| Step checkpoint ↔ restore round-trip | U5, U8 | I1, I2 | ✓ |
| Kill-during-model-request re-send + aborted partial | U5 | I2 | ✓ |
| Queued messages survive crash | — | I4 | — |
| request_id dedupe + outcome replay | U6 | I3 | ✓ |
| Ledger eviction semantics | U6 | I3 | — |
| Batch partial crash | U3 | I8 | — |
| Compaction crash consistency | — | I6 | — |
| Subagent find-and-wait replay | U7 | I7 | — |
| Mode coverage (agent/gateway/acp) | — | I5 | ✓ |
| Journal write failure degradation | U1, U11 | I16 | ✓ |
| Every-boundary crash sweep | — | I9, I10 | — |
| Cancel / stale / crash-loop guards | U10, U12 | I11, I14 | ✓ |
| Journal lifecycle (list/delete/rename/fork) | U9 | — | — |
| Corrupt / future-version journal | U14 | — | — |
| Upgrade from pre-feature session | U8 | I13 | ✓ |
| Inertness vs baseline | — | I12 | — |
| Load / interleaving | U1 | I15 | — |

## D0 findings (fill in before D1 starts)

| Question | Answer (from code, with file:line) | Plan impact |
|---|---|---|
| Baseline `cargo test` result / pre-existing failures | 2026-10-12, branch `feature/durability-core` @ 0999be9. Lib: 2599 pass / 1 fail (`agent::tools::mcp::mcp_presets_api::tests::default_asset_has_no_duplicate_names`). Integration: `providers::anthropic_provider_test::*` and `transcription_provider_test::test_grok_*` fail (need network/keys); the full `cargo test --no-fail-fast` run then hung and was killed, so later test binaries have no baseline. `cargo clippy` has pre-existing warnings in `runner.rs`; `cargo fmt` was clean. | later gates compare against these |
| (a) When is the in-flight user message persisted? | **Confirmed** (2026-10-12): `process_message_inner` builds the messages, runs the turn, and only then calls `persist_finished_turn` → `save_turn` (`messages[skip..]`). A mid-turn crash leaves the user message only in memory. | S1 premise holds; `TurnStart` stores it, restore feeds `turn_messages` through `save_turn` |
| (b) Does `write_temp_file` (`utils/fs.rs`) fsync? | **Yes**: `file.sync_all()` before rename. Session rewrites are fsynced; journal appends are `flush` only, `sync_data` on `TurnStart`/`TurnEnd` and non-replay-safe intents. | D1 wording |
| (c) Real tool names for `replay_safe` opt-in | `read_file`, `list_dir` (`filesystem.rs`), `glob`, `grep` (`search.rs`) exist and are opted in; `message`, `spawn`, `exec`, `write_file`, `edit_file` asserted false. Only `grep` had `read_only()`. | D2 done |
| (d) Which modes call `AgentLoop::run()` | **Confirmed**: only `run_gateway` (`cli/commands.rs`, inside `tokio::join!`). `run_agent`/`run_api` use `process_direct`. | resume driver starts from `run()`; interactive/one-shot get a notice only; api not wired |
| Subagent records persisted for orphan lookup? | **No**: `SubagentManager.tasks` and `running_tasks` are in memory only, and subagent runs have no journal. After a crash the old subagent is gone, so there is nothing to "find". | D12 reduced to an in-process idempotency guard (deterministic task id); a replay after a crash respawns once |
| Can a non-ACP `AgentLoop` be built in tests from a tempdir? | Yes: `AgentLoop::new(bus, PlaceholderProvider, tempdir, Config::default(), None, None, None)` (`make_save_turn_loop`, agent_loop.rs tests). | D14 fixture |
| Journal lifecycle | `rename_session` only sets the title (key unchanged), so nothing to move. `delete_session`, `reset_session` (clear) delete the journal; fork deletes a stale target journal. | D1 done |

## Status

| Step | Status | Gate output recorded | Date / commit |
|---|---|---|---|
| D0 | done (baseline in table above; all questions answered) | baseline recorded | 2026-10-12 |
| D1 | done: `session/journal.rs`, lifecycle hooks in `session/manager.rs`. Follow-up 2026-10-13: storage moved behind the `JournalStore` trait (`SessionStore::journal_store()`), `FileJournalStore` in `session/journal_file.rs`, `InMemoryJournalStore`, shared conformance checks; no path-based journal access remains outside the file store. Not done: `durability.fsync` config key (fsync is fixed: on for `TurnStart`/`TurnEnd`, non-replay-safe intents, ledger `Queued`/`Done`). | `session::journal`, `session::manager` tests | 2026-10-12, uncommitted |
| D2 | done: `Tool::replay_safe()`; opt-ins `read_file`, `list_dir`, `glob`, `grep`, `spawn`; registry test pins the exact set | `only_the_reviewed_read_only_tools_are_replay_safe` | 2026-10-12, uncommitted |
| D3 | done: `agent/turn_journal.rs`, `AgentRunSpec.journal`, runner brackets `execute` and hook-blocked calls, `/stop` closes the turn as `Cancelled` via a drop guard. Not done: user-visible notice when the journal degrades (log + `Degraded` record only). | `runner_journal_*` | 2026-10-12, uncommitted |
| D4 | done: `agent/recovery.rs` (per-call policy, attempt cap, unfinished/to-restore scans, pruning); lazy restore and the resume driver use it. Safe calls are replayed by the resume driver only (restore itself never executes tools). | `agent::recovery` | 2026-10-12, uncommitted |
| D5 | done: `agent/checkpoint_payload.rs` is the single builder/reader; runner writes `StepCheckpoint`s with the turn-local messages. Not done: `_aborted` partial assistant marker (the runner has no partial text at a kill). | `checkpoint_payload`, `runner_journal_writes_a_step_checkpoint_*` | 2026-10-12, uncommitted |
| D6 | done: journal-first `restore_runtime_checkpoint`, metadata checkpoint is a read-only fallback and is no longer written per step when a journal is available; finished turns are pruned from the journal | `restore_*`, `pre_journal_sessions_still_restore_*` | 2026-10-12, uncommitted |
| D7 | done for gateway: `agent/resume.rs` planner + `AgentLoop::resume_interrupted_turns`, started from `run()`; `durability` config section (defaults tested). Interactive and one-shot `agent` print a notice only (no resume prompt); `api` mode is not wired; ACP never resumes. | `agent::resume`, `resume_*` | 2026-10-12, uncommitted |
| D8 | **not done** (`PreCompaction` journaling). Compaction is untouched; archive-before-session ordering in `SessionManager` still applies. | — | — |
| D9 | done: `InboundMessage.request_id`; set from the adapter-supplied `message_id` metadata in `channels/base.rs` (only email and WhatsApp supply one today); never invented | `bus::events` | 2026-10-12, uncommitted |
| D10 | done: `session/inbox.rs` ledger + `LedgerGuard` installed on the bus (`Queued` is written before the message enters the queue). Default cap is 1000 entries per session (plan said 10k; outcomes are up to 4 KB). | `session::inbox`, `bus::queue`, `a_redelivered_request_*` | 2026-10-12, uncommitted |
| D11 | done: startup reconciliation (`plan_redrive`, `redrive_pending_inbound`) after resume; entries from the running process are excluded by a process-start cutoff | `a_message_queued_before_a_crash_is_redriven_exactly_once`, `a_stopped_request_*` | 2026-10-12, uncommitted |
| D12 | done in reduced form: deterministic task id from `(turn_id, call_id)` via `agent/tool_call_context.rs`; `spawn` is replay safe. A replay after a crash respawns once (no subagent state survives a crash). | `agent::subagent`, `agent::tools::spawn` | 2026-10-12, uncommitted |
| D13 | done: approval-broker note, `docs/DURABILITY.md`, README section, config table. Startup summary line is logged by the gateway resume task. | `recovery`/`resume` source-scan tests | 2026-10-12, uncommitted |
| D14 | partly done: in-process crash harness (`agent/crash_points.rs`, test-only) and an 8-point kill sweep (`crash_*`) restarting a new `AgentLoop` over the same workspace; `.github/workflows/test.yml`; recovery suite passed 20× in a loop. **Not done:** real-binary `Child::kill()` tests, `tests/durability_test.rs` layout, mid-stream and double-crash cases, 50-session load test. | `cargo test --lib crash_ resume_ redelivered redriven` | 2026-10-12, uncommitted |

## Review log

- 2026-10-12 — plan reviewed against the code. Fixed: journal extension (`.journal`, not `.jsonl`, to keep it out of `list_sessions`); test layout (`tests/durability_test.rs` entry point); wrong/unverified references; per-mode resume wiring; `InboundMessage` blast radius (23 sites) and `AgentRunSpec` (34 sites); exactly-once wording → effectively-once model turns. Added: D0, verification protocol, resume guards (cancel/stale/attempts/cap/kill switch), degradation visibility, I9–I16, U9–U14, and the S1 finding that the in-flight user message and earlier iterations are absent from the checkpoint shape.
