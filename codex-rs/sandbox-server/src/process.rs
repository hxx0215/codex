use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use codex_app_server_protocol::CommandExecOutputDeltaNotification;
use codex_app_server_protocol::CommandExecOutputStream;
use codex_app_server_protocol::CommandExecResizeParams;
use codex_app_server_protocol::CommandExecResizeResponse;
use codex_app_server_protocol::CommandExecTerminalSize;
use codex_app_server_protocol::CommandExecTerminateParams;
use codex_app_server_protocol::CommandExecTerminateResponse;
use codex_app_server_protocol::CommandExecWriteParams;
use codex_app_server_protocol::CommandExecWriteResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_core::exec::ExecExpiration;
use codex_core::exec::ExecExpirationOutcome;
use codex_core::exec::IO_DRAIN_TIMEOUT_MS;
use codex_core::sandboxing::ExecRequest;
use codex_network_proxy::NetworkProxyHandle;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::exec_output::bytes_to_string_smart;
use codex_sandboxing::SandboxType;
use codex_sandboxing::is_likely_sandbox_denied;
use codex_utils_pty::ProcessHandle;
use codex_utils_pty::SpawnedProcess;
use codex_utils_pty::TerminalSize;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::protocol::COMMAND_EXEC_OUTPUT_DELTA_METHOD;
use crate::protocol::CommandExecOutcome;
use crate::protocol::internal_error;
use crate::protocol::invalid_params;
use crate::protocol::invalid_request;
use crate::transport::ConnectionId;
use crate::transport::ConnectionWriter;

const EXEC_TIMEOUT_EXIT_CODE: i32 = 124;
const OUTPUT_CHUNK_SIZE_HINT: usize = 64 * 1024;

#[derive(Clone)]
pub(crate) struct ProcessManager {
    sessions: Arc<Mutex<HashMap<ConnectionProcessId, ProcessSession>>>,
    next_generated_process_id: Arc<AtomicI64>,
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_generated_process_id: Arc::new(AtomicI64::new(1)),
        }
    }
}

