# Interactive console

Rust Bot ships with a small REPL-style interactive console built on `[reedline](https://github.com/nushell/reedline)`. It uses Emacs-style keybindings by default, supports history, and lets you paste images, paste clipboard text, and write multi-line prompts without leaving the terminal.

## Starting the console

Launch `rust-bot agent` **without** the `-m` / `--message` flag and the binary will drop you into the console:

```bash
cargo run -- agent --config ./configs/openai-compat/config_current_folder.json --logs
```

A short banner is printed, showing the logo and the available shortcuts. Then the prompt appears and the agent is ready for input. Each line you submit is sent to the agent and the response is rendered in the same terminal — Markdown by default, plain text if you passed `--no-markdown`.

> Note: the console requires a real TTY. If stdin is redirected (for example, in a non-interactive pipe), the binary returns a non-zero exit instead of dropping into the REPL.



## Key bindings

The line editor uses **Emacs** keybindings. The full default set is available; the table below highlights the bindings that are most useful day-to-day. Custom bindings added by Rust Bot are marked.

### Movement


| Key            | Action                             |
| -------------- | ---------------------------------- |
| `Ctrl+F` / `→` | Move cursor one character forward  |
| `Ctrl+B` / `←` | Move cursor one character backward |
| `Alt+F`        | Move cursor one word forward       |
| `Alt+B`        | Move cursor one word backward      |
| `Ctrl+A`       | Move to start of line              |
| `Ctrl+E`       | Move to end of line                |
| `Ctrl+N` / `↓` | Next history entry                 |
| `Ctrl+P` / `↑` | Previous history entry             |
| `Ctrl+→`       | Move forward by word               |
| `Ctrl+←`       | Move backward by word              |




### Editing


