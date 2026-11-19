# Self-learnt skills: `skill_manage` tool for rust-bot

## Context

Hermes Agent learns while it works. When it works out a reusable workflow, recovers from a dead end, or is corrected by the user, it calls `skill_manage` to create or patch a skill (`hermes-agent/tools/skill_manager_tool.py`). rust-bot should do the same, switched on by a new skills setting that does not exist yet.

**Feasibility: high, medium-sized job.** rust-bot already has most of the pieces:

- **Skill discovery:** `SkillsLoader` (`src/agent/skills.rs`) reads `<workspace>/skills/<name>/SKILL.md` first, then bundled `./skills`. It has no cache and the system prompt is rebuilt every turn, so a new skill shows up in the index on the next turn. In the current turn the model can still `read_file` it.
- **Safe writes:** `write_atomic` (`src/utils/fs.rs:39`).
- **Find and replace:** `_find_match` and `EditFileTool::_not_found_msg` (`src/agent/tools/filesystem.rs:238`, `:426`).
- **Path guards:** `normalize_lexically` and `_is_under` (`filesystem.rs:92`, `:183`).
- **Config-gated tools:** the `QuestionToolConfig` and `register_question_tool` pattern (`schema.rs:1341`, `registry_helper.rs:170`).
- **Prompt guidance:** `ContextBuilder::with_prompt_section` (`agent_loop.rs:682`).
- **Approval:** `CliAskHook` and `WebsocketsAskHook` (`src/cli/confirm_tools.rs`).

**Gaps:**

- There is no skills config.
- No tool writes skills.
- No prompt tells the model when to learn.
- Approval is all-or-nothing (`confirmBeforeExecute`).

**Agreed scope:**

- Learning happens in the foreground only: the tool plus prompt guidance. The background review fork is a later milestone.
- Config lives at `tools.skills`.
- `requireApproval` is optional and off by default.

**Deliberately not ported from Hermes:**

- batch operations and rollback
- categories
- profiles
- curator, ledger and usage telemetry
- pinned and read-before-write guards
- the security scanner

Approval is the safeguard against a prompt injection persisting itself as a skill.

## Design

### 1. Config: `SkillsToolConfig` → `tools.skills` (`src/config/schema.rs`)

Follow the `QuestionToolConfig` pattern exactly:

- serde camelCase with snake_case aliases
- `default_*` functions and a manual `Default`
- garde validation
- `#[schemars(extend("x-editable"=true, "x-title"="Skills", "x-tools"=["skill_manage"]))]`

Fields:

| field | default | meaning |
|---|---|---|
| `enabled` (alias `enable`) | `false` | registers `skill_manage` and adds the learning guidance to the prompt |
| `requireApproval` | `false` | ask the user before every `skill_manage` call, even when `confirmBeforeExecute` is off |
| `maxSkillChars` | `100_000` (garde min 1000) | size limit for `SKILL.md` and supporting files |
| `maxDescriptionChars` | `1024` (garde min 16) | limit for the frontmatter description |

Wiring:

- Add a `skills` field to `ToolsConfig` (`#[garde(dive)] #[schemars(skip)]`, as `question` has) and to `ToolsConfig::default`.
- Add `section_of::<SkillsToolConfig>("skills")` to `editable_sections()` (`src/agent/tools/tool_settings_api.rs:54`). This gives the WebUI Tools page its form and switch for free.
- Check whether `src/config/overlay.rs::rule_for` needs an entry for child-agent overrides.

### 2. Skill store: `src/agent/skill_store.rs` (new)

This holds the filesystem logic as pure, testable functions, separate from the tool. Each function is documented.

`SkillStore { workspace_skills: PathBuf, builtin_skills: PathBuf, limits: SkillLimits }` exposes:

- **`create(name, content)`**
  - Validates the name, frontmatter and size.
  - Fails if the skill exists in either root.
  - Creates the folder and writes with `write_atomic`.
- **`rewrite(name, content)`:** replaces the whole `SKILL.md`, then validates it.
- **`patch(name, old, new, file_path, replace_all)`**
  - Uses the shared find-and-replace helper (see below).
  - When `SKILL.md` is the target, validates the frontmatter after the patch.
