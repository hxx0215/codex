use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_app_server_protocol::CommandExecResizeParams;
use codex_app_server_protocol::CommandExecTerminateParams;
use codex_app_server_protocol::CommandExecTerminateResponse;
use codex_app_server_protocol::CommandExecWriteParams;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::InitializeResponse;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_core::exec::ExecCapturePolicy;
use codex_core::exec::ExecExpiration;
use codex_core::exec::ExecParams;
use codex_core::sandboxing::SandboxPermissions;
use codex_network_proxy::NetworkProxyConfig;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::SandboxEnforcement;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::shell_environment;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_pty::DEFAULT_OUTPUT_BYTES_CAP;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use crate::Args;
use crate::network::NetworkApprovalBroker;
use crate::network::NetworkApprovalContext;
use crate::permissions::PermissionsApprovalBroker;
use crate::process::ProcessManager;
use crate::process::StartProcessParams;
use crate::process::terminal_size;
use crate::protocol::COMMAND_EXEC_METHOD;
use crate::protocol::COMMAND_EXEC_RESIZE_METHOD;
use crate::protocol::COMMAND_EXEC_TERMINATE_METHOD;
use crate::protocol::COMMAND_EXEC_WRITE_METHOD;
use crate::protocol::INITIALIZE_METHOD;
use crate::protocol::SandboxCommandExecParams;
use crate::protocol::internal_error;
use crate::protocol::invalid_params;
use crate::protocol::invalid_request;
use crate::protocol::method_not_found;
use crate::transport::ConnectionId;
use crate::transport::ConnectionWriter;

pub(crate) struct Server {
    default_permission_profile: PermissionProfile,
    cwd: AbsolutePathBuf,
    codex_linux_sandbox_exe: Option<PathBuf>,
    shell_environment_policy: ShellEnvironmentPolicy,
    process_manager: ProcessManager,
    network_proxy_config: Option<NetworkProxyConfig>,
    network_approvals: NetworkApprovalBroker,
    permissions_approvals: PermissionsApprovalBroker,
    next_connection_id: AtomicU64,
}

pub(crate) async fn run(args: Args) -> anyhow::Result<()> {
    let permission_profile =
        serde_json::from_str::<PermissionProfile>(&args.permission_profile_json)
            .map_err(|err| anyhow::anyhow!("invalid --permission-profile-json: {err}"))?;
    let cwd = absolute_path(args.cwd.as_deref().unwrap_or(Path::new(".")))?;
    let codex_linux_sandbox_exe = resolve_linux_sandbox_exe(args.codex_linux_sandbox_exe)?;
    let network_proxy_config = args
        .network_proxy_config_json
        .as_deref()
        .map(serde_json::from_str::<NetworkProxyConfig>)
        .transpose()
        .map_err(|err| anyhow::anyhow!("invalid --network-proxy-config-json: {err}"))?
        .filter(|config| config.enabled);
    let server = Arc::new(Server {
        default_permission_profile: permission_profile,
        cwd,
        codex_linux_sandbox_exe,
        shell_environment_policy: ShellEnvironmentPolicy::default(),
        process_manager: ProcessManager::default(),
        network_proxy_config,
        network_approvals: NetworkApprovalBroker::default(),
        permissions_approvals: PermissionsApprovalBroker::default(),
        next_connection_id: AtomicU64::new(1),
    });
    let shutdown = CancellationToken::new();
    let uds_handle = args
        .uds
        .map(|path| absolute_path(&path).map(AbsolutePathBuf::into_path_buf))
        .transpose()?
        .map(|path| crate::transport::start_uds(Arc::clone(&server), path, shutdown.clone()))
        .transpose()?;

    let stdio_result = crate::transport::run_stdio(Arc::clone(&server), shutdown.clone()).await;
    shutdown.cancel();
    server.process_manager.shutdown().await;
    if let Some(uds_handle) = uds_handle {
        let _ = uds_handle.await;
    }
    stdio_result.map_err(anyhow::Error::from)
}

