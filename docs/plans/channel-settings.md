# Plan: Settings → Channels page (Email first)

## Context

Today the email channel (IMAP in, SMTP out) can only be set up in `config.json` (`channels.email`) or with the
onboarding wizard. The Settings dialogue in websockets-chat already edits models, providers, tools and MCP servers,
but not channels. Gil wants a **Channels** menu item whose page has an **Email** section that can be expanded and
collapsed. In that section you can:

- see the existing email channel, with **Edit** and **Delete**, or
- use a **Create email channel** button when no channel exists.

Other channel types come later, so the protocol and page are keyed by channel type from the start.

Facts that shape the design:

- **Config shape.** `EmailConfig` (`src/config/channels.rs`) is one untyped entry in
  `ChannelsConfig.extra["email"]` (`#[serde(flatten)]`). There is a single email channel. Passwords are plain
  strings, and `${VAR}` placeholders are allowed.
- **Startup only.** Channels are built once in `ChannelManager::new` (`src/channels/manager.rs:110`) and never
  reloaded.
- **Quiet failures today.** The runtime check `EmailChannel::validate_config` (`src/channels/email.rs:227`) only logs
  what is missing. A malformed `channels.email` is dropped silently (`src/channels/registry.rs:41-49`).

**Decisions (with Gil, 2026-11-15):**

- **Changes apply on restart.** Saving writes `config.json` and shows the existing amber "Restart required → Restart
  now" banner, as for MCP servers. `ChannelManager` is not changed.
- **Menu order:** Models, Providers, **Channels**, Tools, Tool Safety, MCP Servers.

**Defaults chosen (easy to change):**

- **Passwords never go to the browser.** They are `x-secret`, as provider API keys are. A blank password on edit keeps
  the stored one.
- **Delete removes the whole `channels.email` object**, including its per-channel `sendProgress` and similar
  overrides.
- **A save is refused when the channel could not start:**
  - if any of these is blank: IMAP host, username or password; SMTP host, username or password;
  - if `enabled` is on and `consentGranted` is off.
- **Keys the form does not know are kept as they are** (e.g. per-channel `sendProgress`).
- **The form is grouped** under headings: General, Incoming mail (IMAP), Outgoing mail (SMTP), Behaviour,
  Security, Attachments. Grouping is an optional `group` on form fields, so the Tools and MCP forms do not change.

## Status

| Milestone | Status |
|---|---|
| M1. Backend: schema and validation | built 2026-11-15 |
| M2. Backend: pure form module `channel_form.rs` | built 2026-11-15 |
| M3. Backend: WebSocket protocol and handlers | built 2026-11-15 |
| M4. Frontend (chat-ui and websockets-chat) | built 2026-11-15 (host tests pass, `trunk build` ok) |
| M5. Docs, bundle and checks | built 2026-11-15; live-gateway check in headless Chrome passed (steps 3–5 of Verification), read-only check passed |

Uncommitted. Commit only when asked.

### Notes from the build

- **Settings are written in form order.** `apply_settings` walks the schema's properties rather than the submitted
  map, so a new `channels.email` entry lists its keys as the form does (the first live run showed random order).
- **Shared helpers moved to `settings_form.rs`:** the generic `${VAR}` explanation (was copied in
  `model_preset_form.rs` and `provider_form.rs`), `blank_value` and `is_blank` (were private to `provider_form.rs`).
