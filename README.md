# Rust Bot

A simple bot implementation based on [Nanobot](https://github.com/HKUDS/nanobot), written in Rust. It ships as a CLI binary (`rust-bot`) that can run the agent in one-shot mode or as an interactive console, plus an optional `gmail-auth` helper for Gmail OAuth setup.

## Table of contents

- [Pre-requisites](#pre-requisites)
- [Testing](#testing)
- [Build](#build)
- [Quick start](#quick-start)
- [Command line](#command-line) (see [docs/COMMAND_LINE.md](docs/COMMAND_LINE.md))
- [Interactive console](#interactive-console) (including chat commands; see [docs/INTERACTIVE_CONSOLE.md](docs/INTERACTIVE_CONSOLE.md))
- [Gmail support](#gmail-support) (see [docs/GMAIL_SUPPORT.md](docs/GMAIL_SUPPORT.md))
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

Quick tests:

```
cargo test --lib
```



## Build

In order to build the whole project you can use the build script:

```
cargo build -r
```

This produces `target/release/rust-bot` (or `.\target\release\rust-bot.exe` on Windows).

A separate helper binary for Gmail OAuth is built alongside the main CLI:

```
cargo build -r --bin gmail-auth
```

This produces `target/release/gmail-auth` (or `.\target\release\gmail-auth.exe` on Windows).

To build both the CLI and the web-chat UI in one step (requires [Trunk](https://trunkrs.dev); see [Web chat UI](#web-chat-ui)):

Linux:
```bash
./scripts/build_all.sh
```

Windows:
```ps1
.\scripts\build_all.bat [--release]
```

## Quick start

Pre-built packages are on [GitHub Releases](https://github.com/onepointconsulting/rust-bot/releases). Linux example:

```bash
# Download the binary distribution with wget
VERSION=0.3.0
wget "https://github.com/onepointconsulting/rust-bot/releases/download/v${VERSION}/rust-bot-${VERSION}-linux-x86_64.tar.gz"

# Unpack
tar xzf "rust-bot-${VERSION}-linux-x86_64.tar.gz"
cd "rust-bot-${VERSION}-linux-x86_64"

# Run onboard and follow prompted instructions
./rust-bot onboard

# Run on the command line
./rust-bot agent -c ./.rust-bot/config.json -m "Hello!"
```

Omit `-m` to enter the [interactive console](docs/INTERACTIVE_CONSOLE.md). Windows, Linux ARM64, and macOS archives are `rust-bot-<version>-windows-x86_64.zip`, `rust-bot-<version>-linux-aarch64.tar.gz`, and `rust-bot-<version>-macos-aarch64.tar.gz`; after unpacking, run `.\rust-bot.exe onboard` or `./rust-bot onboard`. Onboard writes config and workspace under `./.rust-bot/` next to the binary. See `INSTALL.md` in the archive for the full first-run walkthrough.

---



## Command line

The full command-line reference (the `agent` subcommand and its flags, JWT keypair and token generation, exit codes, and examples) lives in [docs/COMMAND_LINE.md](docs/COMMAND_LINE.md).


---



## Interactive console

Rust Bot ships with a REPL-style interactive console (Emacs-style keybindings, history, image and text paste, multi-line prompts). Key bindings, history, paste behaviour, how to leave the console, and the chat commands (such as `/mcp-preset`) are documented in [docs/INTERACTIVE_CONSOLE.md](docs/INTERACTIVE_CONSOLE.md).

---



## Gmail support

The agent can read and send email through Gmail. Google Cloud setup, the `gmail-auth` OAuth helper, enabling the Gmail tools, and the tool parameters are documented in [docs/GMAIL_SUPPORT.md](docs/GMAIL_SUPPORT.md).

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
│   ├── agent/        # Agent loop, runner, tools, skills, subagents, ACP client
│   ├── api/          # REST API server (login, SSO, media, user registry)
│   ├── bus/          # Internal event bus
│   ├── channels/     # Chat channels and gateway (WebSocket, WhatsApp, email)
│   ├── cli/          # CLI commands, stream rendering, interactive console
│   ├── command/      # Slash-style command router and builtins
│   ├── config/       # Config schema, loader, paths
│   ├── cron/         # Scheduled reminder service
│   ├── heartbeat/    # Periodic heartbeat task service
│   ├── integrations/ # External integrations (Herdr)
│   ├── providers/    # Model providers (Anthropic, OpenAI-compat, …)
│   ├── security/     # JWT, sandboxing, ingress and workspace access policy
│   ├── session/      # Session manager
│   ├── utils/        # Helpers (clipboard, restart, prompts, embedded UI/templates, …)
│   ├── pairing.rs          # DM sender pairing store
│   └── runtime_context.rs  # Persistent context appended to the user prompt
├── configs/          # Sample provider configs
├── templates/        # Workspace seed files (also embedded into the binary at build time)
├── tests/            # Unit and integration tests
├── chat-ui/          # Shared Leptos components (login form, composer, markdown, message bubbles) used by both chat UIs
├── websockets-chat/  # Leptos (Rust + WASM) chat UI for the WebSocket gateway, streaming replies — see websockets-chat/README.md
├── web-chat/         # Leptos (Rust + WASM) chat UI for the REST API (superseded by websockets-chat) — see web-chat/README.md
├── docs/             # Detailed documentation (command line, interactive console, Gmail support, …)
└── README.md
```



### Chat UIs

The Cargo workspace contains three browser-side crates (all Leptos + WASM, built with [Trunk](https://trunkrs.dev)):

| Crate             | Talks to                                      | Status                                         |
| ----------------- | --------------------------------------------- | ---------------------------------------------- |
| `websockets-chat` | WebSocket gateway (`rust-bot gateway`)        | Current UI: token streaming, tool chips, reasoning panels |
| `web-chat`        | REST API (`rust-bot api`)                     | Superseded by `websockets-chat`                |
| `chat-ui`         | — (library, no transport)                     | Shared components used by the two UIs above    |

See [docs/WEBSOCKETS_CHAT.md](docs/WEBSOCKETS_CHAT.md) for how `websockets-chat` is built, embedded into the `rust-bot` binary and run, and [websockets-chat/README.md](websockets-chat/README.md) for the app's own details. The rest of this section covers the older `web-chat` UI.

#### Web chat UI (`web-chat`)

`web-chat/` is a small Leptos + Tailwind chat UI (login + chat) that talks
to the REST API over HTTP only — it shares no code with the main
`rust-bot` crate (only the `chat-ui` components). Build it with [Trunk](https://trunkrs.dev) and serve the output
alongside the API. This chat interface has been superseded by the Websockets Chat UI.

Install the WASM target and [Trunk](https://trunkrs.dev) once:

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk --locked
```

Ensure `~/.cargo/bin` is on your `PATH` (it usually is if `cargo` works). Then build and run:

```bash
cd web-chat && trunk build --release && cd ..
cargo run -- api --config ./configs/simple1/config.json --web-root ./web-chat/dist
```

Or use `./scripts/build_all.sh` to build the release CLI and web-chat together.

Then open `http://127.0.0.1:8900/`. See `[web-chat/README.md](web-chat/README.md)`
for local development (`trunk serve`) instructions.

## License

[MIT](LICENSE) — anyone is free to use, modify, and distribute this code, with attribution.

rust-bot is based on [nanobot](https://github.com/HKUDS/nanobot) (also MIT, © Xubin Ren and the nanobot contributors), which is gratefully acknowledged.
