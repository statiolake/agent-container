mod aws;
mod cli;
mod codex;
mod config_cmd;
mod creds;
mod docker;
mod host_kind;
mod mcp;
mod mcp_client;
mod oauth;
mod paths;
mod policy;
mod proxy_allowlist;
mod server;
mod settings;
mod shared_cred;
mod stdio_mcp;
mod sync;
mod task_runner;
mod tui;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::{AgentKind, Cli, Commands, ConfigCommands};

/// Initialise the tracing subscriber.
///
/// Logs go to a file by default — `$XDG_STATE_HOME/agent-container/log`
/// on Linux, a platform-appropriate fallback elsewhere — because this
/// binary regularly shares the terminal with a TUI (Claude Code inside
/// the container, or our own `config` editor) and any stderr writes
/// during that would corrupt the rendered frame.
///
/// Override with `AGENT_CONTAINER_LOG_FILE`:
/// - a path → append there
/// - `-`    → opt back into stderr (handy for ad-hoc debugging)
fn init_tracing() -> (
    Option<tracing_appender::non_blocking::WorkerGuard>,
    Option<PathBuf>,
) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("agent_container=info,warn"));

    let env = std::env::var("AGENT_CONTAINER_LOG_FILE").ok();
    let destination: Option<PathBuf> = match env.as_deref() {
        Some("-") => None,
        Some(s) if !s.is_empty() => Some(PathBuf::from(s)),
        _ => default_log_path(),
    };

    if let Some(path) = destination {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
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
                return (Some(guard), Some(path));
            }
            Err(e) => {
                eprintln!(
                    "[agent-container] failed to open log file {}: {e}; falling back to stderr",
                    path.display()
                );
            }
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
    (None, None)
}

