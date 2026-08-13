# websockets-chat

A Leptos (Rust + WASM) chat UI for `rust-bot`'s WebSocket gateway
(`rust-bot gateway`, `channels.extra["websocket"]`). Unlike `web-chat`
(which talks to the REST-only `rust-bot api` server and has no dependency
on the rest of this repository), this app connects directly to the
gateway's WebSocket channel and renders token-by-token streaming replies,
live tool-activity chips, and reasoning panels as they arrive.

It shares its login form, message composer, markdown rendering, and
message-bubble styling with `web-chat` via the `chat-ui` crate; only the
WebSocket transport, wire protocol, and streaming-aware chat shell are
specific to this app.

## Prerequisites

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk
```

Tailwind CSS is fetched automatically by Trunk on first build (via the
`data-trunk rel="tailwind-css"` link in `index.html`) — no Node.js/npm
required.

## Mint a usable token

The gateway validates a token's `aud` claim against its own WebSocket
channel path, not the REST API's `aud` — `generate-jwt-token --purpose
webui` resolves that automatically, so no `--aud` flag is needed:

```bash
cargo run --bin generate-jwt -- generate-jwt-token \
  --config ./configs/simple1/config.json \
  --purpose webui \
  --user-email you@example.com \
  --password your-password \
  --users-file ./configs/simple1/users.json
```

This also writes a `channels.extra.websocket` entry into the config file
(if one doesn't already exist), so the gateway and the minted token agree
on the WebSocket path/audience out of the box.

## Develop

In one terminal, run the gateway with the websocket channel enabled (the
config from the step above already has it):

```bash
cargo run -- gateway --config ./configs/simple1/config.json
```

By default the gateway listens on `127.0.0.1:18790`. Its WebSocket path
must be set to `/ws` (matching `app.rs::GATEWAY_WS_PATH`) — **not** the
server-side default of `/` — because the gateway registers the WebSocket
upgrade handler as a literal route at whatever path you configure, and that
takes priority over the static-file fallback that serves this app's own
`index.html` when `gateway.webRoot` points at this app's build (see "Build
for production" below). With path `/`, opening the gateway's URL in a
browser always hits the upgrade handler instead of the SPA and fails with
"Connection header did not include 'upgrade'". Set in your config:

```json
"channels": {
  "websocket": {
    "path": "/ws",
    "jwt": { "aud": "/ws" }
  }
}
```

(`jwt.aud` must always equal `path` exactly — the gateway validates that at
startup. `generate-jwt-token --purpose webui`, per the step above, already
resolves its own `--aud` from whatever `path` your config has.)

In another terminal, serve the UI with live reload:

```bash
cd websockets-chat
trunk serve --open
```

This starts Trunk on `http://127.0.0.1:8902/`. `Trunk.toml` only proxies
the `/v1/login` POST to the gateway (port 18790) — the WebSocket connection
itself is **not** proxied through Trunk's dev server; it connects directly
to `ws://127.0.0.1:18790/ws` from the browser. Cross-origin `ws://` isn't
subject to CORS preflight, and the gateway's CORS layer defaults to
allow-any-origin, so this works without extra configuration.

If your gateway listens somewhere other than `127.0.0.1:18790`, override the
WebSocket base at runtime with the `wsBase` query parameter instead of
editing code:

```
http://127.0.0.1:8902/?wsBase=ws://127.0.0.1:18790
```

## Build for production

```bash
cd websockets-chat
trunk build --release
```

Output (`index.html`, `*.js`, `*.wasm`, `*.css`) is written to
`websockets-chat/dist/`. Point the gateway at it:

```bash
cargo run -- gateway --config ./configs/simple1/config.json --web-root ./websockets-chat/dist
```

Then open `http://<host>:<port>/` — login and the WebSocket connection are
both same-origin, so `app.rs` derives everything it needs from
`window.location` with no `?wsBase=` override required. `--web-root` (or the
equivalent `gateway.webRoot` config field, which otherwise defaults to `./web`
and won't point at this app's build) must be set for `/` to serve anything at
all — and `channels.websocket.path`/`jwt.aud` must both be `/ws` per the
"Develop" section above, or `/` will hit the WebSocket upgrade handler
instead of the SPA, exactly as described there.
