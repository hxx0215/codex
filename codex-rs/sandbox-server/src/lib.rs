#![deny(clippy::print_stdout)]

#[cfg(target_os = "linux")]
mod network;
#[cfg(target_os = "linux")]
mod permissions;
#[cfg(target_os = "linux")]
mod process;
mod protocol;
#[cfg(target_os = "linux")]
mod server;
#[cfg(target_os = "linux")]
mod transport;

use clap::Parser;
use std::path::PathBuf;

pub use protocol::CommandExecOutcome;

/// Runs Codex command execution behind stdio and an optional same-UID UDS.
#[derive(Debug, Parser)]
#[command(version)]
pub struct Args {
    /// Fully materialized codex_protocol::models::PermissionProfile JSON.
    #[arg(long, value_name = "JSON")]
    pub permission_profile_json: String,

    /// Optional absolute UDS path. The socket uses WebSocket framing.
    #[arg(long, value_name = "PATH")]
    pub uds: Option<PathBuf>,

    /// Default cwd used for relative command/exec cwd values.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Path to the codex-linux-sandbox helper.
    #[arg(long, value_name = "PATH")]
    pub codex_linux_sandbox_exe: Option<PathBuf>,

    /// Optional codex_network_proxy::NetworkProxyConfig JSON.
    #[arg(long, value_name = "JSON")]
    pub network_proxy_config_json: Option<String>,
}

#[cfg(target_os = "linux")]
pub async fn run(args: Args) -> anyhow::Result<()> {
    server::run(args).await
}

#[cfg(not(target_os = "linux"))]
pub async fn run(_args: Args) -> anyhow::Result<()> {
    anyhow::bail!("codex-sandbox-server v1 only supports Linux")
}