/// Resolve the default log file path: prefer XDG state (Linux), fall
/// back to XDG data-local (macOS/Windows, where `state_dir()` is None).
fn default_log_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "agent-container")?;
    let base = dirs
        .state_dir()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| dirs.data_local_dir().to_path_buf());
    Some(base.join("agent-container.log"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if should_explain_config_instead_of_tui(&cli) {
        eprintln!(
            "agent-container config opens an interactive TUI, but stdin/stdout is not a TTY; exiting."
        );
        return Ok(());
    }

    let (_log_guard, log_path) = init_tracing();
    if let Some(p) = &log_path {
        eprintln!(
            "[agent-container] logs: {} (set AGENT_CONTAINER_LOG_FILE=- to route to stderr instead)",
            p.display()
        );
    }

    match cli.command {
        Commands::Run {
            agent,
            rebuild_image,
            passthrough,
        } => run_cmd(agent, rebuild_image, passthrough).await,
        Commands::Shell {
            rebuild_image,
            passthrough,
        } => shell_cmd(rebuild_image, passthrough).await,
        Commands::Config {
            command,
            global,
            workspace,
            editor,
        } => dispatch_config(command, global, workspace, editor).await,
    }
}

async fn dispatch_config(
    command: Option<ConfigCommands>,
    global: bool,
    workspace: bool,
    editor: bool,
) -> Result<()> {
    match command {
        Some(ConfigCommands::Show {
            global: show_global,
            workspace: show_workspace,
        }) => config_cmd::run_show(config_cmd::resolve_scope_opt(show_global, show_workspace)),
        None => {
            let scope = config_cmd::resolve_scope(global, workspace);
            if editor {
                config_cmd::run_open_in_editor(scope)
            } else {
                config_cmd::run_editor(scope).await
            }
        }
    }
}

fn should_explain_config_instead_of_tui(cli: &Cli) -> bool {
    matches!(
        &cli.command,
        Commands::Config {
            command: None,
            editor: false,
            ..
        } if !config_cmd::stdio_is_interactive()
    )
}

async fn run_cmd(agent: AgentKind, rebuild_image: bool, passthrough: Vec<String>) -> Result<()> {
    let host = paths::HostPaths::detect()?;

    // Host-side discovery — always performed so broker/sync can populate
    // correctly regardless of which agent is the session primary.
    let bedrock = aws::detect_setup(&host.claude_root.join("settings.json"))
        .context("failed to read Bedrock settings from ~/.claude/settings.json")?;
    let refresh = aws::detect_refresh_command(
        &host.claude_root.join("settings.json"),
        &host.home.join(".claude.json"),
    )
    .context("failed to read awsAuthRefresh from ~/.claude/settings.json or ~/.claude.json")?;
    let mcp_servers = mcp::load_servers(&host.home.join(".claude.json"))
        .context("failed to load MCP servers from ~/.claude.json")?;
    let merged_settings = settings::Settings::load_merged(&host.workspace)
        .context("failed to load agent-container settings (global + workspace)")?;
    let policy = merged_settings.mcp.clone();
    let proxy_allow = merged_settings.proxy.allow.clone();
    let task_runner_tasks = load_task_runner_tasks(&host)?;
    let task_runner = build_task_runner(&task_runner_tasks, &mcp_servers);
    let oauth_store = Arc::new(oauth::OAuthStore::new(
        oauth::load_from_keychain().context("failed to load MCP OAuth entries from Keychain")?,
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
    if let Some(runner) = &task_runner {
        eprintln!(
            "[agent-container] task-runner MCP exposing {} task(s): {}",
            runner.tasks.len(),
            runner.tasks.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }

    // Always attempt to materialise both agents' auth so that whichever
    // agent runs as primary, the other can still be invoked from inside
    // (e.g. Claude's bash tool calling `codex exec ...` or vice versa).
    let claude_is_primary = matches!(agent, AgentKind::Claude);
    let codex_is_primary = matches!(agent, AgentKind::Codex);
    let claude_creds = prepare_claude_credentials(&host, claude_is_primary, bedrock.is_some())?;
    let codex_auth = prepare_codex_auth(&host, codex_is_primary)?;

    docker::ensure_images(&docker::default_dockerfile_dir(), rebuild_image)
        .await
        .context("failed to build or locate container images")?;

    let stdio_bridge = stdio_mcp::PathBridge {
        container_root: host.container_workspace().display().to_string(),
        host_root: host.workspace.display().to_string(),
    };
    let task_runner_enabled = task_runner.is_some();
    let broker = server::spawn(
        bedrock.clone().map(|b| (b, refresh.clone())),
        mcp_servers.clone(),
        task_runner,
        policy,
        oauth_store.clone(),
        Some(stdio_bridge),
    )
    .await?;
    tracing::info!(addr = %broker.addr, "host broker listening");
    let host_kind = host_kind::HostKind::detect()
        .context("failed to detect Docker engine flavour for broker hostname")?;
    let broker_url_from_container = format!(
        "http://{}:{}",
        host_kind.broker_host_name(),
        broker.addr.port()
    );
    tracing::info!(?host_kind, broker_url = %broker_url_from_container, "broker URL for container");

    sync::sync_host_state(
        &host,
        sync::SyncOptions {
            bedrock: bedrock.as_ref(),
            broker_url_from_container: &broker_url_from_container,
            mcp_servers: &mcp_servers,
            task_runner_enabled,
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
        AgentKind::Claude => claude_agent_command(merged_settings.claude.tmux_prefix())?,
        AgentKind::Codex => vec!["codex".to_string()],
    };

    let exit = docker::run(docker::RunOptions {
        host,
        credentials_path,
        codex_auth_path,
        bedrock_setup: bedrock,
        broker_url_from_container,
        agent_command,
        extra_args: passthrough,
        proxy_allow,
    })
    .await?;

    broker.handle.abort();
    drop(claude_creds);
    drop(codex_auth);
    std::process::exit(exit);
}

fn claude_agent_command(tmux_prefix: &str) -> Result<Vec<String>> {
    validate_tmux_key(tmux_prefix)?;
    Ok(vec![
        "sh".to_string(),
        "-lc".to_string(),
        [
            "exec tmux",
            "start-server \\;",
            "set-option -g mouse on \\;",
            &format!("set-option -g prefix {tmux_prefix} \\;"),
            &format!("bind-key {tmux_prefix} send-prefix \\;"),
            "new-session -A -s claude-code -- claude --dangerously-skip-permissions \"$@\"",
        ]
        .join(" "),
        "agent-container-claude".to_string(),
    ])
}

fn validate_tmux_key(key: &str) -> Result<()> {
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        anyhow::bail!(
            "invalid claude.tmux_prefix `{key}`; use a tmux key name such as `C-b` or `C-q`"
        );
    }
    Ok(())
}

async fn shell_cmd(rebuild_image: bool, passthrough: Vec<String>) -> Result<()> {
    let host = paths::HostPaths::detect()?;

    // Discovery is the same as `run`, except we downgrade every auth
    // failure to a warning — if the user is dropping into a shell it's
    // usually to debug something and blocking on missing credentials
    // would be counterproductive.
    let bedrock = aws::detect_setup(&host.claude_root.join("settings.json"))
        .ok()
        .flatten();
    let refresh = aws::detect_refresh_command(
        &host.claude_root.join("settings.json"),
        &host.home.join(".claude.json"),
    )
    .ok()
    .flatten();
    let mcp_servers = mcp::load_servers(&host.home.join(".claude.json")).unwrap_or_default();
    let merged_settings = settings::Settings::load_merged(&host.workspace).unwrap_or_default();
    let policy = merged_settings.mcp.clone();
    let proxy_allow = merged_settings.proxy.allow.clone();
    let task_runner_tasks = load_task_runner_tasks(&host).unwrap_or_default();
    let task_runner = build_task_runner(&task_runner_tasks, &mcp_servers);
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

    docker::ensure_images(&docker::default_dockerfile_dir(), rebuild_image)
        .await
        .context("failed to build or locate container images")?;

    let stdio_bridge = stdio_mcp::PathBridge {
        container_root: host.container_workspace().display().to_string(),
        host_root: host.workspace.display().to_string(),
    };
    let task_runner_enabled = task_runner.is_some();
    let broker = server::spawn(
        bedrock.clone().map(|b| (b, refresh.clone())),
        mcp_servers.clone(),
        task_runner,
        policy,
        oauth_store,
        Some(stdio_bridge),
    )
    .await?;
    let host_kind = host_kind::HostKind::detect()
        .context("failed to detect Docker engine flavour for broker hostname")?;
    let broker_url_from_container = format!(
        "http://{}:{}",
        host_kind.broker_host_name(),
        broker.addr.port()
    );

    sync::sync_host_state(
        &host,
        sync::SyncOptions {
            bedrock: bedrock.as_ref(),
            broker_url_from_container: &broker_url_from_container,
            mcp_servers: &mcp_servers,
            task_runner_enabled,
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
        broker_url_from_container,
        agent_command,
        extra_args: Vec::new(),
        proxy_allow,
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

/// Build the optional task-runner backend, skipping it if the user
/// already has an MCP server by the same name declared in
/// `~/.claude.json` (we'd clobber their setup otherwise). Empty task
/// tables resolve to None so the broker doesn't register an empty MCP.
fn build_task_runner(
    tasks: &std::collections::BTreeMap<String, task_runner::TaskSpec>,
    declared_servers: &[mcp::McpServer],
) -> Option<task_runner::TaskRunner> {
    if tasks.is_empty() {
        return None;
    }
    if declared_servers
        .iter()
        .any(|s| s.name() == task_runner::NAME)
    {
        eprintln!(
            "[agent-container] note: skipping built-in task-runner because ~/.claude.json already declares an MCP server named '{}'",
            task_runner::NAME
        );
        return None;
    }
    Some(task_runner::TaskRunner::new(tasks.clone()))
}

fn load_task_runner_tasks(
    host: &paths::HostPaths,
) -> Result<std::collections::BTreeMap<String, task_runner::TaskSpec>> {
    let global_path = settings::global_path()?;
    let global_root = global_path
        .parent()
        .context("global settings path has no parent")?
        .to_path_buf();
    let global = settings::Settings::load_global().context("failed to load global settings")?;
    let workspace_path = settings::workspace_path(&host.workspace);
    let workspace_root = workspace_path
        .parent()
        .context("workspace settings path has no parent")?
        .to_path_buf();
    let workspace = settings::Settings::load_workspace(&host.workspace)
        .context("failed to load workspace settings")?;

    Ok(task_specs_from_scopes(
        global.task_runner.tasks,
        global_root,
        workspace.task_runner.tasks,
        workspace_root,
    ))
}

fn task_specs_from_scopes(
    global_tasks: std::collections::BTreeMap<String, String>,
    global_root: PathBuf,
    workspace_tasks: std::collections::BTreeMap<String, String>,
    workspace_root: PathBuf,
) -> std::collections::BTreeMap<String, task_runner::TaskSpec> {
    let mut tasks = std::collections::BTreeMap::new();
    for (name, command) in global_tasks {
        tasks.insert(
            name,
            task_runner::TaskSpec {
                command,
                config_root: global_root.clone(),
            },
        );
    }
    for (name, command) in workspace_tasks {
        tasks.insert(
            name,
            task_runner::TaskSpec {
                command,
                config_root: workspace_root.clone(),
            },
        );
    }
    tasks
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
        Err(e) => {
            Err(e).context("failed to prepare Codex auth; run `codex login` on the host first")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn task_specs_use_host_config_roots_and_workspace_overrides() {
        let mut global = BTreeMap::new();
        global.insert(
            "deploy".to_string(),
            "$CONFIG_ROOT/scripts/deploy".to_string(),
        );
        global.insert("global-only".to_string(), "global".to_string());

        let mut workspace = BTreeMap::new();
        workspace.insert(
            "deploy".to_string(),
            "$CONFIG_ROOT/scripts/deploy-workspace".to_string(),
        );

        let specs = task_specs_from_scopes(
            global,
            PathBuf::from("/Users/example/.config/agent-container"),
            workspace,
            PathBuf::from("/Users/example/repo/.agent-container"),
        );

        assert_eq!(
            specs["deploy"].config_root,
            PathBuf::from("/Users/example/repo/.agent-container")
        );
        assert_eq!(
            specs["global-only"].config_root,
            PathBuf::from("/Users/example/.config/agent-container")
        );
    }

    #[test]
    fn claude_runs_in_tmux_with_mouse_and_configured_prefix() {
        let command = claude_agent_command("C-q").unwrap();
        let script = &command[2];

        assert!(script.contains("set-option -g mouse on"));
        assert!(script.contains("set-option -g prefix C-q"));
        assert!(script.contains("bind-key C-q send-prefix"));
        assert!(script.contains("new-session -A -s claude-code"));
    }

    #[test]
    fn claude_tmux_prefix_rejects_shell_metacharacters() {
        let err = claude_agent_command("C-q;touch").unwrap_err();
        assert!(format!("{err:#}").contains("invalid claude.tmux_prefix"));
    }
}
