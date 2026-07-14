#![cfg(target_os = "linux")]

use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdin;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use codex_protocol::models::PermissionProfile;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::sleep;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Message;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

struct SandboxServerProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout_rx: Option<mpsc::Receiver<Result<Value, String>>>,
}

impl SandboxServerProcess {
    fn spawn(
        working_dir: &Path,
        codex_home: &Path,
        uds: Option<&Path>,
        capture_stdout: bool,
    ) -> Result<Self> {
        fs::create_dir_all(codex_home)?;
        let permission_profile: PermissionProfile = PermissionProfile::Disabled;
        let permission_profile_json = serde_json::to_string(&permission_profile)?;
        let mut command = Command::new(codex_utils_cargo_bin::cargo_bin("codex-sandbox-server")?);
        command
            .arg("--permission-profile-json")
            .arg(permission_profile_json)
            .arg("--cwd")
            .arg(working_dir)
            .env("CODEX_HOME", codex_home)
            .stdin(Stdio::piped())
            .stdout(if capture_stdout {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::inherit());
        if let Some(uds) = uds {
            command.arg("--uds").arg(uds);
        }
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().context("sandbox server stdin")?;
        let stdout_rx = if capture_stdout {
            let stdout = child.stdout.take().context("sandbox server stdout")?;
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let message = line.map_err(|err| err.to_string()).and_then(|line| {
                        serde_json::from_str(&line).map_err(|err| err.to_string())
                    });
                    if tx.send(message).is_err() {
                        break;
                    }
                }
            });
            Some(rx)
        } else {
            None
        };
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout_rx,
        })
    }

    fn send_stdio(&mut self, message: &Value) -> Result<()> {
        let stdin = self.stdin.as_mut().context("sandbox server stdin closed")?;
        serde_json::to_writer(&mut *stdin, message)?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    }

    fn recv_stdio(&self) -> Result<Value> {
        self.stdout_rx
            .as_ref()
            .context("sandbox server stdout is not captured")?
            .recv_timeout(TEST_TIMEOUT)
            .context("timed out waiting for sandbox server stdout")?
            .map_err(anyhow::Error::msg)
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    fn wait(&mut self) -> Result<ExitStatus> {
        let deadline = Instant::now() + TEST_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for sandbox server to exit");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    async fn wait_async(&mut self) -> Result<ExitStatus> {
        let deadline = Instant::now() + TEST_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for sandbox server to exit");
            }
            sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for SandboxServerProcess {
    fn drop(&mut self) {
        self.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn stdio_requires_initialize_and_returns_tagged_nonzero_result() -> Result<()> {
    let temp = TempDir::new()?;
    let codex_home = temp.path().join("codex-home");
    let mut server = SandboxServerProcess::spawn(temp.path(), &codex_home, None, true)?;

    server.send_stdio(&json!({
        "id": 1,
        "method": "command/exec",
        "params": {"command": ["sh", "-c", "exit 0"]}
    }))?;
    assert_eq!(
        server.recv_stdio()?,
        json!({
            "id": 1,
            "error": {
                "code": -32600,
                "message": "initialize must be called first"
            }
        })
    );

    server.send_stdio(&initialize_request(2))?;
    assert_eq!(
        server.recv_stdio()?,
        json!({
            "id": 2,
            "result": {
                "userAgent": format!("codex-sandbox-server/{}", env!("CARGO_PKG_VERSION")),
                "codexHome": codex_home,
                "platformFamily": "unix",
                "platformOs": "linux"
            }
        })
    );

    server.send_stdio(&json!({
        "id": 3,
        "method": "command/exec",
        "params": {
            "command": ["sh", "-c", "printf stdout; printf stderr >&2; exit 7"]
        }
    }))?;
    assert_eq!(
        server.recv_stdio()?,
        json!({
            "id": 3,
            "result": {
                "type": "completed",
                "exitCode": 7,
                "stdout": "stdout",
                "stderr": "stderr"
            }
        })
    );

    server.close_stdin();
    assert!(server.wait()?.success());
    Ok(())
}

#[tokio::test]
async fn uds_runs_commands_and_terminates_processes_on_disconnect() -> Result<()> {
    let temp = TempDir::new()?;
    let socket_path = temp.path().join("sandbox.sock");
    let codex_home = temp.path().join("codex-home");
    let mut server =
        SandboxServerProcess::spawn(temp.path(), &codex_home, Some(&socket_path), false)?;
    wait_for_path(&socket_path).await?;
    assert_eq!(
        fs::metadata(&socket_path)?.permissions().mode() & 0o777,
        0o600
    );

    let stream = UnixStream::connect(&socket_path).await?;
    let (mut websocket, _) = client_async("ws://localhost", stream).await?;
    send_websocket_json(&mut websocket, &initialize_request(1)).await?;
    let initialize_response = recv_websocket_json(&mut websocket).await?;
    assert_eq!(initialize_response["id"], json!(1));

    send_websocket_json(
        &mut websocket,
        &json!({
            "id": 2,
            "method": "command/exec",
            "params": {"command": ["sh", "-c", "printf uds"]}
        }),
    )
    .await?;
    assert_eq!(
        recv_websocket_json(&mut websocket).await?,
        json!({
            "id": 2,
            "result": {
                "type": "completed",
                "exitCode": 0,
                "stdout": "uds",
                "stderr": ""
            }
        })
    );

    send_websocket_json(
        &mut websocket,
        &json!({
            "id": 3,
            "method": "command/exec",
            "params": {
                "command": ["sh", "-c", "printf '%s' $$; exec sleep 30"],
                "processId": "disconnect-test",
                "streamStdoutStderr": true,
                "disableTimeout": true
            }
        }),
    )
    .await?;
    let notification = recv_websocket_json(&mut websocket).await?;
    assert_eq!(notification["method"], json!("command/exec/outputDelta"));
    let pid = String::from_utf8(
        STANDARD.decode(
            notification["params"]["deltaBase64"]
                .as_str()
                .context("output delta base64")?,
        )?,
    )?
    .parse::<u32>()?;
    assert!(PathBuf::from(format!("/proc/{pid}")).exists());

    websocket.close(None).await?;
    drop(websocket);
    wait_for_process_exit(pid).await?;

    server.close_stdin();
    assert!(server.wait_async().await?.success());
    assert!(!socket_path.exists());
    Ok(())
}

#[test]
fn uds_refuses_to_replace_an_existing_path() -> Result<()> {
    let temp = TempDir::new()?;
    let socket_path = temp.path().join("sandbox.sock");
    let codex_home = temp.path().join("codex-home");
    fs::write(&socket_path, "owned by caller")?;
    let mut server =
        SandboxServerProcess::spawn(temp.path(), &codex_home, Some(&socket_path), false)?;
    assert!(!server.wait()?.success());
    assert_eq!(fs::read_to_string(&socket_path)?, "owned by caller");
    Ok(())
}

fn initialize_request(id: i64) -> Value {
    json!({
        "id": id,
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "sandbox-server-test",
                "title": null,
                "version": "0.0.0"
            },
            "capabilities": null
        }
    })
}

async fn send_websocket_json<S>(
    websocket: &mut tokio_tungstenite::WebSocketStream<S>,
    message: &Value,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    websocket
        .send(Message::Text(serde_json::to_string(message)?.into()))
        .await?;
    Ok(())
}

async fn recv_websocket_json<S>(
    websocket: &mut tokio_tungstenite::WebSocketStream<S>,
) -> Result<Value>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let message = tokio::time::timeout(TEST_TIMEOUT, websocket.next())
        .await
        .context("timed out waiting for UDS WebSocket message")?
        .context("UDS WebSocket closed")??;
    let text = message.into_text()?;
    Ok(serde_json::from_str(text.as_ref())?)
}

async fn wait_for_path(path: &Path) -> Result<()> {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !path.exists() {
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for {}", path.display());
        }
        sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

async fn wait_for_process_exit(pid: u32) -> Result<()> {
    let proc_path = PathBuf::from(format!("/proc/{pid}"));
    let deadline = Instant::now() + TEST_TIMEOUT;
    while proc_path.exists() {
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for process {pid} to exit");
        }
        sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}
