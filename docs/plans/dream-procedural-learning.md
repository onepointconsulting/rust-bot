# Dream procedural learning: skills as a second Dream output

## Context

rust-bot learns procedures in the **foreground**: when `tools.skills.enabled` is set, `templates/agent/skill_learning.md` tells the model to call `skill_manage` after a reusable workflow, a dead end or a user correction. This depends on the model noticing the lesson during the turn, and it often doesn't.

**Dream** (`src/agent/memory.rs`) is the **background** consolidator. Every `agents.dream.intervalH` it reads unprocessed `memory/history.jsonl` entries (the Consolidator's summaries), runs Phase 1 (analysis, `dream_phase1.md`) and Phase 2 (`read_file`/`edit_file` agent, `dream_phase2.md`), and edits `SOUL.md`, `USER.md` and `memory/MEMORY.md`. All of its output is **declarative** (facts). Procedural lessons that reach history are either dropped or squeezed into MEMORY.md as facts.

**Goal:** have Dream also produce **procedural** output, meaning workspace skills it creates or patches, from the same batch. This complements the foreground mechanism: anything the live turn missed gets a second chance, and lessons repeated across sessions can be merged into one skill.

**Feasibility: high, medium-sized job.** Most of the pieces already exist:

- `SkillStore` (`src/agent/skill_store.rs`): validated create, rewrite, patch and write_file with copy-on-write for bundled skills.
- `SkillManageTool` (`src/agent/tools/skill_manage.rs`).
- `SkillsLoader::list_skill_summaries` (`src/agent/skills.rs`) for the skill index.
- `AgentRunner` and `AgentRunSpec`, already used by Phase 2.
- `render_template` with a Tera `Context`.
- `GitStore` auto-commit, plus `/dream-log` and `/dream-restore`.

**Decisions taken:**

- **Write policy follows `tools.skills.requireApproval`.**
  - `false`: Dream writes skills directly; they are versioned and can be undone.
  - `true`: Dream only stages proposals, which the user accepts or rejects with `/skills-review`.
- **Fix the existing git bug and version `skills/`.** `MemoryStore::new` built `GitStore::new(workspace, vec![])`. With an empty tracked list, `auto_commit` staged nothing, so Dream commits carried no file changes and `/dream-restore` could not restore anything. Only the workspace initialiser (`helpers.rs`) passed the real list.

## Design

### 1. Config: `agents.dream.learnSkills` (`src/config/schema.rs`, `DreamConfig`)

Add `learn_skills: bool`, with serde alias `learn_skills` and default `false`, using the existing `DreamConfig` serde, garde and manual `Default` pattern.

- Skill size limits come from `tools.skills` through `SkillLimits::from(&SkillsToolConfig)`, so both channels share one source of truth.
- The setting does **not** require `tools.skills.enabled`. Background learning can run while the foreground tool is off.
- `DreamConfig` has no WebUI form and no `editable_sections` entry; its only editing surface is the CLI wizard, which gets a yes/no prompt.

### 2. Keep procedural signal in history (`templates/agent/consolidator_archive.md`)

History entries are summaries, so the steps and pitfalls must survive summarisation.

- When `learnSkills` is on, add a category through the Tera context flag `learn_skills`: `Procedures: reusable steps, commands, and the pitfall avoided, as "PROCEDURE: <task> — <steps> — <pitfall>"`.
- The Consolidator gets the flag at construction (`Consolidator::with_learn_skills`, called in `agent_loop.rs`).
- When the setting is off, output is unchanged.

### 3. Phase 1 emits skill findings (`templates/agent/dream_phase1.md`)

- Render Phase 1 with a context holding `learn_skills` and `skill_index` (name plus description from `list_skill_summaries`).
- A conditional block allows `[SKILL] <existing-name | NEW:proposed-name> <lesson>` lines, with rules taken from `skill_learning.md`:
  - only reusable procedures, never facts or preferences
  - prefer an existing skill
  - lessons, not logs
- The pure function `partition_findings(analysis: &str) -> DreamFindings { memory: String, skills: Vec<String> }` in `memory.rs` splits `[SKILL]` lines from the memory lines.
- Phase 2 receives only `memory`, so its behaviour is unchanged, including the `[SKIP]` path.

### 4. Phase 3: skill agent (`Dream::run_skill_phase`, new template `dream_phase3.md`)

Phase 3 runs only when `learn_skills` is on and `skills` is non-empty, so there is no LLM cost otherwise.

- It is an `AgentRunner` with:
  - `ReadFileTool` (workspace-scoped, as in Phase 2)
  - a background-mode `SkillManageTool`, built once (in `Dream::with_skill_learning`)
- The prompt gives the skill findings, the skill index (with each `SKILL.md` path) and the skills directory. The rules are:
  - read before patch
  - one skill per lesson
  - trigger-first description
  - stop without tools when nothing qualifies
- Successful tool events are added to the run's changelog. The commit message becomes `dream: <ts>, N memory change(s), M skill change(s)` (or `M skill proposal(s)`).
- The cursor still advances whatever happens. A Phase 3 failure is logged but does not undo Phase 2, which matches the existing policy.
- `run()` is split into documented helpers: `build_history_text`, `build_file_context`, `run_analysis`, `run_memory_phase`, `run_skill_phase` and `commit_changes`.

### 5. Restricting and staging skill writes (`skill_operation.rs`, `skill_store.rs`, `skill_manage.rs`)

- `enum SkillOperation { Create, Rewrite, Patch, WriteFile, RemoveFile, Delete }` (serde) and `SkillOperation::from_params(&Value)`, holding the field checks that used to live in `SkillManageTool`.
- `SkillStore::apply(&op)` and `SkillStore::check(&op)`: both go through one private `plan` step that validates and, for a patch, computes the result; `apply` then writes it, `check` does not.
- `SkillManageTool` gets a `SkillWriteMode` (`Apply`, the default, or `Propose(Arc<SkillProposalStore>)`) and `allowed_actions`. Dream's instance allows only `create`, `patch` and `write_file`; the schema `enum` and the description list only the allowed actions.
- `src/agent/skill_proposals.rs`, `SkillProposalStore`:
  - stores `memory/skill_proposals/<id>.json`, holding the operation, the time it was staged, the origin (`dream`) and the lesson (`reason`)
  - kept outside `skills/` so that `SkillsLoader` never loads unapproved content
  - `propose` (runs `check` first, so only valid proposals are staged), `list`, `get`, `accept` (applies, which re-validates; reports clearly if the skill changed since) and `reject`.

### 6. `/skills-review` command (`src/command/types.rs`, `builtin.rs`)

- `/skills-review` lists pending proposals: id, action, skill and a short preview.
- `/skills-review <id>` shows the full proposal or diff.
- `/skills-review accept <id>|all` and `/skills-review reject <id>|all`.
- Accepting auto-commits `skills: accept <id>[, <id>…]`.
- The command is in the help list.

### 7. Git versioning fix and `skills/` tracking (`src/utils/gitstore.rs`, `memory.rs`, `helpers.rs`)

- One `pub const MEMORY_TRACKED_PATHS: &[&str] = &["SOUL.md", "USER.md", "memory/MEMORY.md", "skills/"]`, used by the workspace initialiser and `MemoryStore::new`.
- **GitStore directory entries** (paths ending in `/`):
  - `auto_commit` uses `index.add_all` plus `update_all` on the pathspec, so new and deleted skill files are staged.
  - `build_gitignore` emits `!skills/` and `!skills/**`.
  - `revert` checks out the parent tree for every tracked pathspec (`CheckoutBuilder::path(..).force()`), which restores changed files and removes tracked files the parent did not have.
  - The "anything to commit?" check compares the staged tree with `HEAD`'s, so unrelated untracked files no longer produce empty commits.
- `GitStore::ensure_gitignore()` runs from `MemoryStore::new` and appends any missing negation lines to an existing `.gitignore`.
- `/dream-log` and `/dream-restore` now talk about "memory and skill files".

## Implementation notes (where the build differs from the design above)

- **Operation model in its own module.** `SkillOperation` and `SkillAction` live in `src/agent/skill_operation.rs` rather than `skill_store.rs`, so the store, the tool and the proposal store share it without a cycle.
- **`Dream::new` keeps its signature.** Skill learning is configured with the builder `Dream::with_skill_learning(learn_skills, &tools.skills)` (and `Consolidator::with_learn_skills`), which keeps the other call sites unchanged and avoids a 9-argument constructor.
- **Proposal source.** The tool cannot see which finding a call came from, so in `Propose` mode its schema has an optional `reason` parameter, which the Phase 3 prompt asks the model to fill with the lesson. The proposal stores `createdAt` (when it was staged, i.e. the Dream run) and `origin: "dream"`.
- **Proposal ids** are small integers (`1`, `2`, …: one more than the highest pending id), so they are easy to type.
- **Phase 2 is skipped when Phase 1 found only `[SKILL]` lines**, since there is nothing for the memory files. With `learnSkills` off nothing is partitioned and Phase 2 always runs, as before.
- **Change counting.** Only successful writes count as changes: `edit_file` in Phase 2 and `skill_manage` in Phase 3. Previously every successful tool event (including `read_file`) was counted.
- **Commit after a Phase 2 failure.** Any change a run made is committed, even when Phase 2 stopped early, so partial edits are versioned and can be undone. `run()` still returns Phase 2's success.
- **Foreground skills are versioned too.** `skills/` is tracked as a whole, so skills written by the foreground `skill_manage` tool are included in the next Dream or `/skills-review` commit.
- **Routing fix.** `/dream-log <sha>` and `/dream-restore <sha>` were registered as exact-only routes, so the argument forms never reached their handlers. They now also have trailing-space prefix routes, like `/subagents`; `/skills-review` uses the same pattern.

## Milestones

1. **Git fix:** shared tracked paths, directory tracking, `ensure_gitignore`, `revert` for directories.
2. **Operation model:** `SkillOperation`, `SkillStore::check` and `apply`, `allowed_actions`; the tool refactored onto them.
3. **Dream Phase 3:** config, consolidator category, Phase 1 template and `partition_findings`, `run()` split, Phase 3 in direct-write mode.
4. **Proposals:** `SkillProposalStore`, `Propose` mode, `/skills-review`.
5. **Docs and samples.**

## Tests (inline `#[cfg(test)]`, `tempfile`, a scripted stub provider)

- **Config:** `learnSkills` defaults to false; camelCase and snake_case aliases.
- **`partition_findings`:** mixed lines, only skill lines, none, `[SKIP]`, and whitespace, bullet and case edge cases.
- **Dream with a scripted stub provider:**
  - no `[SKILL]` lines means Phase 3 is not called
  - with skill lines and `requireApproval=false`, the skill file is created and the commit includes `skills/`
  - with `requireApproval=true`, a proposal file is written and `skills/` is untouched
  - `learnSkills=false` means the Phase 1 prompt has no skill block
  - with `learnSkills=true`, the Phase 1 prompt lists existing skills
- **Consolidator:** the Procedures category appears only with `learnSkills`.
- **`SkillOperation::from_params`:** required fields per action (moved from the tool tests), rewrite vs patch, serde round trip.
- **`check`:** no side effects on disk; same errors as `apply`; same path as `apply`.
- **`allowed_actions`:** rejection and schema enum/description filtering; propose mode stages instead of writing.
- **`SkillProposalStore`:** propose rejects an invalid operation; list order; accept applies; accept after a conflicting edit fails cleanly; reject deletes; non-numeric ids refused.
- **GitStore:** commits contain a diff; new and deleted skill files are committed; `revert` restores a deleted skill and removes an added one; `ensure_gitignore` is idempotent on an old `.gitignore`; untracked files cause no empty commits.
- **`/skills-review`:** list, detail, accept and reject formatting; accept all commits; argument parsing; `/dream-log` and `/dream-restore` receive their argument.

## Verification

- Run `cargo test -j 2`, then `cargo clippy -j 2`, one after the other and never at the same time.
- **Manual CLI check (direct):**
  1. Set `agents.dream.learnSkills=true` and `tools.skills.requireApproval=false` (sample B).
  2. Run a session with a dead end and the working fix, and let it consolidate (or run `/new`).
  3. Run `/dream`. Confirm `skills/<name>/SKILL.md` appears in the next turn's skill index, `/dream-log` shows the skill diff, and `/dream-restore <sha>` removes it.
- **Manual check (staged):** with `requireApproval=true` (sample C), run `/dream`. Confirm `/skills-review` lists the proposal, `skills/` is unchanged until you accept it, and accepting it commits.
- **Regression:** with `learnSkills=false`, Dream's prompts and output match today's, apart from the git fix now producing real diffs.

### Sample configurations (`config.json`)

Each sample shows only the relevant part of `config.json`; other keys stay as they are.

**A. Default: Dream learns no skills.** Leaving out `learnSkills` gives the same result.

```json
{
  "agents": {
    "dream": { "learnSkills": false }
  }
}
```

Expected:
- Consolidator summaries, the Phase 1 prompt and Phase 2 are as before; Phase 3 never runs.
- Dream commits now carry real diffs, and skills written by the foreground tool are versioned.

**B. Dream writes skills directly.**

```json
{
  "agents": {
    "dream": { "learnSkills": true }
  },
  "tools": {
    "skills": { "enabled": false, "requireApproval": false }
  }
}
```

Expected:
- Summaries keep `PROCEDURE: …` lines; Phase 1 may emit `[SKILL]` lines and lists the existing skills.
- Phase 3 creates or patches skills under `<workspace>/skills/`; it never deletes.
- The Dream commit reads `dream: <ts>, N memory change(s), M skill change(s)`; `/dream-restore <sha>` undoes it.
- The foreground `skill_manage` tool stays off (`tools.skills.enabled` is independent).

**C. Dream proposes, the user reviews.**

```json
{
  "agents": {
    "dream": { "learnSkills": true }
  },
  "tools": {
    "skills": { "enabled": true, "requireApproval": true }
  }
}
```

Expected:
- Phase 3 stages proposals in `<workspace>/memory/skill_proposals/<id>.json`; `skills/` does not change.
- `/skills-review` lists them; `/skills-review <id>` shows the full text or diff.
- `/skills-review accept <id>|all` writes the skills and commits `skills: accept <ids>`; `/skills-review reject <id>|all` drops them.
- A proposal whose skill was edited in the meantime is refused with "no longer applies" and stays pending.
- In the foreground, `skill_manage` still asks for approval on every call.

**D. Tight limits shared with the foreground tool.**

```json
{
  "agents": {
    "dream": { "learnSkills": true }
  },
  "tools": {
    "skills": { "maxSkillChars": 1000, "maxDescriptionChars": 40 }
  }
}
```

Expected:
- Phase 3's `skill_manage` calls are held to the same 1000/40-character limits; a too-long skill is refused and the error is fed back to the model.

**E. snake_case alias.** Loads exactly like sample B.

```json
{
  "agents": {
    "dream": { "learn_skills": true }
  }
}
```
