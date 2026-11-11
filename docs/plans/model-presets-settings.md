# Plan: "Models" page in Settings — create, edit, delete and activate model presets (websockets-chat)

## Goal

Add a **Models** section to the WebUI's Settings dialog (next to Tools, Tool Safety, MCP Servers). On it the user can:

1. See every model preset in `config.json → modelPresets`, with the **active** one clearly marked (green dot +
   "Active" badge, as in the reference screenshot).
2. **Create** a preset with "+ Add model preset" (dashed button, same as "+ Add MCP Server").
3. **Edit** and **delete** a preset.
4. **Activate** a preset: it becomes the gateway's default model straight away, no restart.

A preset as written to `config.json` (the key is the name the user types):

```json
"modelPresets": {
  "openai-gpt-6.1-terra": {
    "label": null,
    "model": "gpt-6.1-terra",
    "provider": "openai",
    "maxTokens": 32768,
    "contextWindowTokens": 131072,
    "temperature": 1.0,
    "reasoningEffort": "medium"
  }
}
```

## Status

| Milestone | Status |
|---|---|
| M1. Preset form + config edits (pure, backend) | built 2026-11-09 |
| M2. Live default switch in `ModelRuntimeResolver` | built 2026-11-09 |
| M3. WebSocket protocol + handlers | built 2026-11-09; end-to-end checked against a live gateway |
| M4. Models page in websockets-chat | built 2026-11-09 |
| M5. Docs, bundle, browser check | docs + `trunk build` done; browser check pending |

M1 and M2 are independent; M3 needs both; M4 needs M3. Commit only when asked.

Implementation notes (deviations from the text below):
- `FieldKind::Number` stores `min`/`max` as `serde_json::Number`, so every `Eq` derive stays.
- The MCP form helpers shared with presets moved to `src/channels/websocket/settings_form.rs`.
- The add/edit/activate edits return `Option<String>` (as `apply_edit` expects); `None` means "no change".
- The `model_presets` event also carries `providers_without_key`; rows carry `delete_blocked_reason` and
  `missing_api_key`. No separate `active_model_preset` signal: the rows mark the active one.
- `AddMcpServerButton`/`AddMcpServerPanel` became `AddItemButton`/`AddItemPanel` (`add_item_panel.rs`);
  `add_mcp_server_panel.rs` keeps only the MCP wording.
- The dialog now opens on Models (first page). Delete asks inline (Delete / Cancel), not with a browser popup.

## Decisions (made with Gil, 2026-11-09)

- **"Activate" means the gateway default, applied live.** It writes `agents.modelPreset` to `config.json` *and* switches
  the running default (`ModelRuntimeResolver`). Chats that chose their own preset in the chat-input picker
  (`model_preset` session metadata) keep it. Every other chat uses the new default from its next turn.
- **Rows have Edit and Delete as well as Activate.** Deleting is refused for the active preset and for the preset
  named by `agents.dream.dreamModelPreset`; `validate_model_presets` would reject either anyway.

### Defaults I chose (not discussed; easy to change)

- **The form fields are exactly the seven preset settings**: `label`, `model`, `provider`, `maxTokens`,
  `contextWindowTokens`, `temperature`, `reasoningEffort`. The name is asked separately, as for MCP servers.
- **`provider` is a drop-down** listing only providers that `create_provider_for` can build: `auto`, `openai`,
  `anthropic`, `openrouter`, `together`, `custom`. `gemini` and `azure_openai` are left out because the factory
  returns "Invalid provider" for them today. A provider that has no API key under `providers.<name>` shows a warning
  note under the field. API keys themselves stay out of this page.
- **`reasoningEffort` is a drop-down**: `none`, `low`, `medium`, `high`, `adaptive`. `none` is stored as `null`,
  as the wizard does.