pub(crate) struct StartProcessParams {
    pub(crate) connection_id: ConnectionId,
    pub(crate) connection_cancellation: CancellationToken,
    pub(crate) writer: ConnectionWriter,
    pub(crate) request_id: RequestId,
    pub(crate) process_id: Option<String>,
    pub(crate) exec_request: ExecRequest,
    pub(crate) tty: bool,
    pub(crate) stream_stdin: bool,
    pub(crate) stream_stdout_stderr: bool,
    pub(crate) output_bytes_cap: Option<usize>,
    pub(crate) size: Option<TerminalSize>,
    pub(crate) execution_cancellation: CancellationToken,
    pub(crate) network_proxy_handle: Option<NetworkProxyHandle>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ConnectionProcessId {
    connection_id: ConnectionId,
    process_id: InternalProcessId,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum InternalProcessId {
    Generated(i64),
    Client(String),
}

impl InternalProcessId {
    fn error_repr(&self) -> String {
        match self {
            Self::Generated(id) => id.to_string(),
            Self::Client(id) => serde_json::to_string(id).unwrap_or_else(|_| format!("{id:?}")),
        }
    }
}

enum Control {
    Write { delta: Vec<u8>, close_stdin: bool },
    Resize { size: TerminalSize },
    Terminate,
}

struct ControlRequest {
    control: Control,
    response_tx: Option<oneshot::Sender<Result<(), JSONRPCErrorError>>>,
}

struct ProcessSession {
    control_tx: mpsc::Sender<ControlRequest>,
    completed_rx: watch::Receiver<bool>,
    execution_cancellation: CancellationToken,
}

struct RunProcessParams {
    writer: ConnectionWriter,
    request_id: RequestId,
    notification_process_id: Option<String>,
    spawned: SpawnedProcess,
    control_rx: mpsc::Receiver<ControlRequest>,
    stream_stdin: bool,
    stream_stdout_stderr: bool,
    expiration: ExecExpiration,
    output_bytes_cap: Option<usize>,
    sandbox: SandboxType,
    execution_cancellation: CancellationToken,
    network_proxy_handle: Option<NetworkProxyHandle>,
}

struct CaptureOutputParams {
    writer: ConnectionWriter,
    process_id: Option<String>,
    output_rx: mpsc::Receiver<Vec<u8>>,
    stdio_timeout_rx: watch::Receiver<bool>,
    stream: CommandExecOutputStream,
    stream_output: bool,
    output_bytes_cap: Option<usize>,
}

struct CapturedOutput {
    response_text: String,
    detection_text: String,
}

impl ProcessManager {
    pub(crate) async fn start(&self, params: StartProcessParams) -> Result<(), JSONRPCErrorError> {
        let StartProcessParams {
            connection_id,
            connection_cancellation,
            writer,
            request_id,
            process_id,
            exec_request,
            tty,
            stream_stdin,
            stream_stdout_stderr,
            output_bytes_cap,
            size,
            execution_cancellation,
            mut network_proxy_handle,
        } = params;
        if process_id.is_none() && (tty || stream_stdin || stream_stdout_stderr) {
            return Err(invalid_request(
                "command/exec tty or streaming requires a client-supplied processId",
            ));
        }
        let process_id = process_id.map_or_else(
            || {
                InternalProcessId::Generated(
                    self.next_generated_process_id
                        .fetch_add(1, Ordering::Relaxed),
                )
            },
            InternalProcessId::Client,
        );
        let process_key = ConnectionProcessId {
            connection_id,
            process_id: process_id.clone(),
        };
        let notification_process_id = match &process_id {
            InternalProcessId::Generated(_) => None,
            InternalProcessId::Client(process_id) => Some(process_id.clone()),
        };
        let ExecRequest {
            command,
            cwd,
            env,
            expiration,
            sandbox,
            arg0,
            ..
        } = exec_request;
        let cwd = cwd
            .to_abs_path()
            .map_err(|err| invalid_request(format!("invalid command cwd: {err}")))?;
        let (program, args) = command
            .split_first()
            .ok_or_else(|| invalid_request("command must not be empty"))?;
        let stream_stdin = tty || stream_stdin;
        let stream_stdout_stderr = tty || stream_stdout_stderr;
        let (control_tx, control_rx) = mpsc::channel(32);
        let (completed_tx, completed_rx) = watch::channel(false);
        {
            let mut sessions = self.sessions.lock().await;
            if connection_cancellation.is_cancelled() {
                execution_cancellation.cancel();
                return Err(invalid_request(
                    "command/exec connection closed before the process could start",
                ));
            }
            if sessions.contains_key(&process_key) {
                return Err(invalid_request(format!(
                    "duplicate active command/exec process id: {}",
                    process_key.process_id.error_repr(),
                )));
            }
            sessions.insert(
                process_key.clone(),
                ProcessSession {
                    control_tx,
                    completed_rx,
                    execution_cancellation: execution_cancellation.clone(),
                },
            );
        }
        let spawned = if tty {
            codex_utils_pty::spawn_pty_process(
                program,
                args,
                cwd.as_path(),
                &env,
                &arg0,
                size.unwrap_or_default(),
            )
            .await
        } else if stream_stdin {
            codex_utils_pty::spawn_pipe_process(program, args, cwd.as_path(), &env, &arg0).await
        } else {
            codex_utils_pty::spawn_pipe_process_no_stdin(program, args, cwd.as_path(), &env, &arg0)
                .await
        };
        let spawned = match spawned {
            Ok(spawned) => spawned,
            Err(err) => {
                self.sessions.lock().await.remove(&process_key);
                execution_cancellation.cancel();
                if let Some(handle) = network_proxy_handle.take() {
                    let _ = handle.shutdown().await;
                }
                return Err(internal_error(format!("failed to spawn command: {err}")));
            }
        };

        let sessions = Arc::clone(&self.sessions);
        tokio::spawn(async move {
            run_process(RunProcessParams {
                writer,
                request_id,
                notification_process_id,
                spawned,
                control_rx,
                stream_stdin,
                stream_stdout_stderr,
                expiration,
                output_bytes_cap,
                sandbox,
                execution_cancellation,
                network_proxy_handle,
            })
            .await;
            let _ = completed_tx.send(true);
            sessions.lock().await.remove(&process_key);
        });
        Ok(())
    }

    pub(crate) async fn write(
        &self,
        connection_id: ConnectionId,
        params: CommandExecWriteParams,
    ) -> Result<CommandExecWriteResponse, JSONRPCErrorError> {
        if params.delta_base64.is_none() && !params.close_stdin {
            return Err(invalid_params(
                "command/exec/write requires deltaBase64 or closeStdin",
            ));
        }
        let delta = params
            .delta_base64
            .map(|delta| {
                STANDARD
                    .decode(delta)
                    .map_err(|err| invalid_params(format!("invalid deltaBase64: {err}")))
            })
            .transpose()?
            .unwrap_or_default();
        self.send_control(
            connection_id,
            params.process_id,
            Control::Write {
                delta,
                close_stdin: params.close_stdin,
            },
        )
        .await?;
        Ok(CommandExecWriteResponse {})
    }

    pub(crate) async fn resize(
        &self,
        connection_id: ConnectionId,
        params: CommandExecResizeParams,
    ) -> Result<CommandExecResizeResponse, JSONRPCErrorError> {
        self.send_control(
            connection_id,
            params.process_id,
            Control::Resize {
                size: terminal_size(params.size)?,
            },
        )
        .await?;
        Ok(CommandExecResizeResponse {})
    }

    pub(crate) async fn terminate(
        &self,
        connection_id: ConnectionId,
        params: CommandExecTerminateParams,
    ) -> Result<CommandExecTerminateResponse, JSONRPCErrorError> {
        self.send_control(connection_id, params.process_id, Control::Terminate)
            .await?;
        Ok(CommandExecTerminateResponse {})
    }

    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        let sessions = {
            let mut sessions = self.sessions.lock().await;
            sessions
                .extract_if(|key, _| key.connection_id == connection_id)
                .map(|(_, session)| session)
                .collect::<Vec<_>>()
        };
        terminate_and_wait(sessions).await;
    }

    pub(crate) async fn shutdown(&self) {
        let sessions = {
            let mut sessions = self.sessions.lock().await;
            sessions
                .drain()
                .map(|(_, session)| session)
                .collect::<Vec<_>>()
        };
        terminate_and_wait(sessions).await;
    }

    async fn send_control(
        &self,
        connection_id: ConnectionId,
        process_id: String,
        control: Control,
    ) -> Result<(), JSONRPCErrorError> {
        let key = ConnectionProcessId {
            connection_id,
            process_id: InternalProcessId::Client(process_id),
        };
        let (control_tx, execution_cancellation) = self
            .sessions
            .lock()
            .await
            .get(&key)
            .map(|session| {
                (
                    session.control_tx.clone(),
                    session.execution_cancellation.clone(),
                )
            })
            .ok_or_else(|| {
                invalid_request(format!(
                    "no active command/exec for process id {}",
                    key.process_id.error_repr(),
                ))
            })?;
        if matches!(&control, Control::Terminate) {
            execution_cancellation.cancel();
        }
        let (response_tx, response_rx) = oneshot::channel();
        control_tx
            .send(ControlRequest {
                control,
                response_tx: Some(response_tx),
            })
            .await
            .map_err(|_| process_ended_error(&key.process_id))?;
        response_rx
            .await
            .map_err(|_| process_ended_error(&key.process_id))?
    }
}

async fn terminate_and_wait(sessions: Vec<ProcessSession>) {
    for session in &sessions {
        session.execution_cancellation.cancel();
        let _ = session
            .control_tx
            .send(ControlRequest {
                control: Control::Terminate,
                response_tx: None,
            })
            .await;
    }
    for mut session in sessions {
        let _ = session.completed_rx.wait_for(|completed| *completed).await;
    }
}

async fn run_process(params: RunProcessParams) {
    let RunProcessParams {
        writer,
        request_id,
        notification_process_id,
        spawned,
        mut control_rx,
        stream_stdin,
        stream_stdout_stderr,
        expiration,
        output_bytes_cap,
        sandbox,
        execution_cancellation,
        network_proxy_handle,
    } = params;
    let started_at = Instant::now();
    let expiration = expiration.wait_with_outcome();
    tokio::pin!(expiration);
    let SpawnedProcess {
        session,
        stdout_rx,
        stderr_rx,
        exit_rx,
    } = spawned;
    tokio::pin!(exit_rx);
    let (stdio_timeout_tx, stdio_timeout_rx) = watch::channel(false);
    let stdout_handle = capture_output(CaptureOutputParams {
        writer: writer.clone(),
        process_id: notification_process_id.clone(),
        output_rx: stdout_rx,
        stdio_timeout_rx: stdio_timeout_rx.clone(),
        stream: CommandExecOutputStream::Stdout,
        stream_output: stream_stdout_stderr,
        output_bytes_cap,
    });
    let stderr_handle = capture_output(CaptureOutputParams {
        writer: writer.clone(),
        process_id: notification_process_id,
        output_rx: stderr_rx,
        stdio_timeout_rx,
        stream: CommandExecOutputStream::Stderr,
        stream_output: stream_stdout_stderr,
        output_bytes_cap,
    });

    let mut expiration_outcome = None;
    let mut control_open = true;
    let exit_code = loop {
        tokio::select! {
            request = control_rx.recv(), if control_open => match request {
                Some(ControlRequest { control, response_tx }) => {
                    if matches!(&control, Control::Terminate) {
                        execution_cancellation.cancel();
                    }
                    let result = handle_control(&session, stream_stdin, control).await;
                    if let Some(response_tx) = response_tx {
                        let _ = response_tx.send(result);
                    }
                }
                None => {
                    control_open = false;
                    execution_cancellation.cancel();
                    session.request_terminate();
                }
            },
            outcome = &mut expiration, if expiration_outcome.is_none() => {
                expiration_outcome = Some(outcome);
                execution_cancellation.cancel();
                session.request_terminate();
            }
            exit = &mut exit_rx => {
                execution_cancellation.cancel();
                if matches!(expiration_outcome, Some(ExecExpirationOutcome::TimedOut)) {
                    break EXEC_TIMEOUT_EXIT_CODE;
                }
                break exit.unwrap_or(-1);
            }
        }
    };
    let timeout_handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(IO_DRAIN_TIMEOUT_MS)).await;
        let _ = stdio_timeout_tx.send(true);
    });
    let stdout = stdout_handle
        .await
        .unwrap_or_else(|_| CapturedOutput::empty());
    let stderr = stderr_handle
        .await
        .unwrap_or_else(|_| CapturedOutput::empty());
    timeout_handle.abort();
    if let Some(handle) = network_proxy_handle {
        let _ = handle.shutdown().await;
    }

    let detection_output = ExecToolCallOutput {
        exit_code,
        stdout: StreamOutput::new(stdout.detection_text.clone()),
        stderr: StreamOutput::new(stderr.detection_text.clone()),
        aggregated_output: StreamOutput::new(format!(
            "{}{}",
            stdout.detection_text, stderr.detection_text
        )),
        duration: started_at.elapsed(),
        timed_out: matches!(expiration_outcome, Some(ExecExpirationOutcome::TimedOut)),
    };
    let outcome = CommandExecOutcome::new(
        is_likely_sandbox_denied(sandbox, &detection_output),
        exit_code,
        stdout.response_text,
        stderr.response_text,
    );
    let result = serde_json::to_value(outcome).unwrap_or(serde_json::Value::Null);
    writer
        .send(JSONRPCMessage::Response(JSONRPCResponse {
            id: request_id,
            result,
        }))
        .await;
}

