# Plan: Settings → Providers page (dedicated + named custom providers)

## Context

API keys, base URLs, headers and the API protocol (`providerBackEnd`) of LLM providers could only be edited in
`config.json`. Two further gaps:

- There was exactly one `providers.custom` slot. The wizard maps EdenAI, Requesty, NanoGPT, z.ai and Xiaomi all onto it,
  so each overwrote the last.
- Provider settings were read once at startup. `ModelRuntimeResolver` kept a startup `Config` and a `provider_cache`
  that was never cleared.

**Goal:** a new **Providers** page in the websockets-chat Settings dialog.

- **The list:** every configured provider, each with Edit and Delete. Laid out like the Models page, no Activate button.
- **"+ Add provider":** opens a panel with a two-way toggle:
  - **Dedicated model provider:** pick openai, anthropic, openrouter or together, then enter the API key. A blank key
    means "use the env var", with a warning when that variable is not set. A collapsible **Customized settings**
    section holds Base URL (default as placeholder), API protocol, and Extra headers as add/delete rows.
  - **Custom model provider:** Name, Base URL, API protocol (OpenAI Chat Completions / OpenAI Responses / Anthropic
    Messages), API key, plus the same Extra headers. The button reads **Create provider**.
- **Saving:** writes `config.json` (validated, with a backup) and **applies live**, with no restart.

**Decisions (with Gil, 2026-11-12):** several **named** custom providers (presets select them by name); changes apply
**live**; the Dedicated drop-down offers only the **buildable** providers: openai, anthropic, openrouter, together.

**Defaults chosen (easy to change):**

- **API keys never go to the browser.** The form shows "configured" or blank; a blank submit keeps the stored key.
  A **Forget stored key** button clears it.
- **Extra headers are sent in clear**, as MCP headers are today (admin, local connections only).
- **Deleting is refused while a provider is in use:** named by `agents.provider` or by any preset.
- **`auto` is unchanged:** it ignores custom providers and env-only keys.

```json
"providers": {
  "anthropic": { "apiKey": "", "apiBase": null, "extraHeaders": { "anthropic-beta": "…" } },
  "customProviders": {
    "my-relay": { "apiKey": "${RELAY_KEY}", "apiBase": "https://gateway.example/v1", "providerBackEnd": "openai_responses" }
  }
}
```

## Status

| Milestone | Status |
|---|---|
| M1. Config schema, factory, catalog (backend, no UI) | built 2026-11-12 |
| M2. Live provider reload in `ModelRuntimeResolver` | built 2026-11-12 |
| M3. WebSocket protocol + handlers | built 2026-11-12 (pure form + handlers + unit tests; no live-gateway check yet) |
| M4. Frontend (websockets-chat + chat-ui) | built 2026-11-12 (tests pass, `trunk build` ok) |
| M5. Docs, bundle, browser check | docs + `trunk build` done; live-gateway protocol smoke test passed 2026-11-12 (Node WS script: add/edit/delete/forget, reserved + in-use refusals, preset offers the custom provider, no key text in replies or logs); visual browser check pending |

M1 and M2 are independent; M3 needs both; M4 needs M3; M5 comes last. Commit only when asked.

## M1. Config schema, factory, catalog (backend, no UI)

- `ProvidersConfig.custom_providers`: `BTreeMap<String, ProviderConfig>`, `customProviders` (alias
  `custom_providers`), skipped when empty. `get_by_name` / `get_by_name_mut`: built-in slots first (incl. `groq`), then
  the custom map.
- `validate_custom_provider_names(&Config)`: valid names (`is_valid_name`), not `auto`, `default` or a registry name.
  Called from `load_config`, `validate_config_text` and `config check`.
- Form metadata: `JsonSchema` for `ProviderConfig` / `ProviderBackend`; `apiKey` is `x-secret`, `providerBackEnd` has
  `x-choices` = `PROVIDER_BACKEND_CHOICES` (`""` = automatic); labels "Base URL", "Extra headers", "API key",
  "API protocol".
