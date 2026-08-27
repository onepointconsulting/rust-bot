# Rust Bot - Installation

This package contains a pre-built `rust-bot` binary, a `gmail-auth` helper
for Gmail OAuth, the `web` (REST API) and `websockets` (gateway) UI assets,
and two sample configurations. The recommended first run is `rust-bot onboard`.

## 1. Unpack

Extract the archive:

```text
rust-bot-<version>-<platform>/
  rust-bot[.exe]
  gmail-auth[.exe]
  INSTALL.md
  web/                 REST API chat UI (rust-bot api)
    index.html
    *.js
    *.wasm
    *.css
  websockets/          Gateway chat UI (rust-bot gateway)
    index.html
    *.js
    *.wasm
    *.css
  configuration/samples/
    openai-compat.json
    anthropic.json
```

The binary has its prompt templates and workspace seed files (`AGENTS.md`,
`SOUL.md`, `TOOLS.md`, `USER.md`, …) compiled in, so it works standalone —
you can move `rust-bot` (or `rust-bot.exe`) anywhere, including away from
this folder.

## 2. Onboard (recommended)

From inside the unpacked folder:

```bash
# Windows (PowerShell / cmd)
.\rust-bot.exe onboard

# Linux / macOS
./rust-bot onboard
```

This writes a config at `./.rust-bot/config.json` by default, creates
`./.rust-bot/workspace`, and (if missing) a `.env` file next to the binary
with `RUST_LOG` settings.

On a first run it asks for:

1. **Provider** — `openrouter`, `anthropic`, `edenai`, or `requesty`
2. **API key** and **endpoint** (the endpoint is pre-filled for that provider)
3. Optional extra HTTP headers
4. **Model** (for example `anthropic/claude-opus-5`)
5. Whether to **configure the gateway web UI** (default: yes)

If you enable the gateway web UI it then asks for streaming, WebSocket host,
WebSocket channel port (default `8765`), gateway listen port (default `18790`),
and JWT authentication (default: yes). JWT writes an Ed25519 keypair under
`.rust-bot/credentials/` and registers the first login user in
`.rust-bot/users.json` (password stored as an Argon2id hash, never in
plaintext).

If you enable JWT, onboard also asks **Require login for the web UI?**
(default: yes). Answering **No** sets `channels.websocket.requireAuth` to
`false`: the gateway still supports login for anyone who wants it (tokens
minted via `generate-jwt-token` still work), but the `websockets-chat` UI
skips the login form and connects as a guest when no token is present. Guest
chats are scoped to the browser's local `client_id` — see the note in
"Gateway (`websockets/`)" below.

Useful flags:

| Flag | Default | Description |
|------|---------|-------------|
| `-c`, `--config` | `./.rust-bot/config.json` | Config file to create or update |
| `-w`, `--workspace` | `./.rust-bot/workspace` | Agent workspace directory |
| `--wizard` | off | Full interactive wizard instead of the short onboard flow |

If the config file already exists, onboard asks whether to overwrite it with
defaults (`y`) or refresh it while keeping existing values (`N`).

The API key is stored in the config file. Keep `config.json`, the
credentials directory, and `users.json` private; do not commit them.

### Example: OpenRouter + gateway web UI (Windows)

This is a typical first install. Prompts are shown as `>`, with example
answers after each one. Use your own API key, email, and password.

```text
C:\temp\rust-bot-temp> rust-bot.exe onboard
Using config: C:\temp\rust-bot-temp\.\.rust-bot\config.json
> Select a provider to configure API key and endpoint  openrouter
> Enter API key  sk-or-v1-<your-openrouter-key>
> Enter endpoint  https://openrouter.ai/api/v1
> Add an extra HTTP header?  No
> Model  anthropic/claude-opus-5
> Configure the gateway web UI?  Yes
> Enable streaming?  Yes
> Enter WebSocket host  127.0.0.1
> Enter WebSocket port  8765          (WebSocket channel port)
> Enter WebSocket port  18790         (gateway listen port)
> Enable JWT authentication?  Yes
> Require login for the web UI?  Yes
Wrote private key: C:\temp\rust-bot-temp\.rust-bot\credentials\private_key.pem
Wrote public key:  C:\temp\rust-bot-temp\.rust-bot\credentials\public_key.pem
> Enter user email for login  you@example.com
> Enter user password for login  ********
Wrote users file: C:\temp\rust-bot-temp\.rust-bot\users.json
```

