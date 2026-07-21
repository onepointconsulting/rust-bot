# Rust Bot - Installation

This package contains a pre-built `rust-bot` binary and two sample
configurations. Follow these steps to get started.

## 1. Unpack

Extract the archive:

```text
rust-bot-<version>-<platform>/
  rust-bot[.exe]
  INSTALL.md
  templates/
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
.\rust-bot.exe onboard --config .\configuration\samples\openai-compat.json
.\rust-bot.exe agent -m "Hello!" --config .\configuration\samples\openai-compat.json

# Linux / macOS
./rust-bot onboard --config ./configuration/samples/openai-compat.json
./rust-bot agent -m "Hello!" --config ./configuration/samples/openai-compat.json
```

Swap in `configuration/samples/anthropic.json` to use the Anthropic sample
instead.

Omit `-m/--message` to start the interactive console.

If you want to create a new configuration though, you can run this command, that will create a new default configuration that you can edit yourself:

```bash
rust-bot.exe onboard --config ./rust-bot/config.json
```

## 5. Next steps

- Copy a sample config to a location of your choice and adjust it (workspace
  path, ports, tool settings) once you are up and running.
- See the main [README](https://github.com/onepointconsulting/rust-bot#readme)
  for full CLI documentation, the interactive console, Gmail setup, and the
  configuration reference.