- **`write_file(name, file_path, file_content)` / `remove_file(name, file_path)`**
  - Paths must sit under `references/`, `templates/`, `scripts/` or `assets/`.
  - Absolute paths and `..` are rejected, then the path is checked with `_is_under` against the skill folder.
- **`delete(name)`:** workspace skills only. Bundled skills are refused.
- **Copy-on-write for bundled skills:** modifying a bundled skill first copies its folder into `<workspace>/skills/<name>`. The workspace copy already shadows the bundled one in `list_skills`, so the bundled files stay read-only.

Validation helpers, each its own unit-tested function:

- **`validate_skill_name`:** `^[a-z0-9][a-z0-9._-]*$`, at most 64 characters. Because the loader uses a flat layout, this also blocks the path traversal that `SkillsLoader::load_skill` currently allows.
- **`validate_skill_document`:**
  - Requires a frontmatter block.
  - Requires `name` (equal to the folder name) and `description` (within the limit).
  - Requires a non-empty body and enforces the size limit.
  - Reuses `STRIP_SKILL_FRONTMATTER` and the line parser behind `get_skill_metadata`, with those helpers made `pub(crate)`. No YAML crate is added.
- **`validate_supporting_path`**

All writes take the store's `tokio::sync::Mutex<()>`, because gateway sessions share one registry.

**Removing duplication in `filesystem.rs`:** move the find, uniqueness-check and replace logic out of `EditFileTool::execute` into a `pub(crate) fn find_and_replace(content, old, new, replace_all) -> Result<(String, usize), String>`. The error side carries the existing closest-match diff from `_not_found_msg`. `EditFileTool` and `SkillStore::patch` then both call it, and the existing `edit_file` tests guard the refactor.

### 3. Tool: `src/agent/tools/skill_manage.rs` (new)

- `SkillManageTool { store: Arc<SkillStore> }` implements `Tool`, with `name()` returning `"skill_manage"` and `read_only()` returning `false`.
- **Parameters:** a flat single-operation schema:
  - `action`: one of `create | patch | write_file | remove_file | delete`
  - `name`
  - `content`
  - `old_string` / `new_string` / `replace_all`
  - `file_path` / `file_content`
- `patch` with `content` alone is a full rewrite, as in Hermes. `execute` checks which fields each action requires, sends the call to the store, and returns a short JSON result: success, message, path, and a hint to `read_file` the path to use it now.
- **Description:** adapted from Hermes's `_skill_manage_description`, without the batch, category and curator wording:
  - skills are procedural memory
  - prefer `patch`
  - `description` should read "Use when <trigger>. <behaviour>."
  - "write lessons, not logs"
- **Registration:**
  - Add `register_skill_manage_tool(&SkillsToolConfig, &workspace, &mut ToolRegistry)` in `src/utils/registry_helper.rs`. It returns early when `!enabled`.
  - Call it from `AgentLoop::register_default_tools` (`agent_loop.rs:1043`). This means passing `skills_config` through, as `question_config` is.
  - Subagents (`subagent.rs::build_tool_registry`) do **not** get the tool.
  - No Herdr wiring is needed, because the tool never blocks on the terminal.

### 4. Prompt guidance: when to learn

Add `templates/agent/skill_learning.md`, rendered by a prompt section that is registered in `AgentLoop::new` only when `tools.skills.enabled` is true (next to the ACP section, `agent_loop.rs:682`). It tells the model to call `skill_manage` after:

1. working out a non-trivial, reusable workflow
2. hitting errors or dead ends and then finding the approach that works
3. a correction from the user about approach, steps or style

It also says:

- patch an existing skill (especially one used in this task) before creating a new one
- keep descriptions trigger-first
- don't record one-off facts (those belong to memory)
- learn after the task's work, not instead of it

Prompt sections are already skipped in Minimal mode, which is consistent with how skills behave.

The section also checks that `skill_manage` is actually in the registry. If `disabledTools` removed it, no guidance is shown, so the prompt never points to a tool that isn't there.

### 5. Approval scope: `requireApproval`

