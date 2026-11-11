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

API keys are not edited here; they stay under `providers.<name>` in the configuration file. A row whose provider has no key is flagged. Read-only connections (remote, anonymous or non-admin) see the rows without buttons or forms.

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

```json
{"event": "provider_models", "provider": "together",
 "models": [{"id": "meta-llama/Llama-4-70B", "display_name": "Llama 4 70B", "context_length": 131072, "model_type": "chat"}]}
```

`models` is sorted by `id` without duplicates; `display_name`, `context_length` and `model_type` (`chat`, `embedding`, `image`, …) are `null` when the provider does not report them, and Gemini's `models/` prefix is dropped. When refused or failed, the reply is `{"event": "provider_models", "provider": "…", "detail": "…", "error": "…"}` with `detail` `access_denied`, `admin_required`, `child_agent_config`, `missing_provider`, `config_error` or `listing_failed` (unknown provider, unset `${VAR}`, timeout after 15 s, non-2xx answer: `error` includes the status and the provider's message).
