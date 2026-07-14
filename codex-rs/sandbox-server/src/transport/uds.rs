use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use codex_app_server_protocol::JSONRPCMessage;
use futures::SinkExt;
use futures::StreamExt;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use super::CHANNEL_CAPACITY;
use super::ConnectionId;
use super::ConnectionWriter;
use super::handle_incoming_json;
use crate::server::Server;

const SOCKET_MODE: u32 = 0o600;

pub(crate) fn start_uds(
    server: Arc<Server>,
    socket_path: PathBuf,
    shutdown: CancellationToken,
) -> io::Result<JoinHandle<()>> {
    if socket_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("UDS path already exists: {}", socket_path.display()),
        ));
    }
    let listener = UnixListener::bind(&socket_path)?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(SOCKET_MODE))?;
    let metadata = fs::metadata(&socket_path)?;
    let socket_guard = SocketFileGuard {
        path: socket_path,
        dev: metadata.dev(),
        ino: metadata.ino(),
    };

    Ok(tokio::spawn(async move {
        let _socket_guard = socket_guard;
        let mut connections = JoinSet::new();
        loop {
            let accepted = tokio::select! {
                _ = shutdown.cancelled() => break,
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(err) = result {
                        tracing::warn!("UDS connection task failed: {err}");
                    }
                    continue;
                }
                accepted = listener.accept() => accepted,
            };
            let (stream, _) = match accepted {
                Ok(accepted) => accepted,
                Err(err) => {
                    tracing::warn!("UDS accept failed: {err}");
                    continue;
                }
            };
            if !peer_has_same_uid(&stream) {
                tracing::warn!("rejecting UDS peer with a different UID");
                continue;
            }
            let connection_id = server.next_connection_id();
            let server = Arc::clone(&server);
            let connection_shutdown = shutdown.child_token();
            connections.spawn(async move {
                run_connection(server, connection_id, stream, connection_shutdown).await;
            });
        }
        while let Some(result) = connections.join_next().await {
            if let Err(err) = result {
                tracing::warn!("UDS connection task failed during shutdown: {err}");
            }
        }
    }))
}

fn peer_has_same_uid(stream: &UnixStream) -> bool {
    let Ok(credential) = stream.peer_cred() else {
        return false;
    };
    credential.uid() == unsafe { libc::geteuid() }
}

async fn run_connection(
    server: Arc<Server>,
    connection_id: ConnectionId,
    stream: UnixStream,
    shutdown: CancellationToken,
) {
    let websocket = match accept_async(stream).await {
        Ok(websocket) => websocket,
        Err(err) => {
            tracing::warn!(%connection_id, "UDS WebSocket handshake failed: {err}");
            return;
        }
    };
    let (mut sink, mut stream) = websocket.split();
    let (writer_tx, mut writer_rx) = mpsc::channel::<JSONRPCMessage>(CHANNEL_CAPACITY);
    let writer = ConnectionWriter::new(writer_tx);
    let writer_shutdown = shutdown.child_token();
    let writer_handle = tokio::spawn(async move {
        loop {
            let message = tokio::select! {
                _ = writer_shutdown.cancelled() => break,
                message = writer_rx.recv() => match message {
                    Some(message) => message,
                    None => break,
                },
            };
            let json = match serde_json::to_string(&message) {
                Ok(json) => json,
                Err(err) => {
                    tracing::error!("failed to serialize JSON-RPC message: {err}");
                    continue;
                }
            };
            if sink.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
    });

    let mut initialized = false;
    loop {
        let message = tokio::select! {
            _ = shutdown.cancelled() => break,
            message = stream.next() => message,
        };
        let Some(message) = message else {
            break;
        };
        match message {
            Ok(Message::Text(text)) => {
                handle_incoming_json(
                    &server,
                    connection_id,
                    &writer,
                    &mut initialized,
                    text.as_ref(),
                )
                .await;
            }
            Ok(Message::Binary(bytes)) => match std::str::from_utf8(&bytes) {
                Ok(text) => {
                    handle_incoming_json(&server, connection_id, &writer, &mut initialized, text)
                        .await;
                }
                Err(err) => tracing::warn!(%connection_id, "invalid UTF-8 WebSocket frame: {err}"),
            },
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
            Err(err) => {
                tracing::warn!(%connection_id, "UDS WebSocket read failed: {err}");
                break;
            }
        }
    }

    server.connection_closed(connection_id).await;
    shutdown.cancel();
    writer_handle.abort();
}

struct SocketFileGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Drop for SocketFileGuard {
    fn drop(&mut self) {
        let should_remove = fs::metadata(&self.path)
            .map(|metadata| metadata.dev() == self.dev && metadata.ino() == self.ino)
            .unwrap_or(false);
        if should_remove {
            let _ = fs::remove_file(&self.path);
        }
    }
}
