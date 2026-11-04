# Tool execution pipeline

How one tool call the model asked for becomes a tool result. Code: `src/agent/tool_pipeline.rs`, driven by
`AgentRunner::execute_tools` in `src/agent/runner.rs`. Design notes and history:
`docs/plans/tool-execution-pipeline.md`.

## Stages

A call goes through two phases. The runner prepares every call of a batch, one after another, and then executes
them, concurrently where the tools allow it.

| # | stage              | phase   | what it does                                                        |
|---|--------------------|---------|---------------------------------------------------------------------|
| 1 | parse guard        | prepare | reject arguments the provider could not parse as JSON               |
| 2 | resolve + validate | prepare | find the declared tool, cast and validate its arguments             |
| 3 | authorize          | prepare | `AgentHook::authorize_call`; a denial is journaled as `Blocked`     |
| 4 | state guards       | prepare | message circuit breaker, repeated external lookup block             |
| 5 | journal            | execute | write-ahead intent; strict durability mode refuses the call on fail |
| 6 | execute            | execute | run under the call identity; isolate panics; optional timeout       |
| 7 | post-execute       | execute | `AgentHook::after_tool_call`: redact, withhold or annotate the raw result |
| 8 | normalize          | both    | make the text non-empty, persist an oversized one, cut it to the budget; add the retry hint |
| 9 | journal finish     | execute | terminal record with the normalized text                            |
| 10| observe            | both    | with `fail_on_tool_error`, an error becomes fatal; `on_tool_result` fires |

Every stage returns `Result<_, ToolOutcome>`. A failing stage short-circuits with an error *result*, so the model
always gets exactly one tool message per call.

## Ordering guarantees

- A call is never authorized before it validated: the user is not asked about unknown tools or invalid arguments.
- Authorization happens in call order, one call at a time, before any call of the same batch executes.
- A denied call is not journaled as an intent, never executes, and does not count towards the circuit breaker or
  the repeated-lookup count.
- The intent is on disk before `Tool::execute` starts; a terminal record follows even when the tool panics or
  times out.
- The post-execute hook sees the raw result, before it is cut to the budget, persisted to disk (an oversized one)
  or journaled, so a redaction reaches every copy. The journal and the transcript hold the same normalized text.
- Results come back in the order of the model's calls, however they were executed.

## Hooks

`AgentHook` (`src/agent/hook.rs`) has these tool methods.

- `on_tool_calls(&mut ctx)`: once per batch, before anything is authorized. For announcing and logging only; it
  cannot veto. Used for tool-hint progress, the ACP `tool_call` cards and subagent logging.
- `on_tool_result(&ToolEvent)`: one call finished (ran, failed, was rejected or denied). Fires as each call
  completes, so a concurrent batch reports in completion order. Observing only; `ToolEvent` carries the call id,
  tool name, status, detail and `duration_ms`.
- `after_tool_call(&ToolCallResultView) -> PostCallDecision`: a call ran; decide what the model sees. The decision
  can replace the text (`replacement`), withhold it (`block_reason`: the model gets an error naming the reason) and
  add context (`added_context`: delivered once, as one hidden user message after every result of the batch, so
  each call keeps its result right behind it). Not called for calls that never ran. A hook that panics withholds
  the result. `CompositeHook` feeds each hook the result as the earlier ones left it and stops at the first block.
- `authorize_call(&ToolCallAuthorization) -> CallDecision`: per validated call. `Allow` (the default) or
  `Deny { reason }`; the reason is shown to the model as `Error: tool call blocked by hook: <reason>`.

A hook that has to ask someone resolves the question itself and returns the answer. **A question nobody can
answer is a denial**: no terminal, no chat, no bus, a timeout, or a panicking hook. `CompositeHook` and
`LoopHookChain` ask their hooks in order and stop at the first denial.

| hook                | asks                                  | cannot ask when                              |
|---------------------|---------------------------------------|----------------------------------------------|
| `CliAskHook`        | a terminal prompt per call            | stdin is not a terminal                      |
| `WebsocketsAskHook` | an approval card in the web chat      | channel is not a websocket chat, no bus      |
| `AcpSessionHook`    | `session/request_permission`          | the client refuses, errors or stays silent   |

`tools.confirmBeforeExecute` turns the first two on. With it on, tool calls from channels that have no web chat
(cron, email, WhatsApp, ...) are denied.

## Approval grants (web chat)

An approval card can do more than answer one call. The user can also pre-approve later calls:

| choice                              | covers                                         | ends when                                  |
|-------------------------------------|------------------------------------------------|--------------------------------------------|
| Run selected                        | the selected calls only                        | -                                          |
| allow all for this turn             | every tool                                     | the turn that asked ends                   |
| always allow `<tool>` this session  | the tools of the approved calls                | the chat is cleared or deleted, or revoked |
| allow everything this session       | every tool                                     | the chat is cleared or deleted, or revoked |

- **The gateway owns the grants** (`ToolApprovalBroker`, in memory, keyed by chat). A restart forgets them on
  purpose. The client only names the scope in the `grant` field of `tool_approval_response` (`turn`, `session`,
  `session_tool`); the turn and the tool names come from the stored request, so an answer cannot widen what it was
  shown.
- A grant is only recorded when at least one call was approved. Denying with a grant attached grants nothing.
- `WebsocketsAskHook` checks the grants before it publishes a card. A granted call is still journaled and executed
  like any other, and the gateway logs that a grant approved it.
- A grant never leaves its chat: other chats, cron and email turns are not affected. Calls that cannot be asked
  about still deny.
- The gateway sends `tool_grants_updated {chat_id, session_all, tools, turn}` to every tab whenever the grants
  change, and replays it on `attach` (without the turn grant, since nothing there knows whether the turn still
  runs). The web chat shows a banner with a **Revoke** button, which sends `revoke_tool_grants {chat_id}`.

## Tool results

A tool says whether a call failed with a flag, not a text convention. `Tool::execute_result` returns a
`ToolResult { content, is_error }`; the default classifies the text of `execute` by the legacy `"Error"` prefix
(`is_error_text`), so a tool only overrides it when the prefix is wrong for it. Migrated so far:

- `shell`: a command that ran is a successful call whatever it printed (output may start with "Error"); a
  command that was blocked, could not be run or timed out is an error. A non-zero exit code is reported in the
  text, not as a failed call.
- MCP tools: a timeout, a transport failure, a file reference that could not be expanded, or a result the server
  flagged with `isError` is an error. Before, none of these were noticed (their texts do not start with "Error").

The journal status, the telemetry status, the retry hint and `fail_on_tool_error` all follow the flag.

## Failure handling

- A tool that **panics** returns `Error: tool '<name>' crashed: <message>`.
- With `tools.executeTimeoutSeconds` set (`0` = off, the default), a tool that runs too long is cancelled and
  returns `Error: tool '<name>' timed out after <n>s and was cancelled`. Tools that wait on something other than
  computation opt out with `Tool::exempt_from_execute_timeout` (`shell`, `question`, `run_agent`).
- Both are journaled as `Error`. The crash points used by the durability tests sit outside this isolation on
  purpose: they simulate a process kill.