- **`ToolConfigForm`** gained an optional `submit_label` and `on_cancel`, so the create panel reuses it ("Create
  channel" + Cancel) instead of a new form component. `AddItemButton`'s label is now a `String`.
- **The key reference** went to a new `docs/EMAIL_CHANNEL.md`: no document listed the config keys, and
  `INTERACTIVE_CONSOLE.md` is about the console.
- **Error naming:** a refusal for missing settings names the first blank setting (`imapUsername`) and lists the
  labels of all blank ones in the message.

## M1. Backend: schema and validation (no UI)

- **`src/config/channels.rs` — form metadata on `EmailConfig`:** `JsonSchema`, a doc comment per field (help text),
  `x-label`, `x-secret` on both passwords, `x-group` per field, `range(min = 5)` on `pollIntervalSeconds`; fixed the
  wrong `// 10MB` comment.
- **`EmailConfig::missing_required_settings(&self) -> Vec<&'static str>`:** camelCase names of the blank required
  fields. `EmailChannel::validate_config` calls it.
- **Load-time check `validate_channel_configs(&Config)`:** `channels.email`, when present, must deserialise as
  `EmailConfig`. Called from `load_config`, `config_file::validate_config_text` and `check-config`. `registry.rs` logs
  a warning instead of silently falling back to the default.
- **Optional form-field grouping:** `group: String` on `ToolFieldSummary`, from `x-group`, skipped when empty.

## M2. Backend: pure form module `src/channels/websocket/channel_form.rs`

- `SettingsChannelType { Email }` with `key()`, `title()`, `from_key()`, `parse()`, `ALL`, `form_properties()` and
  `check_entry()`. A later channel adds one variant.
- `ChannelRow { channel_type, title, configured, enabled, summary, notes, form, new_form }`.
- `channels_overview(text, with_forms)`, `with_channel_added`, `with_channel_settings` (`None` = no change),
  `without_channel`, each edit with an `_in` variant taking the environment lookup.
- New `ToggleError` variants: `UnknownChannelType`, `ChannelExists`, `ChannelNotConfigured`.
- Unit tests: round trips, blank password kept, no password text in rows, `${VAR}` kept and unset refused, unknown
  keys survive, missing settings and enabling without consent refused, double add / missing channel / unknown type
  refused, form key order.

## M3. Backend: WebSocket protocol and handlers

| Client envelope | Effect |
|---|---|
| `list_channels` | reply `channels {channels, can_modify}`; forms only when `can_modify` |
| `add_channel {channelType, values}` | add, save, then reply `channels` with `changed {action: "added", channelType}` and `restart_required` |
| `set_channel_config {channelType, values}` | change and save; reply as above with `"updated"` (`restart_required` only when something changed) |
| `delete_channel {channelType}` | remove and save; reply as above with `"deleted"` |

- `EnvelopeType` / `WsOutboundEvent` variants in `types.rs`; handlers and the shared `apply_channel_edit` in
  `runtime.rs` (`setting_write_denial`, then `CONFIG_WRITE_LOCK`, then `config_toggles::apply_edit`; no live reload).
- `SettingKind` `channel_add`, `channel_config`, `channel_delete`.
- Tests: anonymous list denied; anonymous and remote writes refused naming the kind and channel type; the `channels`
  body carries no password text; mapping tests.

## M4. Frontend (chat-ui and websockets-chat)

- `ChannelSummary`, `ChannelChange` and `ToolFieldSummary.group` in `chat-ui/src/models.rs`.
- Protocol: `list_channels`, `add_channel`, `set_channel_config`, `delete_channel`, `ServerEvent::Channels`.
- `SettingsPage::Channels` after Providers; panel signals `channels`, `channels_loaded`, `editing_channel`,
  `adding_channel`, `confirm_delete_channel`, `collapsed_channel_sections`; actions `on_add_channel`,
  `on_save_channel`, `on_delete_channel`; `"channel_"` routing of `setting_error`.
- `components/channels_page.rs`: collapsible `ChannelSection` per row, badge, Edit/Delete, create panel. Pure helpers
  `section_badge`, `row_summary_line`, `delete_question`, `create_button_label`, `toggle_section`,
  `new_form_draft_key`, with tests.
- `group_fields` + group headings in `ToolConfigForm` (tested).
- `components/delete_confirmation.rs` replaces the copies in `providers_page.rs` and `model_presets_page.rs`.

## M5. Docs, bundle and checks

- `docs/WEBSOCKETS_CHAT.md`: "Settings → Channels" section with the protocol.
- `docs/EMAIL_CHANNEL.md`: `channels.email` reference table and example.
- This plan, with the status table.
- `trunk build` in `websockets-chat`.

## Verification

1. **Backend tests:** `cargo test -j 2`.
2. **Frontend tests:** `cargo test` in `websockets-chat` and `chat-ui`, then `trunk build`.
3. **Live gateway, local admin connection (headless Chrome):**
   - Channels shows the Email section, expanded, with "+ Create email channel". ✔
   - Creating without the required settings is refused and names the missing fields. ✔
   - Creating with `${IMAP_PASSWORD}` and an unset variable is refused; with values filled in it saves and shows the
     restart banner. ✔
   - The `channels` frames contain no password text. ✔
   - Edit the poll interval with the password left blank: `config.json` keeps the password and a `.bck` backup is
     written. ✔
   - Collapse and expand the section. ✔
   - Delete asks first, then removes `channels.email`. ✔
4. **Restart:** "Restart now" restarts the gateway, and the email channel's start log line appears (with consent
   given). ✔
5. **Read-only:** a non-admin connection sees the row without forms or buttons, and writes are refused
   (`admin_required`). ✔

## Out of scope

- Live start and stop of channels.
- WhatsApp and WebSocket channel forms (they come later as new `SettingsChannelType` variants).
- Several email channels.
- Editing the global `channels.allowFrom`, `sendProgress` and similar.
- Moving email passwords into `.env.docker`.
