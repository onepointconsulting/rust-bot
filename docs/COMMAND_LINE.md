# Command line

Run the agent from the terminal:

```
cargo run -- agent [OPTIONS]
```

After a release build (`cargo build -r`), use:

```
rust-bot agent [OPTIONS]
```

Running just `rust-bot agent` will assume the configuration file is `.rust-bot\config.json`

## `agent` subcommand

Run the agent from the command line. For the full, up-to-date option list:

```
cargo run -- agent --help
```


| Flag                           | Default                   | Description                                                                                  |
| ------------------------------ | ------------------------- | -------------------------------------------------------------------------------------------- |
| `-m`, `--message`              | *(none)*                  | Message to send to the agent. Omit to enter the [interactive console](INTERACTIVE_CONSOLE.md). |
| `-s`, `--session`              | `cli:direct`              | Session ID                                                                                   |
| `-w`, `--workspace`            | `~/.rust-bot/workspace`   | Workspace directory                                                                          |
| `-c`, `--config`               | `~/.rust-bot/config.json` | Config file path                                                                             |
| `--markdown` / `--no-markdown` | `true`                    | Render assistant output as Markdown                                                          |
| `--logs` / `--no-logs`         | `false`                   | Show runtime logs during chat                                                                |


## JWT keypair and tokens

JWT auth for the REST API and WebSocket gateway is configured through two
`rust-bot` subcommands (no separate helper binary).

Generate an Ed25519 keypair and write the key paths into the config:

```
cargo run -- generate-jwt-keypair --config ./path/to/config.json
```

After a release build:

```
rust-bot generate-jwt-keypair --config ./path/to/config.json
```

Keys are written to `./.rust-bot/credentials/` by default. Pass
`--credentials-dir` to choose another directory, or `--force` to overwrite
existing keys.

Mint a bearer token and register a user:

```
cargo run -- generate-jwt-token --config ./path/to/config.json \
  --user-email user@example.com --users-file ./path/to/users.json \
  --password "correct horse battery staple"
```

`--user-email`, `--users-file`, and `--password` are required. Optional
flags: `--iss`, `--aud`, `--purpose` (e.g. `webui` for the WebSocket chat
UI), `--expires-in-months` (default: 6). The JWT is printed to stdout. The
password is hashed with Argon2id and stored in the users file for
`/v1/login`.

For the full option list:

```
cargo run -- generate-jwt-keypair --help
cargo run -- generate-jwt-token --help
```


## Exit codes


| Code | Constant                   | Meaning                                                                  |
| ---- | -------------------------- | ------------------------------------------------------------------------ |
| `0`  | `SUCCESS`                  | Success (also used after spawning a restarted process on Windows)        |
| `1`  | `GENERAL_ERROR`            | Config or general CLI error                                              |
| `3`  | `INVALID_PROVIDER`         | Invalid provider (unknown value in `agents.provider`)                    |
| `4`  | `GMAIL_CONFIG_ERROR`       | Gmail tool credentials missing (OAuth client secret or token cache)      |
| `5`  | `CHANNEL_ALLOW_FROM_EMPTY` | Channel has an empty `allowFrom` list (set `["*"]` or specific user IDs) |


Constants live in `src/utils/exit_codes.rs`.

Workspace seed files (`AGENTS.md`, `SOUL.md`, `TOOLS.md`, `USER.md`, …) are compiled into the
binary, so onboarding always works even without a sibling `templates/` folder. Drop a
`templates/` directory next to the binary (or set `RUST_BOT_TEMPLATES_DIR`) to override the
bundled defaults with your own.

## Examples

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
