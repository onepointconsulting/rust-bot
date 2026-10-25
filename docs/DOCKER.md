# Running rust-bot in Docker

The image runs `rust-bot gateway` (the WebUI plus channels) as an unprivileged user. The
configuration is the same hierarchical `config.json` as everywhere else; it is **not**
flattened into environment variables.

## Quick start

```bash
cp .env.docker.example .env.docker      # fill in OPENAI_API_KEY, OPENAI_API_MODEL, ...
docker compose up -d --build
# WebUI: http://127.0.0.1:18790
```

## How configuration reaches the container

| What | How |
|------|-----|
| The structured config | `/data/config.json` on the `/data` volume. Seeded from `docker/config.template.json` on the first start, then yours to edit (WebUI Settings dialog, or directly on the host; see [Choosing the host folder](#choosing-the-host-folder-behind-data)). |
| Secrets (API keys, tokens) | Never in the file. Write `"apiKey": "${OPENAI_API_KEY}"` and pass the variable with `env_file`, `-e`, or a Docker/Kubernetes secret exposed as an environment variable. rust-bot expands `${VAR}` in any string value at any depth, and refuses to start naming the variable if one is unset. |
| Which file to read | `RUST_BOT_CONFIG` (default `/data/config.json`). Every subcommand honours it when `--config` is omitted. |
| Where state is kept | Next to the config file, unless `RUST_BOT_DATA_DIR` points elsewhere. |

### Choosing the host folder behind `/data`

`docker-compose.yml` defines `rust-bot-data` as a named volume bound to a folder on the
host, so you can see and edit the files directly:

```yaml
volumes:
  rust-bot-data:
    driver: local
    driver_opts:
      type: none
      o: bind
      device: C:\var\rust-bot\openrouter1\data   # change to your folder
```

`device` must point to an **existing** folder (Docker does not create it). The folder
becomes `/data` inside the container and should contain:

| Path in the folder | Required | Purpose |
|--------------------|----------|---------|
| `config.json` | Recommended | Your configuration. If it is missing, the template from the image is copied in on the first start. |
| `credentials/` | Optional | JWT key pair (`private_key.pem`, `public_key.pem`), Gmail `client_secret.json` and `token_cache.json`. |
| `users.json` | Optional | Login users for the WebUI when `channels.websocket.jwt` is enabled. |

Everything else (`workspace/`, `sessions/`, `logs/`, `cron/`, ...) is created by rust-bot on
first use.

Paths inside `config.json` are read **inside the container**, so they must be Linux paths
under `/data`, not host paths:

```json
"privateKeyPath": "/data/credentials/private_key.pem",
"publicKeyPath": "/data/credentials/public_key.pem",
"usersFile": "/data/users.json"
```

Two more things the config must get right in a container: `gateway.host` must be `0.0.0.0`
(with `127.0.0.1` the published port cannot reach the gateway) and `gateway.port` must match
the container side of the port mapping (`18790`).

Notes:

- Volume options are fixed when the volume is created. After changing `device`, remove the
  volume first (`docker compose down -v`); with a bind volume this does not delete your files.
- On Docker Desktop for Windows, if a plain `C:\...` path is rejected, use the VM's view of
  the drive, e.g. `/run/desktop/mnt/host/c/var/rust-bot/openrouter1/data`.
- Prefer not to use a named volume? Replace `- rust-bot-data:/data` with a direct bind mount
  such as `- C:\var\rust-bot\openrouter1\data:/data` and delete the `volumes:` block.

### Mounting a read-only config (Kubernetes ConfigMap, Compose `configs:`)

The WebUI saves settings by rewriting the config file, so a read-only mount disables that.
Keep state writable by separating it:

```yaml
environment:
  RUST_BOT_CONFIG: /etc/rust-bot/config.json
  RUST_BOT_DATA_DIR: /data
volumes:
  - rust-bot-data:/data
configs:
  - source: rust-bot-config
    target: /etc/rust-bot/config.json
```

The entrypoint only seeds a config when the file is missing, so a mounted one is used as is.

## Checking a config

```bash
docker compose run --rm rust-bot check-config
```

`check-config` reports invalid JSON, schema errors, unknown model presets and every
`${VAR}` that is not set, and exits non-zero on errors. The entrypoint runs it before
`gateway` and `api`. It also warns when the gateway listens on a non-loopback address while
`channels.websocket.jwt.enabled` is not `true`.

## Security

- **The WebUI has no login by default.** Anyone who can reach the port can use the agent's
  tools, including the shell. `docker-compose.yml` therefore publishes the port on the
  host's loopback only. Put an authenticating reverse proxy in front of it, or enable
  `channels.websocket.jwt`, before exposing it.
- The container is the shell tool's sandbox. The template sets `tools.restrictToWorkspace`
  to `true`; mount only what the agent should be able to touch.
- Never copy `credentials/` or `.env*` files into an image: `.dockerignore` excludes them.

## Adding runtimes

The slim image has `git` and `curl`. For MCP servers and skills that need Python or Node:

```bash
docker build --build-arg EXTRA_PACKAGES="python3 nodejs npm" -t rust-bot .
```
