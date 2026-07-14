use clap::Parser;
use codex_sandbox_server::Args;

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(codex_sandbox_server::run(args))
}
