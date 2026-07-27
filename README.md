# Rust Bot

A simple bot implementation based on [Nanobot](https://github.com/HKUDS/nanobot), written in Rust. It ships as a CLI binary (`rust-bot`) that can run the agent in one-shot mode or as an interactive console, plus an optional `gmail-auth` helper for Gmail OAuth setup.

## Table of contents

- [Pre-requisites](#pre-requisites)
- [Testing](#testing)
- [Build](#build)
- [Quick start](#quick-start)
- [Command line](#command-line)
  - [`agent` subcommand](#agent-subcommand)
  - [Exit codes](#exit-codes)
  - [Examples](#examples)
- [Interactive console](#interactive-console)
  - [Starting the console](#starting-the-console)
  - [Key bindings](#key-bindings)
  - [History](#history)
  - [Image paste](#image-paste)
  - [Multi-line input](#multi-line-input)
  - [Leaving the console](#leaving-the-console)
- [Gmail support](#gmail-support)
  - [Google Cloud setup](#google-cloud-setup)
  - [OAuth helper (`gmail-auth`)](#oauth-helper-gmail-auth)
  - [Enabling the Gmail tools](#enabling-the-gmail-tools)
  - [Gmail agent tools](#gmail-agent-tools)
- [Configuration](#configuration)
- [Project layout](#project-layout)

---

## Pre-requisites

- Install [Rust](https://www.rust-lang.org/tools/install) (stable toolchain).
- A working terminal. On Windows use Windows Terminal, PowerShell 7, or `cmd`; on macOS / Linux any modern terminal emulator works.

## Testing

Some tests require a `.env` file with the parameters listed in `.env_local`.

```
cargo test
```

Integration tests:

```
cargo test --tests
```

## Build

```
cargo build -r
```

This produces `target/release/rust-bot` (or `.\target\release\rust-bot.exe` on Windows).

A separate helper binary for Gmail OAuth is built alongside the main CLI:

```
cargo build -r --bin gmail-auth
```

This produces `target/release/gmail-auth` (or `.\target\release\gmail-auth.exe` on Windows).

## Quick start

```bash
# Run a single prompt
cargo run -- agent -m "What files are in the workspace?" \
    --config ./configs/openai-compat/config.json

# Or, after a release build:
./target/release/rust-bot agent -m "Hello!"
```

The agent will use the workspace directory (`~/.rust-bot/workspace` by default) for reading and writing files.

---

## Command line

Run the agent from the terminal:

```
cargo run -- agent [OPTIONS]
```

After a release build (`cargo build -r`), use:

```
rust-bot agent [OPTIONS]
```

### `agent` subcommand

Run the agent from the command line. For the full, up-to-date option list:

```
cargo run -- agent --help
```

| Flag | Default | Description |
|------|---------|-------------|
| `-m`, `--message` | _(none)_ | Message to send to the agent. Omit to enter the [interactive console](#interactive-console). |
| `-s`, `--session` | `cli:direct` | Session ID |
| `-w`, `--workspace` | `~/.rust-bot/workspace` | Workspace directory |
| `-c`, `--config` | `~/.rust-bot/config.json` | Config file path |
| `--markdown` / `--no-markdown` | `true` | Render assistant output as Markdown |
| `--logs` / `--no-logs` | `false` | Show runtime logs during chat |

### Exit codes

| Code | Constant | Meaning |
|------|----------|---------|
| `0` | `SUCCESS` | Success (also used after spawning a restarted process on Windows) |
| `1` | `GENERAL_ERROR` | Config or general CLI error |
| `3` | `INVALID_PROVIDER` | Invalid provider (unknown value in `agents.provider`) |
| `4` | `GMAIL_CONFIG_ERROR` | Gmail tool credentials missing (OAuth client secret or token cache) |
| `5` | `CHANNEL_ALLOW_FROM_EMPTY` | Channel has an empty `allowFrom` list (set `["*"]` or specific user IDs) |

Constants live in `src/utils/exit_codes.rs`.

Workspace seed files (`AGENTS.md`, `SOUL.md`, `TOOLS.md`, `USER.md`, …) are compiled into the
binary, so onboarding always works even without a sibling `templates/` folder. Drop a
`templates/` directory next to the binary (or set `RUST_BOT_TEMPLATES_DIR`) to override the
bundled defaults with your own.

### Examples

```bash
# Single message
cargo run -- agent -m "What files are in the workspace?" \
    --config ./configs/openai-compat/config.json

# Custom session and workspace
cargo run -- agent -m "hello" -s myproject:cli -w ~/.rust-bot/workspace

# Plain-text output, with runtime logs
cargo run -- agent -m "status" --no-markdown --logs
```

```ps1
cargo run -- agent -m "How is the weather in London?" --config ./configs/openai-compat/config.json --logs
cargo run -- agent -m "How is the weather in London?" --config ./configs/openai-compat/config.json --no-logs
cargo run -- agent -m "Can you please give me a quick summary of the services offered by Onepoint Consulting Ltd from London? Then please write this summary to a file called onepoint.html in the workspace folder." --config ./configs/openai-compat/config.json --logs
cargo run -- agent -m "Which are the main competitors of Onepoint Consulting Ltd? Can you create an html page with the information on competitors with the onepoint_competitors?" --config ./configs/openai-compat/config.json --logs
cargo run -- agent -m "Can you produce a commit message for the staged files in the current git project (current folder)?" --config ./configs/openai-compat/config_current_folder.json --logs
cargo run -- agent -m "Can you add all files that are not staged to the staging area in the current folder? Use git ..." --config ./configs/openai-compat/config_current_folder.json --logs
cargo run -- agent -m "Can you write a nice commit message for the staged files? Use git ..." --config ./configs/openai-compat/config_current_folder.json --logs

# Interactive mode (see the Interactive console section below)
cargo run -- agent --config ./configs/openai-compat/config_current_folder.json --logs
```

```bash
cargo build -r
./target/release/rust-bot agent -m "What files are in the workspace?"
```

```ps1
cargo build -r
.\target\release\rust-bot agent -m "What files are in the workspace?"
.\target\release\rust-bot agent --config ./configs/openai-compat/config_current_folder.json --no-logs
```

---

## Interactive console

Rust Bot ships with a small REPL-style interactive console built on [`reedline`](https://github.com/nushell/reedline). It uses Emacs-style keybindings by default, supports history, and lets you paste images, paste clipboard text, and write multi-line prompts without leaving the terminal.

### Starting the console

Launch `rust-bot agent` **without** the `-m` / `--message` flag and the binary will drop you into the console:

```bash
cargo run -- agent --config ./configs/openai-compat/config_current_folder.json --logs
```

A short banner is printed, showing the logo and the available shortcuts. Then the prompt appears and the agent is ready for input. Each line you submit is sent to the agent and the response is rendered in the same terminal — Markdown by default, plain text if you passed `--no-markdown`.

> Note: the console requires a real TTY. If stdin is redirected (for example, in a non-interactive pipe), the binary returns a non-zero exit instead of dropping into the REPL.

### Key bindings

The line editor uses **Emacs** keybindings. The full default set is available; the table below highlights the bindings that are most useful day-to-day. Custom bindings added by Rust Bot are marked.

#### Movement

| Key | Action |
|-----|--------|
| `Ctrl+F` / `→` | Move cursor one character forward |
| `Ctrl+B` / `←` | Move cursor one character backward |
| `Alt+F` | Move cursor one word forward |
| `Alt+B` | Move cursor one word backward |
| `Ctrl+A` | Move to start of line |
| `Ctrl+E` | Move to end of line |
| `Ctrl+N` / `↓` | Next history entry |
| `Ctrl+P` / `↑` | Previous history entry |
| `Ctrl+→` | Move forward by word |
| `Ctrl+←` | Move backward by word |

#### Editing

| Key | Action |
|-----|--------|
| `Ctrl+D` | Delete character under cursor; exits the console if the line is empty |
| `Ctrl+H` / `Backspace` | Delete character before cursor |
| `Alt+D` | Delete word forward |
| `Alt+Backspace` | Delete word backward |
| `Ctrl+K` | Kill to end of line |
| `Ctrl+U` | Kill to start of line |
| `Ctrl+W` | Kill previous word |
| `Ctrl+Y` | Yank (paste) last killed text |
| `Ctrl+T` | Transpose characters |
| `Alt+T` | Transpose words |
| `Ctrl+I` / `Tab` | _(custom)_ Paste image from clipboard — see [Image paste](#image-paste) |
| `Alt+V` | _(custom)_ Paste clipboard text into the current prompt — see [Text paste](#text-paste) |
| `Ctrl+O` | _(custom)_ Insert a newline, do not submit — see [Multi-line input](#multi-line-input) |
| `Alt+Enter` / `Shift+Enter` | Insert a newline, do not submit (only on terminals that report the modifier — see [Multi-line input](#multi-line-input)) |
| `Ctrl+C` | Cancel the current line and re-show the prompt (does not exit) |
| `Ctrl+L` | Clear the screen |

#### History search

| Key | Action |
|-----|--------|
| `Ctrl+R` | Start incremental reverse search; type to narrow, `Enter` to accept, `Ctrl+C` / `Ctrl+G` to cancel |
| `Ctrl+S` | Forward search (continues a `Ctrl+R` session) |

#### Submission

| Key | Action |
|-----|--------|
| `Enter` | Submit the current line as a message to the agent |

### History

The console keeps the **last 100 lines** in a file-backed history, loaded automatically on the next start. The file lives at `~/.rust-bot/cli_history` (see `get_cli_history_path` in `src/config/paths.rs`) and is created on first use. Use `↑` / `↓` to walk it, or `Ctrl+R` to fuzzy-search.

If the history file is unreadable (permissions, first run, etc.) the console starts with an empty history and logs a warning.

### Image paste

Pressing `Ctrl+I` (or `Tab`) reads the current clipboard image and inserts a sentinel token into the buffer. On submit, the sentinel is replaced by the actual image and sent to the agent alongside the text.

- The image is stored in a temporary file in the workspace; if the message is sent successfully the file is cleaned up.
- `Ctrl+I` is bound to image paste because `Tab` would otherwise complete completions; if you don't have an image on the clipboard the binding is a safe no-op (the sentinel stays in the buffer and is stripped on submit).
- The console uses **bracketed paste mode**, so multi-line text pasted from the terminal is treated as one block rather than being submitted early.

### Text paste

Pressing `Alt+V` captures the current clipboard text and inserts it into the prompt at the cursor position. The text is sent as part of the same message when you press `Enter`.

This is useful when pasting larger snippets, code, logs, or text that may contain newlines. Internally, each paste is recorded with an index and a line-count hint (e.g. `[PASTED_TEXT-#0 12 lines]`). On submit, every sentinel is replaced by its corresponding captured text using that index, so the substitution is always correct regardless of cursor position or paste order.

- **Single-line text** is inserted directly into the buffer as plain text — no sentinel is used.
- **Multi-line text** is stored separately and represented by an indexed sentinel in the buffer. The sentinel shows the paste index and line count so you can see what is queued.
- Multiple `Alt+V` presses in one prompt are fully supported; each paste gets its own index and is substituted independently on submit.
- If clipboard text cannot be read, the paste is treated as empty and the placeholder is stripped on submit.

### Multi-line input

By default, `Enter` submits the current line. To continue a thought on a new line without sending the message, press `Ctrl+O` — a newline is inserted and you can keep typing. The whole block is sent to the agent as a single message when you finally press `Enter`.

This is useful for pasting code blocks, listing steps, or writing prompts that span several lines.

> **Why not `Ctrl+Enter`?** The console enables `ENABLE_VIRTUAL_TERMINAL_INPUT` (required for bracketed paste). In that mode Windows Terminal reports `Ctrl+Enter`, `Shift+Enter`, and `Alt+Enter` all as a plain `Enter`, so the modifier is lost before it reaches the line editor. A `Ctrl`+letter combo (`Ctrl+O`) arrives as a distinct control byte and works reliably on every platform. `Alt+Enter` / `Shift+Enter` still work on terminals that disambiguate them (e.g. kitty-protocol-capable emulators on Unix).

### Leaving the console

Any of the following will exit the console:

- Type `exit` or `quit` and press `Enter`.
- Press `Ctrl+D` on an empty line.
- Send an interrupt (the binary is still long-running after the console returns, so this only closes the prompt, not the process).

The console always prints the banner on entry — that's the easiest way to confirm you've launched interactive mode rather than one-shot mode.

---

## Gmail support

Rust Bot can expose two Gmail agent tools when enabled in config:

- **`gmail`** — reads messages from the user's inbox (read-only)
- **`gmail_email_send`** — sends an email to a recipient (plain text or HTML)

Access is granted through Google OAuth; credentials are stored on disk and reused by the agent. Both tools share the same `client_secret.json` and `token_cache.json` paths from config.

### Google Cloud setup

1. Open the [Google Cloud Console](https://console.cloud.google.com/) and create or select a project.
2. Enable the **Gmail API** for that project.
3. Configure the **OAuth consent screen** (External or Internal, depending on your use case). If you plan to use send, ensure the consent screen allows the **Send email on your behalf** scope (`gmail.send`).
4. Create an OAuth **Desktop app** client and download the client secret JSON.
5. Ensure the client allows the redirect URI `http://localhost:8080` (required for the installed-app flow used by `gmail-auth`).

Save the downloaded file as `client_secret.json` before running the OAuth helper (see below). The `gmail-auth` helper looks for `./credentials/client_secret.json` by default; the agent expects credential files under `~/.rust-bot/credentials/` unless you override the paths in config (see [Enabling the Gmail tools](#enabling-the-gmail-tools)).

> Credential files contain secrets and are gitignored. Do not commit `client_secret.json` or `token_cache.json`.

### OAuth helper (`gmail-auth`)

The `gmail-auth` binary is a standalone utility (not part of the main agent loop) that walks through Google login, requests Gmail **read** and **send** access, and writes a `token_cache.json` file containing the refresh and access tokens. Run it once per machine (or again if tokens are revoked or scopes change).

**Prerequisites:** place `client_secret.json` in `./credentials/` (or update the path in `src/bin/gmail-auth.rs`).

```bash
# Run the OAuth flow (opens a browser, listens on localhost:8080)
cargo run --bin gmail-auth

# Or build a release binary
cargo build --release --bin gmail-auth
./target/release/gmail-auth
```

On Windows (PowerShell):

```ps1
cargo run --bin gmail-auth
cargo build --release --bin gmail-auth
.\target\release\gmail-auth.exe
```

What happens:

1. A local HTTP server on port **8080** receives the OAuth callback (no manual code copy-paste).
2. You sign in with Google and grant Gmail read and send permission.
3. Tokens are persisted to **`token_cache.json`** in the project root.
4. The helper fetches a few inbox subjects to confirm read access works.

If you previously authenticated with read-only scope, delete the old `token_cache.json` and run `gmail-auth` again so the cache includes `gmail.send`.

Copy the generated files to the credential directory the agent uses (defaults shown):

```bash
mkdir -p ~/.rust-bot/credentials
cp client_secret.json ~/.rust-bot/credentials/
cp token_cache.json ~/.rust-bot/credentials/
```

If your config points elsewhere (for example `configs/openai-compat/config_gmail.json` uses `~/.rust-bot/workspace/credentials/`), copy the files to those paths instead.

### Enabling the Gmail tools

Enable both tools in your agent config under `tools.gmail`:

```json
"gmail": {
  "enable": true,
  "client_secret_path": "~/.rust-bot/credentials/client_secret.json",
  "token_cache_path": "~/.rust-bot/credentials/token_cache.json",
  "max_results": 20
}
```

There is no separate flag for send — when `enable` is `true`, the agent registers **`gmail`** and **`gmail_email_send`**.

A sample config with Gmail enabled is in `configs/openai-compat/config_gmail.json`. Run the agent with that config once credentials are in place:

```bash
# Read inbox
cargo run -- agent -m "Summarize my latest inbox emails" \
  --config ./configs/openai-compat/config_gmail.json

# Send plain-text email (the model chooses the gmail_email_send tool)
cargo run -- agent -m "Send an email to alice@example.com with subject Hello and body Hi Alice" \
  --config ./configs/openai-compat/config_gmail.json

# Send HTML email (ask the model to use format html and HTML in the body)
cargo run -- agent -m "Send an HTML email to alice@example.com with subject Report and body containing a bold greeting" \
  --config ./configs/openai-compat/config_gmail.json
```

The agent uses the cached tokens from `token_cache.json` and refreshes them automatically via `yup-oauth2` when they expire.

### Gmail agent tools

| Tool name | Purpose | Key parameters |
|-----------|---------|----------------|
| `gmail` | List and read inbox messages | `limit`, `after`, `before`, `only_subject`, `body_limit` |
| `gmail_email_send` | Send an email | `to`, `subject`, `body` (required); `format` (optional) |

#### `gmail_email_send` parameters

| Parameter | Required | Default | Description |
|-----------|----------|---------|-------------|
| `to` | yes | — | Recipient email address |
| `subject` | yes | — | Email subject (non-ASCII characters are RFC 2047–encoded) |
| `body` | yes | — | Message body: plain text or HTML, depending on `format` |
| `format` | no | `plain` | `plain` for `text/plain`, or `html` for `text/html` |

When `format` is `html`, pass HTML markup in `body` (for example `<p>Hello</p>`). Gmail renders it as HTML in the recipient's client. When omitted or set to `plain`, the body is sent as plain text.

Send uses the Gmail API `users.messages.send` endpoint with an RFC 2822 MIME message encoded as base64url. Messages are sent from the authenticated Google account.

---

## Configuration

The agent reads its configuration from a JSON file passed via `--config`. Sample configs live in `configs/`:

- `configs/openai-compat/` — OpenAI-compatible providers (e.g. local servers, OpenRouter, etc.)
- `configs/openai-compat/config_current_folder.json` — same provider, scoped to the current directory

Typical keys: provider, model, API key / base URL (read from env), and channel settings. See `src/config/schema.rs` for the full schema.

The first run will seed the workspace directory with `AGENTS.md`, `SOUL.md`, `TOOLS.md`, and `USER.md`, plus the standard folder layout. These come from the compiled-in template bundle by default; drop a `templates/` directory next to the binary (or set `RUST_BOT_TEMPLATES_DIR`) if you want to override them with your own copies.

---

## Project layout

```
rust-bot/
├── src/
│   ├── bin/
│   │   └── gmail-auth.rs   # OAuth helper for Gmail token setup
│   ├── agent/        # Agent loop, runner, tools, skills
│   ├── bus/          # Internal event bus
│   ├── cli/          # CLI commands, stream rendering, interactive console
│   ├── command/      # Slash-style command router and builtins
│   ├── config/       # Config schema, loader, paths
│   ├── cron/         # Scheduled reminder service
│   ├── providers/    # Model providers (Anthropic, OpenAI-compat, …)
│   ├── security/     # Sandboxing and policy
│   ├── session/      # Session manager
│   └── utils/        # Helpers (clipboard, restart, prompts, …)
├── configs/          # Sample provider configs
├── templates/        # Workspace seed files (also embedded into the binary at build time)
├── tests/            # Unit and integration tests
├── web-chat/         # Leptos (Rust + WASM) chat UI, independent workspace member — see web-chat/README.md
└── README.md
```

### Web chat UI

`web-chat/` is a small Leptos + Tailwind chat UI (login + chat) that talks
to the REST API over HTTP only — it shares no code with the rest of the
crate. Build it with [Trunk](https://trunkrs.dev) and serve the output
alongside the API:

```bash
cd web-chat && trunk build --release && cd ..
cargo run -- api --config ./configs/simple1/config.json --web-root ./web-chat/dist
```

Then open `http://127.0.0.1:8900/`. See [`web-chat/README.md`](web-chat/README.md)
for local development (`trunk serve`) instructions.

## License

See repository for license information.
