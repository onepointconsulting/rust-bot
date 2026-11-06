# Running rust-bot in Docker

The image runs `rust-bot gateway` (the WebUI plus channels) as an unprivileged user. The
configuration is the same hierarchical `config.json` as everywhere else; it is **not**
flattened into environment variables.

The image is published on Docker Hub as [`gilf/rust-bot`](https://hub.docker.com/r/gilf/rust-bot)
for `linux/amd64` and `linux/arm64`, tagged with the rust-bot version and `latest`.

## Quick start: `rust-bot onboard --mode docker`

With rust-bot installed and Docker running:

```bash
rust-bot onboard --mode docker
```

This asks the usual onboard questions (provider, API key, model) and creates the first web UI
user. Then it writes an instance folder, pulls the image, starts the container and waits until it
is healthy:

```
✓ rust-bot is running at http://127.0.0.1:18790 (log in with the user you created)
```

### What it creates

```
rust-bot-docker/               (--dir; default ./rust-bot-docker)
├── docker-compose.yml         image gilf/rust-bot:<version of your rust-bot>
├── .env.docker                API keys (secrets); never commit it
├── .gitignore                 "*": the folder holds secrets
└── data/                      mounted as /data in the container
    ├── config.json            paths under /data/..., API keys as ${VAR} placeholders
    ├── credentials/           JWT key pair for the web UI login
    └── users.json             the user you created (password stored as an Argon2id hash)
```

Workspace, sessions, memory and cron jobs are created in `data/` once the container runs.

A Docker instance differs from `--mode local` / `--mode user` as follows:

- **The web UI and its login are always on.** JWT is enabled and login is required, so guests
  can't use it. The port is published on `127.0.0.1` only.
- **The container settings are fixed:** the gateway binds `0.0.0.0:18790` inside the
  container. `--port` only changes the host side of the mapping.
- **API keys are not in `config.json`.** They are moved into `.env.docker`, and the config refers
  to them as `${ANTHROPIC_API_KEY}`, `${OPENROUTER_API_KEY}`, ... A settings change in the web UI
  keeps the placeholders.
- **Logs go to the container's stdout:** `docker compose logs -f`. Set the level with
  `RUST_LOG` (default `info`) in your shell or in a `.env` file next to `docker-compose.yml`.
- **Linux:** the compose file runs the container as your user (`user: "<uid>:<gid>"`), so it
  can write to the bind-mounted `data/` folder. Docker Desktop on Windows and macOS doesn't
  need this.

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--dir <DIR>` | `./rust-bot-docker` | Instance folder |
| `--name <NAME>` | `rust-bot` | Compose project and container name; lowercase letters, digits, `-`, `_` |
| `--port <PORT>` | `18790` | Host port of the web UI (on `127.0.0.1`) |
| `--image <IMAGE>` | `gilf/rust-bot:<version>` | Run another image, e.g. one with extra runtimes (see [Adding runtimes](#adding-runtimes)) |
| `--build <SOURCE_DIR>` | | Build the image from a rust-bot source checkout instead of pulling it |
| `--no-start` | | Only write the files; start later with `docker compose up -d` |

`--mode docker` cannot be combined with `--wizard`, `--workspace` or `--config`.

### Several instances

Give each instance its own folder, name and port:

```bash
rust-bot onboard --mode docker --dir ./bot-sales   --name bot-sales   --port 18791
rust-bot onboard --mode docker --dir ./bot-support --name bot-support --port 18792
```

The checks run **before** any question is asked. A container name that belongs to another
instance folder, or a host port already in use, stops the command with a hint to pass `--name`
or `--port`.

### Re-running: refresh, overwrite, upgrade

Running the command again on an existing folder asks:

- **`N` (refresh, the default):** keeps the config, keys, users and secrets. It rewrites the
  compose file and resets any setting the container depends on (bind address, port, JWT, login),
  saying so for each one. Then it pulls and runs `docker compose up -d`, which recreates the
  container only if the image or the compose file changed.
- **`y` (overwrite):** asks every question again and replaces the config and the key pair.
  Existing users stay in `users.json`.

The compose file pins the image to the version of the rust-bot that wrote it. To **upgrade**,
update rust-bot and refresh the instance:

```bash
rust-bot onboard --mode docker --dir ./rust-bot-docker      # answer N
```

### When Docker is not available

If `docker` is not installed, the daemon is not running (start Docker Desktop), or Compose v2
(`docker compose`) is missing, the command says so at the start. It still writes the files and
then prints the commands to start the instance by hand. The exit code is 0.

If the container does not come up, the command prints the last 50 lines of its log and exits with
code 1. Examples: a healthcheck that keeps failing, or a crash that makes the container restart.

## Manual setup from a source checkout

```bash
cp .env.docker.example .env.docker      # fill in OPENAI_API_KEY, OPENAI_API_MODEL, ...
docker compose up -d --build
# WebUI: http://127.0.0.1:18790
```

This uses the repository's own `docker-compose.yml` and builds the image locally. The rest of
this page explains that setup; most of it applies to a `--mode docker` instance as well.

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

The web UI channel also sets `channels.websocket.runtimeSurface` to `"native"` (both the image
template and `onboard --mode docker` do this). With `"browser"`, the gateway allows
configuration changes only from loopback clients. Inside the container, your browser's
connection arrives from the Docker bridge gateway (for example `172.17.0.1`), not from
`127.0.0.1`. The gateway would then treat you as remote and hide "+ Add MCP Server", the
per-server settings forms and the MCP and tool on/off switches. This is safe because the port
is published on the host's loopback only and these controls still need a signed-in admin. See
[Security](#security) before you publish the port any wider.

> **Upgrading an existing install:** the template only seeds `/data/config.json` on the first
> start. If the Settings page shows no "+ Add MCP Server" and the switches are locked, change
> `"runtimeSurface": "browser"` to `"native"` in the host folder's `config.json`, then restart
> the container (`docker compose restart`). For an instance created with `onboard --mode docker`,
> re-running it and answering `N` (refresh) makes this change for you.

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
- **`runtimeSurface: "native"` trusts every client that can reach the port.** The Docker
  config uses it because the port is published on `127.0.0.1` only (see
  [Choosing the host folder](#choosing-the-host-folder-behind-data)). If you publish the port on
  `0.0.0.0` or put it behind a reverse proxy, any signed-in admin who can reach it can change
  the configuration (add MCP servers, switch tools on and off). Setting it back to `"browser"`
  does not limit this to the host: in a container no client counts as loopback, so it turns
  these controls off for everyone.
- The container is the shell tool's sandbox. The template sets `tools.restrictToWorkspace`
  to `true`; mount only what the agent should be able to touch.
- Never copy `credentials/` or `.env*` files into an image: `.dockerignore` excludes them.

## Adding runtimes

The image is deliberately slim and has only `git` and `curl`. **MCP servers or skills that need
Node (`npx`) or Python (`uvx`, `python3`) will not run in it.** To add those runtimes, build your
own image:

```bash
docker build --build-arg EXTRA_PACKAGES="python3 nodejs npm" -t rust-bot-full .
```

You can also derive one from the published image:

```dockerfile
FROM gilf/rust-bot:0.4.5
USER root
RUN apt-get update && apt-get install -y --no-install-recommends python3 nodejs npm \
    && rm -rf /var/lib/apt/lists/*
USER rustbot
```

Then point an instance at it: `rust-bot onboard --mode docker --image rust-bot-full`. For an
existing instance, answer `N` to refresh it.
