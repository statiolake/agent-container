mod cli;
mod creds;
mod docker;
mod paths;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::{Cli, Commands};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("agent_container=info,warn")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Run { passthrough } => run_cmd(passthrough).await,
    }
}

async fn run_cmd(passthrough: Vec<String>) -> Result<()> {
    let host = paths::HostPaths::detect()?;
    let credentials = creds::prepare(&host.claude_root).context(
        "failed to prepare Claude OAuth credentials; run `claude /login` on the host first",
    )?;
    if credentials.is_expired() {
        eprintln!(
            "[agent-container] warning: host Claude credentials appear expired; refresh them with `claude /login` before running if the container cannot refresh on its own."
        );
    }

    docker::ensure_images(&docker::default_dockerfile_dir())
        .await
        .context("failed to build or locate container images")?;

    let exit = docker::run(docker::RunOptions {
        host,
        credentials_path: credentials.path.clone(),
        extra_args: passthrough,
    })
    .await?;

    drop(credentials);
    std::process::exit(exit);
}
