# Plan: tool execution pipeline (modelled on the DeepSeek harness)

## Goal

Make the execution of one tool call an explicit, staged pipeline:

`guard → resolve → validate → authorize → journal → execute → normalize → observe`

Today the stages exist but are tangled: `AgentRunner::run_tool` is one long function with seven early returns that
each hand-build the same `(result, event, fatal)` tuple, authorization happens per batch and before validation, a
tool failure is detected by the text `"Error"`, and nothing protects the turn from a panicking or hanging tool.

## Status

| Phase | Branch | Status |
|---|---|---|
| 1. Structure (no behaviour change) | `feature/tool-pipeline-p1-structure` | built and committed 2026-11-03 |
| 2. Robustness | `feature/tool-pipeline-p2-robustness` | committed 2026-11-03 |
| 3. Per-call authorize | `feature/tool-pipeline-p3-authorize` | committed 2026-11-03 |
| 3b. Approval grants (web chat) | `feature/tool-approval-grants` | built 2026-11-03, uncommitted |
| 4. Result model, post-hooks, observability | `feature/tool-pipeline-p4-results` | built 2026-11-04, uncommitted |

One branch per phase. Phase N+1 branches from Phase N until N is merged. Commit only when asked.

## Decisions (made with Gil, 2026-11-03)

- **Unanswerable approvals deny outright.** No fallback setting. Simpler and more secure. Applies to a non-terminal
  stdin in the CLI, a non-websocket channel under the websocket hook, and a websocket hook with no bus wired.
- **The batch-level `before_execute_tools` is removed** once Phase 3 has migrated every hook.
- **The execute timeout defaults to off.** Some tools (`exec`, ACP sessions) legitimately run long.
- **One branch per phase.**

## Reference: what the DeepSeek doc says

Source: `deepseek-ai/deepseek-harness`, `docs/tool-execution-pipeline.md` (commit `ce7073d`). The page is a short
Mermaid flowchart plus two paragraphs. It does not use the stage names above; the mapping is ours. It says nothing
about argument validation, parallelism, idempotency or truncation.

1. `tool/call` is logged before anything runs (our journal).
2. `tools/pre-execute` waterfall: hooks, permission, sandbox. Allow, deny or ask.
3. Monotonic guards: deny or abstain, cannot be reordered.
4. `ctx.approval`: one-shot prompt for asks; "absent or unanswerable: deny".
5. `tools/execute` waterfall around the tool body: timeout, retry, metrics.
6. `projectContent`, then the `tools/post-execute` waterfall: accept, block, replace or add context.
7. The result is snapshotted; **any throw at any stage becomes an `isError` result**, not an exception.
8. `finalizeContent`, then a synchronous `tools/result` notification, then the logged `tool/result` event.
9. `additionalContexts` are injected only after the whole batch settled, so call/result pairs stay adjacent.

## Pipeline today (before this plan)

Code: `src/agent/runner.rs`.

| Stage | Where | Notes |
|---|---|---|
| authorize | `AgentHook::before_execute_tools` (`hook.rs`), merged by `ToolHookDecision::merge`, panics fail closed | batch level; sees raw, uncast calls |
| guard | message circuit breaker, repeated-lookup block, malformed-args check | inline in `run_tool` |
| resolve + validate | `ToolRegistry::prepare_call` | deferred-tool gating, `cast_params`, `validate_params` |
| journal | `TurnJournal::begin_tool_call` (write-ahead), `InFlightToolCall::finish`, `record_blocked_call` | strict mode refuses the call |
| execute | `Tool::execute` under `with_tool_call` | no timeout, no panic isolation |
| normalize | `normalize_tool_result` in the run loop | empty-result marker, persist to disk, truncate; separate from the journal copy |
| observe | `HashMap<String, String>` events, `after_iteration` | stringly typed, no duration |

Gaps found: no explicit pipeline type; authorize before validate and per batch only; failure signalled by the
`"Error"` prefix; panics in `execute` not isolated; no execute timeout; no post-execute hook; untyped observe;
normalize outside the pipeline; fail-open approval paths (`confirm_tools.rs`, non-terminal stdin, non-websocket
channel, missing bus).

## Phase 1: structure (no behaviour change)

Make the pipeline explicit and testable.

- `ToolOutcome { content, status, detail, error, fatal }` with constructors `ok`, `error`, `rejected`,
  `circuit_tripped`, `blocked_by_hook`, `from_tool_result`; `observed(fail_on_tool_error)`; `into_triple(name)`
  keeps the old `(result, event, fatal)` shape so callers and tests do not move.