fn capture_output(params: CaptureOutputParams) -> tokio::task::JoinHandle<CapturedOutput> {
    tokio::spawn(async move {
        let CaptureOutputParams {
            writer,
            process_id,
            mut output_rx,
            mut stdio_timeout_rx,
            stream,
            stream_output,
            output_bytes_cap,
        } = params;
        let mut response = Vec::new();
        let mut detection = Vec::new();
        let mut observed_bytes = 0usize;
        loop {
            let mut chunk = tokio::select! {
                chunk = output_rx.recv() => match chunk {
                    Some(chunk) => chunk,
                    None => break,
                },
                _ = stdio_timeout_rx.wait_for(|timed_out| *timed_out) => break,
            };
            while chunk.len() < OUTPUT_CHUNK_SIZE_HINT
                && let Ok(next_chunk) = output_rx.try_recv()
            {
                chunk.extend_from_slice(&next_chunk);
            }
            let capped_len = output_bytes_cap
                .map(|cap| cap.saturating_sub(observed_bytes).min(chunk.len()))
                .unwrap_or(chunk.len());
            let capped_chunk = &chunk[..capped_len];
            observed_bytes += capped_len;
            detection.extend_from_slice(capped_chunk);
            let cap_reached = Some(observed_bytes) == output_bytes_cap;
            if stream_output {
                if let Some(process_id) = process_id.as_ref() {
                    let params = CommandExecOutputDeltaNotification {
                        process_id: process_id.clone(),
                        stream,
                        delta_base64: STANDARD.encode(capped_chunk),
                        cap_reached,
                    };
                    writer
                        .send(JSONRPCMessage::Notification(JSONRPCNotification {
                            method: COMMAND_EXEC_OUTPUT_DELTA_METHOD.to_string(),
                            params: serde_json::to_value(params).ok(),
                        }))
                        .await;
                }
            } else {
                response.extend_from_slice(capped_chunk);
            }
            if cap_reached {
                break;
            }
        }
        CapturedOutput {
            response_text: bytes_to_string_smart(&response),
            detection_text: bytes_to_string_smart(&detection),
        }
    })
}