impl Server {
    pub(crate) fn next_connection_id(&self) -> ConnectionId {
        ConnectionId(self.next_connection_id.fetch_add(1, Ordering::Relaxed))
    }

    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        self.network_approvals.connection_closed(connection_id);
        self.permissions_approvals.connection_closed(connection_id);
        self.process_manager.connection_closed(connection_id).await;
    }

    pub(crate) async fn handle_message(
        self: &Arc<Self>,
        connection_id: ConnectionId,
        writer: &ConnectionWriter,
        connection_cancellation: &CancellationToken,
        initialized: &mut bool,
        message: JSONRPCMessage,
    ) {
        let request = match message {
            JSONRPCMessage::Request(request) => request,
            JSONRPCMessage::Response(response) => {
                self.permissions_approvals
                    .handle_response(connection_id, response.clone());
                self.network_approvals
                    .handle_response(connection_id, response);
                return;
            }
            JSONRPCMessage::Error(error) => {
                self.permissions_approvals
                    .handle_error(connection_id, error.clone());
                self.network_approvals.handle_error(connection_id, error);
                return;
            }
            JSONRPCMessage::Notification(_) => {
                tracing::debug!(%connection_id, "ignoring JSON-RPC notification");
                return;
            }
        };
        if request.method == INITIALIZE_METHOD {
            if *initialized {
                send_error(writer, request.id, invalid_request("already initialized")).await;
                return;
            }
            match parse_params::<InitializeParams>(&request) {
                Ok(_params) => match initialize_response() {
                    Ok(response) => {
                        *initialized = true;
                        send_result(writer, request.id, response).await;
                    }
                    Err(err) => {
                        send_error(writer, request.id, internal_error(err.to_string())).await
                    }
                },
                Err(err) => send_error(writer, request.id, err).await,
            }
            return;
        }
        if !*initialized {
            send_error(
                writer,
                request.id,
                invalid_request("initialize must be called first"),
            )
            .await;
            return;
        }

        let request_id = request.id.clone();
        let result = self
            .handle_initialized_request(
                connection_id,
                writer.clone(),
                connection_cancellation.clone(),
                request,
            )
            .await;
        if let Err(err) = result {
            send_error(writer, request_id, err).await;
        }
    }

    async fn handle_initialized_request(
        self: &Arc<Self>,
        connection_id: ConnectionId,
        writer: ConnectionWriter,
        connection_cancellation: CancellationToken,
        request: JSONRPCRequest,
    ) -> Result<(), JSONRPCErrorError> {
        match request.method.as_str() {
            COMMAND_EXEC_METHOD => {
                let params = parse_params::<SandboxCommandExecParams>(&request)?;
                if params.additional_permissions.is_none() {
                    let start = self
                        .prepare_command_exec(
                            connection_id,
                            writer,
                            connection_cancellation,
                            request.id,
                            params,
                        )
                        .await?;
                    return self.process_manager.start(start).await;
                }
                let server = Arc::clone(self);
                tokio::spawn(async move {
                    let request_id = request.id;
                    let result = server
                        .prepare_command_exec(
                            connection_id,
                            writer.clone(),
                            connection_cancellation,
                            request_id.clone(),
                            params,
                        )
                        .await;
                    let result = match result {
                        Ok(start) => server.process_manager.start(start).await,
                        Err(err) => Err(err),
                    };
                    if let Err(err) = result {
                        send_error(&writer, request_id, err).await;
                    }
                });
                Ok(())
            }
            COMMAND_EXEC_WRITE_METHOD => {
                let params = parse_params::<CommandExecWriteParams>(&request)?;
                let response = self.process_manager.write(connection_id, params).await?;
                send_result(&writer, request.id, response).await;
                Ok(())
            }
            COMMAND_EXEC_RESIZE_METHOD => {
                let params = parse_params::<CommandExecResizeParams>(&request)?;
                let response = self.process_manager.resize(connection_id, params).await?;
                send_result(&writer, request.id, response).await;
                Ok(())
            }
            COMMAND_EXEC_TERMINATE_METHOD => {
                let params = parse_params::<CommandExecTerminateParams>(&request)?;
                if self
                    .permissions_approvals
                    .cancel_process(connection_id, &params.process_id)
                {
                    send_result(&writer, request.id, CommandExecTerminateResponse {}).await;
                    return Ok(());
                }
                let response = self
                    .process_manager
                    .terminate(connection_id, params)
                    .await?;
                send_result(&writer, request.id, response).await;
                Ok(())
            }
            method => Err(method_not_found(format!("unknown method: {method}"))),
        }
    }

    async fn prepare_command_exec(
        &self,
        connection_id: ConnectionId,
        writer: ConnectionWriter,
        connection_cancellation: CancellationToken,
        request_id: RequestId,
        params: SandboxCommandExecParams,
    ) -> Result<StartProcessParams, JSONRPCErrorError> {
        let SandboxCommandExecParams {
            command_exec: params,
            additional_permissions,
        } = params;
        if params.command.is_empty() {
            return Err(invalid_request("command must not be empty"));
        }
        if params.sandbox_policy.is_some() && params.permission_profile.is_some() {
            return Err(invalid_request(
                "`permissionProfile` cannot be combined with `sandboxPolicy`",
            ));
        }
        if params.permission_profile.is_some() {
            return Err(invalid_params(
                "named permission profiles are unavailable; use sandboxPolicy or the startup profile",
            ));
        }
        if params.size.is_some() && !params.tty {
            return Err(invalid_params("command/exec size requires tty: true"));
        }
        if params.disable_output_cap && params.output_bytes_cap.is_some() {
            return Err(invalid_params(
                "command/exec cannot set both outputBytesCap and disableOutputCap",
            ));
        }
        if params.disable_timeout && params.timeout_ms.is_some() {
            return Err(invalid_params(
                "command/exec cannot set both timeoutMs and disableTimeout",
            ));
        }

        let cwd = params
            .cwd
            .as_deref()
            .map_or_else(|| self.cwd.clone(), |cwd| self.cwd.join(cwd));
        let additional_permissions = additional_permissions
            .map(codex_protocol::models::AdditionalPermissionProfile::try_from)
            .transpose()
            .map_err(|err| invalid_params(format!("invalid additionalPermissions: {err}")))?
            .map(codex_sandboxing::policy_transforms::normalize_additional_permissions)
            .transpose()
            .map_err(|err| invalid_params(format!("invalid additionalPermissions: {err}")))?
            .filter(|permissions| !permissions.is_empty());
        if let Some(additional_permissions) = additional_permissions.as_ref() {
            self.permissions_approvals
                .request_approval(
                    connection_id,
                    writer.clone(),
                    params.process_id.clone(),
                    params.command.clone(),
                    cwd.clone(),
                    additional_permissions.clone().into(),
                )
                .await?;
        }
        let mut env = shell_environment::create_env(&self.shell_environment_policy, None);
        if let Some(env_overrides) = params.env {
            for (key, value) in env_overrides {
                match value {
                    Some(value) => {
                        env.insert(key, value);
                    }
                    None => {
                        env.remove(&key);
                    }
                }
            }
        }
        let timeout_ms = params
            .timeout_ms
            .map(|timeout_ms| {
                u64::try_from(timeout_ms).map_err(|_| {
                    invalid_params(format!(
                        "command/exec timeoutMs must be non-negative, got {timeout_ms}",
                    ))
                })
            })
            .transpose()?;
        let expiration = if params.disable_timeout {
            ExecExpiration::Cancellation(CancellationToken::new())
        } else {
            timeout_ms.into()
        };
        let output_bytes_cap = if params.disable_output_cap {
            None
        } else {
            Some(params.output_bytes_cap.unwrap_or(DEFAULT_OUTPUT_BYTES_CAP))
        };
        let capture_policy = if params.disable_output_cap {
            ExecCapturePolicy::FullBuffer
        } else {
            ExecCapturePolicy::ShellTool
        };
        let permission_profile = params
            .sandbox_policy
            .as_ref()
            .map(|policy| permission_profile_from_sandbox_policy(policy, &self.cwd))
            .unwrap_or_else(|| self.default_permission_profile.clone());
        let permission_profile = codex_sandboxing::policy_transforms::effective_permission_profile(
            &permission_profile,
            additional_permissions.as_ref(),
        );
        let execution_cancellation = CancellationToken::new();
        let (network, network_proxy_handle) = match self.network_proxy_config.as_ref() {
            Some(config) => {
                let context = NetworkApprovalContext {
                    connection_id,
                    writer: writer.clone(),
                    process_id: params.process_id.clone(),
                    command: params.command.clone(),
                    cwd: cwd.clone(),
                    cancellation: execution_cancellation.clone(),
                };
                let (proxy, handle) = self.network_approvals.start_proxy(config, context).await?;
                (Some(proxy), Some(handle))
            }
            None => (None, None),
        };
        let exec_request = codex_core::exec::build_exec_request(
            ExecParams {
                command: params.command,
                cwd,
                expiration,
                capture_policy,
                env,
                network,
                network_environment_id: None,
                sandbox_permissions: SandboxPermissions::UseDefault,
                windows_sandbox_level: WindowsSandboxLevel::Disabled,
                windows_sandbox_private_desktop: false,
                justification: None,
                arg0: None,
            },
            &permission_profile,
            &self.cwd,
            &[],
            &self.codex_linux_sandbox_exe,
            /*use_legacy_landlock*/ false,
        )
        .map_err(|err| internal_error(format!("failed to prepare sandbox: {err}")))?;
        let size = params.size.map(terminal_size).transpose()?;

        Ok(StartProcessParams {
            connection_id,
            connection_cancellation,
            writer,
            request_id,
            process_id: params.process_id,
            exec_request,
            tty: params.tty,
            stream_stdin: params.stream_stdin,
            stream_stdout_stderr: params.stream_stdout_stderr,
            output_bytes_cap,
            size,
            execution_cancellation,
            network_proxy_handle,
        })
    }
}