- **`temperature` is limited to 0.0–1.0**, the existing garde range on `ModelPresetConfig`.
- **The implicit `default` preset (the flat `agents.*` fields) is a read-only row**, shown first and labelled "Default
  (agent settings)". It can be activated, which removes `agents.modelPreset`, but not edited or deleted. Without it the
  user could never switch back to a config that uses no preset.
- **Edits are applied live too.** Saving an edit to the active preset re-resolves the default straight away. Adding
  or deleting a preset updates the list in the chat-input picker.
- **Every write gets the full list back** as one `model_presets` event that names what changed. The list is small,
  so this is simpler than the MCP pattern of one reply per row.
- **Same permissions as MCP server writes**: `setting_write_denial` refuses anonymous, remote and non-admin
  connections. Read-only connections see the list but get no forms or buttons.

## What exists today (verified)

- **Config** (`src/config/schema.rs`):
  - `ModelPresetConfig` (≈l.321) already has the exact shape above. `provider` defaults to `"auto"` and
    `reasoning_effort` is `Option<String>`.
  - `Config.model_presets: HashMap<String, ModelPresetConfig>` (≈l.1939); `AgentsConfig.model_preset: Option<String>`.
  - `validate_model_presets` (≈l.2287) rejects a preset named `"default"`, an empty `model`, garde failures and
    dangling references. The name `"default"` comes from `RESERVED_MODEL_PRESET_NAME` (≈l.2279).
  - `ModelPresetConfig` does **not** derive `JsonSchema` yet.
- **Runtime** (`src/agent/model_runtime.rs`):
  - `ModelRuntimeResolver { config: Config, provider_cache, default_runtime: Mutex<ModelRuntime> }`.
  - `select_preset(name)` exists but has no callers.
  - The `config` is a startup snapshot, so a preset added later cannot be resolved yet.
  - `available_preset_names()` puts `default` first.
  - The websocket runtime shares the resolver as `shared.runtime_resolver: Arc<ModelRuntimeResolver>` (runtime.rs ≈l.5544).
- **Per-chat preset**: the existing `set_model_preset` envelope (runtime.rs ≈l.762) and `ModelPresetPicker` in
  `chat-ui/src/components/chat_input.rs`. `attached` replies send `model_presets` (names) and `model_preset`
  (≈l.4397). This feature leaves them as they are and only refreshes the picker's list.
- **Reasoning effort choices**: there is no enum. The only list is `REASONING_EFFORT_CHOICES` in `src/cli/wizard.rs:79`.
- **Pieces to reuse from MCP Servers and Tools**:
  - `src/channels/websocket/mcp_server_form.rs` builds a form from a schemars schema and edits the raw JSON text,
    keeping `${VAR}` placeholders. Its helpers: `form_properties`, `shown_value`, `form_fields`.
  - `src/agent/tools/tool_settings_api.rs` has `FieldKind`, `field_kind` (`x-choices` gives a `Choice`),
    `summarize_field`, `check_value`, `ToolSettingsSummary` and `ToolFieldSummary`. There is **no float kind**, so
    `temperature` needs one.
  - `src/channels/websocket/config_toggles.rs` has `parse_root`, `render`, `is_valid_name` and `apply_edit`
    (validate, back up, write atomically). Writes are serialised by `MCP_CONFIG_WRITE_LOCK` (runtime.rs ≈l.2966).
  - `setting_write_denial`, `send_setting_error` and `SettingKind` are in runtime.rs.
  - Frontend: `SettingsPage` / `SettingsPanel` / `SettingsActions` (`settings_pages.rs`), `AddMcpServerButton` /
    `AddMcpServerPanel` (`add_mcp_server_panel.rs`), `ToolConfigForm` / `FieldInput` (`tool_config_form.rs`), and
    `Tile`, `SectionHeading`, `ListPageShell` (`list_page.rs`).

---

## M1. Preset form + config edits (pure, backend)

