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
| 2. Robustness | `feature/tool-pipeline-p2-robustness` | built 2026-11-03, uncommitted |
| 3. Per-call authorize with `Ask` | `feature/tool-pipeline-p3-authorize` | not started |
| 4. Result model, post-hooks, observability | `feature/tool-pipeline-p4-results` | not started |

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

A tool must not be able to hang or crash a turn. Built 2026-11-03.

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

## Phase 3: per-call authorize with `Ask`

The user approves validated calls, not raw ones.

- New hook method `authorize_call(&ResolvedCall) -> CallDecision` with `Allow`, `Deny { reason }`,
  `Ask { prompt }`. Runs in the pipeline after validate and before `journal_begin`.
- Fail closed, as in the DeepSeek doc: a hook panic denies; an unanswerable `Ask` denies. Replace the three
  fail-open paths (`CliAskHook` with non-terminal stdin, `WebsocketsAskHook` on a non-websocket channel, and with
  no bus) with a denial that tells the model confirmation was required but could not be asked.
- Migrate `CliAskHook`, `WebsocketsAskHook`, `AcpSessionHook`, `SubagentHook`, `LoopHook` and the test hooks.
- Remove the batch-level `before_execute_tools`, `ToolHookDecision`'s batch semantics and
  `execute_tools_honoring_decision` once nothing uses them. Keep the fail-closed merge of several hooks.
- Denied calls journal through the existing `record_blocked_call`; the abort path (`Abort`) needs an equivalent.
- Tests: ask, deny one call of a batch, invalid calls never prompt, panicking hook, unanswerable ask. Smoke-test
  the CLI and websockets-chat by hand, because approvals are user-facing.

Risk: medium-high (three channels, ordering semantics).

## Phase 4: result model, post-hooks, observability

- **`ToolResult { content, is_error }`** replaces the `"Error"` prefix. First `is_error_result` reads the flag and
  falls back to the prefix; then migrate the tools gradually. Journal and event status derive from the flag.
- **Post-execute hook** `after_tool_call(&ToolOutcome) -> PostDecision` (`Accept`, `Replace`, `Block`,
  `AddContext`). `AddContext` text is queued and appended as one hidden user-role message after all tool results of
  the batch, so call/result adjacency holds. Use cases: secret redaction, output policy.
- **Typed `ToolEvent`** (`name`, `status`, `detail`, `duration_ms`) replacing `HashMap<String, String>`, plus an
  `on_tool_result` hook that fires as each call finishes. Keep a `to_map()` adapter while consumers migrate.
- **Normalize inside the pipeline**, so the journal copy and the transcript copy come from one function and one
  budget (today they are truncated separately).

Risk: medium. The `is_error` migration touches every tool; commit each sub-item separately.

## Cross-cutting

- Docs: update `docs/DURABILITY.md` where the journal lifecycle changes; add `docs/TOOL_PIPELINE.md` (stage
  diagram, ordering guarantees) when Phase 3 changes the order.
- One commit per phase (Phase 4: one per sub-item).
- Per phase: `cargo fmt`, `cargo clippy --all-targets`, `cargo test`. `wizard.rs:667` has a pre-existing clippy
  warning (`redundant_closure`), unrelated.
- Leave `docs/Newsletter_Brief.pdf` out of every commit.

## Open questions

- Phase 3: does `AcpSessionHook` have a use for `Ask` beyond its current permission requests?
