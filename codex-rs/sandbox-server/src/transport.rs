use std::fmt;
use std::sync::Arc;

use codex_app_server_protocol::JSONRPCMessage;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::server::Server;

mod stdio;
mod uds;

pub(crate) use stdio::run_stdio;
pub(crate) use uds::start_uds;

pub(crate) const CHANNEL_CAPACITY: usize = 128;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConnectionId(pub(crate) u64);

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone)]
pub(crate) struct ConnectionWriter {
    tx: mpsc::Sender<JSONRPCMessage>,
}

impl ConnectionWriter {
    pub(crate) fn new(tx: mpsc::Sender<JSONRPCMessage>) -> Self {
        Self { tx }
    }

    pub(crate) async fn send(&self, message: JSONRPCMessage) {
        let _ = self.tx.send(message).await;
    }
}

async fn handle_incoming_json(
    server: &Arc<Server>,
    connection_id: ConnectionId,
    writer: &ConnectionWriter,
    connection_cancellation: &CancellationToken,
    initialized: &mut bool,
    payload: &str,
) {
    match serde_json::from_str::<JSONRPCMessage>(payload) {
        Ok(message) => {
            server
                .handle_message(
                    connection_id,
                    writer,
                    connection_cancellation,
                    initialized,
                    message,
                )
                .await;
        }
        Err(err) => tracing::warn!(%connection_id, "invalid JSON-RPC message: {err}"),
    }
}
