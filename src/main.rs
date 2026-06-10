mod aws;
mod cli;
mod codex;
mod config_cmd;
mod container_notice;
mod creds;
mod docker;
mod host_fs;
mod host_kind;
mod keychain;
mod mcp;
mod mcp_auth;
mod mcp_client;
mod mcp_recovery;
mod oauth;
mod paths;
mod policy;
mod proxy_allowlist;
mod server;
mod settings;
mod shared_cred;
mod staging_archive;
mod stdio_mcp;
mod sync;
mod task_runner;
mod tui;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::cli::{AgentKind, Cli, Commands, ConfigCommands, McpCommands};

struct AgentBrokerSet {
    claude: server::RunningServer,
    codex: server::RunningServer,
    claude_url_from_container: String,
    codex_url_from_container: String,
}

struct BrokerInputs<'a> {
    host: &'a paths::HostPaths,
    bedrock: Option<(aws::BedrockSetup, Option<String>)>,
    claude_mcp_servers: Vec<mcp::McpServer>,
    codex_mcp_servers: Vec<mcp::McpServer>,
    claude_policy: policy::McpPolicy,
    codex_policy: policy::McpPolicy,
    claude_task_runner: Option<task_runner::TaskRunner>,
    codex_task_runner: Option<task_runner::TaskRunner>,
    claude_host_fs: Option<host_fs::HostFs>,
    codex_host_fs: Option<host_fs::HostFs>,
    oauth_store: Arc<oauth::OAuthStore>,
    claude_task_runner_enabled: bool,
    codex_task_runner_enabled: bool,
}

struct AgentConfigSync<'a> {
    host: &'a paths::HostPaths,
    bedrock: Option<&'a aws::BedrockSetup>,
    claude_broker_url: &'a str,
    codex_broker_url: &'a str,
    claude_mcp_servers: &'a [mcp::McpServer],
    codex_mcp_servers: &'a [mcp::McpServer],
    claude_task_runner_enabled: bool,
    codex_task_runner_enabled: bool,
    claude_host_fs_enabled: bool,
    codex_host_fs_enabled: bool,
}

struct AgentServices {
    claude_policy: policy::McpPolicy,
    codex_policy: policy::McpPolicy,
    proxy_allow: Vec<String>,
    filesystem: settings::FilesystemPolicy,
    claude_task_runner: Option<task_runner::TaskRunner>,
    codex_task_runner: Option<task_runner::TaskRunner>,
    claude_host_fs: Option<host_fs::HostFs>,
    codex_host_fs: Option<host_fs::HostFs>,
    claude_task_runner_enabled: bool,
    codex_task_runner_enabled: bool,
    claude_host_fs_enabled: bool,
    codex_host_fs_enabled: bool,
}

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
            tmux,
            passthrough,
        } => run_cmd(agent, rebuild_image, tmux, passthrough).await,
        Commands::Shell {
            rebuild_image,
            passthrough,
        } => shell_cmd(rebuild_image, passthrough).await,
        Commands::Exec {
            rebuild_image,
            passthrough,
        } => exec_cmd(rebuild_image, passthrough).await,
        Commands::Config {
            command,
            global,
            workspace,
            editor,
        } => dispatch_config(command, global, workspace, editor).await,
        Commands::Mcp { agent, command } => dispatch_mcp(agent, command).await,
    }
}

async fn dispatch_mcp(agent: Option<AgentKind>, command: McpCommands) -> Result<()> {
    let host = paths::HostPaths::detect()?;
    let agent = match agent {
        Some(agent) => agent,
        None => {
            let merged_settings = settings::Settings::load_merged(&host.workspace)
                .context("failed to load agent-container settings (global + workspace)")?;
            agent_kind_from_default(merged_settings.general.default_agent())
        }
    };
    match command {
        McpCommands::List => mcp_list_cmd(&host, agent).await,
        McpCommands::Auth { server } => mcp_auth_cmd(&host, agent, &server).await,
    }
}

