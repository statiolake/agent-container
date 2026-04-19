mod aws;
mod cli;
mod codex;
mod config_cmd;
mod creds;
mod docker;
mod mcp;
mod mcp_client;
mod oauth;
mod paths;
mod policy;
mod server;
mod stdio_mcp;
mod sync;
mod tui;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::{AgentKind, Cli, Commands, ConfigCommands};

/// Initialise the tracing subscriber. If `AGENT_CONTAINER_LOG_FILE` is
/// set, logs go there (append, no ANSI) via a non-blocking writer so the
/// container's TUI on stderr/stdout stays unharmed. Otherwise logs go to
/// stderr as before.
fn init_tracing() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("agent_container=info,warn"));

    if let Ok(path) = std::env::var("AGENT_CONTAINER_LOG_FILE") {
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(file) => {
                let (writer, guard) = tracing_appender::non_blocking(file);
                tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_target(false)
                    .with_ansi(false)
                    .with_writer(writer)
                    .init();
                return Some(guard);
            }
            Err(e) => {
                eprintln!(
                    "[agent-container] failed to open log file {path}: {e}; falling back to stderr"
                );
            }
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
    None
}

#[tokio::main]
async fn main() -> Result<()> {
    let _log_guard = init_tracing();

    let cli = Cli::parse();
    match cli.command {
        Commands::Run { agent, passthrough } => run_cmd(agent, passthrough).await,
        Commands::Shell { passthrough } => shell_cmd(passthrough).await,
        Commands::Config { command } => match command {
            ConfigCommands::Mcp => config_cmd::run().await,
        },
    }
}

async fn run_cmd(agent: AgentKind, passthrough: Vec<String>) -> Result<()> {
    let host = paths::HostPaths::detect()?;

    // Host-side discovery — always performed so broker/sync can populate
    // correctly regardless of which agent is the session primary.
    let bedrock = aws::detect_setup(&host.claude_root.join("settings.json"))
        .context("failed to read Bedrock settings from ~/.claude/settings.json")?;
    let refresh = aws::detect_refresh_command(&host.home.join(".claude.json"))
        .context("failed to read awsAuthRefresh from ~/.claude.json")?;
    let mcp_servers = mcp::load_servers(&host.home.join(".claude.json"))
        .context("failed to load MCP servers from ~/.claude.json")?;
    let policy = policy::McpPolicy::load().context(
        "failed to load MCP allowlist policy; fix or remove ~/.config/agent-container/mcp.toml",
    )?;
    let oauth_store = Arc::new(oauth::OAuthStore::new(
        oauth::load_from_keychain()
            .context("failed to load MCP OAuth entries from Keychain")?,
    ));

    if let Some(setup) = &bedrock {
        eprintln!(
            "[agent-container] Bedrock mode detected (profile={}); the container will fetch fresh AWS credentials on demand through the host broker.",
            setup.profile
        );
    }
    if !mcp_servers.is_empty() {
        let labels: Vec<_> = mcp_servers
            .iter()
            .map(|s| format!("{}({})", s.name(), s.transport_label()))
            .collect();
        eprintln!(
            "[agent-container] proxying {} MCP server(s) through broker: {}",
            mcp_servers.len(),
            labels.join(", ")
        );
    }

    // Always attempt to materialise both agents' auth so that whichever
    // agent runs as primary, the other can still be invoked from inside
    // (e.g. Claude's bash tool calling `codex exec ...` or vice versa).
    let claude_is_primary = matches!(agent, AgentKind::Claude);
    let codex_is_primary = matches!(agent, AgentKind::Codex);
    let claude_creds =
        prepare_claude_credentials(&host, claude_is_primary, bedrock.is_some())?;
    let codex_auth = prepare_codex_auth(&host, codex_is_primary)?;

    docker::ensure_images(&docker::default_dockerfile_dir())
        .await
        .context("failed to build or locate container images")?;

    let broker = server::spawn(
        bedrock.clone().map(|b| (b, refresh.clone())),
        mcp_servers.clone(),
        policy,
        oauth_store.clone(),
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

    codex::write_container_config(&host.home, &host.container_home)
        .context("failed to write codex config.toml into container home")?;

    let credentials_path = claude_creds
        .as_ref()
        .map(|c| c.path.clone())
        .unwrap_or_else(|| PathBuf::from("/dev/null"));
    let codex_auth_path = codex_auth
        .as_ref()
        .map(|c| c.path.clone())
        .unwrap_or_else(|| PathBuf::from("/dev/null"));

    let agent_command = match agent {
        AgentKind::Claude => vec![
            "claude".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ],
        AgentKind::Codex => vec!["codex".to_string()],
    };

    let exit = docker::run(docker::RunOptions {
        host,
        credentials_path,
        codex_auth_path,
        bedrock_setup: bedrock,
        broker_addr: broker.addr,
        agent_command,
        extra_args: passthrough,
    })
    .await?;

    broker.handle.abort();
    drop(claude_creds);
    drop(codex_auth);
    std::process::exit(exit);
}

async fn shell_cmd(passthrough: Vec<String>) -> Result<()> {
    let host = paths::HostPaths::detect()?;

    // Discovery is the same as `run`, except we downgrade every auth
    // failure to a warning — if the user is dropping into a shell it's
    // usually to debug something and blocking on missing credentials
    // would be counterproductive.
    let bedrock = aws::detect_setup(&host.claude_root.join("settings.json")).ok().flatten();
    let refresh = aws::detect_refresh_command(&host.home.join(".claude.json"))
        .ok()
        .flatten();
    let mcp_servers = mcp::load_servers(&host.home.join(".claude.json")).unwrap_or_default();
    let policy = policy::McpPolicy::load().unwrap_or_default();
    let oauth_store = Arc::new(oauth::OAuthStore::new(
        oauth::load_from_keychain().unwrap_or_default(),
    ));

    let claude_creds = match creds::prepare(&host.claude_root) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("[agent-container] note: Claude credentials unavailable: {e:#}");
            None
        }
    };
    let codex_auth = match codex::prepare_auth(&host.home) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("[agent-container] note: Codex auth unavailable: {e:#}");
            None
        }
    };

    docker::ensure_images(&docker::default_dockerfile_dir())
        .await
        .context("failed to build or locate container images")?;

    let broker = server::spawn(
        bedrock.clone().map(|b| (b, refresh.clone())),
        mcp_servers.clone(),
        policy,
        oauth_store,
    )
    .await?;
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

    codex::write_container_config(&host.home, &host.container_home)
        .context("failed to write codex config.toml into container home")?;

    let credentials_path = claude_creds
        .as_ref()
        .map(|c| c.path.clone())
        .unwrap_or_else(|| PathBuf::from("/dev/null"));
    let codex_auth_path = codex_auth
        .as_ref()
        .map(|c| c.path.clone())
        .unwrap_or_else(|| PathBuf::from("/dev/null"));

    let agent_command = if passthrough.is_empty() {
        vec!["bash".to_string(), "-l".to_string()]
    } else {
        // Join the passthrough into a single `bash -lc "cmd"` so quoting
        // works the way users expect from a normal interactive shell.
        let joined = passthrough.join(" ");
        vec!["bash".to_string(), "-lc".to_string(), joined]
    };

    let exit = docker::run(docker::RunOptions {
        host,
        credentials_path,
        codex_auth_path,
        bedrock_setup: bedrock,
        broker_addr: broker.addr,
        agent_command,
        extra_args: Vec::new(),
    })
    .await?;

    broker.handle.abort();
    drop(claude_creds);
    drop(codex_auth);
    std::process::exit(exit);
}

