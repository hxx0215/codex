use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::NetworkApprovalProtocol;
use codex_app_server_protocol::NetworkPolicyRuleAction;
use codex_app_server_protocol::RequestId;
use codex_network_proxy::ConfigReloader;
use codex_network_proxy::ConfigReloaderFuture;
use codex_network_proxy::ConfigState;
use codex_network_proxy::NetworkDecision;
use codex_network_proxy::NetworkPolicyDecider;
use codex_network_proxy::NetworkPolicyRequest;
use codex_network_proxy::NetworkProtocol;
use codex_network_proxy::NetworkProxy;
use codex_network_proxy::NetworkProxyConfig;
use codex_network_proxy::NetworkProxyConstraints;
use codex_network_proxy::NetworkProxyHandle;
use codex_network_proxy::NetworkProxyState;
use codex_network_proxy::build_config_state;
use codex_network_proxy::normalize_host;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Serialize;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::protocol::COMMAND_EXEC_REQUEST_NETWORK_APPROVAL_METHOD;
use crate::protocol::internal_error;
use crate::transport::ConnectionId;
use crate::transport::ConnectionWriter;

const APPROVAL_REQUEST_ID_PREFIX: &str = "network-approval:";
const DENIED_REASON: &str = "network access was not approved";

#[derive(Clone)]
pub(crate) struct NetworkApprovalBroker {
    inner: Arc<NetworkApprovalBrokerInner>,
}

struct NetworkApprovalBrokerInner {
    next_request_id: AtomicU64,
    pending: Mutex<HashMap<RequestId, PendingApproval>>,
    session_cache: Mutex<HashMap<ConnectionId, HashMap<NetworkCacheKey, CachedDecision>>>,
}

struct PendingApproval {
    connection_id: ConnectionId,
    response_tx: oneshot::Sender<Result<CommandExecutionRequestApprovalResponse, String>>,
}

struct PendingGuard {
    broker: NetworkApprovalBroker,
    request_id: RequestId,
}

#[derive(Clone)]
pub(crate) struct NetworkApprovalContext {
    pub(crate) connection_id: ConnectionId,
    pub(crate) writer: ConnectionWriter,
    pub(crate) process_id: Option<String>,
    pub(crate) command: Vec<String>,
    pub(crate) cwd: AbsolutePathBuf,
    pub(crate) cancellation: CancellationToken,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ApprovalProtocol {
    Http,
    Https,
    Socks5Tcp,
    Socks5Udp,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct NetworkCacheKey {
    host: String,
    port: u16,
    protocol: ApprovalProtocol,
}

#[derive(Clone, Copy)]
enum CachedDecision {
    Allow,
    Deny,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NetworkApprovalRequestParams {
    process_id: Option<String>,
    host: String,
    port: u16,
    protocol: NetworkApprovalProtocol,
    command: Vec<String>,
    cwd: AbsolutePathBuf,
}

#[derive(Clone)]
struct StaticNetworkProxyReloader {
    state: ConfigState,
}

impl StaticNetworkProxyReloader {
    fn new(state: ConfigState) -> Self {
        Self { state }
    }
}

impl ConfigReloader for StaticNetworkProxyReloader {
    fn source_label(&self) -> String {
        "sandbox-server startup configuration".to_string()
    }

    fn maybe_reload(&self) -> ConfigReloaderFuture<'_, Option<ConfigState>> {
        Box::pin(async { Ok(None) })
    }

    fn reload_now(&self) -> ConfigReloaderFuture<'_, ConfigState> {
        Box::pin(async { Ok(self.state.clone()) })
    }
}

impl Default for NetworkApprovalBroker {
    fn default() -> Self {
        Self {
            inner: Arc::new(NetworkApprovalBrokerInner {
                next_request_id: AtomicU64::new(1),
                pending: Mutex::new(HashMap::new()),
                session_cache: Mutex::new(HashMap::new()),
            }),
        }
    }
}

impl NetworkApprovalBroker {
    pub(crate) async fn start_proxy(
        &self,
        config: &NetworkProxyConfig,
        context: NetworkApprovalContext,
    ) -> Result<(NetworkProxy, NetworkProxyHandle), codex_app_server_protocol::JSONRPCErrorError>
    {
        let state = build_config_state(config.clone(), NetworkProxyConstraints::default())
            .map_err(|err| internal_error(format!("failed to configure network proxy: {err}")))?;
        let reloader = Arc::new(StaticNetworkProxyReloader::new(state.clone()));
        let state = Arc::new(NetworkProxyState::with_reloader(state, reloader));
        let broker = self.clone();
        let decider: Arc<dyn NetworkPolicyDecider> = Arc::new(move |request| {
            let broker = broker.clone();
            let context = context.clone();
            async move { broker.decide(context, request).await }
        });
        let proxy = NetworkProxy::builder()
            .state(state)
            .policy_decider_arc(decider)
            .build()
            .await
            .map_err(|err| internal_error(format!("failed to build network proxy: {err}")))?;
        let handle = proxy
            .run()
            .await
            .map_err(|err| internal_error(format!("failed to start network proxy: {err}")))?;
        Ok((proxy, handle))
    }

