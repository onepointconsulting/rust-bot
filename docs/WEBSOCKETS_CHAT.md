# Websockets chat UI

`websockets-chat/` is the browser UI for `rust-bot`'s WebSocket gateway. It is a Leptos (Rust + WASM) single-page app styled with Tailwind, and it renders token-by-token streaming replies, live tool-activity chips and reasoning panels as they arrive. It shares its login form, message composer and markdown rendering with the older [`web-chat`](../web-chat/README.md) UI through the `chat-ui` crate.

For the app's own details (optional login, guest session isolation, minting tokens, dev-server proxying) see [websockets-chat/README.md](../websockets-chat/README.md). This page covers how it is built, how it ends up inside the executable, and how to run it.

## Embedded in the executable

The compiled UI is part of the `rust-bot` binary:

1. `trunk build --release` writes the bundle (`index.html`, `*.js`, `*.wasm`, `*.css`) to `websockets-chat/dist/`.
2. At `cargo build` time, `build.rs` copies `websockets-chat/dist/` into the build output directory, and `rust-embed` compiles those files into the binary (`src/utils/embedded_gateway_ui.rs`).
3. When you run `./rust-bot gateway`, the gateway serves the UI at `/` straight from the binary. There is nothing to unpack, copy or deploy next to the executable: the files are served from memory, and unknown paths fall back to `index.html` so the single-page app routes correctly.

This means the Trunk build must happen **before** the Cargo build, otherwise the binary is built without the UI. If `websockets-chat/dist/` is missing, `build.rs` embeds a stub page instead (so `cargo build` and `cargo test` still work), and the gateway then serves no web UI. Release archives are built with the UI included.

Which UI the gateway serves is decided in this order:

1. `--web-root <dir>` on the command line.
2. `gateway.webRoot` in the config file.
3. The UI embedded in the binary.

A web root is only used if it is a directory containing `index.html`; otherwise the gateway logs a warning and falls back to the embedded UI.

## Prerequisites

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk --locked
```

Tailwind CSS is downloaded by Trunk on the first build, so Node.js/npm is not required.

## Build

Build the UI and then the binary, in that order:

```bash
cd websockets-chat
trunk build --release
cd ..
cargo build -r
```

Or use the helper script, which runs Trunk for `web-chat` and `websockets-chat` and then `cargo build -r`:

```bash
./scripts/build_all.sh            # Linux / macOS
.\scripts\build_all.bat --release # Windows
```

## Run

### Gateway config

The gateway needs a `websocket` channel whose path is `/ws`, not the default `/`. A WebSocket handler registered at `/` would take priority over the page that serves the UI, and opening the gateway URL in a browser would fail with "Connection header did not include 'upgrade'". `jwt.aud` must equal `path`:

```json
"channels": {
  "websocket": {
    "path": "/ws",
    "jwt": { "aud": "/ws" }
  }
}
```

By default the gateway requires login. Mint a token and register a user with `generate-jwt-token --purpose webui` (see [Command line](COMMAND_LINE.md#jwt-keypair-and-tokens)); this also writes the matching `channels.extra.websocket` entry into your config. To allow guests, set `"requireAuth": false`. Both options are described in [websockets-chat/README.md](../websockets-chat/README.md).

### Using the embedded UI

```bash
./rust-bot gateway --config ./.rust-bot/config.json
```

Then open `http://127.0.0.1:18790/` (the default gateway address; override with `--port` or `gateway.port`). Login and the WebSocket connection are both same-origin, so no further setup is needed.

To serve a freshly built bundle without recompiling `rust-bot`:

```bash
./rust-bot gateway --config ./.rust-bot/config.json --web-root ./websockets-chat/dist
```

### Development with live reload

Run the gateway in one terminal, and the Trunk dev server in another:

```bash
cargo run -- gateway --config ./configs/simple1/config.json
```

```bash
cd websockets-chat
trunk serve --open
```

Trunk serves the UI on `http://127.0.0.1:8902/` and proxies only the `/v1/login` request to the gateway. The browser opens the WebSocket directly to `ws://127.0.0.1:18790/ws`. If your gateway listens elsewhere, pass the address in the URL:

```
http://127.0.0.1:8902/?wsBase=ws://127.0.0.1:18790
```
