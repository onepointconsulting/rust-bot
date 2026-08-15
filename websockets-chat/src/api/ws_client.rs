//! Thin WebSocket transport wrapper around `gloo_net`'s futures-based
//! `WebSocket`, split into a send half ([`WsSender`]) and a receive half
//! ([`WsReceiver`]).
//!
//! Deliberately kept free of business logic: JSON parsing lives in
//! [`crate::protocol`], turn/entry bookkeeping in [`crate::state`]. This
//! module's only job is opening the socket, handing back the sink/stream
//! halves, and pumping received frames through
//! [`crate::protocol::parse_server_event`]. Because it's pure I/O plumbing
//! over wasm-only APIs (`gloo_net`'s browser `WebSocket` binding), it isn't
//! unit-tested directly — there's nothing left in it to test once the
//! logic it delegates to is covered by `protocol.rs`/`state.rs`'s host-target
//! tests.

use chat_ui::api::ApiError;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use gloo_net::websocket::futures::WebSocket;
use gloo_net::websocket::Message;
use leptos::task::spawn_local;

use crate::protocol::parse_server_event;
use crate::protocol::ServerEvent;

/// Send half of an open gateway connection.
///
/// Wraps `gloo_net`'s `SplitSink` so callers depend on this crate's types
/// (and [`chat_ui::api::ApiError`]) instead of reaching into `gloo_net`
/// directly at every call site.
pub struct WsSender(SplitSink<WebSocket, Message>);

impl WsSender {
    /// Send a single text frame — in practice, a serialized
    /// [`crate::protocol::ClientEnvelope`].
    pub async fn send_text(&mut self, text: String) -> Result<(), ApiError> {
        self.0
            .send(Message::Text(text))
            .await
            .map_err(|err| ApiError::new(err.to_string()))
    }

    /// Gracefully close the underlying WebSocket send half, initiating the
    /// close handshake.
    ///
    /// Used by `app.rs` for *intentional* teardown (logout): closing the
    /// sink causes the paired receive half's stream to end promptly, which
    /// fires [`spawn_receive_loop`]'s `on_close` callback so the app can
    /// react without waiting on the browser's own idle-connection timeout.
    pub async fn close(&mut self) -> Result<(), ApiError> {
        self.0
            .close()
            .await
            .map_err(|err| ApiError::new(err.to_string()))
    }
}

/// Receive half of an open gateway connection, as handed to
/// [`spawn_receive_loop`].
pub type WsReceiver = SplitStream<WebSocket>;

/// Open a WebSocket connection to `url` (as built by
/// [`crate::state::build_ws_url`]) and split it into a send/receive pair.
pub fn connect(url: &str) -> Result<(WsSender, WsReceiver), ApiError> {
    let ws = WebSocket::open(url).map_err(|err| ApiError::new(err.to_string()))?;
    let (sink, stream) = ws.split();
    Ok((WsSender(sink), stream))
}

/// Spawn a background task that polls `stream` for frames, parses each text
/// frame via [`parse_server_event`], and invokes `on_event` with the result.
///
/// Non-text frames and frames that fail to parse are logged and skipped
/// rather than propagated: a single malformed or unexpected frame from the
/// gateway shouldn't take down the whole receive loop for the rest of the
/// session. The loop ends (and logs as much) once the stream itself ends,
/// i.e. the connection closed — at which point `on_close` fires exactly
/// once, letting the app layer drive reconnect/backoff logic
/// (`app.rs::handle_connection_closed`) without this module knowing
/// anything about that policy.
pub fn spawn_receive_loop(
    mut stream: WsReceiver,
    on_event: impl Fn(ServerEvent) + 'static,
    on_close: impl FnOnce() + 'static,
) {
    spawn_local(async move {
        while let Some(frame) = stream.next().await {
            match frame {
                Ok(Message::Text(text)) => match parse_server_event(&text) {
                    Ok(event) => on_event(event),
                    Err(err) => {
                        log::warn!("failed to parse gateway event ({err}); raw frame: {text}");
                    }
                },
                Ok(Message::Bytes(_)) => {
                    log::warn!("ignoring unexpected binary WebSocket frame from gateway");
                }
                Err(err) => {
                    log::warn!("WebSocket stream error: {err}");
                }
            }
        }
        log::info!("WebSocket receive loop ended: connection closed");
        on_close();
    });
}