async fn mcp_list_cmd(host: &paths::HostPaths, agent: AgentKind) -> Result<()> {
    let servers = load_agent_mcp_servers(host, agent)?;
    let settings = settings::Settings::load_merged(&host.workspace)
        .context("failed to load agent-container settings (global + workspace)")?;
    let policy = match agent {
        AgentKind::Claude => &settings.claude_code.mcp,
        AgentKind::Codex => &settings.codex.mcp,
    };
    let oauth_entries = oauth::load_from_keychain().unwrap_or_else(|e| {
        eprintln!("[agent-container] warning: failed to read MCP OAuth records: {e:#}");
        std::collections::HashMap::new()
    });

    println!("MCP servers for {agent:?}:");
    if servers.is_empty() {
        println!("  (none)");
        return Ok(());
    }

    for server in servers {
        println!("  {}", server.name());
        println!("    Transport: {}", server.transport_label());
        match &server {
            mcp::McpServer::Http(http) => {
                println!("    URL: {}", http.url);
                println!("    Auth: {}", mcp_auth_status(http, &oauth_entries));
            }
            mcp::McpServer::Stdio(stdio) => {
                println!("    Command: {}", stdio_command_summary(stdio));
                println!("    Auth: Unsupported");
            }
        }
        println!("    Policy: {}", mcp_policy_status(policy, server.name()));
    }
    Ok(())
}

async fn mcp_auth_cmd(host: &paths::HostPaths, agent: AgentKind, server_name: &str) -> Result<()> {
    let servers = load_agent_mcp_servers(host, agent)?;
    let server = servers
        .iter()
        .find(|server| server.name() == server_name)
        .with_context(|| format!("MCP server '{server_name}' is not declared for {agent:?}"))?;
    let mcp::McpServer::Http(server) = server else {
        bail!("MCP server '{server_name}' uses stdio; MCP OAuth applies only to HTTP transports");
    };
    mcp_auth::authenticate(server).await
}

fn load_agent_mcp_servers(
    host: &paths::HostPaths,
    agent: AgentKind,
) -> Result<Vec<mcp::McpServer>> {
    match agent {
        AgentKind::Claude => mcp::load_servers(&host.home.join(".claude.json"))
            .context("failed to load MCP servers from ~/.claude.json"),
        AgentKind::Codex => mcp::load_codex_servers(&host.home.join(".codex/config.toml"))
            .context("failed to load MCP servers from ~/.codex/config.toml"),
    }
}

fn mcp_auth_status(
    server: &mcp::HttpMcpServer,
    oauth_entries: &std::collections::HashMap<String, oauth::McpOAuthEntry>,
) -> String {
    if server
        .headers
        .keys()
        .any(|key| key.eq_ignore_ascii_case("authorization"))
    {
        return "Static Authorization header".to_string();
    }
    let Some(entry) = oauth_entries.get(&server.name) else {
        return "OAuth not logged in".to_string();
    };
    let Some(expires_at) = entry.expires_at_ms else {
        return "OAuth token present".to_string();
    };
    let remaining = expires_at - oauth::now_ms();
    if remaining <= 0 {
        return "OAuth expired".to_string();
    }
    format!("OAuth valid for {}", format_duration_ms(remaining))
}

fn mcp_policy_status(policy: &policy::McpPolicy, server: &str) -> String {
    let Some(server_policy) = policy.servers.get(server) else {
        return "default".to_string();
    };
    let status = if server_policy.enabled {
        "enabled"
    } else {
        "disabled"
    };
    if server_policy.tools.is_empty() {
        status.to_string()
    } else {
        format!("{status}, {} tool override(s)", server_policy.tools.len())
    }
}

