# syntax=docker/dockerfile:1.7
#
# rust-bot gateway image. See docs/DOCKER.md for how configuration and secrets are passed.
#
#   docker build -t rust-bot .
#   docker compose up -d

# ── build stage ────────────────────────────────────────────────────────────────
# The toolchain version, the clippy/rustfmt components and the wasm32 target all come from
# rust-toolchain.toml, which rustup installs on first use of cargo.
FROM rust:1-bookworm AS build

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev cmake \
    && rm -rf /var/lib/apt/lists/*

# Own layer: trunk takes minutes to compile and changes rarely.
RUN cargo install trunk --locked

WORKDIR /src
COPY . .

# The gateway UI is compiled into the binary (build.rs embeds websockets-chat/dist),
# so the WASM bundle must exist before the binary is built.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    (cd websockets-chat && trunk build --release) \
    && cargo build --release --bin rust-bot \
    && cp target/release/rust-bot /usr/local/bin/rust-bot

# ── runtime stage ──────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# git and curl are used by the agent's shell tool; add the runtimes your MCP servers and
# skills need (python3, nodejs, ...) in a derived image or with EXTRA_PACKAGES.
ARG EXTRA_PACKAGES=""
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl git tini ${EXTRA_PACKAGES} \
    && rm -rf /var/lib/apt/lists/*

# Non-root: the shell tool runs commands as this user, so keep it unprivileged.
RUN useradd --create-home --uid 1000 rustbot \
    && mkdir -p /data \
    && chown rustbot:rustbot /data

COPY --from=build /usr/local/bin/rust-bot /usr/local/bin/rust-bot
COPY docker/config.template.json /usr/share/rust-bot/config.template.json
COPY docker/entrypoint.sh /usr/local/bin/docker-entrypoint.sh

# One volume holds the config and all runtime state (sessions, cron, logs, workspace).
ENV RUST_BOT_CONFIG=/data/config.json \
    HOME=/home/rustbot
VOLUME /data
WORKDIR /data
USER rustbot

EXPOSE 18790
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:18790/v1/auth/config >/dev/null || exit 1

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/docker-entrypoint.sh"]
CMD ["gateway"]