### 1.1 Shared choice lists (`src/config/schema.rs`)
- Add `pub const REASONING_EFFORT_CHOICES: [&str; 5]` and `pub const MODEL_PRESET_PROVIDER_CHOICES: [&str; 6]`, each
  with a doc comment. Remove the copy in `wizard.rs` and import the shared constant there (one source of truth).
- Derive `schemars::JsonSchema` on `ModelPresetConfig`:
  - add doc comments that become the field help, ending with `Default: …`;
  - add `x-choices` on `provider` and `reasoning_effort`;
  - add `minimum`/`maximum` on `max_tokens`, `context_window_tokens` and `temperature`.
- Add a unit test that the schema's `x-choices` match the constants and that every provider choice is accepted by
  `create_provider_for` (except for a missing key).

### 1.2 Float fields (`src/agent/tools/tool_settings_api.rs`)
- Add `FieldKind::Number { min: Option<f64>, max: Option<f64> }`. `field_kind` maps `"number"` to it.
- `check_value` accepts JSON numbers within the range.
- Tests for both. The `Eq` derive has to go, or `min`/`max` must be stored as strings; choose whichever compiles
  with the least churn and note it.

### 1.3 New module `src/channels/websocket/model_preset_form.rs`
Modelled on `mcp_server_form.rs`. Every function is a pure function of the config text and has a doc comment:

```rust
/// One row of the Models page: the preset, whether it is the configured default, and its form.
pub struct ModelPresetRow { name, label: Option<String>, model, provider, active: bool,
                            built_in: bool /* the implicit "default" */, deletable: bool,
                            form: Option<ToolSettingsSummary> }

pub fn model_preset_rows(text: &str, with_forms: bool) -> Result<Vec<ModelPresetRow>, ToggleError>;
pub fn new_model_preset_form() -> ToolSettingsSummary;
pub fn with_model_preset_added(text: &str, name: &str, values: &HashMap<String, Value>) -> Result<String, ToggleError>;
pub fn with_model_preset_settings(text: &str, name: &str, values: &HashMap<String, Value>) -> Result<String, ToggleError>;
pub fn without_model_preset(text: &str, name: &str) -> Result<String, ToggleError>;
pub fn with_default_model_preset(text: &str, name: &str) -> Result<String, ToggleError>; // "default" removes the key
```

- Names: `is_valid_name`, not `default`, not already used (for add). The key is `modelPresets`, also accepting
  `model_presets`, via `existing_or_default_key`.
- `reasoningEffort: "none"` is written as `null`.
- Each result is checked by deserialising it into `Config` and running `validate_model_presets`, so an edit cannot
  produce a file the bot refuses at startup. Delete therefore fails cleanly on an active or dream preset.
- `${VAR}` placeholders are checked as in `mcp_server_form::check_placeholders`. If that helper fits, move it into a
  shared spot rather than copying it.
- Add a `ToggleError` variant or variants for "name already used", "preset in use" and "reserved name".

**M1 tests** (in-module):
- add appends and leaves the rest of the file untouched;
- add rejects a duplicate, `default`, an empty `model` and out-of-range values;
- edit changes only the named preset;
- `none` is stored as null;
- delete refuses the active and dream presets;
- activating `default` removes `agents.modelPreset`;
- rows mark exactly one row active, with `default` first and `built_in`;
- `${VAR}` values survive a round trip.

---

## M2. Live default switch in `ModelRuntimeResolver`

- Move the catalog into `presets: RwLock<HashMap<String, ModelPresetConfig>>` and resolve presets against it.
  Provider settings (`providers.*`) stay in the startup `config`, so the provider cache stays valid.
- New method:
  ```rust
  /// Replace the preset catalog with `presets` and make `default_name` the process-wide default
  /// (`None`/"default" = flat agents.* fields). Sessions with their own preset are untouched;
  /// an override that names a deleted preset falls back to the default, as today.
  pub fn reload_presets(&self, presets: HashMap<String, ModelPresetConfig>, default_name: Option<&str>) -> Result<ModelRuntime, String>;
  ```
