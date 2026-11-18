# Websockets chat UI

`websockets-chat/` is the browser UI for `rust-bot`'s WebSocket gateway. It is a Leptos (Rust + WASM) single-page app styled with Tailwind, and it renders token-by-token streaming replies, live tool-activity chips and reasoning panels as they arrive. It shares its login form, message composer and markdown rendering with the older [`web-chat`](../web-chat/README.md) UI through the `chat-ui` crate.

For the app's own details (optional login, guest session isolation, minting tokens, dev-server proxying) see [websockets-chat/README.md](../websockets-chat/README.md). This page covers how it is built, how it ends up inside the executable, and how to run it.

## Embedded in the executable

The compiled UI is part of the `rust-bot` binary:

1. `trunk build --release` writes the bundle (`index.html`, `*.js`, `*.wasm`, `*.css`) to `websockets-chat/dist/`.
2. At `cargo build` time, `build.rs` copies `websockets-chat/dist/` into the build output directory, and `rust-embed` compiles those files into the binary (`src/utils/embedded_gateway_ui.rs`).
3. When you run `./rust-bot gateway`, the gateway serves the UI at `/` straight from the binary. There is nothing to unpack, copy or deploy next to the executable: the files are served from memory, and unknown paths fall back to `index.html` so the single-page app routes correctly.

This means the Trunk build must happen **before** the Cargo build, otherwise the binary is built without the UI. If `websockets-chat/dist/` is missing, `build.rs` embeds a stub page instead (so `cargo build` and `cargo test` still work), and the gateway then serves no web UI. Release archives are built with the UI included.

Which UI the gateway serves is decided in this order:

1. `--web-root <dir>` on the command line.
2. `gateway.webRoot` in the config file.
3. The UI embedded in the binary.

A web root is only used if it is a directory containing `index.html`; otherwise the gateway logs a warning and falls back to the embedded UI.

## Prerequisites

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk --locked
```

Tailwind CSS is downloaded by Trunk on the first build, so Node.js/npm is not required.

## Build

Build the UI and then the binary, in that order:

```bash
cd websockets-chat
trunk build --release
cd ..
cargo build -r
```

Or use the helper script, which runs Trunk for `web-chat` and `websockets-chat` and then `cargo build -r`:

```bash
./scripts/build_all.sh            # Linux / macOS
.\scripts\build_all.bat --release # Windows
```

## Run

### Gateway config

The gateway needs a `websocket` channel whose path is `/ws`, not the default `/`. A WebSocket handler registered at `/` would take priority over the page that serves the UI, and opening the gateway URL in a browser would fail with "Connection header did not include 'upgrade'". `jwt.aud` must equal `path`:

```json
"channels": {
  "websocket": {
    "path": "/ws",
    "jwt": { "aud": "/ws" }
  }
}
```

By default the gateway requires login. Mint a token and register a user with `generate-jwt-token --purpose webui` (see [Command line](COMMAND_LINE.md#jwt-keypair-and-tokens)); this also writes the matching `channels.extra.websocket` entry into your config. To allow guests, set `"requireAuth": false`. Both options are described in [websockets-chat/README.md](../websockets-chat/README.md).

### Using the embedded UI

```bash
./rust-bot gateway --config ./.rust-bot/config.json
```

Then open `http://127.0.0.1:18790/` (the default gateway address; override with `--port` or `gateway.port`). Login and the WebSocket connection are both same-origin, so no further setup is needed.

To serve a freshly built bundle without recompiling `rust-bot`:

```bash
./rust-bot gateway --config ./.rust-bot/config.json --web-root ./websockets-chat/dist
```

### Development with live reload

Run the gateway in one terminal, and the Trunk dev server in another:

```bash
cargo run -- gateway --config ./configs/simple1/config.json
```

```bash
cd websockets-chat
trunk serve --open
```

Trunk serves the UI on `http://127.0.0.1:8902/` and proxies only the `/v1/login` request to the gateway. The browser opens the WebSocket directly to `ws://127.0.0.1:18790/ws`. If your gateway listens elsewhere, pass the address in the URL:

```
http://127.0.0.1:8902/?wsBase=ws://127.0.0.1:18790
```

## Settings → Models

The Settings dialog opens on its **Models** page. It lists the model presets of `config.json` (`modelPresets`), with the built-in **Default (agent settings)** row first; that row stands for the flat `agents.model` / `agents.provider` / … settings. The active preset has a green dot and an "Active" badge. A signed-in, local administrator can:

- **Activate** a row. It becomes the gateway's default model at once, no restart: `agents.modelPreset` is written (removed for the built-in row) and the running agent switches. Chats that picked their own preset in the chat input keep it; every other chat uses the new default from its next message.
- **Edit** a preset: label, model, provider, max tokens, context window, temperature (0.0–1.0; not sent to OpenAI, which uses the Responses API, see [Interactive console](INTERACTIVE_CONSOLE.md#model-presets)) and reasoning effort (`none`, `low`, `medium`, `high`, `xhigh`, `max`, `adaptive`; `none` is stored as `null`). Saving the active preset applies from the next message.
- **Pick the model from a list.** In the edit and add forms the provider comes first. Once one is chosen, a drop-down under the Model input lists the models that provider offers, asked from its API with the configured key (see [Listing a provider's models](#listing-a-providers-models)); providers that type their models (Together) are grouped as Chat, Embedding, Image, …. Picking one fills the Model input, which stays editable for names the provider does not list. Each provider is asked once while the dialog is open; a failed listing shows the provider's message and a **Retry** button.
- **Delete** a preset. Refused for the active preset and for the one Dream uses (`agents.dream.dreamModelPreset`).
- **+ Add model preset** with a name and the same form. Settings left blank get their defaults, so the file shows the whole preset.

API keys are set on the [Providers page](#settings--providers). A row whose provider has no key (stored or in its environment variable) is flagged. Read-only connections (remote, anonymous or non-admin) see the rows without buttons or forms.

Every change is validated like `check-config` before it is written, and the previous file is backed up next to it as `config_<timestamp>.json.bck`.

### Protocol

| Client envelope | Fields | Effect |
|---|---|---|
| `list_model_presets` | – | Reply with `model_presets`. |
| `add_model_preset` | `name`, `values` | Add the preset; refused when the name is `default` or already used. |
| `set_model_preset_config` | `name`, `values` | Change the submitted settings of the preset. |
| `delete_model_preset` | `name` | Delete the preset. |
| `set_default_model_preset` | `name` | Make the preset (`default`: the agent settings) the gateway's default, live. |
| `list_provider_models` | `provider` | Reply with `provider_models`: the models that provider offers. |

`values` maps setting names as the form calls them (`model`, `provider`, `maxTokens`, `contextWindowTokens`, `temperature`, `reasoningEffort`, `label`) to values. The per-chat `set_model_preset` envelope is unrelated: it changes one chat only.

Every successful write, and the list, is answered with one event:

```json
{"event": "model_presets",
 "presets": [{"name": "default", "label": "Default (agent settings)", "model": "…", "provider": "auto",
              "active": false, "built_in": true, "deletable": false, "delete_blocked_reason": "…",
              "missing_api_key": false},
             {"name": "openai-gpt-6.1-terra", "label": null, "model": "gpt-6.1-terra", "provider": "openai",
              "active": true, "built_in": false, "deletable": false, "delete_blocked_reason": "…",
              "missing_api_key": false, "form": {"…": "…"}}],
 "active": "openai-gpt-6.1-terra",
 "can_modify": true,
 "new_preset_form": {"…": "…"},
 "providers_without_key": ["together"],
 "changed": {"action": "activated", "name": "openai-gpt-6.1-terra"}}
```

`changed` (`action`: `added`, `updated`, `deleted` or `activated`) is absent on a plain list; `form` and `new_preset_form` only go to connections that may change the configuration. A refused write is a `setting_error` with `kind` `model_preset_add`, `model_preset_config`, `model_preset_delete` or `model_preset_activate`. When the file was saved but the running agent could not switch to it (for example a `${VAR}` that is not set), the `detail` is `saved_reload_failed`: restart rust-bot to apply the saved change.

#### Listing a provider's models

`list_provider_models` asks the provider's own API which models it offers, with the key, base URL and extra headers configured under `providers.<name>` (falling back to the provider's environment variable, e.g. `OPENAI_API_KEY`; `auto` lists the provider `auto` would pick). It has the same access rules as the writes above, because it uses the configured credentials.

| Provider | Request |
|---|---|
| `openai`, `openrouter`, `together`, `gemini`, `custom` | `GET {apiBase}/models` with `Authorization: Bearer` (none without a key, for local servers such as Ollama or LM Studio) |
| `anthropic` | `GET {apiBase}/v1/models` with `x-api-key` and `anthropic-version`, every page followed |
| `azure_openai` | Not listable: presets name deployments, so type the name. |

A provider with `providerBackEnd` set is listed the way that API lists models: `anthropic` like the `anthropic` row, `openai_compat` and `openai_responses` like the first row.

```json
{"event": "provider_models", "provider": "together",
 "models": [{"id": "meta-llama/Llama-4-70B", "display_name": "Llama 4 70B", "context_length": 131072, "model_type": "chat"}]}
```

`models` is sorted by `id` without duplicates; `display_name`, `context_length` and `model_type` (`chat`, `embedding`, `image`, …) are `null` when the provider does not report them, and Gemini's `models/` prefix is dropped. When refused or failed, the reply is `{"event": "provider_models", "provider": "…", "detail": "…", "error": "…"}` with `detail` `access_denied`, `admin_required`, `child_agent_config`, `missing_provider`, `config_error` or `listing_failed` (unknown provider, unset `${VAR}`, timeout after 15 s, non-2xx answer: `error` includes the status and the provider's message).

## Settings → Providers

The **Providers** page (after Models) lists every LLM provider of `config.json`: the dedicated ones (`providers.openai`, `anthropic`, `openrouter`, `together`; the legacy `providers.custom` slot shows as a custom provider named `custom`) and the named custom ones (`providers.customProviders.<name>`), with **Edit** and **Delete** per row. A signed-in, local administrator can:

- **+ Add provider**, with a two-way toggle:
  - **Dedicated model provider.** Pick OpenAI, Anthropic, OpenRouter or Together AI and enter the API key. A blank key means "use the provider's environment variable" (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, …); an orange warning shows while that variable is not set. A collapsible **Customized settings** section holds the Base URL (the default is the placeholder), the API protocol and extra headers. A provider added with nothing entered gets its default Base URL written, so that it has a row.
  - **Custom model provider.** A name, the Base URL (required), the API protocol (OpenAI Chat Completions, OpenAI Responses or Anthropic Messages), the API key and extra headers. **Create provider** adds it under `providers.customProviders`. Model presets select it by that name in their provider list; its models are listed with the protocol chosen here. Names follow the rules of preset names and may not be `auto`, `default` or the name of a built-in provider.
- **Edit** a provider: the same fields with the name fixed. **Forget stored key** clears the stored key so the environment variable applies again.
- **Delete** a provider. A custom provider is removed; a dedicated slot is reset to an empty key. Refused while `agents.provider` or a model preset names the provider; the message lists who uses it.

**Keys never reach the browser.** The rows and forms say only whether a key is stored; a blank key field keeps the stored one, and the gateway never logs submitted values. Extra headers are sent in clear, like MCP server headers, and only to local administrators. Read-only connections (remote, anonymous or non-admin) see the rows without buttons, forms or header values.

Changes **apply live**. The gateway validates and saves `config.json` (the previous file is backed up as `config_<timestamp>.json.bck`), then rebuilds its providers: the next message of every chat uses the new key, Base URL, headers and protocol. A turn already running finishes with the settings it started with. If the file was saved but the live switch failed (for example a `${VAR}` that is not set), the `setting_error` has `detail` `saved_reload_failed`: restart rust-bot to apply it. Three things are read only at startup and still need a restart: the transcription key (`channels/manager.rs`), the environment forwarded to child agents and the secrets extracted for Docker.

`auto` is unchanged: it picks the first of openai, openrouter, custom, anthropic, together that has a *stored* key, and ignores custom providers and keys that are only in the environment. Name the provider in the preset to use those.

### Protocol

| Client envelope | Fields | Effect |
|---|---|---|
| `list_providers` | – | Reply with `providers`. |
| `add_provider` | `kind` (`dedicated` or `custom`), `name`, `values` | Add the provider. A dedicated `name` is one of the four and not yet configured; a custom one is new, valid and not reserved. |
| `set_provider_config` | `name`, `values` | Change the submitted settings. A blank `apiKey` keeps the stored key; a blank `apiBase`, empty `extraHeaders` or `providerBackEnd: ""` (automatic) removes the setting. |
| `forget_provider_key` | `name` | Clear the stored key. |
| `delete_provider` | `name` | Remove or reset the provider; refused while in use. |

`values` maps `apiKey`, `apiBase`, `extraHeaders` (a name -> value map) and `providerBackEnd` (`openai_compat`, `openai_responses`, `anthropic` or empty) to values. Every success, and the list, is answered with one event:

```json
{"event": "providers",
 "providers": [{"name": "anthropic", "kind": "dedicated", "label": "Anthropic", "api_base": null,
                "default_api_base": "https://api.anthropic.com", "backend": null, "backend_default": "anthropic",
                "api_key_configured": false, "env_var": "ANTHROPIC_API_KEY", "env_var_set": false,
                "used_by": ["agents.provider"], "deletable": false, "delete_blocked_reason": "…",
                "form": {"…": "…"}}],
 "dedicated_choices": [{"name": "openai", "label": "OpenAI", "env_var": "OPENAI_API_KEY", "env_var_set": true,
                        "default_api_base": "https://api.openai.com/v1", "configured": false}],
 "backend_choices": [{"value": "", "label": "Automatic"}],
 "can_modify": true,
 "new_provider_form": {"…": "…"},
 "changed": {"action": "added", "name": "my-relay"}}
```

`changed` (`action`: `added`, `updated`, `deleted` or `key_forgotten`) is absent on a plain list; `form` and `new_provider_form` only go to connections that may change the configuration. A refused write is a `setting_error` with `kind` `provider_add`, `provider_config` or `provider_delete`. After a provider change the page also asks for `list_model_presets`, so the Models page's missing-key flags and provider list follow.

## Settings → Channels

The **Channels** page (after Providers) has one section per channel type; today only **Email** (`channels.email`, IMAP in and SMTP out, see [EMAIL_CHANNEL.md](EMAIL_CHANNEL.md)). A section's header shows the type and a badge (**Enabled**, **Disabled** or **Not set up**); clicking it collapses or expands the section. Every section starts expanded. A signed-in, local administrator can:

- **+ Create email channel** when none is set up. It opens the email form with Cancel and **Create channel**; there is no name to enter, as there is one email channel at most.
- **Edit** the channel: the same form, filled in.
- **Delete** the channel, after an inline question. The whole `channels.email` entry goes, including any per-channel overrides such as `sendProgress`.

The form is grouped under headings: General, Incoming mail (IMAP), Outgoing mail (SMTP), Behaviour, Security and Attachments. A set-up channel shows `sender · IMAP host` and warnings such as "Consent not granted: the channel will not start".

A save is refused when the channel could not start: when the IMAP or SMTP host, username or password is blank, or when it is enabled without `consentGranted`. A `${VAR}` placeholder must name a variable that is set for the gateway process. Keys the form does not know are kept.

**Passwords never reach the browser.** They are secret fields: the form only says that one is stored, a blank password keeps it, and the gateway never logs submitted values. Read-only connections (remote, anonymous or non-admin) see the sections without forms or buttons.

Changes **apply after a restart**: channels are built once when the gateway starts. A save writes `config.json` (the previous file is backed up as `config_<timestamp>.json.bck`) and shows the amber "Restart now" banner.

### Protocol

| Client envelope | Fields | Effect |
|---|---|---|
| `list_channels` | – | Reply with `channels`. |
| `add_channel` | `channelType`, `values` | Set up a channel of that type; refused when one exists. |
| `set_channel_config` | `channelType`, `values` | Change the submitted settings. A blank password keeps the stored one. |
| `delete_channel` | `channelType` | Remove the channel with all its settings. |

`channelType` is `email`. `values` maps the form's setting names (`imapHost`, `pollIntervalSeconds`, `allowFrom`, …) to values. Every success, and the list, is answered with one event:

```json
{"event": "channels",
 "channels": [{"channel_type": "email", "title": "Email", "configured": true, "enabled": true,
               "summary": "bot@example.com · imap.example.com", "notes": [],
               "form": {"…": "…"}}],
 "can_modify": true,
 "changed": {"action": "updated", "channelType": "email"},
 "restart_required": true}
```

A type that is not set up has `configured: false` and a blank `new_form` instead of `form`. `form` and `new_form` only go to connections that may change the configuration. `changed` (`action`: `added`, `updated` or `deleted`) and `restart_required` (whether the file changed) are absent on a plain list. A refused write is a `setting_error` with `kind` `channel_add`, `channel_config` or `channel_delete`, `name` the channel type and `detail` such as `invalid_value`, `channel_exists`, `channel_not_configured` or `unknown_channel_type`.