fn permission_profile_from_sandbox_policy(
    policy: &codex_app_server_protocol::SandboxPolicy,
    cwd: &AbsolutePathBuf,
) -> PermissionProfile {
    let policy = policy.to_core();
    let file_system_policy =
        FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(&policy, cwd);
    let network_policy = NetworkSandboxPolicy::from(&policy);
    PermissionProfile::from_runtime_permissions_with_enforcement(
        SandboxEnforcement::from_legacy_sandbox_policy(&policy),
        &file_system_policy,
        network_policy,
    )
}

fn parse_params<T: DeserializeOwned>(request: &JSONRPCRequest) -> Result<T, JSONRPCErrorError> {
    serde_json::from_value(request.params.clone().unwrap_or(serde_json::Value::Null))
        .map_err(|err| invalid_params(format!("invalid params for {}: {err}", request.method)))
}

async fn send_result(writer: &ConnectionWriter, id: RequestId, result: impl Serialize) {
    match serde_json::to_value(result) {
        Ok(result) => {
            writer
                .send(JSONRPCMessage::Response(JSONRPCResponse { id, result }))
                .await;
        }
        Err(err) => send_error(writer, id, internal_error(err.to_string())).await,
    }
}

async fn send_error(writer: &ConnectionWriter, id: RequestId, error: JSONRPCErrorError) {
    writer
        .send(JSONRPCMessage::Error(JSONRPCError { id, error }))
        .await;
}

fn initialize_response() -> anyhow::Result<InitializeResponse> {
    let codex_home = AbsolutePathBuf::from_absolute_path(codex_core::config::find_codex_home()?)?;
    Ok(InitializeResponse {
        user_agent: format!("codex-sandbox-server/{}", env!("CARGO_PKG_VERSION")),
        codex_home,
        platform_family: "unix".to_string(),
        platform_os: "linux".to_string(),
    })
}

fn resolve_linux_sandbox_exe(explicit: Option<PathBuf>) -> anyhow::Result<Option<PathBuf>> {
    if let Some(explicit) = explicit {
        return Ok(Some(absolute_path(&explicit)?.into_path_buf()));
    }
    let current_exe = std::env::current_exe()?;
    let sibling = current_exe
        .parent()
        .map(|parent| parent.join("codex-linux-sandbox"));
    Ok(sibling.filter(|path| path.is_file()))
}

fn absolute_path(path: &Path) -> anyhow::Result<AbsolutePathBuf> {
    AbsolutePathBuf::relative_to_current_dir(path).map_err(anyhow::Error::from)
}
