# Durability (crash recovery)

rust-bot survives a process kill in the middle of a turn. After a restart the
turn finishes correctly: tools that are safe to repeat are re-run once, tools
that are not are reported to the model as interrupted and never repeated, no
queued message is lost and no message gets a second model turn.

**What is promised.** Effectively-once *model turns*, not exactly-once delivery.
A crash between sending a reply and recording it as answered can send that
reply once more after the restart (at-least-once outbound). Durability is
against **process kill**; power-loss durability only applies to the records that
are explicitly synced (turn start/end, tool intents of non-replay-safe tools,
ledger entries).

## How it works

Every session has a run journal. With the default file store it sits next to
the transcript: `<workspace>/sessions/<session key>.journal` (JSON lines,
append-only; not `.jsonl`, so it is never listed as a session). A finished turn
is removed from it, so the file is usually empty between turns.

### Storage layer

Journals are stored through the `JournalStore` trait
(`src/session/journal.rs`), which a `SessionStore` exposes with
`journal_store()`, so one backend can hold sessions, archives and journals.
Nothing outside the file store knows about paths or files.

| Operation | Contract |
|---|---|
| `append(session, record, durability)` | atomic (whole record or nothing, concurrent appends never interleave), reaches the OS before returning (`Flushed`) or stable storage (`Synced`); returns the per-session sequence number |
| `scan(session)` / `scan_all()` | every intact record in order; damaged ones are skipped and counted; `scan_all` is how startup finds journals without knowing session keys |
| `rewrite(session, transform)` | atomic replacement of the journal's records; sequence numbers are never reused |
| `delete(session)` | idempotent |

Implementations: `FileJournalStore` (`journal_file.rs`, the default) and
`InMemoryJournalStore` (tests). Any new backend must pass the shared checks in
`journal::conformance` (ordering, concurrent appends, rewrite, delete,
isolation, awkward session keys); add a test that calls
`conformance::run` with a factory for fresh stores. The store is synchronous,
like `SessionStore`. A backend with an async driver needs the whole store layer
to become async first.

| Record | Written | Used for |
|---|---|---|
| `turn_start` | when a turn begins (stores the user message, reply route, request id, attempt) | the transcript only gets the user message when the turn ends |
| `step_checkpoint` | before the first model request, after the assistant requests tools, after tools finished, on the final answer | rebuilding the turn: every message produced so far |
| `tool_intent` | `intent` before `Tool::execute`; `done` / `error` / `blocked` / `cancelled` after | deciding per call what recovery may do |
| `turn_end` | when the turn ends (`completed`, `failed`, `cancelled` by `/stop`, `abandoned`) | a turn with a terminal record is never resumed |
| `turn_restored` | when recovery merged an interrupted turn into the transcript | a turn is merged exactly once |
| `inbox` | `queued` when a message is published, `processing`, `done` with the reply | duplicate detection and re-driving queued messages |

Finished turns are removed from the journal when the turn ends, so its size
stays bounded.

### Recovery policy for an interrupted tool call

Decided per `(turn_id, call_id)`:

* a terminal record exists: never executed again;
* intent only and the tool declares `replay_safe()`: executed once more with the
  arguments stored in the intent record (a second crash during that re-run is
  not retried again);
* otherwise: the model receives `Error: Task interrupted before this tool
  finished.` and the call is never repeated.

`replay_safe()` defaults to `false`. Opted in: `read_file`, `list_dir`, `glob`,
`grep`, and `spawn` (its task id is derived from the call identity, so a replay
finds the existing subagent instead of starting another; a subagent that died
with the process is started once more). MCP and ACP tools are never replay safe.
Recovery never touches the tool-approval broker: an unsafe call is never
re-executed, so no approval prompt can be triggered by recovery.

### Startup

* **gateway** resumes interrupted turns automatically, then re-drives messages
  that were queued but never answered. The reply of a resumed turn is published
  to the chat the turn came from.
* **api** and the interactive/one-shot **agent** modes do not auto-resume.
  The interrupted turn is merged into the session transcript, with its tool
  calls marked interrupted, when that session is next used.
* **acp** never auto-resumes (the client drives `session/load`).

A turn is resumed only when it has no terminal record (so a turn stopped with
`/stop` is not), is not older than `resumeMaxAgeSecs`, and has been resumed fewer
than `maxResumeAttempts` times (so a turn that itself kills the process cannot
crash-loop the service; it is closed as `abandoned`). Only the newest unfinished
turn of a session is resumed.

### Inbound idempotency

A message with a `request_id` (`<channel>:<chat_id>:<platform message id>`,
filled from the `message_id` the channel adapter stores in the message metadata;
never invented) is recorded in the ledger when it is published. A redelivery of
an answered request gets the stored reply (truncated to 4 KB) back and starts no
turn; a redelivery of an unfinished one is dropped. Channels that supply no
message id get at-least-once behaviour. Once evicted (see below) a request id is
processed again.

## Configuration

All keys live under `durability` in the config file and have defaults.

| Key | Default | Meaning |
|---|---|---|
| `autoResume` | `true` | resume interrupted turns when the gateway starts |
| `resumeMaxAgeSecs` | `86400` | older turns are not resumed (reported as stale) |
| `maxResumeAttempts` | `2` | resume attempts before a turn is abandoned |
| `maxConcurrentResumes` | `4` | interrupted turns resumed in parallel |
| `ledgerTtlDays` | `14` | how long an answered request id is remembered |
| `ledgerMaxEntries` | `1000` | answered request ids kept per session |

## Limits

* One rust-bot process per workspace (as before). Two processes appending to the
  same journal are not supported.
* A journal that cannot be written (read-only or full disk) is logged and
  flagged as degraded; the tool call and the turn still proceed without crash
  safety.
* Resumed turns are not registered as active tasks, so `/stop` cannot abort one.