Onboard then creates the workspace, writes `.env` if needed, and prints
next-step commands. After this session the important paths are:

| Path | Purpose |
|------|---------|
| `.rust-bot/config.json` | Main config (provider, model, gateway, JWT paths) |
| `.rust-bot/workspace/` | Agent files and memory |
| `.rust-bot/credentials/` | JWT private/public keys |
| `.rust-bot/users.json` | Login email + password hash |
| `.env` | Log level (`RUST_LOG=info`) and log file path |

### Run after onboard

From the same folder:

```bash
# Windows — gateway + websockets chat UI
.\rust-bot.exe gateway -c .\.rust-bot\config.json --web-root .\websockets

# Linux / macOS
./rust-bot gateway -c ./.rust-bot/config.json --web-root ./websockets
```

Open `http://127.0.0.1:18790/` and sign in with the email and password you
entered during onboard.

Other modes:

```bash
# One-shot chat
.\rust-bot.exe agent -c .\.rust-bot\config.json -m "Hello!"

# Interactive console
.\rust-bot.exe agent -c .\.rust-bot\config.json

# REST API + web-chat UI
.\rust-bot.exe api -c .\.rust-bot\config.json --web-root .\web
```

On Linux / macOS, use `./rust-bot` instead of `.\rust-bot.exe` and forward
slashes in paths.

## 3. Optional: sample configurations

If you prefer not to use onboard, two ready-to-use configs are in
`configuration/samples/`:

- `openai-compat.json` — OpenAI-compatible provider (for example OpenRouter)
- `anthropic.json` — Anthropic API directly

Both reference environment variables for secrets, so no keys are stored in
the files themselves.

For `openai-compat.json`:

| Variable | Description |
|----------|-------------|
| `OPENAI_API_KEY` | API key for your OpenAI-compatible provider |
| `OPENAI_API_BASE` | Base URL, e.g. `https://openrouter.ai/api/v1` |
| `OPENAI_API_MODEL` | Model name, e.g. `google/gemini-3-flash-preview` |

For `anthropic.json`:

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | Your Anthropic API key |
| `ANTHROPIC_API_BASE` | Base URL, e.g. `https://api.anthropic.com` |
| `ANTHROPIC_API_MODEL` | Model name, e.g. `claude-sonnet-5` |

Both configs also reference `BRAVE_API_KEY` for web search; leave it unset
(or set `tools.web.enable` to `false` in the config) if you don't need it.

You can either export these variables in your shell, or place them in a
`.env` file next to the binary (it is loaded automatically on startup).

Then copy a sample to a working location and pass it with `-c` / `--config`.
To add JWT keys and a login user to a sample config, use the CLI commands
in the next section (onboard already does this when you enable JWT).

## 4. Extra JWT users and tokens

Onboard with JWT enabled already generates a keypair and the first web UI
user. Use these commands to mint extra users, or to add JWT to a config
that was created without onboard.

1. In your config, set `api.jwt.enabled` to `true` and a non-empty
   `api.jwt.aud` (audience). Optionally set `api.jwt.iss` (default:
   `rust-bot`).
2. Generate an Ed25519 keypair and write the key paths into the config
   (skip this if onboard already wrote `.rust-bot/credentials/`):

```bash
# Windows
.\rust-bot.exe generate-jwt-keypair --config .\.rust-bot\config.json

# Linux / macOS
./rust-bot generate-jwt-keypair --config ./.rust-bot/config.json
```

Keys are written to `./.rust-bot/credentials/` by default
(`private_key.pem` and `public_key.pem`). Pass `--credentials-dir` to choose
another directory, or `--force` to overwrite existing keys.

3. Mint a bearer token and register a user:

```bash
# Windows
.\rust-bot.exe generate-jwt-token --config .\.rust-bot\config.json --user-email user@example.com --users-file .\.rust-bot\users.json --purpose webui --password "correct horse battery staple"

# Linux / macOS
./rust-bot generate-jwt-token --config ./.rust-bot/config.json --user-email user@example.com --users-file ./.rust-bot/users.json --purpose webui --password "correct horse battery staple"
```