- Stage functions in `src/agent/tool_pipeline.rs`, each `Result<_, ToolOutcome>`: `guard`, `resolve_and_validate`,
  `journal_begin`, `execute`, `journal_finish`, normalize (`ToolOutcome::from_tool_result`), observe.
- `ToolRegistry::prepare_call` split into `resolve_tool` and `validate_call`; `prepare_call` stays as a wrapper.
- `is_error_result` is the single place that encodes the `"Error"` prefix convention.
- One shared `ERROR_HINT` constant (was duplicated in `runner.rs` and `registry.rs`).
- Authorize stays where it is (batch level, before `run_tool`); it is stage 0 in the docs only.
- Tests: characterization tests of every `run_tool` exit path written **before** the refactor, unit tests per
  stage and for `ToolOutcome`, registry tests for the split.

Risk: low. Files: `runner.rs`, `tool_pipeline.rs`, `tools/registry.rs`, `agent/mod.rs`.

## Phase 2: robustness

A tool must not be able to hang or crash a turn. Committed 2026-11-03.

- **Panic isolation.** The execute stage runs the tool under `catch_unwind`. A panic becomes
  `Error: tool '<name>' crashed: <message>` (message capped at 200 chars), which is journaled as a terminal
  `Error`, so there is no orphan intent. The crash points (`after_intent`, `before_terminal`) stay outside the
  isolation, because they simulate a process kill and must still unwind.
- **Execute timeout.** `tools.executeTimeoutSeconds` (`0` = off, the default; shown in the Tools page General
  form). A call that exceeds it is cancelled by dropping its future and returns
  `Error: tool '<name>' timed out after <n>s and was cancelled`. `AgentRunSpec::tool_timeout` carries it; only
  the main agent loop sets it.
- **Per-tool override** is a trait flag, `Tool::exempt_from_execute_timeout`, not a config map: tools that wait on
  something other than computation opt out. Set for `shell` (own `tools.exec.timeout`), `question` (waits for the
  user) and `run_agent` (a whole agent turn). MCP tools and the other built-ins are subject to the timeout.
- **Journal status for a timeout is `Error`, not a new variant.** A new `ToolCallStatus` would be written into
  journals that older binaries cannot deserialize, and recovery treats `Error` correctly already.
- **Child processes:** answered. `shell` kills its process tree when its future is dropped
  (`kill_process_tree_sync` guard), so a timeout does not leave orphans. Other tools with child processes (MCP
  stdio servers) are not tied to the call and are unaffected.
- **Shared test fakes** live in `src/agent/tools/test_support.rs` (`#[cfg(test)]`): `ScriptedTool`, `SlowTool`,
  `PanickingTool`, `spec_with_tool`.
- Retry stays out of scope; if added later it applies only where `replay_safe()` is true.

Risk: medium. Not covered: a tool that blocks a thread synchronously (no `.await`) cannot be cancelled by the
timeout; it would need `spawn_blocking` at the tool.

## Phase 3: per-call authorize

The user approves validated calls, not raw ones. Built 2026-11-03. See `docs/TOOL_PIPELINE.md` for the result.

- **`AgentHook::authorize_call(&ToolCallAuthorization) -> CallDecision`**, with `Allow` and `Deny { reason }`.
  The request carries the call id, tool name, the cast and validated arguments, and the turn's channel and
  chat id. There is **no `Ask` outcome**: a hook that has to ask someone awaits the answer itself and returns the
  final decision, so a second approval mechanism would only duplicate that.
- **The batch-level `before_execute_tools` and `ToolHookDecision` are gone**, with `Deny`, `DenyCalls`, `Abort`,
  `HOOK_ABORT_STOP_REASON` and `execute_tools_honoring_decision`. No production hook ever returned `Abort`.
- **`AgentHook::on_tool_calls(&mut ctx)` replaces it as a non-gating batch announcement.** `LoopHook` (tool hints
  and progress), `SubagentHook` (logging) and `AcpSessionHook` (the `tool_call` cards) announce a batch rather
  than decide about it, so they keep a once-per-batch hook that cannot veto.
- **Order inside the pipeline:** parse guard, resolve + validate, **authorize**, then the stateful guards
  (circuit breaker, repeated-lookup count), then journal and execute. The stateful guards moved after authorize
  so a denied call neither counts towards the breaker's streak nor towards the lookup count.
- **Prepare, then execute, per batch.** The runner prepares the calls of a batch one after another (so approval
  prompts come one at a time, in call order, with no overlapping cards) and then executes them, concurrently where
  the tools allow it.
- **Fail closed everywhere.** A hook that panics denies. `CompositeHook` stops at the first denial so nobody is
  asked about a call another hook refused. The three fail-open paths now deny with a reason the model can read:
  `CliAskHook` with stdin not a terminal, `WebsocketsAskHook` on a channel that is not a websocket chat (or has
  no chat id), and with no bus wired.
