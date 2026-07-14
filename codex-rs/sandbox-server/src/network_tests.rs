use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_network_proxy::NetworkDecision;
use codex_network_proxy::NetworkMode;
use codex_network_proxy::NetworkPolicyRequest;
use codex_network_proxy::NetworkProtocol;
use codex_network_proxy::NetworkProxyConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::NetworkApprovalBroker;
use super::NetworkApprovalContext;
use crate::transport::ConnectionId;
use crate::transport::ConnectionWriter;

#[tokio::test]
async fn accept_for_session_continues_and_caches_the_request() -> anyhow::Result<()> {
    let broker = NetworkApprovalBroker::default();
    let (tx, mut rx) = mpsc::channel(4);
    let context = approval_context(ConnectionWriter::new(tx));
    let request = policy_request();
    let decision = tokio::spawn({
        let broker = broker.clone();
        let context = context.clone();
        let request = request.clone();
        async move { broker.decide(context, request).await }
    });

    let approval = rx.recv().await.expect("network approval request");
    assert_eq!(
        serde_json::to_value(&approval)?,
        json!({
            "id": "network-approval:1",
            "method": "command/exec/requestNetworkApproval",
            "params": {
                "processId": "build-1",
                "host": "example.com",
                "port": 443,
                "protocol": "https",
                "command": ["cargo", "build"],
                "cwd": "/workspace"
            }
        })
    );
    broker.handle_response(
        ConnectionId(7),
        JSONRPCResponse {
            id: RequestId::String("network-approval:1".to_string()),
            result: serde_json::to_value(CommandExecutionRequestApprovalResponse {
                decision: CommandExecutionApprovalDecision::AcceptForSession,
            })?,
        },
    );
    assert_eq!(decision.await?, NetworkDecision::Allow);

    assert_eq!(
        broker.decide(context, request).await,
        NetworkDecision::Allow
    );
    assert!(rx.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn execution_cancellation_denies_pending_approval() -> anyhow::Result<()> {
    let broker = NetworkApprovalBroker::default();
    let (tx, mut rx) = mpsc::channel(4);
    let context = approval_context(ConnectionWriter::new(tx));
    let cancellation = context.cancellation.clone();
    let decision = tokio::spawn({
        let broker = broker.clone();
        async move { broker.decide(context, policy_request()).await }
    });

    let _approval = rx.recv().await.expect("network approval request");
    cancellation.cancel();
    let decision = tokio::time::timeout(Duration::from_secs(1), decision).await??;
    assert!(matches!(decision, NetworkDecision::Deny { .. }));
    assert!(
        broker
            .inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn proxy_holds_the_original_request_until_approval() -> anyhow::Result<()> {
    let target = TcpListener::bind("127.0.0.1:0").await?;
    let target_addr = target.local_addr()?;
    let (accepted_tx, mut accepted_rx) = mpsc::channel(2);
    let target_task = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = target.accept().await?;
            accepted_tx.send(()).await?;
            let mut request = vec![0; 4096];
            let _ = socket.read(&mut request).await?;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nproxy-ok",
                )
                .await?;
        }
        anyhow::Ok(())
    });

    let broker = NetworkApprovalBroker::default();
    let (tx, mut rx) = mpsc::channel(4);
    let context = approval_context(ConnectionWriter::new(tx));
    let config = NetworkProxyConfig {
        enabled: true,
        enable_socks5: false,
        enable_socks5_udp: false,
        allow_upstream_proxy: false,
        mode: NetworkMode::Full,
        allow_local_binding: true,
        ..NetworkProxyConfig::default()
    };
    let (proxy, handle) = broker
        .start_proxy(&config, context)
        .await
        .map_err(|err| anyhow::anyhow!(err.message))?;
    let mut env = HashMap::new();
    proxy.apply_to_env(&mut env);
    let proxy_addr = env["HTTP_PROXY"]
        .strip_prefix("http://")
        .expect("HTTP proxy URL")
        .trim_end_matches('/')
        .parse::<SocketAddr>()?;

    let first_request = tokio::spawn(send_http_request(proxy_addr, target_addr));
    let approval = rx.recv().await.expect("network approval request");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), accepted_rx.recv())
            .await
            .is_err()
    );
    let request_id = match approval {
        JSONRPCMessage::Request(request) => request.id,
        message => anyhow::bail!("expected approval request, got {message:?}"),
    };
    broker.handle_response(
        ConnectionId(7),
        JSONRPCResponse {
            id: request_id,
            result: serde_json::to_value(CommandExecutionRequestApprovalResponse {
                decision: CommandExecutionApprovalDecision::AcceptForSession,
            })?,
        },
    );
    assert_eq!(first_request.await??, "proxy-ok");
    accepted_rx.recv().await.expect("first target connection");

    assert_eq!(
        send_http_request(proxy_addr, target_addr).await?,
        "proxy-ok"
    );
    accepted_rx.recv().await.expect("second target connection");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .is_err()
    );

    handle.shutdown().await?;
    target_task.await??;
    Ok(())
}

fn approval_context(writer: ConnectionWriter) -> NetworkApprovalContext {
    NetworkApprovalContext {
        connection_id: ConnectionId(7),
        writer,
        process_id: Some("build-1".to_string()),
        command: vec!["cargo".to_string(), "build".to_string()],
        cwd: AbsolutePathBuf::from_absolute_path("/workspace").expect("absolute path"),
        cancellation: CancellationToken::new(),
    }
}

fn policy_request() -> NetworkPolicyRequest {
    NetworkPolicyRequest {
        protocol: NetworkProtocol::HttpsConnect,
        host: "Example.COM".to_string(),
        port: 443,
        environment_id: None,
        client_addr: None,
        method: None,
        command: None,
        exec_policy_hint: None,
        execution_id: None,
    }
}

async fn send_http_request(
    proxy_addr: SocketAddr,
    target_addr: SocketAddr,
) -> anyhow::Result<String> {
    let mut stream = TcpStream::connect(proxy_addr).await?;
    stream
        .write_all(
            format!(
                "GET http://{target_addr}/test HTTP/1.1\r\nHost: {target_addr}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    Ok(response
        .split_once("\r\n\r\n")
        .map_or(response.as_str(), |(_, body)| body)
        .to_string())
}
