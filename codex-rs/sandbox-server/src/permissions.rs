use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_app_server_protocol::AdditionalPermissionProfile;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Serialize;
use tokio::sync::oneshot;

use crate::protocol::COMMAND_EXEC_REQUEST_PERMISSIONS_APPROVAL_METHOD;
use crate::protocol::invalid_request;
use crate::transport::ConnectionId;
use crate::transport::ConnectionWriter;

const APPROVAL_REQUEST_ID_PREFIX: &str = "permissions-approval:";

#[derive(Clone)]
pub(crate) struct PermissionsApprovalBroker {
    inner: Arc<PermissionsApprovalBrokerInner>,
}

struct PermissionsApprovalBrokerInner {
    next_request_id: AtomicU64,
    pending: Mutex<HashMap<RequestId, PendingApproval>>,
}

struct PendingApproval {
    connection_id: ConnectionId,
    process_id: Option<String>,
    response_tx: oneshot::Sender<Result<CommandExecutionRequestApprovalResponse, String>>,
}

struct PendingGuard {
    broker: PermissionsApprovalBroker,
    request_id: RequestId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PermissionsApprovalRequestParams {
    process_id: Option<String>,
    command: Vec<String>,
    cwd: AbsolutePathBuf,
    additional_permissions: AdditionalPermissionProfile,
}

impl Default for PermissionsApprovalBroker {
    fn default() -> Self {
        Self {
            inner: Arc::new(PermissionsApprovalBrokerInner {
                next_request_id: AtomicU64::new(1),
                pending: Mutex::new(HashMap::new()),
            }),
        }
    }
}

impl PermissionsApprovalBroker {
    pub(crate) async fn request_approval(
        &self,
        connection_id: ConnectionId,
        writer: ConnectionWriter,
        process_id: Option<String>,
        command: Vec<String>,
        cwd: AbsolutePathBuf,
        additional_permissions: AdditionalPermissionProfile,
    ) -> Result<(), codex_app_server_protocol::JSONRPCErrorError> {
        let request_id = RequestId::String(format!(
            "{APPROVAL_REQUEST_ID_PREFIX}{}",
            self.inner.next_request_id.fetch_add(1, Ordering::Relaxed)
        ));
        let (response_tx, response_rx) = oneshot::channel();
        {
            let mut pending = self
                .inner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(process_id) = process_id.as_ref()
                && pending.values().any(|approval| {
                    approval.connection_id == connection_id
                        && approval.process_id.as_ref() == Some(process_id)
                })
            {
                return Err(invalid_request(format!(
                    "duplicate pending command/exec process id: {process_id:?}"
                )));
            }
            pending.insert(
                request_id.clone(),
                PendingApproval {
                    connection_id,
                    process_id: process_id.clone(),
                    response_tx,
                },
            );
        }
        let _guard = PendingGuard {
            broker: self.clone(),
            request_id: request_id.clone(),
        };
        let params = serde_json::to_value(PermissionsApprovalRequestParams {
            process_id,
            command,
            cwd,
            additional_permissions,
        })
        .map_err(|err| invalid_request(format!("invalid permissions approval request: {err}")))?;
        writer
            .send(JSONRPCMessage::Request(JSONRPCRequest {
                id: request_id,
                method: COMMAND_EXEC_REQUEST_PERMISSIONS_APPROVAL_METHOD.to_string(),
                params: Some(params),
                trace: None,
            }))
            .await;

        let response = response_rx
            .await
            .map_err(|_| invalid_request("permissions approval response channel closed"))?
            .map_err(invalid_request)?;
        match response.decision {
            CommandExecutionApprovalDecision::Accept => Ok(()),
            CommandExecutionApprovalDecision::AcceptForSession
            | CommandExecutionApprovalDecision::AcceptWithExecpolicyAmendment { .. }
            | CommandExecutionApprovalDecision::ApplyNetworkPolicyAmendment { .. }
            | CommandExecutionApprovalDecision::Decline
            | CommandExecutionApprovalDecision::Cancel => {
                Err(invalid_request("additional permissions were not approved"))
            }
        }
    }

    pub(crate) fn handle_response(&self, connection_id: ConnectionId, response: JSONRPCResponse) {
        let parsed = serde_json::from_value(response.result)
            .map_err(|err| format!("invalid permissions approval response: {err}"));
        self.resolve(connection_id, response.id, parsed);
    }

    pub(crate) fn handle_error(&self, connection_id: ConnectionId, error: JSONRPCError) {
        self.resolve(
            connection_id,
            error.id,
            Err(format!(
                "permissions approval request failed: {}",
                error.error.message
            )),
        );
    }

    pub(crate) fn cancel_process(&self, connection_id: ConnectionId, process_id: &str) -> bool {
        let request_id = {
            let pending = self
                .inner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending
                .iter()
                .find(|(_, approval)| {
                    approval.connection_id == connection_id
                        && approval.process_id.as_deref() == Some(process_id)
                })
                .map(|(request_id, _)| request_id.clone())
        };
        if let Some(request_id) = request_id {
            self.resolve(
                connection_id,
                request_id,
                Err("permissions approval cancelled".to_string()),
            );
            true
        } else {
            false
        }
    }

    pub(crate) fn connection_closed(&self, connection_id: ConnectionId) {
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
                Err("permissions approval connection closed".to_string()),
            );
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

#[cfg(test)]
#[path = "permissions_tests.rs"]
mod tests;
