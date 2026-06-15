# Rust Bot

This is a simple bot implementation based on Nanobot in Rust.

## Pre-requisites

Install Rust

## Testing

Some of the tests require an .env file with some parameters specified in .env_local.

```
cargo test
```

#### Integration tests

```
cargo test --tests
```

## Build 

Use:

```
cargo build -r
```

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
| `-m`, `--message` | _(none)_ | Message to send to the agent |
| `-s`, `--session` | `cli:direct` | Session ID |
| `-w`, `--workspace` | `~/.rust-bot/workspace` | Workspace directory |
| `-c`, `--config` | `~/.rust-bot/config.json` | Config file path |
| `--markdown` / `--no-markdown` | `true` | Render assistant output as Markdown |
| `--logs` / `--no-logs` | `false` | Show runtime logs during chat |

Omitting `-m` / `--message` is reserved for a future interactive REPL (not yet implemented).

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
cargo run -- agent -m "What files are in the workspace?" --config ./configs/openai-compat/config.json

# Custom session and workspace
cargo run -- agent -m "hello" -s myproject:cli -w ~/.rust-bot/workspace

# Plain-text output, with runtime logs
cargo run -- agent -m "status" --no-markdown --logs
```

```ps1
cargo run -- agent -m "How is the weather in London?" --config ./configs/openai-compat/config.json --logs
cargo run -- agent -m "How is the weather in London?" --config ./configs/openai-compat/config.json --no-logs
cargo run -- agent -m "Can you please give me a quick summary of the services offered by Onepoint Consulting Ltd from London? Then please write this summary to a file called onepoint.html in the workspace folder." --config ./configs/openai-compat/config.json --logs
cargo run -- agent -m "Which are the main competitors of Onepoint Consulting Ltd?" --config ./configs/openai-compat/config.json --logs
cargo run -- agent -m "Can you produce a commit message for the staged files in the current git project (current folder)?" --config ./configs/openai-compat/config_current_folder.json --logs
```

```bash
cargo build -r
./target/release/rust-bot agent -m "What files are in the workspace?"
```

```ps1
cargo build -r
.\target\release\rust-bot agent -m "What files are in the workspace?"
```