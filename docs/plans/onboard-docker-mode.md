# Plan: `rust-bot onboard --mode docker` — create and start a containerised rust-bot instance

## Goal

One command takes a user from nothing to a running rust-bot gateway in Docker:

```
rust-bot onboard --mode docker
```

1. Run the onboard questions (provider, model, first web UI user). The web UI, JWT and login are always on.
2. Write a self-contained instance folder: `docker-compose.yml`, `.env.docker`, and `data/` holding
   `config.json`, `credentials/` and `users.json`.
3. If Docker is available, pull the image, start the container, wait until it is healthy, and print the URL.

The image is published to Docker Hub as `gilf/rust-bot` (amd64 + arm64) by a GitHub workflow.

## Status

All milestones are on one branch, `feature/docker-onboard`.

| Milestone | Status |
|---|---|
| M1. Publish the image | committed (`ba484ea`, `.github/workflows/docker-publish.yml`); `gilf/rust-bot:latest` already on Docker Hub (amd64 + arm64, pushed by Gil) |
| M2. Onboard refactor (no behaviour change) | built 2026-11-05, uncommitted; all lib tests pass; manual interactive `onboard` check pending |
| M3. `onboard --mode docker`: generate files | built 2026-11-05, uncommitted; all lib tests pass; verified end to end on Windows with `gilf/rust-bot:0.4.5` (healthy, chat works, web UI settings save keeps `${VAR}` placeholders). Until M4, it always prints the manual `docker compose up -d` steps |
| M4. `onboard --mode docker`: start the container | built 2026-11-05, uncommitted (`src/cli/docker_runtime.rs` + start flow in `onboard_docker.rs`); all lib tests pass; checked against real Docker Desktop: name clash, busy port, healthy start, crash loop (broken 0.4.4 image) |
| M5. Docs and smoke tests | not started |

M1 is independent of M2–M4. M3 builds on M2, and M4 on M3. Commit only when asked.

## Decisions (made with Gil, 2026-11-05)

- **A third onboard mode, `onboard --mode docker`**, next to `local` and `user`. It is not a top-level command.
  `OnboardMode` gains a `Docker` variant. The Docker-only flags live in a flattened `OnboardDockerOptions` group
  on `OnboardArgs`, shown under their own help heading.
- **Container paths are built in, not remapped (option a).** The config is built with `/data` as its instance
  root. Keypair and users-file creation get a separate *host* output directory. No step rewrites host paths into
  container paths afterwards.
- **Web UI, JWT and login are all forced on.** No "configure the web UI?", "enable JWT?" or "require login?"
  question: `websocket.jwt.enabled = true` and `websocket.requireAuth = true`. `gateway.host` and
  `websocket.host` are fixed to `0.0.0.0`, and the container port to `18790`. The user is asked only for the first
  user's email, password and role. The **host** port comes from `--port`.
- **Secrets are `${VAR}` placeholders in `config.json`, with the values in a generated `.env.docker`.** This
  matches `docs/DOCKER.md` and the existing `docker-compose.yml`. For now only provider API keys move; see "Later".
