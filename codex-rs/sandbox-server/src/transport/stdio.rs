use std::io;
use std::sync::Arc;

use codex_app_server_protocol::JSONRPCMessage;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::CHANNEL_CAPACITY;
use super::ConnectionId;
use super::ConnectionWriter;
use super::handle_incoming_json;
use crate::server::Server;

pub(crate) async fn run_stdio(server: Arc<Server>, shutdown: CancellationToken) -> io::Result<()> {
    let connection_id = ConnectionId(0);
    let connection_cancellation = CancellationToken::new();
    let (writer_tx, mut writer_rx) = mpsc::channel::<JSONRPCMessage>(CHANNEL_CAPACITY);
    let writer = ConnectionWriter::new(writer_tx);
    let writer_shutdown = shutdown.child_token();
    let writer_handle = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        loop {
            let message = tokio::select! {
                _ = writer_shutdown.cancelled() => break,
                message = writer_rx.recv() => match message {
                    Some(message) => message,
                    None => break,
                },
            };
            let mut json = match serde_json::to_vec(&message) {
                Ok(json) => json,
                Err(err) => {
                    tracing::error!("failed to serialize JSON-RPC message: {err}");
                    continue;
                }
            };
            json.push(b'\n');
            if stdout.write_all(&json).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut initialized = false;
    loop {
        let line = tokio::select! {
            _ = shutdown.cancelled() => break,
            line = lines.next_line() => line?,
        };
        let Some(line) = line else {
            break;
        };
        handle_incoming_json(
            &server,
            connection_id,
            &writer,
            &connection_cancellation,
            &mut initialized,
            &line,
        )
        .await;
    }

    connection_cancellation.cancel();
    server.connection_closed(connection_id).await;
    shutdown.cancel();
    writer_handle.abort();
    Ok(())
}
