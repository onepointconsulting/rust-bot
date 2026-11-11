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
| `-r`, `--resume [query]`       | *(none)*                  | Pick a previous CLI session to resume (see [Resuming a session](#resuming-a-session)). Cannot be combined with `-s`. |
| `-w`, `--workspace`            | `~/.rust-bot/workspace`   | Workspace directory                                                                          |
| `-c`, `--config`               | `~/.rust-bot/config.json` | Config file path                                                                             |
| `--markdown` / `--no-markdown` | `true`                    | Render assistant output as Markdown                                                          |
| `--logs` / `--no-logs`         | `false`                   | Show runtime logs during chat                                                                |


### Resuming a session

`rust-bot agent -r` opens a searchable list of earlier CLI sessions, newest first. Each row shows
the session title (or its key when it has no title), how long ago it was last used, its key and
the size of its session file:

```
Resume session
╭──────────────────────────────────────────────╮
│ ⌕ Search…                                     │
╰──────────────────────────────────────────────╯
❯ HMRC R&D evidence report for chatbot engine
  1 week ago · ems · 678.5KB

  Rust-bot-side-bar plan review
  2 weeks ago · cli:direct · 1.2MB

Space to preview · Ctrl+R to rename · Type to search · Esc to cancel
```

| Key                 | Action                                                                                  |
| ------------------- | --------------------------------------------------------------------------------------- |
| ↑ / ↓               | Move the cursor (wraps around)                                                          |
| PgUp / PgDn, Home / End | Move a page at a time, or to the first / last session                               |
| Enter               | Resume the highlighted session                                                          |
| Esc / Ctrl+C        | Leave without resuming ("No session selected.")                                         |
| Typing              | Filter: every word must appear (case-insensitively) in the title or key                 |
| Backspace / Ctrl+U  | Delete a character / clear the search                                                   |
| Space               | With an empty search, show or hide the last few turns of the highlighted session; otherwise it types a space |
| Ctrl+R              | Rename the highlighted session (Enter saves, Esc stops renaming)                        |

`rust-bot agent -r <query>` resumes directly when the query is an exact session key or matches
exactly one session; otherwise the picker opens with the search already filled in. `-r` combines
with `-m`, so `rust-bot agent -r -m "hi"` picks a session and sends one message to it. Without an
interactive terminal (e.g. piped input), `--resume` only works when the query resolves to a
single session; use `-s <key>` instead.

The list only contains CLI sessions (`cli:*` keys and names given with `-s`). Websocket, ACP,
cron, API, heartbeat and other channel sessions are left out on purpose: they live in the same
`sessions/` folder, but the process that owns them (usually the gateway) caches them in memory
and rewrites the whole file on each save, and nothing locks the files. Continuing them from the
CLI would lose turns or overwrite the gateway's, and the turns would not show up in the web UI's
transcript.

## `onboard` subcommand

Create (or refresh) an instance: config, workspace and, optionally, the gateway web UI with a
first login user. For the full option list:

```
cargo run -- onboard --help
```

| Flag | Default | Description |
|------|---------|-------------|
| `--mode <MODE>` | *(see below)* | `local` = `./.rust-bot/`, `user` = `~/.rust-bot/`, `docker` = a Docker instance in `./rust-bot-docker/` |
| `-w`, `--workspace` | `workspace/` next to the config | Workspace directory (not with `--mode docker`) |
| `-c`, `--config` | | Config file path (not with `--mode`) |
| `--wizard` | `false` | Interactive wizard with every setting (not with `--mode docker`) |

Without `--mode`, onboard reuses an existing `./.rust-bot/`, then `~/.rust-bot/`, and otherwise
creates `./.rust-bot/`.

Flags that only apply with `--mode docker` (see [DOCKER.md](DOCKER.md#quick-start-rust-bot-onboard---mode-docker)):

| Flag | Default | Description |
|------|---------|-------------|
| `--dir <DIR>` | `./rust-bot-docker` | Instance folder (compose file, `.env.docker`, `data/`) |
| `--name <NAME>` | `rust-bot` | Compose project and container name |
| `--port <PORT>` | `18790` | Host port of the web UI, published on `127.0.0.1` |
| `--image <IMAGE>` | `gilf/rust-bot:<version>` | Image to run (not with `--build`) |
| `--build <SOURCE_DIR>` | | Build the image from a rust-bot source checkout |
| `--no-start` | `false` | Write the files only; do not pull or start |

```
rust-bot onboard --mode local
rust-bot onboard --mode docker --port 18791 --name bot-two --dir ./bot-two
```

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
UI), `--expires-in-months` (default: 6), `--role` (`admin` or `user`, default
`user`). The JWT is printed to stdout. The password is hashed with Argon2id
and stored in the users file for `/v1/login`.

Only `admin` users see the web UI's Settings and can change the configuration
or restart rust-bot; `user` accounts can chat only. The role is stored in the
users file and stamped into every token minted at login. To promote an
existing user, edit their `"role"` in the users file (`"admin"` / `"user"`);
entries without a role are plain users. Tokens issued before roles existed
also count as `user`.

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
