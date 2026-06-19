# Rust Bot

A simple bot implementation based on [Nanobot](https://github.com/HKUDS/nanobot), written in Rust. It ships as a single CLI binary (`rust-bot`) that can run the agent in one-shot mode or as an interactive console.

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

| Code | Meaning |
|------|---------|
| `0` | Success |
| `1` | Config or general CLI error |
| `2` | Workspace templates unavailable (no `templates/` in the current working directory and the workspace is not fully seeded with `AGENTS.md`, `SOUL.md`, `TOOLS.md`, and `USER.md`) |
| `3` | Invalid provider (unknown value in `agents.provider`) |

Run from the project root (or any directory that contains `templates/`) so workspace seed files can be created on first use.

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
| `Ctrl+Enter` | _(custom)_ Insert a newline, do not submit — see [Multi-line input](#multi-line-input) |
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

This is useful when pasting larger snippets, code, logs, or text that may contain newlines. The console captures the clipboard contents through a small sentinel token and replaces that token before sending the message.

- Multiple `Alt+V` presses in one prompt are supported; captured snippets are inserted in order.
- If clipboard text cannot be read, the paste is treated as empty and the placeholder is stripped on submit.

### Multi-line input

By default, `Enter` submits the current line. To continue a thought on a new line without sending the message, press `Ctrl+Enter` — a newline is inserted and you can keep typing. The whole block is sent to the agent as a single message when you finally press `Enter` on an empty trailing line (or on the last actual line).

This is useful for pasting code blocks, listing steps, or writing prompts that span several lines.

### Leaving the console

Any of the following will exit the console:

- Type `exit` or `quit` and press `Enter`.
- Press `Ctrl+D` on an empty line.
- Send an interrupt (the binary is still long-running after the console returns, so this only closes the prompt, not the process).

The console always prints the banner on entry — that's the easiest way to confirm you've launched interactive mode rather than one-shot mode.

---

## Configuration

The agent reads its configuration from a JSON file passed via `--config`. Sample configs live in `configs/`:

- `configs/openai-compat/` — OpenAI-compatible providers (e.g. local servers, OpenRouter, etc.)
- `configs/openai-compat/config_current_folder.json` — same provider, scoped to the current directory

Typical keys: provider, model, API key / base URL (read from env), and channel settings. See `src/config/schema.rs` for the full schema.

The first run will seed the workspace directory with `AGENTS.md`, `SOUL.md`, `TOOLS.md`, and `USER.md` from `templates/`, plus the standard folder layout. Run the binary from the project root (or any directory containing `templates/`) so this seeding can happen.

---

## Project layout

```
rust-bot/
├── src/
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
├── templates/        # Workspace seed files
├── tests/            # Unit and integration tests
└── README.md
```

## License

See repository for license information.