- Keep `flat_default` (built from the startup `initial_provider`) so that activating `default` restores it exactly.
- `available_preset_names` reads the `RwLock`.
- **M2 tests**, using the existing `build_resolver` / `for_tests` helpers:
  - reload adds a preset that `resolve_preset` can then find;
  - reload with a new default changes `current_default`;
  - a session override survives a reload;
  - a deleted override falls back;
  - `default` restores the flat runtime.

---

## M3. WebSocket protocol + handlers

### 3.1 Server (`src/channels/websocket/types.rs`, `runtime.rs`)

| Client envelope | Handler | Effect |
|---|---|---|
| `{"type":"list_model_presets"}` | `handle_list_model_presets` | sends `model_presets` |
| `{"type":"add_model_preset","name","values"}` | `handle_add_model_preset` | M1 edit + reload |
| `{"type":"set_model_preset_config","name","values"}` | `handle_set_model_preset_config` | M1 edit + reload |
| `{"type":"delete_model_preset","name"}` | `handle_delete_model_preset` | M1 edit + reload |
| `{"type":"set_default_model_preset","name"}` | `handle_set_default_model_preset` | M1 edit + reload |

The existing `set_model_preset` keeps its per-chat meaning, so the activate envelope gets a different name.

- One reply event:
  ```json
  {"event":"model_presets","presets":[…rows…],"active":"openai-gpt-6.1-terra","can_modify":true,
   "new_preset_form":{…},"changed":{"action":"added|updated|deleted|activated","name":"…"}}
  ```
  `changed` is absent on a plain list.
- The four writers share one helper, `apply_model_preset_edit(shared, edit_fn, action, name)`. It:
  1. calls `setting_write_denial`;
  2. takes `MCP_CONFIG_WRITE_LOCK` (rename it to `CONFIG_WRITE_LOCK` in the same change, since it now guards more than MCP);
  3. runs `config_toggles::apply_edit`;
  4. loads the saved file with env vars resolved, without panicking: no `load_config`, which panics; use the
     `config_file::validate_config_text` path plus `resolve_config_env_vars`;
  5. calls `runtime_resolver.reload_presets`;
  6. sends `model_presets`.
- Errors go through `send_setting_error` with new `SettingKind` values `model_preset_add`, `model_preset_config`,
  `model_preset_delete` and `model_preset_activate`.
- If the file was saved but the live reload failed (e.g. the provider has no key), report it as a `setting_error`
  whose detail is `saved_reload_failed`, so the user knows a restart is needed.
- **M3 tests** (runtime.rs test module, following the patterns of `setting_toggles_are_refused_for_anonymous_and_remote_connections`
  and `test_runtime_resolver_with_preset`):
  - every write is refused for anonymous and remote connections;
  - the list hides forms for read-only connections;
  - after activation the resolver's default has changed;
  - error kinds are named correctly;
  - `types.rs` envelope parse tests.

---

## M4. Models page (websockets-chat + chat-ui)

### 4.1 Wire layer (`websockets-chat/src/protocol.rs`)
- Add `ClientEnvelope::{list_model_presets, add_model_preset, set_model_preset_config, delete_model_preset,
  set_default_model_preset}` and `ServerEvent::ModelPresets { presets, active, can_modify, new_preset_form, changed }`.
- Add `ModelPresetSummary` in `chat-ui/src/models.rs`.
- Add `ToolFieldKind::Number` and render it in `FieldInput` as `<input type="number" step="0.1">`.
- Serialise/deserialise tests like `client_envelope_add_mcp_server_serializes_name_and_values`.

### 4.2 Page (`settings_pages.rs` + new `components/model_presets_page.rs`)
- `SettingsPage::Models`, placed first in `ALL`.
  - Title: "Models".
  - Subtitle: "Create model presets and choose which one the agent uses. API keys are set per provider in the
    configuration file."