Add a small wrapper hook, `ToolScopedHook { inner: Arc<dyn AgentHook>, tool_names: HashSet<String> }`, next to the ask hooks in `src/cli/confirm_tools.rs`:

- `authorize_call` returns `Allow` for any tool not in `tool_names` and asks `inner` for the rest.
- `revoke_grants` is passed through to `inner`.

Install it in `src/cli/commands.rs`:

- **CLI (`:630`):** install it when `!confirm_before_execute && skills.enabled && skills.require_approval`.
- **Gateway (`:1158`):** same condition. The `ToolApprovalBroker` must also be created in that case, and `set_bus` still runs on the concrete `WebsocketsAskHook`.
- **ACP serve (`src/cli/acp.rs:196`):** check this path for the same treatment.

Channels that cannot ask (e.g. email) get `unanswerable`, which denies, as decided in the tool-pipeline work. The guidance template should mention this.

### 6. Docs

- Add a short `templates/TOOLS.md` note. Existing workspaces won't receive it, so the prompt section is the real carrier.

### 7. Implementation notes (where the build differs from the design above)

- **Overlay:** `overlay.rs::rule_for` has no `tools.skills` entry. A parent can already remove the tool from a child with `disabledTools`. Every path added to that table is a security decision, so nothing was added.
- **Frontmatter:** the line parser in `skills.rs` was extracted as `pub(crate) fn parse_skill_frontmatter`. It returns the fields and the body, and `get_skill_metadata` now uses it too.
- **Find and replace:** the signature is `find_and_replace(content, old, new, replace_all, target_label, old_param_name) -> Result<Replacement { text, count }, String>`.
  - It also keeps CRLF line endings.
  - Error messages name the parameter, so `edit_file` says `old_text` and `skill_manage` says `old_string`.
- **Path check:** `filesystem.rs` exposes `pub(crate) fn is_path_under`. The store checks every read and write target with it, so a symlink cannot escape the skill folder.
- **Store locking:** the store is synchronous and uses a `std::sync::Mutex<()>`, like the other file tools, which also do synchronous file work. The lock is never held across an `.await`.
- **Bundled skills:** a bundled skill is copied into the workspace only after the patch has been validated, so a failed patch leaves no copy behind.
- **Tool results:** the tool returns plain text, not JSON:
  - success: message, path and a `read_file` hint;
  - failure: a `ToolResult::error` carrying the store's message.
- **Approval:**
  - `ToolScopedHook` lives in `src/agent/hook.rs`, next to `CompositeHook`, because it is not CLI-specific.
  - `src/cli/confirm_tools.rs` adds `asks_for_tool_approval(&ToolsConfig)` and `approval_hook_for(&ToolsConfig, hook)`, used by both the CLI and the gateway.
  - `SkillsToolConfig::needs_scoped_approval(confirm_before_execute)` holds the rule.
- **ACP serve:** unchanged. `acp::mapping::tool_kind_for` maps `skill_manage` to `Other`, which always asks the ACP client.

## Milestones