- **Plain relative bind mount `./data:/data`**, not a named volume with `driver_opts`. It is portable (no `C:\`
  vs `/run/desktop/mnt/host/c` issue), self-contained, and has no "volume options are fixed at creation" trap.
- **Linux uid:** on Linux the compose file gets `user: "<uid>:<gid>"` of the invoking user, so the container can
  write to the bind-mounted folder. Docker Desktop (Windows/macOS) does not need it.
- **Docker detection is three-level:** `docker` CLI on PATH → daemon reachable (`docker info`) → Compose v2
  (`docker compose version`). On any failure the files are still written and the manual commands printed.
- **Registry: Docker Hub, `gilf/rust-bot`, public.** Tags `<version>` (from `Cargo.toml`) and `latest`, for both
  `linux/amd64` (x86-64) and `linux/arm64` (aarch64). Gil adds the `DOCKERHUB_USERNAME` and `DOCKERHUB_TOKEN`
  repository secrets.
- **The generated compose file pins `gilf/rust-bot:<CARGO_PKG_VERSION>`**, so the CLI that wrote the config
  matches the binary that reads it. `--image` overrides the image, and `--build <src>` builds from a source
  checkout (bootstrapping before M1 ships, and for development).
- **Re-running over an existing instance** reuses onboard's overwrite/refresh prompt, then runs `pull` and `up -d`.
  Refresh rewrites the compose file with the current CLI version, so re-running a newer CLI also upgrades the instance.
- **Logs go to the container's stdout** (`docker compose logs`). `RUST_LOG` is set in the compose `environment:`
  section. No `.env` file or `logs/` folder is written for logging.
- **After `up -d`** the command waits for the container's healthcheck. On failure it prints `docker compose logs`
  output instead of leaving a container stuck restarting.
- **Instance folder defaults to `./rust-bot-docker`** (`--dir` overrides it). **`--name` defaults to `rust-bot`**
  and sets the compose project name and `container_name`, so several instances can coexist in different folders.
- **Workspace templates are not synced on the host.** The gateway already calls `sync_workspace_templates` on
  start (`src/cli/commands.rs`, gateway startup), inside the container, as the container user.

### Defaults I chose (not discussed; easy to change)

- `--mode docker` **conflicts with `--wizard`**. The wizard's channel and gateway menus would let the user undo the
  container invariants. Wizard support can come later, with those menus restricted.
- `--mode docker` **conflicts with `--workspace`**. The workspace is always `/data/workspace` inside the container.
  (`--config` already conflicts with every `--mode`.)
- The first web UI user is still asked for a role, with the same default as onboard.
- **Overwrite on an existing Docker instance** asks every question again and **replaces the keypair**
  (`WebUiOptions::replace_existing_keys`). Users already in `users.json` are kept; the new user is added or
  updated. Refresh changes only what the container needs, and prints a note for every value it had to reset.
- **The Docker checks run before the questions** (M4): a container name owned by another instance folder
  (compose `working_dir` label) or a busy host port stops the run with a hint (`--name` / `--port`) before any
  prompt. Docker being unusable (no CLI, daemon down, no Compose v2) only warns; the files are written and the
  manual steps printed.
- **A `restarting` container counts as crashed** and fails at once with the log tail, instead of waiting out the
  120 s health timeout.
- **The compose template's line endings are normalised to LF** when rendering, and `.gitattributes` keeps
  `docker/*.yml` LF (a Windows checkout with `core.autocrlf` had made it CRLF).
- Docker flags are `Option`s with the defaults applied in `resolve_docker_settings`, so `validate_onboard_args` can
  tell "given" from "default" without `ArgMatches`.
- The instance folder gets a `.gitignore` ignoring everything, as `onboard --mode local` does. `.env.docker` and
  `credentials/` hold secrets.

## Generated layout

```
<dir>/                         (default ./rust-bot-docker)
├── docker-compose.yml
├── .env.docker                secrets: <PROVIDER>_API_KEY, ...
├── .gitignore                 "*"
└── data/                      → /data in the container
    ├── config.json            all paths under /data/..., secrets as ${VAR}
    ├── credentials/
    │   ├── private_key.pem
    │   └── public_key.pem
    └── users.json             first web UI user (Argon2id hash + token)
```

Generated `docker-compose.yml` (rendered from a template, comments kept):

```yaml
name: <name>
services:
  rust-bot:
    image: gilf/rust-bot:<version>      # or build: <src> with --build
    container_name: <name>
    restart: unless-stopped
    env_file: .env.docker
    environment:
      RUST_LOG: ${RUST_LOG:-info}
    user: "<uid>:<gid>"                 # Linux only
    ports:
      - "127.0.0.1:<host_port>:18790"
    volumes:
      - ./data:/data
```

---

## M1. Publish the image to Docker Hub

New workflow `.github/workflows/docker-publish.yml`.

- **Trigger:** push to `deployment`, like the release workflows, plus `workflow_dispatch` for manual runs.
- **Version:** read `[package].version` from `Cargo.toml` with the same `awk` step as `release-linux.yml`.
- **Multi-arch without QEMU:** a matrix of native runners, `ubuntu-latest` (amd64) and `ubuntu-24.04-arm`
  (arm64, already used by `release-linux-aarch64.yml`). Each builds the existing `Dockerfile` with
  `docker/build-push-action`, pushes **by digest**, and uploads the digest as an artifact. A final `merge` job
  creates the multi-arch manifest with `docker buildx imagetools create` and tags it `gilf/rust-bot:<version>` and
  `gilf/rust-bot:latest`. Compiling trunk + cargo under QEMU would take hours; native runners avoid that.
- **Cache:** `cache-from/cache-to: type=gha,scope=<arch>`.
- **Skip if already published:** if `gilf/rust-bot:<version>` already exists
  (`docker buildx imagetools inspect`), skip the build so a re-push of the same version does not overwrite it.
- **Login:** `docker/login-action` with the `DOCKERHUB_USERNAME` (`gilf`) and `DOCKERHUB_TOKEN` (read/write access
  token) repository secrets.
- **Labels:** OCI labels (`org.opencontainers.image.version`, `.revision`, `.source`) via `docker/metadata-action`.

Verification: run the workflow once by hand, then `docker manifest inspect gilf/rust-bot:<version>` lists both
platforms, and `docker run --rm gilf/rust-bot:<version> --version` works on x86-64 and on an arm64 machine.

---

## M2. Onboard refactor (no behaviour change)

Make the onboard building blocks reusable by the Docker mode without copying them. `onboard`, `onboard --mode
local|user` and `onboard --wizard` behave exactly as today.

### 2.1 Instance layout: where files are written vs what the config records

New type (in `src/cli/onboard.rs`, or a small `src/cli/instance_layout.rs`):

```rust
/// Where an instance's files are written on this machine, and the root its config refers to them by.
/// For `local`/`user` both are the instance directory. For `docker` the host folder is `<dir>/data`
/// and the config root is `/data`.
pub struct InstanceLayout {
    pub host_dir: PathBuf,
    pub config_root: PathBuf,
}
```

with `host_path(relative)` and `config_path(relative)` helpers. Unit tests: identical roots behave like today;
`/data` root yields `/data/credentials/private_key.pem` while writing to `<host>/credentials/...`.

### 2.2 Functions that take the layout instead of a single `config_path`

| Function | Today | After |
|---|---|---|
| `Config::rebase_default_paths(instance_dir)` | one dir | called with `layout.config_root` |
| `run_generate_keypair_with_config(config, credentials_dir, force)` (`commands.rs`) | writes and records the same path | writes to `layout.host_path("credentials")`, records `layout.config_path(...)` |
| `create_users_file(config, config_path, websocket)` (`wizard.rs`) | users file next to config; **mints the token by reading `config.api.jwt.private_key_path`** | writes `layout.host_path("users.json")`, records the config path; mints with the **host** key path, because the config path `/data/...` does not exist on the host |
| `configure_websocket_channel(config, config_path, jwt_default)` | asks streaming, host, port, JWT, require-login | takes `WebUiOptions` (below) and the layout |

`path_for_config` (normalising separators) stays and is applied to config-side paths only. A `/data/...` path must
keep forward slashes on Windows: test this.

### 2.3 Options for the web UI questions

```rust
/// Which web UI answers are fixed by the caller instead of asked.
pub struct WebUiOptions {
    pub fixed_bind_host: Option<String>,  // docker: Some("0.0.0.0")
    pub fixed_port: Option<u16>,          // docker: Some(18790), the container side
    pub force_jwt: bool,                  // docker: true
    pub force_login: bool,                // docker: true (requireAuth, no question)
}
```

`WebUiOptions::interactive()` reproduces today's behaviour and is what `local`/`user` pass.

### 2.4 Split `run_onboard`

Extract the "new config" branch (`choose_providers` → `config_model` → `create_default_model_presets` → web UI) into
`build_new_instance_config(layout, web_ui_options, ...) -> Result<Config, CliError>`. Extract the existing-config
prompt (`confirm_overwrite` + refresh/overwrite) into a function returning an enum
`ExistingConfigChoice { Overwrite, Refresh }`. `run_onboard` keeps its side effects around them: `.gitignore`,
`create_env_file`, template sync, next steps.

### 2.5 Secrets out of the config

`configure_provider` writes the API key into `config.providers.<slot>.apiKey`. Add a pure function

```rust
/// Move every non-empty provider API key into the returned map as `<SLOT>_API_KEY`
/// and leave `${<SLOT>_API_KEY}` in its place.
pub fn extract_provider_secrets(config: &mut Config) -> BTreeMap<String, String>
```

It is used only by the Docker mode, but it lives next to the provider code. Unit-tested: key moved, placeholder
left, empty keys untouched, names stable (`OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, ...).

Tests: every existing `onboard` / `wizard` test still passes unchanged; new tests for `InstanceLayout`, the
host-vs-config split in keypair and users-file creation (using a temp dir), `WebUiOptions` forcing, and
`extract_provider_secrets`.

---

## M3. `onboard --mode docker`: generate the instance folder

### 3.1 CLI specification (`src/cli/command_specifications.rs`)

```rust
pub enum OnboardMode {
    Local,
    User,
    /// Create a Docker instance: `./rust-bot-docker/` (compose file, .env.docker, data/), then start it.
    Docker,
}

/// Arguments of `rust-bot onboard`.
pub struct OnboardArgs {
    #[arg(long, value_enum, conflicts_with = "config")] mode: Option<OnboardMode>,
    // workspace, config, wizard as today
    #[command(flatten, next_help_heading = "Docker (with --mode docker)")]
    pub docker: OnboardDockerOptions,
}

/// Flags that only apply to `onboard --mode docker`.
pub struct OnboardDockerOptions {
    /// Instance folder (compose file, .env.docker, data/)        default ./rust-bot-docker
    #[arg(long)] dir: Option<PathBuf>,
    /// Compose project and container name                         default rust-bot
    #[arg(long, default_value = "rust-bot")] name: String,
    /// Host port published on 127.0.0.1                           default 18790
    #[arg(long, default_value_t = 18790)] port: u16,
    /// Image to run instead of gilf/rust-bot:<version>
    #[arg(long, conflicts_with = "build")] image: Option<String>,
    /// Build the image from this rust-bot source checkout instead of pulling
    #[arg(long)] build: Option<PathBuf>,
    /// Write the files only; do not pull or start anything
    #[arg(long)] no_start: bool,
}
```

clap cannot express "this flag requires `--mode docker`" (only `requires = "mode"`, regardless of the value), so a
pure `validate_onboard_args(&OnboardArgs) -> Result<(), String>` rejects:
- a Docker flag given explicitly without `--mode docker`. Use `ArgMatches::value_source` so the defaults of `--name`/`--port` do
  not count; otherwise make `name`/`port` `Option`s with defaults applied in code.
- `--mode docker` with `--wizard` or `--workspace`.

Unit-tested with `Cli::try_parse_from` next to the existing onboard spec tests in `command_specifications.rs`.

`OnboardMode::instance_dir` gets the `Docker` arm (`current_dir/rust-bot-docker`, overridden by `--dir`), and
`resolve_onboard_targets` maps it to `<dir>/data/config.json`.

### 3.2 Flow (new module `src/cli/onboard_docker.rs`)

`run_onboard` branches early: `if args.mode == Some(OnboardMode::Docker) { return run_docker_onboard(args) }`.

1. Resolve `<dir>`. `layout = InstanceLayout { host_dir: <dir>/data, config_root: "/data" }`.
2. If `<dir>/data/config.json` exists, use the M2 overwrite/refresh prompt. Refresh keeps the config and secrets and
   regenerates only the compose file (with the current version tag).
3. Otherwise run `build_new_instance_config(layout, WebUiOptions { 0.0.0.0, 18790, force_jwt, force_login })`.
4. Apply the container invariants and assert them before saving: `gateway.host = 0.0.0.0`, `gateway.port = 18790`,
   websocket JWT enabled and `requireAuth = true`, `agents.workspace = /data/workspace`,
   `api.usersFile = /data/users.json`. One function, unit-tested.
5. `extract_provider_secrets` → write `.env.docker`. On refresh, merge: keep existing values, add new keys.
6. Save `data/config.json`.
7. Render `docker-compose.yml` from `docker/compose.template.yml` (`include_str!`, simple `{{placeholder}}`
   replacement). No YAML dependency, and the comments survive. Values: name, image or build context, host port,
   Linux `user:` line.
8. Write `<dir>/.gitignore` if missing (reuse `write_instance_gitignore`).
9. Skip `create_env_file` (`.env` + `logs/` in cwd) and the host template sync. See decisions.
10. If `--no-start`: print the next steps (`cd <dir> && docker compose up -d`) and stop.

Linux uid/gid: read with `libc::getuid/getgid` behind `#[cfg(target_os = "linux")]` (or `id -u`/`id -g` if `libc`
is not already a dependency). The pure renderer takes `Option<(u32, u32)>` so it can be tested on every platform.

Tests: argument validation, compose rendering (image vs build, with and without `user:`, port), the
container-invariant function, `.env.docker` writing and merging, a config generated under `/data` contains no host
path (scan every string field for the temp-dir prefix), and a re-run on an existing folder in refresh mode.

---

## M4. `onboard --mode docker`: start the container

Behind a small trait so the orchestration is testable without Docker:

```rust
/// The Docker operations `onboard --mode docker` needs; the real one shells out to `docker`.
trait DockerRuntime {
    fn availability(&self) -> DockerAvailability;     // NoCli | DaemonDown | NoComposeV2 | Ready
    fn host_port_free(&self, port: u16) -> bool;       // TcpListener::bind on 127.0.0.1
    fn pull(&self, dir: &Path) -> Result<(), String>;  // docker compose pull   (or build with --build)
    fn up(&self, dir: &Path) -> Result<(), String>;    // docker compose up -d
    fn health(&self, container: &str) -> HealthState;  // docker inspect --format {{.State.Health.Status}}
    fn logs_tail(&self, dir: &Path, lines: usize) -> String;
}
```

Flow after M3:

1. `availability()`. Anything but `Ready` prints a specific message and the manual commands, then exits 0 (the files
   are written). Examples: "Docker is installed but not running, start Docker Desktop", "install the Compose v2 plugin".
2. Check the host port before `up`. If it is taken and the container is not already this instance's, stop with a
   message naming the port and `--port`.
3. `pull` (or `build`) streams its output to the terminal.
4. `up -d`.
5. Poll `health` every 2 s for up to 120 s. The image's `start-period` is 20 s, and `check-config` runs first.
   - `healthy` → print `http://127.0.0.1:<port>`, the login email, and `docker compose logs -f` / `down` hints.
   - `unhealthy`, exited, or timeout → print the last 50 log lines and point at
     `docker compose run --rm rust-bot check-config`.

Tests: a fake `DockerRuntime` drives each branch (no CLI, daemon down, port taken, pull fails, healthy,
unhealthy with logs).

---

## M5. Docs and smoke tests

- `docs/DOCKER.md`: new "Quick start with `rust-bot onboard --mode docker`" section at the top, keeping the manual
  compose setup below it. `docs/COMMAND_LINE.md`: the new mode and its flags.
- Note in `DOCKER.md`: the published image is the slim one (`git`, `curl`). **MCP servers or skills that need
  Node or Python will not run in it.** Use `--build <src>` with `EXTRA_PACKAGES`, or a derived image. A
  published `-full` variant is listed under "Later".
- The repo's own `docker-compose.yml` (build from source, `.env.docker` in the repo) stays as the developer
  setup. Its comment points at `onboard --mode docker` for end users.
- Manual smoke tests: Windows + Docker Desktop, Linux (native, non-1000 uid), and arm64 (Raspberry Pi / Apple
  Silicon). Each covers a fresh instance, a re-run with refresh, `--no-start`, Docker not running, a port already
  in use, and logging in to the web UI with the created user (and confirming guest use is refused).

## Verification (per milestone)

- `cargo build`, `cargo clippy --all-targets`, `cargo test --lib cli::` and the existing onboard/wizard tests.
- M2: `rust-bot onboard` on a fresh folder produces the same `config.json` as before (diff against a run from `main`).
- M3: `rust-bot onboard --mode docker --no-start` produces the layout above; `grep` the generated config for the
  host path → no hits. `rust-bot onboard --help` shows the Docker heading.
- M4: a real run against Docker Desktop reaches `healthy` and the web UI login works with the created user.

## Later (out of scope)

- `--mode docker` with `--wizard`, with the channel and gateway menus restricted to container-safe answers.
- A published `gilf/rust-bot:<version>-full` image with Python and Node for MCP servers.
- Moving secrets out of `extraHeaders` and channel configs (email passwords, tokens) into `.env.docker` as well.
- Exposing beyond loopback: an optional reverse-proxy (Caddy) service in the generated compose file.
