//! The hook's view of the ACP connection.
//!
//! The agent loop (and the hooks inside it) is built before the client has
//! connected, so the hook cannot hold the connection directly. It holds an
//! [`AcpLink`] instead; the real implementation is a [`ConnectionSlot`] that is
//! bound once the first request arrives, and tests substitute a fake.

use std::sync::OnceLock;

use agent_client_protocol::schema::v1::{
    RequestPermissionRequest, RequestPermissionResponse, SessionNotification, SessionUpdate,
};
use agent_client_protocol::{Client, ConnectionTo};
use async_trait::async_trait;

/// What the hook needs from the client connection.
#[async_trait]
pub trait AcpLink: Send + Sync {
    /// Send one `session/update` notification for `session_id`.
    async fn send_update(&self, session_id: &str, update: SessionUpdate) -> Result<(), String>;

    /// Send `session/request_permission` and wait for the client's answer.
    async fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, String>;
}

/// Late-bound holder for the connection to the client.
#[derive(Default)]
pub struct ConnectionSlot {
    connection: OnceLock<ConnectionTo<Client>>,
}

impl ConnectionSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember the connection. Only the first call has an effect.
    pub fn bind(&self, connection: &ConnectionTo<Client>) {
        let _ = self.connection.set(connection.clone());
    }

    fn connection(&self) -> Result<&ConnectionTo<Client>, String> {
        self.connection
            .get()
            .ok_or_else(|| "ACP client connection is not established yet".to_string())
    }
}

#[async_trait]
impl AcpLink for ConnectionSlot {
    async fn send_update(&self, session_id: &str, update: SessionUpdate) -> Result<(), String> {
        self.connection()?
            .send_notification(SessionNotification::new(session_id.to_string(), update))
            .map_err(|error| error.to_string())
    }

    async fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, String> {
        // Safe to block here: hooks run inside a spawned turn, never inside
        // the connection's dispatch loop.
        self.connection()?
            .send_request(request)
            .block_task()
            .await
            .map_err(|error| error.to_string())
    }
}