1. **Config:** `SkillsToolConfig` plus tests, `editable_sections` entry, overlay check.
2. **Store:** `skill_store.rs` with validation and actions, plus the `find_and_replace` extraction from `filesystem.rs`, all unit-tested.
3. **Tool and guidance:** `skill_manage` tool, gated registration, prompt section and template.
4. **Approval:** `ToolScopedHook` plus CLI and gateway wiring.
5. **Later (out of scope now):** a Hermes-style background skill review after N tool iterations (could reuse Dream's background-job pattern), `GitStore` versioning of `skills/` for undo, and an optional content scan.

## Tests (inline `#[cfg(test)]`, `tempfile::tempdir()`)

- **Config:** defaults (disabled), camelCase and snake_case aliases, garde ranges, section shown in `editable_sections`.
- **Store:**
  - name validation (valid names, traversal, uppercase, too long)
  - frontmatter validation (missing block, missing `name` or `description`, name mismatch, empty body, oversized)
  - create, then fail on a duplicate
  - patch: unique, ambiguous, not found (the result includes the diff hint), and breaking the frontmatter is rejected
  - write and remove a supporting file, including rejection of paths outside the allowed folders and `..`
  - copy-on-write of a bundled skill
  - delete of a bundled skill is refused
  - the written skill appears in `SkillsLoader::list_skill_summaries`
- **Tool:**
  - required fields per action
  - JSON result shape
  - not registered when disabled; registered when enabled
- **Prompt section:** present only when enabled.
- **Hooks:** `ToolScopedHook` allows other tools and sends `skill_manage` to `inner`; use a fake inner hook.
- **Regression:** existing `edit_file` tests still pass after the extraction.

## Verification

- `cargo test -j 2` (don't run clippy at the same time), then `cargo clippy -j 2`.
- **Manual CLI check:**
  1. Set `tools.skills.enabled=true`.
  2. Give the agent a task that involves a dead end, and correct it.
  3. Confirm it calls `skill_manage`, that `<workspace>/skills/<name>/SKILL.md` exists, and that the next turn's skill index lists it.
- **Approval check:** with `requireApproval=true`, the CLI prompt appears for `skill_manage` only.
- **WebUI check:** a "Skills" section with a switch appears on the Tools page, and approval works in websockets-chat.

### Sample `tools.skills` configurations (`config.json`)

Each sample shows only the relevant part of `config.json`; other `tools` keys stay as they are.

**A. Default: feature off.** Leaving out `skills` gives the same result.

```json
{
  "tools": {
    "skills": { "enabled": false }
  }
}
```

Expected:
- `skill_manage` is not in the tool list and there is no learning guidance in the system prompt.
- Existing skills still load and appear in the skill index as before.

**B. Learning on, no approval.**

```json
{
  "tools": {
    "skills": { "enabled": true }
  }
}
```

Expected:
- `skill_manage` is registered and the guidance appears in the system prompt.
- Skill writes happen without a prompt.
- `requireApproval`, `maxSkillChars` and `maxDescriptionChars` take their defaults (`false`, `100000`, `1024`).

**C. Learning on, approval for skill writes only.**

```json
{
  "tools": {
    "confirmBeforeExecute": false,
    "skills": {
      "enabled": true,
      "requireApproval": true
    }
  }
}
```

Expected:
- In the CLI and websockets-chat, only `skill_manage` calls ask for approval; `read_file`, `exec` and other tools run without asking.
- On channels that cannot ask (e.g. email), `skill_manage` is denied.

**D. Approval for every tool (`requireApproval` has no extra effect).**

```json
{
  "tools": {
    "confirmBeforeExecute": true,
    "skills": {
      "enabled": true,
      "requireApproval": true
    }
  }
}
```

Expected:
- Every tool call asks, `skill_manage` included.
- `ToolScopedHook` is not installed, so you get one prompt per call, not two.

**E. Tight limits, to test the validation errors.**

```json
{
  "tools": {
    "skills": {
      "enabled": true,
      "maxSkillChars": 1000,
      "maxDescriptionChars": 40
    }
  }
}
```

Expected:
- Ask the agent to save a long skill; `skill_manage` returns a size error naming the 1000-character limit.
- A description longer than 40 characters is rejected with a description-length error.

**F. snake_case aliases and `enable`.** These must load exactly like sample C.

```json
{
  "tools": {
    "skills": {
      "enable": true,
      "require_approval": true,
      "max_skill_chars": 100000,
      "max_description_chars": 1024
    }
  }
}
```

**G. Invalid values: startup / config validation must fail.**

```json
{
  "tools": {
    "skills": {
      "enabled": true,
      "maxSkillChars": 500,
      "maxDescriptionChars": 8
    }
  }
}
```

Expected:
- garde rejects `maxSkillChars < 1000` and `maxDescriptionChars < 16`, and the error message names both fields.
- The WebUI Tools page form refuses to save these values.

**H. Learning on, but the tool disabled through `disabledTools`.**

```json
{
  "tools": {
    "disabledTools": ["skill_manage"],
    "skills": { "enabled": true }
  }
}
```

Expected:
- `skill_manage` is removed from the tool list but still shown in the WebUI tool catalogue, as other disabled tools are.
- Decide during implementation whether the learning guidance should then also be suppressed. Recommended: yes, the prompt section checks the registry, not just the config flag.
