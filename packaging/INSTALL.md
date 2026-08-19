# Rust Bot - Installation

This package contains a pre-built `rust-bot` binary, a `gmail-auth` helper
for Gmail OAuth, the `web-chat` web UI assets, and two sample
configurations. Follow these steps to get started.

## 1. Unpack

Extract the archive:

```text
rust-bot-<version>-<platform>/
  rust-bot[.exe]
  gmail-auth[.exe]
  INSTALL.md
  templates/
  web/
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
this folder. The bundled `templates/` folder is only needed if you want to
customize those defaults: keep it next to the binary, or set
`RUST_BOT_TEMPLATES_DIR` to point at it from elsewhere, and your copies will
be used instead of the built-in ones.

## 2. Choose a sample configuration

Two ready-to-use configs are provided in `configuration/samples/`:

- `openai-compat.json` - uses an OpenAI-compatible provider (e.g. OpenRouter)
- `anthropic.json` - uses the Anthropic API directly

Both reference environment variables for secrets, so no keys are stored in
the files themselves.

## 3. Set the required environment variables

For `openai-compat.json`:

| Variable | Description |
|----------|--------------|
| `OPENAI_API_KEY` | API key for your OpenAI-compatible provider |
| `OPENAI_API_BASE` | Base URL, e.g. `https://openrouter.ai/api/v1` |
| `OPENAI_API_MODEL` | Model name, e.g. `google/gemini-3-flash-preview` |

For `anthropic.json`:

| Variable | Description |
|----------|--------------|
| `ANTHROPIC_API_KEY` | Your Anthropic API key |
| `ANTHROPIC_API_BASE` | Base URL, e.g. `https://api.anthropic.com` |
| `ANTHROPIC_API_MODEL` | Model name, e.g. `claude-sonnet-5` |

Both configs also reference `BRAVE_API_KEY` for web search; leave it unset
(or set `tools.web.enable` to `false` in the config) if you don't need it.

You can either export these variables in your shell, or place them in a
`.env` file next to the binary (it is loaded automatically on startup).

## 4. Onboard and run

From inside the unpacked folder:

```bash
# Windows (PowerShell / cmd)
.\rust-bot.exe onboard

# Linux / macOS
./rust-bot onboard
```

This will help you through a minimal setup.

Swap in `configuration/samples/anthropic.json` to use the Anthropic sample
instead.

Omit `-m/--message` to start the interactive console.

If you want to create a new configuration though, you can run this command, that will create a new default configuration that you can edit yourself:

```bash
rust-bot.exe onboard --config ./rust-bot/config.json
```

## 5. API JWT keys and tokens

Use the `rust-bot` CLI when you enable the REST API (`rust-bot api`) with JWT
auth.

1. In your config, set `api.jwt.enabled` to `true` and a non-empty
   `api.jwt.aud` (audience). Optionally set `api.jwt.iss` (default:
   `rust-bot`).
2. Generate an Ed25519 keypair and write the key paths into the config:

```bash
# Windows
.\rust-bot.exe generate-jwt-keypair --config .\path\to\config.json

# Linux / macOS
./rust-bot generate-jwt-keypair --config ./path/to/config.json
```

Keys are written to `./.rust-bot/credentials/` by default
(`private_key.pem` and `public_key.pem`). Pass `--credentials-dir` to choose
another directory, or `--force` to overwrite existing keys.

3. Mint a bearer token for API clients:

```bash
# Windows
.\rust-bot.exe generate-jwt-token --config .\path\to\config.json --user-email user@example.com --users-file .\path\to\users.json

# Linux / macOS
./rust-bot generate-jwt-token --config ./path/to/config.json --user-email user@example.com --users-file ./path/to/users.json
```

The JWT is printed to stdout. `--user-email` and `--users-file` are
required: the email identifies the user (it is not embedded in the token
itself), and `--users-file` points to a JSON file mapping emails to their
minted tokens. The file is created if it does not exist. Registration fails
if the email is already present in the file. Optional flags: `--iss`,
`--aud`, `--expires-in-months` (default: 6), and `--password`. Send the
token as `Authorization: Bearer <token>` when calling the API.

Pass `--password` to also store a credential for the user:

```bash
# Windows
.\rust-bot.exe generate-jwt-token --config .\path\to\config.json --user-email user@example.com --users-file .\path\to\users.json --password "correct horse battery staple"

# Linux / macOS
./rust-bot generate-jwt-token --config ./path/to/config.json --user-email user@example.com --users-file ./path/to/users.json --password "correct horse battery staple"
```

The password is never stored or printed in plaintext; it is hashed with
Argon2id before being written to the users file as `password_hash`. Users
registered without `--password` simply omit that field.

Keep private keys, minted tokens, and the users file secret; do not commit
them.

## 6. Helper tools

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

## 7. Web chat UI

The package includes a pre-built `web-chat` UI (`web/index.html`, `*.js`,
`*.wasm`) — a small login + chat interface for the REST API. It's served
automatically by `rust-bot api` when a web root is configured.

```bash
# Windows
.\rust-bot.exe api --config .\path\to\config.json --web-root .\web

# Linux / macOS
./rust-bot api --config ./path/to/config.json --web-root ./web
```

Then open `http://<host>:<port>/` in a browser (the default is
`http://127.0.0.1:8900/`). You can also set `api.webRoot` in the config
file instead of passing `--web-root` every time; if `./web` exists next to
the binary and neither is set, it is used automatically.

The UI needs `api.jwt.enabled: true` to log in (see "API JWT keys and
tokens" above) and a user registered via `rust-bot generate-jwt-token
--password ...` so it has an email/password to sign in with.

## 8. Next steps

- Copy a sample config to a location of your choice and adjust it (workspace
  path, ports, tool settings) once you are up and running.
- See the main [README](https://github.com/onepointconsulting/rust-bot#readme)
  for full CLI documentation, the interactive console, Gmail setup, and the
  configuration reference.