fn stdio_command_summary(server: &mcp::StdioMcpServer) -> String {
    std::iter::once(server.command.as_str())
        .chain(server.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_duration_ms(ms: i64) -> String {
    let seconds = (ms / 1000).max(0);
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
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

async fn spawn_agent_brokers(inputs: BrokerInputs<'_>) -> Result<AgentBrokerSet> {
    let BrokerInputs {
        host,
        bedrock,
        claude_mcp_servers,
        codex_mcp_servers,
        claude_policy,
        codex_policy,
        claude_task_runner,
        codex_task_runner,
        claude_host_fs,
        codex_host_fs,
        oauth_store,
        claude_task_runner_enabled,
        codex_task_runner_enabled,
    } = inputs;

    let stdio_bridge = stdio_mcp::PathBridge {
        container_root: host.container_workspace().display().to_string(),
        host_root: host.workspace.display().to_string(),
    };
    let claude = server::spawn(
        bedrock,
        claude_mcp_servers,
        claude_task_runner,
        claude_host_fs,
        claude_policy,
        oauth_store.clone(),
        Some(stdio_bridge),
        Some(server::McpReloadConfig {
            workspace: host.workspace.clone(),
            task_runner_enabled: claude_task_runner_enabled,
            policy_scope: server::McpPolicyScope::ClaudeCode,
        }),
    )
    .await?;
    tracing::info!(addr = %claude.addr, "Claude Code broker listening");

    let codex = server::spawn(
        None,
        codex_mcp_servers,
        codex_task_runner,
        codex_host_fs,
        codex_policy,
        oauth_store,
        Some(stdio_mcp::PathBridge {
            container_root: host.container_workspace().display().to_string(),
            host_root: host.workspace.display().to_string(),
        }),
        Some(server::McpReloadConfig {
            workspace: host.workspace.clone(),
            task_runner_enabled: codex_task_runner_enabled,
            policy_scope: server::McpPolicyScope::Codex,
        }),
    )
    .await?;
    tracing::info!(addr = %codex.addr, "Codex broker listening");

    let host_kind = host_kind::HostKind::detect()
        .context("failed to detect Docker engine flavour for broker hostname")?;
    let claude_url_from_container = format!(
        "http://{}:{}",
        host_kind.broker_host_name(),
        claude.addr.port()
    );
    let codex_url_from_container = format!(
        "http://{}:{}",
        host_kind.broker_host_name(),
        codex.addr.port()
    );
    tracing::info!(?host_kind, broker_url = %claude_url_from_container, "broker URL for container");

    Ok(AgentBrokerSet {
        claude,
        codex,
        claude_url_from_container,
        codex_url_from_container,
    })
}

fn sync_agent_configs(input: AgentConfigSync<'_>) -> Result<()> {
    sync::sync_host_state(
        input.host,
        sync::SyncOptions {
            bedrock: input.bedrock,
            broker_url_from_container: input.claude_broker_url,
            mcp_servers: input.claude_mcp_servers,
            task_runner_enabled: input.claude_task_runner_enabled,
            host_fs_enabled: input.claude_host_fs_enabled,
        },
    )
    .context("failed to sync host Claude Code state into container")?;

    codex::write_container_config(
        &input.host.home,
        &input.host.staged_home,
        input.codex_broker_url,
        input.codex_mcp_servers,
        input.codex_task_runner_enabled,
        input.codex_host_fs_enabled,
    )
    .context("failed to write codex config.toml into container home")?;
    Ok(())
}

fn build_agent_services(
    host: &paths::HostPaths,
    settings: &settings::Settings,
    task_runner_tasks: &BTreeMap<String, task_runner::TaskSpec>,
    claude_mcp_servers: &[mcp::McpServer],
    codex_mcp_servers: &[mcp::McpServer],
) -> AgentServices {
    let claude_task_runner = build_task_runner(task_runner_tasks, claude_mcp_servers);
    let codex_task_runner = build_task_runner(task_runner_tasks, codex_mcp_servers);
    let claude_host_fs = build_host_fs(&host.workspace, claude_mcp_servers);
    let codex_host_fs = build_host_fs(&host.workspace, codex_mcp_servers);

    AgentServices {
        claude_policy: settings.claude_code.mcp.clone(),
        codex_policy: settings.codex.mcp.clone(),
        proxy_allow: settings.proxy.allow.clone(),
        filesystem: settings.filesystem.clone(),
        claude_task_runner_enabled: claude_task_runner.is_some(),
        codex_task_runner_enabled: codex_task_runner.is_some(),
        claude_host_fs_enabled: claude_host_fs.is_some(),
        codex_host_fs_enabled: codex_host_fs.is_some(),
        claude_task_runner,
        codex_task_runner,
        claude_host_fs,
        codex_host_fs,
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

async fn run_cmd(
    agent_override: Option<AgentKind>,
    rebuild_image: bool,
    tmux: bool,
    passthrough: Vec<String>,
) -> Result<()> {
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
    let claude_mcp_servers = mcp::load_servers(&host.home.join(".claude.json"))
        .context("failed to load MCP servers from ~/.claude.json")?;
    let codex_mcp_servers = mcp::load_codex_servers(&host.home.join(".codex/config.toml"))
        .context("failed to load MCP servers from ~/.codex/config.toml")?;
    let merged_settings = settings::Settings::load_merged(&host.workspace)
        .context("failed to load agent-container settings (global + workspace)")?;
    let agent = agent_override
        .unwrap_or_else(|| agent_kind_from_default(merged_settings.general.default_agent()));
    let task_runner_tasks = task_runner::load_specs_from_settings(&host.workspace)?;
    let services = build_agent_services(
        &host,
        &merged_settings,
        &task_runner_tasks,
        &claude_mcp_servers,
        &codex_mcp_servers,
    );
    let oauth_store = Arc::new(oauth::OAuthStore::new(
        oauth::load_from_keychain().context("failed to load MCP OAuth entries from Keychain")?,
    ));

    if let Some(setup) = &bedrock {
        eprintln!(
            "[agent-container] Bedrock mode detected (profile={}); the container will fetch fresh AWS credentials on demand through the host broker.",
            setup.profile
        );
    }
    if !claude_mcp_servers.is_empty() {
        let labels: Vec<_> = claude_mcp_servers
            .iter()
            .map(|s| format!("{}({})", s.name(), s.transport_label()))
            .collect();
        eprintln!(
            "[agent-container] proxying {} MCP server(s) through broker: {}",
            claude_mcp_servers.len(),
            labels.join(", ")
        );
    }
    if !codex_mcp_servers.is_empty() {
        let labels: Vec<_> = codex_mcp_servers
            .iter()
            .map(|s| format!("{}({})", s.name(), s.transport_label()))
            .collect();
        eprintln!(
            "[agent-container] proxying {} Codex MCP server(s) through broker: {}",
            codex_mcp_servers.len(),
            labels.join(", ")
        );
    }
    if let Some(runner) = &services.claude_task_runner {
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

    let brokers = spawn_agent_brokers(BrokerInputs {
        host: &host,
        bedrock: bedrock.clone().map(|b| (b, refresh.clone())),
        claude_mcp_servers: claude_mcp_servers.clone(),
        codex_mcp_servers: codex_mcp_servers.clone(),
        claude_policy: services.claude_policy.clone(),
        codex_policy: services.codex_policy.clone(),
        claude_task_runner: services.claude_task_runner,
        codex_task_runner: services.codex_task_runner,
        claude_host_fs: services.claude_host_fs,
        codex_host_fs: services.codex_host_fs,
        oauth_store: oauth_store.clone(),
        claude_task_runner_enabled: services.claude_task_runner_enabled,
        codex_task_runner_enabled: services.codex_task_runner_enabled,
    })
    .await?;

    sync_agent_configs(AgentConfigSync {
        host: &host,
        bedrock: bedrock.as_ref(),
        claude_broker_url: &brokers.claude_url_from_container,
        codex_broker_url: &brokers.codex_url_from_container,
        claude_mcp_servers: &claude_mcp_servers,
        codex_mcp_servers: &codex_mcp_servers,
        claude_task_runner_enabled: services.claude_task_runner_enabled,
        codex_task_runner_enabled: services.codex_task_runner_enabled,
        claude_host_fs_enabled: services.claude_host_fs_enabled,
        codex_host_fs_enabled: services.codex_host_fs_enabled,
    })?;

    let credentials_path = claude_creds
        .as_ref()
        .map(|c| c.path.clone())
        .unwrap_or_else(|| PathBuf::from("/dev/null"));
    let codex_auth_path = codex_auth
        .as_ref()
        .map(|c| c.path.clone())
        .unwrap_or_else(|| PathBuf::from("/dev/null"));
    let codex_history = codex::prepare_history_mounts(&host.home)
        .context("failed to prepare Codex history mounts")?;

    let agent_command = agent_command(agent, tmux, merged_settings.claude.tmux_prefix())?;

    let exit = docker::run(docker::RunOptions {
        host,
        credentials_path,
        codex_auth_path,
        codex_history,
        bedrock_setup: bedrock,
        broker_url_from_container: brokers.claude_url_from_container.clone(),
        agent_command,
        extra_args: passthrough,
        proxy_allow: services.proxy_allow,
        filesystem: services.filesystem,
    })
    .await?;

    brokers.claude.handle.abort();
    brokers.codex.handle.abort();
    drop(claude_creds);
    drop(codex_auth);
    std::process::exit(exit);
}

fn agent_kind_from_default(agent: settings::DefaultAgent) -> AgentKind {
    match agent {
        settings::DefaultAgent::Claude => AgentKind::Claude,
        settings::DefaultAgent::Codex => AgentKind::Codex,
    }
}

fn agent_command(agent: AgentKind, tmux: bool, tmux_prefix: &str) -> Result<Vec<String>> {
    match (agent, tmux) {
        (AgentKind::Claude, false) => Ok(vec![
            "claude".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ]),
        (AgentKind::Codex, false) => Ok(vec!["codex".to_string()]),
        (AgentKind::Claude, true) => claude_tmux_agent_command(tmux_prefix),
        (AgentKind::Codex, true) => codex_tmux_agent_command(tmux_prefix),
    }
}

fn claude_tmux_agent_command(tmux_prefix: &str) -> Result<Vec<String>> {
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

fn codex_tmux_agent_command(tmux_prefix: &str) -> Result<Vec<String>> {
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
            "new-session -A -s codex -- codex \"$@\"",
        ]
        .join(" "),
        "agent-container-codex".to_string(),
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
    let claude_mcp_servers = mcp::load_servers(&host.home.join(".claude.json")).unwrap_or_default();
    let codex_mcp_servers =
        mcp::load_codex_servers(&host.home.join(".codex/config.toml")).unwrap_or_default();
    let merged_settings = settings::Settings::load_merged(&host.workspace).unwrap_or_default();
    let task_runner_tasks =
        task_runner::load_specs_from_settings(&host.workspace).unwrap_or_default();
    let services = build_agent_services(
        &host,
        &merged_settings,
        &task_runner_tasks,
        &claude_mcp_servers,
        &codex_mcp_servers,
    );
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

    let brokers = spawn_agent_brokers(BrokerInputs {
        host: &host,
        bedrock: bedrock.clone().map(|b| (b, refresh.clone())),
        claude_mcp_servers: claude_mcp_servers.clone(),
        codex_mcp_servers: codex_mcp_servers.clone(),
        claude_policy: services.claude_policy.clone(),
        codex_policy: services.codex_policy.clone(),
        claude_task_runner: services.claude_task_runner,
        codex_task_runner: services.codex_task_runner,
        claude_host_fs: services.claude_host_fs,
        codex_host_fs: services.codex_host_fs,
        oauth_store,
        claude_task_runner_enabled: services.claude_task_runner_enabled,
        codex_task_runner_enabled: services.codex_task_runner_enabled,
    })
    .await?;

    sync_agent_configs(AgentConfigSync {
        host: &host,
        bedrock: bedrock.as_ref(),
        claude_broker_url: &brokers.claude_url_from_container,
        codex_broker_url: &brokers.codex_url_from_container,
        claude_mcp_servers: &claude_mcp_servers,
        codex_mcp_servers: &codex_mcp_servers,
        claude_task_runner_enabled: services.claude_task_runner_enabled,
        codex_task_runner_enabled: services.codex_task_runner_enabled,
        claude_host_fs_enabled: services.claude_host_fs_enabled,
        codex_host_fs_enabled: services.codex_host_fs_enabled,
    })?;

    let credentials_path = claude_creds
        .as_ref()
        .map(|c| c.path.clone())
        .unwrap_or_else(|| PathBuf::from("/dev/null"));
    let codex_auth_path = codex_auth
        .as_ref()
        .map(|c| c.path.clone())
        .unwrap_or_else(|| PathBuf::from("/dev/null"));
    let codex_history = codex::prepare_history_mounts(&host.home)
        .context("failed to prepare Codex history mounts")?;

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
        codex_history,
        bedrock_setup: bedrock,
        broker_url_from_container: brokers.claude_url_from_container.clone(),
        agent_command,
        extra_args: Vec::new(),
        proxy_allow: services.proxy_allow,
        filesystem: services.filesystem,
    })
    .await?;

    brokers.claude.handle.abort();
    brokers.codex.handle.abort();
    drop(claude_creds);
    drop(codex_auth);
    std::process::exit(exit);
}

async fn exec_cmd(rebuild_image: bool, passthrough: Vec<String>) -> Result<()> {
    if passthrough.is_empty() {
        bail!("agent-container exec requires a command after `--`");
    }
    shell_cmd(rebuild_image, passthrough).await
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
/// tables still register an empty MCP server so a running session can
/// discover tasks added later via settings reload.
fn build_task_runner(
    tasks: &std::collections::BTreeMap<String, task_runner::TaskSpec>,
    declared_servers: &[mcp::McpServer],
) -> Option<task_runner::TaskRunner> {
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

fn build_host_fs(
    workspace: &std::path::Path,
    declared_servers: &[mcp::McpServer],
) -> Option<host_fs::HostFs> {
    if declared_servers.iter().any(|s| s.name() == host_fs::NAME) {
        eprintln!(
            "[agent-container] note: skipping built-in host-fs because ~/.claude.json already declares an MCP server named '{}'",
            host_fs::NAME
        );
        return None;
    }
    Some(host_fs::HostFs::new(workspace.to_path_buf()))
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

    #[test]
    fn claude_runs_directly_by_default() {
        let command = agent_command(AgentKind::Claude, false, "C-q;touch").unwrap();

        assert_eq!(command, ["claude", "--dangerously-skip-permissions"]);
    }

    #[test]
    fn codex_runs_directly_by_default() {
        let command = agent_command(AgentKind::Codex, false, "C-q;touch").unwrap();

        assert_eq!(command, ["codex"]);
    }

    #[test]
    fn claude_runs_in_tmux_with_mouse_and_configured_prefix() {
        let command = agent_command(AgentKind::Claude, true, "C-q").unwrap();
        let script = &command[2];

        assert!(script.contains("set-option -g mouse on"));
        assert!(script.contains("set-option -g prefix C-q"));
        assert!(script.contains("bind-key C-q send-prefix"));
        assert!(script.contains("new-session -A -s claude-code"));
    }

    #[test]
    fn codex_runs_in_tmux_with_mouse_and_configured_prefix() {
        let command = agent_command(AgentKind::Codex, true, "C-q").unwrap();
        let script = &command[2];

        assert!(script.contains("set-option -g mouse on"));
        assert!(script.contains("set-option -g prefix C-q"));
        assert!(script.contains("bind-key C-q send-prefix"));
        assert!(script.contains("new-session -A -s codex"));
    }

    #[test]
    fn claude_tmux_prefix_rejects_shell_metacharacters() {
        let err = agent_command(AgentKind::Claude, true, "C-q;touch").unwrap_err();
        assert!(format!("{err:#}").contains("invalid claude.tmux_prefix"));
    }
}