The JWT is printed to stdout. `--user-email`, `--users-file`, and
`--password` are required: the email identifies the user (it is not
embedded in the token itself), `--users-file` points to a JSON file mapping
emails to their minted tokens, and `--password` is hashed with Argon2id
before being written to that file as `password_hash`. The file is created
if it does not exist. Registration fails if the email is already present in
the file. Optional flags: `--iss`, `--aud`, `--purpose` (`webui` for the
gateway chat UI), `--expires-in-months` (default: 6). Send the token as
`Authorization: Bearer <token>` when calling the REST API.

The password is never stored or printed in plaintext.

Keep private keys, minted tokens, and the users file secret; do not commit
them.

## 5. Helper tools

The package also includes an optional Gmail OAuth utility.

### Gmail OAuth (`gmail-auth`)

Use this once per machine to authorize Gmail read/send access for the agent.

1. Create a Google Cloud OAuth **Desktop app** client, enable the Gmail API,
   and allow the redirect URI `http://localhost:8080`.
2. Save the downloaded client secret as `credentials/client_secret.json`
   relative to your current working directory (create the `credentials/`
   folder if needed).
3. Run the helper:

```bash
# Windows
.\gmail-auth.exe

# Linux / macOS
./gmail-auth
```

4. Complete the Google login in the browser. Tokens are written to
   `token_cache.json` in the current directory.
5. Copy both credential files into the paths configured under
   `tools.gmail` (sample defaults shown below):

```bash
# Linux / macOS
mkdir -p ~/.rust-bot/workspace/credentials
cp credentials/client_secret.json ~/.rust-bot/workspace/credentials/
cp token_cache.json ~/.rust-bot/workspace/credentials/

# Windows (PowerShell)
New-Item -ItemType Directory -Force -Path "$HOME\.rust-bot\workspace\credentials"
Copy-Item .\credentials\client_secret.json "$HOME\.rust-bot\workspace\credentials\"
Copy-Item .\token_cache.json "$HOME\.rust-bot\workspace\credentials\"
```

Then set `tools.gmail.enable` to `true` in your config. Re-run `gmail-auth`
if tokens are revoked or scopes change.

## 6. Web chat UIs

The package includes two pre-built UIs:

- `websockets/` — login + streaming chat for `rust-bot gateway` (this is
  what onboard configures when you answer **Configure the gateway web UI?**)
- `web/` — login + chat for the REST `rust-bot api` server

### Gateway (`websockets/`)

```bash
# Windows
.\rust-bot.exe gateway --config .\.rust-bot\config.json --web-root .\websockets

# Linux / macOS
./rust-bot gateway --config ./.rust-bot/config.json --web-root ./websockets
```

Then open `http://<host>:<port>/` in a browser. With the example onboard
session above that is `http://127.0.0.1:18790/`. You can also set
`gateway.webRoot` in the config instead of passing `--web-root` every time.

Sign in with the email and password created during onboard (or with a user
added later via `rust-bot generate-jwt-token --purpose webui`).

If `channels.websocket.requireAuth` is `false` (answered **No** to "Require
login for the web UI?" during onboard, or set by hand — see
`websockets-chat/README.md`'s "Optional login" section), the UI skips the
login form entirely and connects as a guest. A guest's chat list is scoped to
a `client_id` stored in the browser's `LocalStorage`: it survives reloads and
new tabs in the same browser, but a different browser, incognito window, or
cleared site data starts a fresh, empty chat list (previous chats stay on
disk but are hidden — guest scoping fails closed, not open).

### REST API (`web/`)

```bash
# Windows
.\rust-bot.exe api --config .\.rust-bot\config.json --web-root .\web

# Linux / macOS
./rust-bot api --config ./.rust-bot/config.json --web-root ./web
```

Then open `http://<host>:<port>/` (the API default is
`http://127.0.0.1:8900/`). You can also set `api.webRoot` in the config
file instead of passing `--web-root` every time; if `./web` exists next to
the binary and neither is set, it is used automatically.

The REST UI needs `api.jwt.enabled: true` and a registered user.

## 7. Next steps

- Edit `.rust-bot/config.json` (workspace path, ports, tools) once you are
  up and running.
- Add more login users with `rust-bot generate-jwt-token` (see above).
- For a full walkthrough of every config section, run
  `rust-bot onboard --wizard`.
- See the main [README](https://github.com/onepointconsulting/rust-bot#readme)
  for full CLI documentation, the interactive console, Gmail setup, and the
  configuration reference.