- `SettingsPanel` gains `model_presets`, `active_model_preset`, `new_preset_form` and `editing_preset` signals.
  `SettingsActions` gains `on_add_model_preset`, `on_save_model_preset`, `on_delete_model_preset` and
  `on_activate_model_preset`, wired in `app.rs` next to the MCP senders.
- Each row (rounded card, as in the screenshot):
  - a `Tile`, the label or name in bold, a green dot plus an "Active" pill when active, and a muted
    `model · provider` subline;
  - on the right: **Activate** (hidden when already active), **Edit** (opens `ToolConfigForm` inline), and
    **Delete** (disabled with a tooltip when `!deletable`; asks before deleting).
  - The built-in `default` row shows only Activate.
- Below the rows, a dashed **"+ Add model preset"** button opens a panel with a Name input plus the blank form. Reuse
  `AddMcpServerButton`/`AddMcpServerPanel` by giving them a label and callback (rename to
  `AddItemButton`/`AddItemPanel`) instead of copying them.
- When `model_presets` arrives, also update `app.rs`'s `model_presets` signal so the chat-input picker shows new and
  deleted presets.
- Keep the logic in pure helpers so it can be tested, as the existing pages do:
  - `active_row_index`
  - `row_actions(row) -> {activate, edit, delete}`
  - `notice_for(changed)`, e.g. "Preset 'x' is now active."
  - `apply_model_presets_event`
- **M4 tests**:
  - the page list and title (update `SettingsPage::ALL` test);
  - the actions offered per row (active, built-in, dream-referenced);
  - the notice text;
  - the event replaces the list and the picker names.

---

## M5. Docs, bundle, browser check

- `docs/WEBSOCKETS_CHAT.md`: add the five envelopes and the `model_presets` event.
- Add a short "Model presets" section (fields, choices, live activation vs per-chat picker) to `docs/INTERACTIVE_CONSOLE.md`
  next to `/model-preset`. There is no dedicated doc today.
- Rebuild the bundle with `trunk build` in `websockets-chat`.

## Verification (per milestone)

- Always: `cargo build`, `cargo clippy --all-targets`, `cargo test --lib`, plus `cargo test` inside `websockets-chat`.
- M1: `cargo test --lib channels::websocket::model_preset_form`. Check by hand that a preset added through the pure
  function round-trips through `load_config` and `check-config`.
- M2: `cargo test --lib agent::model_runtime`.
- M3: `cargo test --lib channels::websocket`.
- M4/M5, manual check in the browser:
  1. Open Settings → Models. The active preset has the dot and badge.
  2. Add `openai-gpt-6.1-terra` with the values above. It appears, and `config.json` contains the exact block.
  3. Activate it. Without restarting, send a message in a chat that has no per-chat preset; the turn uses
     `gpt-6.1-terra` (log / model shown in the chat).
  4. A chat whose picker is set to another preset keeps that preset.
  5. Edit the temperature and check that it is saved. Delete is disabled on the active row. Activate "Default",
     then delete the preset; it disappears from the picker.
  6. Reload the page. The state persists, and the gateway still starts after a restart.
  7. A read-only connection sees the rows without buttons.

## Risks

| Risk | Mitigation |
|---|---|
| `load_config` panics on a bad file during the live reload | Use the validating, non-panicking path; the file is validated before it is written anyway |
| The provider of an activated preset has no API key, so the reload fails | Save anyway, report `saved_reload_failed`, warn in the form when the provider has no key |
| A float field breaks `Eq` on the shared `FieldKind` | Decide in 1.2 and keep the change local |
| Two writers race on `config.json` | The same global write lock as the MCP and tool settings |
| Dream / subagents keep the old default | They read through the same resolver, so they pick it up; covered by an M2 test |

## Out of scope

- Editing API keys or provider settings (`providers.*`). Today that stays in the config file editor.
- Building `gemini` / `azure_openai` providers in the factory.
- Changing `/model`, which still changes the process-wide model directly.
- A CLI equivalent; the wizard already covers it.