- Factory: `resolve_provider_credentials` accepts the buildable providers or any custom name (built with the `custom`
  registry spec). A blank stored key falls back to `spec.env_key` (`configured_or_env_api_key`, moved here from the
  catalog). `custom_provider_names(&Config)`.
- Catalog: `build_model_listing_request` lists a custom name with the `custom` spec and its configured protocol.
- Preset form: provider choices = `MODEL_PRESET_PROVIDER_CHOICES` + custom names, for the form and for validation;
  `provider_lacks_api_key` counts the env var.

## M2. Live provider reload: `src/agent/model_runtime.rs`

- Providers behind a lock. `reload_providers(&self, ProvidersConfig) -> Result<ModelRuntime, String>`: swap, clear
  `provider_cache`, rebuild the flat default provider (keep the old one and return the error on failure), re-resolve
  the current default preset, store and return it.
- Chats in the middle of a turn keep their `Arc` provider; the next turn resolves fresh.
- **Still needs a restart:** transcription keys (`channels/manager.rs`), child-agent env forwarding, Docker secret
  extraction.

## M3. WebSocket protocol + handlers

New pure module `channels/websocket/provider_form.rs` (modelled on `model_preset_form.rs`, editing the raw config text
to keep `${VAR}` and key order): `ProviderRow`, `providers_overview`, `with_provider_added`, `with_provider_settings`,
`without_provider`, `with_provider_key_forgotten`, `live_providers`. A blank secret is never written;
`providerBackEnd: ""` removes the key; a custom provider needs a Base URL. New `ToggleError` variants
`UnknownProvider`, `ProviderExists`, `ProviderInUse`.

| Client envelope | Effect |
|---|---|
| `list_providers` | reply `providers` (rows; forms only when `can_modify`) |
| `add_provider` `{kind, name, values}` | add, save, reload, reply |
| `set_provider_config` `{name, values}` | change, save, reload, reply |
| `forget_provider_key` `{name}` | clear the stored key |
| `delete_provider` `{name}` | remove or reset, save, reload, reply |

Shared helper `apply_provider_edit` (copy of `apply_model_preset_edit`). New `SettingKind` values `provider_add`,
`provider_config`, `provider_delete`. Submitted values are never logged. Read-only connections get rows without forms
and without header values.

## M4. Frontend (websockets-chat + chat-ui)

`ProviderSummary`, `DedicatedProviderChoice`, `ProviderChange`; protocol constructors and `ServerEvent::Providers`;
`SettingsPage::Providers` after Models; `components/providers_page.rs`, `segmented_toggle.rs`, `key_value_rows.rs`;
pure helpers with tests (`row_subline`, `missing_key_note`, `delete_question`, `dedicated_choices_left`, `form_ready`,
header-lines round trip). After a provider change the page also requests `list_model_presets`.

## M5. Docs, bundle, browser check

`docs/WEBSOCKETS_CHAT.md` ("Settings → Providers"), `docs/INTERACTIVE_CONSOLE.md` (`customProviders`, env fallback),
`trunk build` in `websockets-chat`, then the browser check: list shows no keys in WS frames; add dedicated anthropic
with a blank key (env warning); add custom `my-relay`, pick it in a preset, activate and chat; edit the key live;
delete refused while in use; read-only connection sees no forms.

## Risks

| Risk | Mitigation |
|---|---|
| Keys leaking to the browser or logs | `x-secret`, values never in rows, `send_setting_error` logs only kind and name; a test asserts rows contain no key text |
| Live rebuild of the flat default provider fails | Keep the old provider, report `saved_reload_failed` |
| Env fallback changes behaviour for existing configs | Only explicit providers with a blank key (401 today); `auto` unchanged |
| Custom name shadowing a built-in | Built-in slots resolve first; reserved/registry names rejected on load and save |

## Out of scope

Renaming a custom provider; Azure, Gemini, Groq and new registry providers in the Dedicated list; `auto` considering
custom providers or env-only keys; live reload of transcription keys and child-agent env forwarding; moving custom
provider keys into `.env.docker` during Docker onboarding; wizard creating named custom entries.