    pub(crate) fn handle_response(&self, connection_id: ConnectionId, response: JSONRPCResponse) {
        let parsed = serde_json::from_value(response.result)
            .map_err(|err| format!("invalid network approval response: {err}"));
        self.resolve(connection_id, response.id, parsed);
    }

    pub(crate) fn handle_error(&self, connection_id: ConnectionId, error: JSONRPCError) {
        self.resolve(
            connection_id,
            error.id,
            Err(format!(
                "network approval request failed: {}",
                error.error.message
            )),
        );
    }

    pub(crate) fn connection_closed(&self, connection_id: ConnectionId) {
        self.inner
            .session_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&connection_id);
        let pending_ids = {
            let pending = self
                .inner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending
                .iter()
                .filter(|(_, approval)| approval.connection_id == connection_id)
                .map(|(request_id, _)| request_id.clone())
                .collect::<Vec<_>>()
        };
        for request_id in pending_ids {
            self.resolve(
                connection_id,
                request_id,
                Err("network approval connection closed".to_string()),
            );
        }
    }

    async fn decide(
        &self,
        context: NetworkApprovalContext,
        request: NetworkPolicyRequest,
    ) -> NetworkDecision {
        let protocol = ApprovalProtocol::from(request.protocol);
        let key = NetworkCacheKey {
            host: normalize_host(&request.host),
            port: request.port,
            protocol,
        };
        if let Some(decision) = self.cached_decision(context.connection_id, &key) {
            return decision.into_network_decision();
        }

        let request_id = RequestId::String(format!(
            "{APPROVAL_REQUEST_ID_PREFIX}{}",
            self.inner.next_request_id.fetch_add(1, Ordering::Relaxed)
        ));
        let (response_tx, response_rx) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                request_id.clone(),
                PendingApproval {
                    connection_id: context.connection_id,
                    response_tx,
                },
            );
        let _guard = PendingGuard {
            broker: self.clone(),
            request_id: request_id.clone(),
        };
        let params = NetworkApprovalRequestParams {
            process_id: context.process_id,
            host: key.host.clone(),
            port: key.port,
            protocol: protocol.into(),
            command: context.command,
            cwd: context.cwd,
        };
        let params = match serde_json::to_value(params) {
            Ok(params) => params,
            Err(err) => return NetworkDecision::deny(format!("invalid approval request: {err}")),
        };
        context
            .writer
            .send(JSONRPCMessage::Request(JSONRPCRequest {
                id: request_id,
                method: COMMAND_EXEC_REQUEST_NETWORK_APPROVAL_METHOD.to_string(),
                params: Some(params),
                trace: None,
            }))
            .await;