| Key                         | Action                                                                                                                   |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `Ctrl+D`                    | Delete character under cursor; exits the console if the line is empty                                                    |
| `Ctrl+H` / `Backspace`      | Delete character before cursor                                                                                           |
| `Alt+D`                     | Delete word forward                                                                                                      |
| `Alt+Backspace`             | Delete word backward                                                                                                     |
| `Ctrl+K`                    | Kill to end of line                                                                                                      |
| `Ctrl+U`                    | Kill to start of line                                                                                                    |
| `Ctrl+W`                    | Kill previous word                                                                                                       |
| `Ctrl+Y`                    | Yank (paste) last killed text                                                                                            |
| `Ctrl+T`                    | Transpose characters                                                                                                     |
| `Alt+T`                     | Transpose words                                                                                                          |
| `Ctrl+I` / `Tab`            | *(custom)* Paste image from clipboard — see [Image paste](#image-paste)                                                  |
| `Alt+V`                     | *(custom)* Paste clipboard text into the current prompt — see [Text paste](#text-paste)                                  |
| `Ctrl+O`                    | *(custom)* Insert a newline, do not submit — see [Multi-line input](#multi-line-input)                                   |
| `Alt+Enter` / `Shift+Enter` | Insert a newline, do not submit (only on terminals that report the modifier — see [Multi-line input](#multi-line-input)) |
| `Ctrl+C`                    | Cancel the current line and re-show the prompt (does not exit)                                                           |
| `Ctrl+L`                    | Clear the screen                                                                                                         |




### History search


| Key      | Action                                                                                             |
| -------- | -------------------------------------------------------------------------------------------------- |
| `Ctrl+R` | Start incremental reverse search; type to narrow, `Enter` to accept, `Ctrl+C` / `Ctrl+G` to cancel |
| `Ctrl+S` | Forward search (continues a `Ctrl+R` session)                                                      |




### Submission


| Key     | Action                                            |
| ------- | ------------------------------------------------- |
| `Enter` | Submit the current line as a message to the agent |




## History

The console keeps the **last 100 lines** in a file-backed history, loaded automatically on the next start. The file lives at `~/.rust-bot/cli_history` (see `get_cli_history_path` in `src/config/paths.rs`) and is created on first use. Use `↑` / `↓` to walk it, or `Ctrl+R` to fuzzy-search.

If the history file is unreadable (permissions, first run, etc.) the console starts with an empty history and logs a warning.

## Image paste

Pressing `Alt+I` (or `Tab`) reads the current clipboard image and inserts a sentinel token into the buffer. On submit, the sentinel is replaced by the actual image and sent to the agent alongside the text.

- The image is stored in a temporary file in the workspace; if the message is sent successfully the file is cleaned up.
- `Alt+I` is bound to image paste because `Tab` would otherwise complete completions; if you don't have an image on the clipboard the binding is a safe no-op (the sentinel stays in the buffer and is stripped on submit).
- The console uses **bracketed paste mode**, so multi-line text pasted from the terminal is treated as one block rather than being submitted early.



## Text paste

Pressing `Alt+V` captures the current clipboard text and inserts it into the prompt at the cursor position. The text is sent as part of the same message when you press `Enter`.

This is useful when pasting larger snippets, code, logs, or text that may contain newlines. Internally, each paste is recorded with an index and a line-count hint (e.g. `[PASTED_TEXT-#0 12 lines]`). On submit, every sentinel is replaced by its corresponding captured text using that index, so the substitution is always correct regardless of cursor position or paste order.

- **Single-line text** is inserted directly into the buffer as plain text — no sentinel is used.
- **Multi-line text** is stored separately and represented by an indexed sentinel in the buffer. The sentinel shows the paste index and line count so you can see what is queued.
- Multiple `Alt+V` presses in one prompt are fully supported; each paste gets its own index and is substituted independently on submit.
- If clipboard text cannot be read, the paste is treated as empty and the placeholder is stripped on submit.



## Multi-line input

By default, `Enter` submits the current line. To continue a thought on a new line without sending the message, press `Ctrl+O` — a newline is inserted and you can keep typing. The whole block is sent to the agent as a single message when you finally press `Enter`.

This is useful for pasting code blocks, listing steps, or writing prompts that span several lines.

> **Why not** `Ctrl+Enter`**?** The console enables `ENABLE_VIRTUAL_TERMINAL_INPUT` (required for bracketed paste). In that mode Windows Terminal reports `Ctrl+Enter`, `Shift+Enter`, and `Alt+Enter` all as a plain `Enter`, so the modifier is lost before it reaches the line editor. A `Ctrl`+letter combo (`Ctrl+O`) arrives as a distinct control byte and works reliably on every platform. `Alt+Enter` / `Shift+Enter` still work on terminals that disambiguate them (e.g. kitty-protocol-capable emulators on Unix).



## Leaving the console

Any of the following will exit the console:

- Type `exit` or `quit` and press `Enter`.
- Press `Ctrl+D` on an empty line.
- Send an interrupt (the binary is still long-running after the console returns, so this only closes the prompt, not the process).

The console always prints the banner on entry — that's the easiest way to confirm you've launched interactive mode rather than one-shot mode.

## Chat commands

These are typed directly into a running conversation (the interactive console, or
any connected channel) — distinct from the `rust-bot <subcommand>` process
launchers documented under [Command line](COMMAND_LINE.md).


| Command            | Description                                                                     |
| ------------------ | ------------------------------------------------------------------------------- |
| `/new`             | Start a new conversation                                                        |
| `/stop`            | Stop the current task                                                           |
| `/restart`         | Restart the bot                                                                 |
| `/status`          | Show bot status                                                                 |
| `/model`           | Show the current model                                                          |
| `/model-preset`    | Show the current preset's model and provider, or switch: `/model-preset <name>` |
| `/model-presets`   | List available model presets                                                    |
| `/dream`           | Manually trigger Dream consolidation                                            |
| `/dream-log`       | Show what the last Dream changed                                                |
| `/dream-restore`   | Revert memory to a previous state                                               |
| `/help`            | Show available commands                                                         |
| `/mcp-list`        | List connected MCP servers                                                      |
| `/mcp-preset`      | Manage built-in MCP server presets — see [below](#mcp-preset)                   |
| `/tools`           | List available tools                                                            |
| `/workspace`       | Show or switch the session's workspace scope                                    |
| `/goal`            | Start/check/cancel a sustained session goal                                     |
| `/cleanup`         | Remove stray files from the workspace                                           |
| `/list-sessions`   | List available sessions in the current workspace                                |
| `/example-prompts` | List example prompts                                                            |




### `/mcp-preset`

Enable, disable, test, or list built-in MCP server presets (GitHub, Playwright,
Brave Search, etc.) without hand-editing `config.json`. The catalog is loaded
from the bundled defaults, merged with any overrides/extras at the path in
`tools.mcpPresetsPath` (default `~/.rust-bot/mcp_presets.json`) — a user entry
with the same name as a bundled preset overrides it; new names are pure
additions.

```bash
/mcp-preset list
/mcp-preset enable github github_token=ghp_xxx
/mcp-preset test github
/mcp-preset disable github
```

- `list` — shows every preset with its status (`configured`,
`missing_credentials`, `missing_dependency`, `not_installed`), plus any
custom (non-preset) MCP servers already in `config.json`.
- `enable <name> [field=value ...]` — materializes the preset into a full
MCP server config and writes it into `config.json`. Any field a preset needs
(an API key, a token) can be supplied inline as `field=value`; if omitted, an
already-configured value is reused, and failing that, a matching environment
variable is referenced as `${VAR_NAME}` in the saved config (the secret
itself is never copied into `config.json` if it's only set as an env var).
**Requires a restart** — the running process only reads its MCP server list
once at startup.
- `disable <name>` — removes the server from `config.json`. Also requires
a restart.
- `test <name>` — connects to an already-enabled server right away (no
restart needed) and reports how many tools it exposes, or why the connection
failed.