- Denied calls are journaled as `Blocked` with the validated arguments (`record_blocked_call`).
- `AcpSessionHook` announces the batch in `on_tool_calls`, asks for permission and sends `in_progress` in
  `authorize_call`.

Consequences to be aware of:
- With `tools.confirmBeforeExecute` on, the gateway attaches `WebsocketsAskHook` to the whole agent loop, so
  **every tool call on a channel without a websocket chat (cron, email, WhatsApp, ...) is now denied**, where it
  used to run unconfirmed. Subagents are unaffected (their runs use `SubagentHook` only).
- A batch of N calls now shows N approval cards one after another in the web chat, where it used to show one card
  with N calls. Batching the card again would need a batch-shaped `authorize_calls` over the validated calls.

Risk: medium-high (three channels, ordering semantics). Needs a manual smoke test of the CLI and websockets-chat.

## Phase 3b: approval grants (web chat)

Added after Phase 3, because one card per call made long turns tedious. Built 2026-11-03.

- Choices on the approval card: allow all for this turn, always allow the selected tools this session, allow
  everything this session. A banner lists active grants with a **Revoke** button. See `docs/TOOL_PIPELINE.md`.
- `ToolCallAuthorization` gained `turn_id` (from the journal's turn) so a turn grant expires with its turn.
- Grants live in the broker, in memory, per chat; they are cleared by `clear_session`, `delete_chat` and revoke.
- The CLI prompt got the same choices afterwards (`feature/cli-approval-grants`), on a shared `ApprovalGrants`
  store; they last until the CLI exits and there is no revoke command. ACP permission requests are unchanged.
- Decided with Gil: per-tool grants and the revoke banner are part of the first version; a blanket session grant
  would otherwise switch confirmation off for the chat without any sign of it.

## Phase 4: result model, post-hooks, observability

Built 2026-11-04, in four parts. See `docs/TOOL_PIPELINE.md` for the result.

- **Typed events.** `ToolEvent { call_id, tool_name, status, detail, duration_ms }` (`agent/tool_event.rs`) and
  `AgentHook::on_tool_result`, fired as each call completes. `ToolEvent::to_map` keeps the `name`/`status`/`detail`
  maps that `ctx.tool_events` and the outbound events still carry, so no consumer changed; the duration reaches
  hooks only. Adding a `duration_ms` key to those maps (and to the web chat's tool activity) is a follow-up.
- **`ToolResult { content, is_error }`** replaces the prefix check. `Tool::execute_result` defaults to classifying
  `execute`'s text, so only tools that need it migrated: `shell` (output starting with "Error" is not a failure)
  and MCP tools (their failures were invisible: none started with "Error", and the server's `isError` was ignored).
  Visible effect: MCP failures now get the retry hint, an `error` event and a journal `Error`.
- **Normalize inside the pipeline.** `normalize_tool_result` moved from the runner to the pipeline and runs before
  the journal's terminal record, so the journal and the transcript hold the same text from one budget. The retry
  hint is added after normalizing, so a budget never cuts it. Results that are content-block arrays (images) are
  normalized as blocks, never cut mid-JSON. The telemetry detail still comes from the raw result.
- **Post-execute hook.** `AgentHook::after_tool_call(&ToolCallResultView) -> PostCallDecision { replacement,
  block_reason, added_context }`. It runs on the raw result, before normalizing, so redaction also reaches the
  copy persisted for oversized results and the journal. Added context is queued and appended as one hidden user
  message (`_hidden_history`) after every tool result of the batch. A panicking hook withholds the result.

Not done: a typed result for the other tools (they keep the prefix convention), a CLI/web-chat display of
`duration_ms`, and any built-in hook that redacts or blocks (the mechanism exists; no policy ships with it).

Risk: medium. Behaviour change to know about: MCP failures are now errors (hint, event status, journal status).

## Cross-cutting

- Docs: update `docs/DURABILITY.md` where the journal lifecycle changes; add `docs/TOOL_PIPELINE.md` (stage
  diagram, ordering guarantees) when Phase 3 changes the order.
- One commit per phase (Phase 4: one per sub-item).
- Per phase: `cargo fmt`, `cargo clippy --all-targets`, `cargo test`. `wizard.rs:667` has a pre-existing clippy
  warning (`redundant_closure`), unrelated.
- Leave `docs/Newsletter_Brief.pdf` out of every commit.

## Open questions

- Phase 3: does `AcpSessionHook` have a use for `Ask` beyond its current permission requests?