impl CapturedOutput {
    fn empty() -> Self {
        Self {
            response_text: String::new(),
            detection_text: String::new(),
        }
    }
}

async fn handle_control(
    session: &ProcessHandle,
    stream_stdin: bool,
    control: Control,
) -> Result<(), JSONRPCErrorError> {
    match control {
        Control::Write { delta, close_stdin } => {
            if !stream_stdin {
                return Err(invalid_request(
                    "stdin streaming is not enabled for this command/exec",
                ));
            }
            if !delta.is_empty() {
                session
                    .writer_sender()
                    .send(delta)
                    .await
                    .map_err(|_| invalid_request("stdin is already closed"))?;
            }
            if close_stdin {
                session.close_stdin();
            }
            Ok(())
        }
        Control::Resize { size } => session
            .resize(size)
            .map_err(|err| invalid_request(format!("failed to resize PTY: {err}"))),
        Control::Terminate => {
            session.request_terminate();
            Ok(())
        }
    }
}

pub(crate) fn terminal_size(
    size: CommandExecTerminalSize,
) -> Result<TerminalSize, JSONRPCErrorError> {
    if size.rows == 0 || size.cols == 0 {
        return Err(invalid_params(
            "command/exec size rows and cols must be greater than 0",
        ));
    }
    Ok(TerminalSize {
        rows: size.rows,
        cols: size.cols,
    })
}

fn process_ended_error(process_id: &InternalProcessId) -> JSONRPCErrorError {
    invalid_request(format!(
        "command/exec {} is no longer running",
        process_id.error_repr(),
    ))
}
