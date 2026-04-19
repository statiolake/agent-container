mod aws;
mod cli;
mod config_cmd;
mod creds;
mod docker;
mod mcp;
mod mcp_client;
mod paths;
mod policy;
mod server;
mod sync;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::{Cli, Commands, ConfigCommands};

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
        Commands::Config { command } => match command {
            ConfigCommands::Mcp => config_cmd::run().await,
        },
    }
}

async fn run_cmd(passthrough: Vec<String>) -> Result<()> {
    let host = paths::HostPaths::detect()?;

    let bedrock = aws::detect_setup(&host.claude_root.join("settings.json"))
        .context("failed to read Bedrock settings from ~/.claude/settings.json")?;
    let refresh = aws::detect_refresh_command(&host.home.join(".claude.json"))
        .context("failed to read awsAuthRefresh from ~/.claude.json")?;
    let mcp_servers = mcp::load_http_servers(&host.home.join(".claude.json"))
        .context("failed to load MCP servers from ~/.claude.json")?;
    let policy = policy::McpPolicy::load()
        .context("failed to load MCP allowlist policy; fix or remove ~/.config/agent-container/mcp.toml")?;

    if let Some(setup) = &bedrock {
        eprintln!(
            "[agent-container] Bedrock mode detected (profile={}); the container will fetch fresh AWS credentials on demand through the host broker.",
            setup.profile
        );
    }
    if !mcp_servers.is_empty() {
        let names: Vec<_> = mcp_servers.iter().map(|s| s.name.as_str()).collect();
        eprintln!(
            "[agent-container] proxying {} MCP server(s) through broker: {}",
            mcp_servers.len(),
            names.join(", ")
        );
    }

    // When Bedrock is active the container does not need Anthropic OAuth, so
    // credential-prep failures degrade to a warning.
    let credentials = match creds::prepare(&host.claude_root) {
        Ok(c) => Some(c),
        Err(e) if bedrock.is_some() => {
            eprintln!(
                "[agent-container] note: skipping Anthropic credentials (using Bedrock): {e:#}"
            );
            None
        }
        Err(e) => {
            return Err(e).context(
                "failed to prepare Claude OAuth credentials; run `claude /login` on the host first",
            );
        }
    };
    if let Some(c) = &credentials
        && c.is_expired()
    {
        eprintln!(
            "[agent-container] warning: host Claude credentials appear expired; refresh them with `claude /login` before running if the container cannot refresh on its own."
        );
    }

    docker::ensure_images(&docker::default_dockerfile_dir())
        .await
        .context("failed to build or locate container images")?;

    let broker = server::spawn(
        bedrock.clone().map(|b| (b, refresh.clone())),
        mcp_servers.clone(),
        policy,
    )
    .await?;
    tracing::info!(addr = %broker.addr, "host broker listening");
    let broker_url_from_container =
        format!("http://host.docker.internal:{}", broker.addr.port());

    sync::sync_host_state(
        &host,
        sync::SyncOptions {
            bedrock: bedrock.is_some(),
            broker_url_from_container: &broker_url_from_container,
            mcp_servers: &mcp_servers,
        },
    )
    .context("failed to sync host Claude Code state into container")?;

    let credentials_path = credentials
        .as_ref()
        .map(|c| c.path.clone())
        .unwrap_or_else(|| std::path::PathBuf::from("/dev/null"));

    let exit = docker::run(docker::RunOptions {
        host,
        credentials_path,
        bedrock_setup: bedrock,
        broker_addr: broker.addr,
        extra_args: passthrough,
    })
    .await?;

    broker.handle.abort();
    drop(credentials);
    std::process::exit(exit);
}
