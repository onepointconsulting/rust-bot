# web-chat

A small Leptos (Rust + WASM) chat UI for the `rust-bot` REST API
(`/v1/login`, `/v1/chat/completions`, `/v1/chat/commands`). It has no
dependency on the rest of this repository's Rust code — it only talks to
the bot over the documented OpenAPI surface (see `/api-docs/openapi.json`
on a running `rust-bot api` server).

## Prerequisites

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk
```

Tailwind CSS is fetched automatically by Trunk on first build (via the
`data-trunk rel="tailwind-css"` link in `index.html`) — no Node.js/npm
required.

## Develop

In one terminal, run the bot's API server:

```bash
cargo run -- api --config ./configs/simple1/config.json
```

In another, serve the UI with live reload (proxies `/v1`, `/health`,
`/swagger-ui`, `/api-docs` to `http://127.0.0.1:8900`, see `Trunk.toml`):

```bash
cd web-chat
trunk serve --open
```

## Build for production

```bash
cd web-chat
trunk build --release
```

Output (`index.html`, `*.js`, `*.wasm`, `*.css`) is written to
`web-chat/dist/`. Point `rust-bot api` at it:

```bash
rust-bot api --config ./config.json --web-root ./web-chat/dist
```

Then open `http://<host>:<port>/` — the API and UI are served from the
same origin, alongside the existing `/v1/*`, `/health`, and `/swagger-ui`
routes.
