use serde::Deserialize;
use serde::Serialize;

use codex_app_server_protocol::AdditionalPermissionProfile;
use codex_app_server_protocol::CommandExecParams;
use codex_app_server_protocol::JSONRPCErrorError;

pub(crate) const INITIALIZE_METHOD: &str = "initialize";
pub(crate) const COMMAND_EXEC_METHOD: &str = "command/exec";
pub(crate) const COMMAND_EXEC_WRITE_METHOD: &str = "command/exec/write";
pub(crate) const COMMAND_EXEC_RESIZE_METHOD: &str = "command/exec/resize";
pub(crate) const COMMAND_EXEC_TERMINATE_METHOD: &str = "command/exec/terminate";
pub(crate) const COMMAND_EXEC_OUTPUT_DELTA_METHOD: &str = "command/exec/outputDelta";
pub(crate) const COMMAND_EXEC_REQUEST_NETWORK_APPROVAL_METHOD: &str =
    "command/exec/requestNetworkApproval";
pub(crate) const COMMAND_EXEC_REQUEST_PERMISSIONS_APPROVAL_METHOD: &str =
    "command/exec/requestPermissionsApproval";

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SandboxCommandExecParams {
    #[serde(flatten)]
    pub(crate) command_exec: CommandExecParams,
    pub(crate) additional_permissions: Option<AdditionalPermissionProfile>,
}

pub(crate) fn invalid_request(message: impl Into<String>) -> JSONRPCErrorError {
    rpc_error(-32600, message)
}

pub(crate) fn invalid_params(message: impl Into<String>) -> JSONRPCErrorError {
    rpc_error(-32602, message)
}

pub(crate) fn method_not_found(message: impl Into<String>) -> JSONRPCErrorError {
    rpc_error(-32601, message)
}

pub(crate) fn internal_error(message: impl Into<String>) -> JSONRPCErrorError {
    rpc_error(-32603, message)
}

fn rpc_error(code: i64, message: impl Into<String>) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code,
        message: message.into(),
        data: None,
    }
}

/// Final result for `command/exec`.
///
/// Both variants deliberately have the same payload shape so clients can map
/// the tagged wire value directly into an algebraic data type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum CommandExecOutcome {
    Completed {
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    SandboxDenied {
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
}

impl CommandExecOutcome {
    pub(crate) fn new(
        sandbox_denied: bool,
        exit_code: i32,
        stdout: String,
        stderr: String,
    ) -> Self {
        if sandbox_denied {
            Self::SandboxDenied {
                exit_code,
                stdout,
                stderr,
            }
        } else {
            Self::Completed {
                exit_code,
                stdout,
                stderr,
            }
        }
    }
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;