fn prepare_claude_credentials(
    host: &paths::HostPaths,
    primary: bool,
    has_bedrock: bool,
) -> Result<Option<creds::CredentialFile>> {
    match creds::prepare(&host.claude_root) {
        Ok(c) => {
            if c.is_expired() {
                eprintln!(
                    "[agent-container] warning: host Claude credentials appear expired; refresh them with `claude /login` before running if the container cannot refresh on its own."
                );
            }
            Ok(Some(c))
        }
        Err(e) if !primary => {
            eprintln!(
                "[agent-container] note: Claude credentials unavailable; the in-container 'claude' binary will fail until `claude /login` is run on the host: {e:#}"
            );
            Ok(None)
        }
        Err(e) if has_bedrock => {
            eprintln!(
                "[agent-container] note: skipping Anthropic credentials (using Bedrock): {e:#}"
            );
            Ok(None)
        }
        Err(e) => Err(e).context(
            "failed to prepare Claude OAuth credentials; run `claude /login` on the host first",
        ),
    }
}

fn prepare_codex_auth(
    host: &paths::HostPaths,
    primary: bool,
) -> Result<Option<codex::CodexAuthFile>> {
    match codex::prepare_auth(&host.home) {
        Ok(f) => Ok(Some(f)),
        Err(e) if !primary => {
            eprintln!(
                "[agent-container] note: Codex auth unavailable; the in-container 'codex' binary will fail until `codex login` is run on the host: {e:#}"
            );
            Ok(None)
        }
        Err(e) => Err(e).context(
            "failed to prepare Codex auth; run `codex login` on the host first",
        ),
    }
}