        let response = tokio::select! {
            response = response_rx => response
                .map_err(|_| "network approval response channel closed".to_string())
                .and_then(std::convert::identity),
            () = context.cancellation.cancelled() => {
                Err("network approval cancelled".to_string())
            }
        };
        match response {
            Ok(response) => self.apply_decision(context.connection_id, key, response.decision),
            Err(reason) => NetworkDecision::deny(reason),
        }
    }

    fn apply_decision(
        &self,
        connection_id: ConnectionId,
        key: NetworkCacheKey,
        decision: CommandExecutionApprovalDecision,
    ) -> NetworkDecision {
        match decision {
            CommandExecutionApprovalDecision::Accept => NetworkDecision::Allow,
            CommandExecutionApprovalDecision::AcceptForSession => {
                self.cache_decision(connection_id, key, CachedDecision::Allow);
                NetworkDecision::Allow
            }
            CommandExecutionApprovalDecision::ApplyNetworkPolicyAmendment {
                network_policy_amendment,
            } => {
                if normalize_host(&network_policy_amendment.host) != key.host {
                    return NetworkDecision::deny("network approval amendment host mismatch");
                }
                let decision = match network_policy_amendment.action {
                    NetworkPolicyRuleAction::Allow => CachedDecision::Allow,
                    NetworkPolicyRuleAction::Deny => CachedDecision::Deny,
                };
                self.cache_decision(connection_id, key, decision);
                decision.into_network_decision()
            }
            CommandExecutionApprovalDecision::AcceptWithExecpolicyAmendment { .. } => {
                NetworkDecision::deny("execpolicy amendments are invalid for network approval")
            }
            CommandExecutionApprovalDecision::Decline
            | CommandExecutionApprovalDecision::Cancel => NetworkDecision::deny(DENIED_REASON),
        }
    }

    fn resolve(
        &self,
        connection_id: ConnectionId,
        request_id: RequestId,
        response: Result<CommandExecutionRequestApprovalResponse, String>,
    ) {
        let pending = {
            let mut pending = self
                .inner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending
                .get(&request_id)
                .is_some_and(|approval| approval.connection_id == connection_id)
            {
                pending.remove(&request_id)
            } else {
                None
            }
        };
        if let Some(pending) = pending {
            let _ = pending.response_tx.send(response);
        }
    }

    fn cached_decision(
        &self,
        connection_id: ConnectionId,
        key: &NetworkCacheKey,
    ) -> Option<CachedDecision> {
        self.inner
            .session_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&connection_id)
            .and_then(|cache| cache.get(key).copied())
    }

    fn cache_decision(
        &self,
        connection_id: ConnectionId,
        key: NetworkCacheKey,
        decision: CachedDecision,
    ) {
        self.inner
            .session_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(connection_id)
            .or_default()
            .insert(key, decision);
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.broker
            .inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.request_id);
    }
}

impl CachedDecision {
    fn into_network_decision(self) -> NetworkDecision {
        match self {
            Self::Allow => NetworkDecision::Allow,
            Self::Deny => NetworkDecision::deny(DENIED_REASON),
        }
    }
}

impl From<NetworkProtocol> for ApprovalProtocol {
    fn from(protocol: NetworkProtocol) -> Self {
        match protocol {
            NetworkProtocol::Http => Self::Http,
            NetworkProtocol::HttpsConnect => Self::Https,
            NetworkProtocol::Socks5Tcp => Self::Socks5Tcp,
            NetworkProtocol::Socks5Udp => Self::Socks5Udp,
        }
    }
}

impl From<ApprovalProtocol> for NetworkApprovalProtocol {
    fn from(protocol: ApprovalProtocol) -> Self {
        match protocol {
            ApprovalProtocol::Http => Self::Http,
            ApprovalProtocol::Https => Self::Https,
            ApprovalProtocol::Socks5Tcp => Self::Socks5Tcp,
            ApprovalProtocol::Socks5Udp => Self::Socks5Udp,
        }
    }
}

#[cfg(test)]
#[path = "network_tests.rs"]
mod tests;
