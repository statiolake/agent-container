//! `agent-container config …` — scope-aware settings editor.
//!
//! - `config show [--global|--workspace]` prints TOML.
//! - `config [--global|--workspace]` opens the ratatui editor.
//! - `config [--global|--workspace] --editor` opens `$EDITOR` on the
//!   settings file directly.
//!
//! Scope flags select the file to *write* (or, for `show`, the file to
//! read in isolation). Without flags, writes default to workspace and
//! `show` defaults to the merged view — matching VS Code semantics where
//! the workspace is the usual place to pin project-specific overrides.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc;

use anyhow::{Context, Result, bail};

use crate::mcp::{self, McpServer};
use crate::oauth::{OAuthStore, load_from_keychain};
use crate::paths::HostPaths;
use crate::policy::McpPolicy;
use crate::settings::{self, Scope, Settings, TaskDefinition};
use crate::tui::{
    self, McpAgent, McpCatalogCommand, McpCatalogEvent, McpServerEntry, Outcome, ToolEntry,
    TuiInput,
};

pub fn stdio_is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Resolve scope flags to a concrete [`Scope`], defaulting to workspace.
/// The flags are already mutually exclusive at the clap layer.
pub fn resolve_scope(global: bool, _workspace: bool) -> Scope {
    if global {
        Scope::Global
    } else {
        Scope::Workspace
    }
}

/// Same as [`resolve_scope`] but returns `None` when neither flag is
/// set — used by `show` to mean "print the merged view".
pub fn resolve_scope_opt(global: bool, workspace: bool) -> Option<Scope> {
    if global {
        Some(Scope::Global)
    } else if workspace {
        Some(Scope::Workspace)
    } else {
        None
    }
}

/// Entry point for the scope-aware TUI editor.
pub async fn run_editor(initial_scope: Scope) -> Result<()> {
    let host = HostPaths::detect()?;

    let claude_servers = mcp::load_servers(&host.home.join(".claude.json"), &host.workspace)
        .context("failed to load MCP servers from ~/.claude.json")?;
    let codex_servers = mcp::load_codex_servers(&host.home.join(".codex/config.toml"))
        .context("failed to load MCP servers from ~/.codex/config.toml")?;

    let oauth = Arc::new(OAuthStore::new(
        load_from_keychain().context("failed to load MCP OAuth entries from Keychain")?,
    ));

    // Load both scope files up-front so the TUI can switch between them
    // without re-entering. `merged` drives the MCP tool-row enabled bit
    // so the UI reflects what actually takes effect at runtime.
    let global_settings = Settings::load_scope(Scope::Global, &host.workspace)
        .context("failed to load global settings")?;
    let workspace_settings = Settings::load_scope(Scope::Workspace, &host.workspace)
        .context("failed to load workspace settings")?;
    let merged = Settings::load_merged(&host.workspace)
        .context("failed to load agent-container settings")?;

    if claude_servers.is_empty() {
        eprintln!(
            "[agent-container] note: no MCP servers declared in ~/.claude.json top-level or current project; the Claude Code MCP tab will be empty."
        );
    }
    if codex_servers.is_empty() {
        eprintln!(
            "[agent-container] note: no MCP servers declared in ~/.codex/config.toml; the Codex MCP tab will be empty."
        );
    }

    let (event_tx, event_rx) = mpsc::channel();
    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
    spawn_mcp_catalog_workers(
        claude_servers.clone(),
        codex_servers.clone(),
        oauth.clone(),
        event_tx,
        command_rx,
    );

    // The TUI keeps two complete McpPolicy / tasks views in memory and
    // edits the active scope's view directly. Keep a copy of the catalog
    // here so the post-save minimisation can inspect every (server,
    // tool) pair regardless of which scope the user wound up saving.
    let input = TuiInput {
        workspace: host.workspace.clone(),
        initial_scope,
        general_global: global_settings.general.clone(),
        general_workspace: workspace_settings.general.clone(),
        claude_global: global_settings.claude.clone(),
        claude_workspace: workspace_settings.claude.clone(),
        proxy_allow_global: global_settings.proxy.allow.clone(),
        proxy_allow_workspace: workspace_settings.proxy.allow.clone(),
        filesystem_global: global_settings.filesystem.clone(),
        filesystem_workspace: workspace_settings.filesystem.clone(),
        claude_servers: server_entries(&claude_servers),
        codex_servers: server_entries(&codex_servers),
        claude_tool_catalog: Vec::new(),
        codex_tool_catalog: Vec::new(),
        mcp_events: Some(event_rx),
        mcp_commands: Some(command_tx),
        mcp_global: global_settings.claude_code.mcp.clone(),
        mcp_workspace: workspace_settings.claude_code.mcp.clone(),
        codex_mcp_global: global_settings.codex.mcp.clone(),
        codex_mcp_workspace: workspace_settings.codex.mcp.clone(),
        tasks_global: global_settings.task_runner.tasks.clone(),
        tasks_workspace: workspace_settings.task_runner.tasks.clone(),
    };
    let _ = merged; // formerly drove the per-row enabled bit; now per-scope.

    match tui::run_selection(input)? {
        Outcome::Save(out) => {
            let out = *out;
            let scopes = if out.save_both_scopes {
                vec![Scope::Global, Scope::Workspace]
            } else {
                vec![out.saved_scope]
            };
            for scope in scopes {
                let path = save_tui_scope(&out, scope, &host, &global_settings)?;
                println!("Saved to {} ({:?} scope)", path.display(), scope);
            }
            println!("Re-run `agent-container run` to pick up changes.");
        }
        Outcome::Cancel => {
            println!("Cancelled; settings file unchanged.");
        }
    }

    Ok(())
}

/// Save one layer of the TUI output while preserving unrelated settings in
/// that layer. When a scope move happened, the workspace layer must be
/// minimised against the final Global buffers from the same TUI session;
/// otherwise a promoted task or MCP override could be immediately written
/// back as a redundant workspace entry.
fn save_tui_scope(
    out: &tui::TuiOutput,
    scope: Scope,
    host: &HostPaths,
    global_settings: &Settings,
) -> Result<PathBuf> {
    let default_mcp = McpPolicy::default();
    let default_tasks = BTreeMap::new();
    let (base_mcp, base_codex_mcp, base_tasks) = match scope {
        Scope::Global => (&default_mcp, &default_mcp, &default_tasks),
        Scope::Workspace if out.save_both_scopes => {
            (&out.mcp_global, &out.codex_mcp_global, &out.tasks_global)
        }
        Scope::Workspace => (
            &global_settings.claude_code.mcp,
            &global_settings.codex.mcp,
            &global_settings.task_runner.tasks,
        ),
    };

    // Load the target scope fresh (not merged) so untouched sections of its
    // settings.toml survive this save verbatim.
    let mut target = Settings::load_scope(scope, &host.workspace)
        .context("failed to reload target-scope settings for save")?;
    target.proxy.allow = match scope {
        Scope::Global => out.proxy_allow_global.clone(),
        Scope::Workspace => out.proxy_allow_workspace.clone(),
    };
    target.general = match scope {
        Scope::Global => out.general_global.clone(),
        Scope::Workspace => out.general_workspace.clone(),
    };
    target.claude = match scope {
        Scope::Global => out.claude_global.clone(),
        Scope::Workspace => out.claude_workspace.clone(),
    };
    target.filesystem = match scope {
        Scope::Global => out.filesystem_global.clone(),
        Scope::Workspace => out.filesystem_workspace.clone(),
    };
    target.claude_code.mcp = match scope {
        Scope::Global => out.mcp_global.clone(),
        Scope::Workspace => out.mcp_workspace.clone(),
    };
    minimise_policy_against_base(
        &mut target.claude_code.mcp,
        base_mcp,
        &out.claude_tool_catalog,
    );
    target.codex.mcp = match scope {
        Scope::Global => out.codex_mcp_global.clone(),
        Scope::Workspace => out.codex_mcp_workspace.clone(),
    };
    minimise_policy_against_base(
        &mut target.codex.mcp,
        base_codex_mcp,
        &out.codex_tool_catalog,
    );
    let edited_tasks = match scope {
        Scope::Global => out.tasks_global.clone(),
        Scope::Workspace => out.tasks_workspace.clone(),
    };
    target.task_runner.tasks = minimise_tasks_against_base(edited_tasks, base_tasks);

    let path = settings::path(scope, &host.workspace)?;
    target.save_to(&path).context("failed to save settings")?;
    Ok(path)
}

/// `config show` — print the settings TOML for the requested scope (or
/// the merged view when `scope` is `None`).
pub fn run_show(scope: Option<Scope>) -> Result<()> {
    let host = HostPaths::detect()?;
    let (label, settings) = match scope {
        Some(s) => {
            let path = settings::path(s, &host.workspace)?;
            let loaded = Settings::load_scope(s, &host.workspace)
                .with_context(|| format!("failed to read {}", path.display()))?;
            (format!("# {:?} ({})", s, path.display()), loaded)
        }
        None => (
            "# merged (global ∪ workspace)".to_string(),
            Settings::load_merged(&host.workspace)?,
        ),
    };
    let raw = toml::to_string_pretty(&settings).context("failed to serialize settings")?;
    println!("{label}");
    print!("{raw}");
    if !raw.ends_with('\n') {
        println!();
    }
    Ok(())
}

/// `config --editor` — open the target scope's settings.toml in `$EDITOR`.
///
/// Creates the file (with a brief template comment) if it does not exist
/// so the editor has something to show. Validates the file on save so a
/// typo in TOML doesn't silently brick the next `run`.
pub fn run_open_in_editor(scope: Scope) -> Result<()> {
    let host = HostPaths::detect()?;
    let path = settings::path(scope, &host.workspace)?;

    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&path, template_for(scope))
            .with_context(|| format!("failed to create {}", path.display()))?;
    }

    let editor = std::env::var("EDITOR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "vi".to_string());

    // $EDITOR may be a compound command (e.g. `code -w`) so we hand it to
    // a shell, then rely on `"$@"` to pass the path as a single argument
    // regardless of spaces. `sh -c 'cmd "$@"' -- <path>` is the portable
    // idiom here.
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg("--")
        .arg(&path)
        .status()
        .with_context(|| format!("failed to spawn editor `{editor}`"))?;
    if !status.success() {
        bail!("editor `{editor}` exited with {status}");
    }

    // Validate the TOML on the way out so a mistyped key is flagged now
    // rather than at the next `agent-container run`.
    if let Err(e) = Settings::load_from(&path) {
        eprintln!(
            "[agent-container] warning: {} does not parse as valid settings — fix it before the next `run`: {e:#}",
            path.display()
        );
    } else {
        println!("Saved {}", path.display());
    }
    Ok(())
}

fn template_for(scope: Scope) -> String {
    let header = match scope {
        Scope::Global => {
            "# agent-container global settings\n# Applies to every workspace unless overridden locally.\n"
        }
        Scope::Workspace => {
            "# agent-container workspace settings\n# Merged on top of the global settings at runtime.\n"
        }
    };
    format!(
        "{header}\n# Uncomment examples below.\n# [general]\n# default_agent = \"codex\"\n# bedrock_region = \"ap-northeast-1\"\n\n# [proxy]\n# allow = [\"^my-internal\\\\.example$\"]\n\n# [filesystem]\n# mounts = [{{ path = \"/Users/me/project-notes\", readonly = true }}]\n# hide = [\"(^|/)\\\\.env(\\\\..*)?$\"]\n# readonly = [\"(^|/)\\\\.claude(/|$)\"]\n\n# Claude Code MCP policy:\n# [claude_code.mcp.servers.github]\n# enabled = true\n# [claude_code.mcp.servers.github.tools]\n# list_issues = true\n# create_issue = false\n\n# Codex MCP policy:\n# [codex.mcp.servers.local-tools.tools]\n# search = true\n# mutate = false\n\n# [claude]\n# tmux_prefix = \"C-b\"\n# skip_bypass_permissions_warning = false\n"
    )
}

fn server_entries(servers: &[McpServer]) -> Vec<McpServerEntry> {
    servers
        .iter()
        .map(|server| McpServerEntry {
            name: server.name().to_string(),
            transport: server.transport_label().to_string(),
        })
        .collect()
}

fn spawn_mcp_catalog_workers(
    claude_servers: Vec<McpServer>,
    codex_servers: Vec<McpServer>,
    oauth: Arc<OAuthStore>,
    events: mpsc::Sender<McpCatalogEvent>,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<McpCatalogCommand>,
) {
    for server in claude_servers.iter().cloned() {
        spawn_fetch(McpAgent::Claude, server, oauth.clone(), events.clone());
    }
    for server in codex_servers.iter().cloned() {
        spawn_fetch(McpAgent::Codex, server, oauth.clone(), events.clone());
    }

    tokio::spawn(async move {
        while let Some(command) = commands.recv().await {
            match command {
                McpCatalogCommand::Reload { agent, server_name } => {
                    if let Some(server) =
                        find_server(agent, &server_name, &claude_servers, &codex_servers)
                    {
                        spawn_fetch(agent, server, oauth.clone(), events.clone());
                    }
                }
                McpCatalogCommand::Auth { agent, server_name } => {
                    let Some(server) =
                        find_server(agent, &server_name, &claude_servers, &codex_servers)
                    else {
                        continue;
                    };
                    let events = events.clone();
                    let oauth = oauth.clone();
                    tokio::spawn(async move {
                        let _ = events.send(McpCatalogEvent::Loading {
                            agent,
                            server_name: server.name().to_string(),
                        });
                        if let McpServer::Http(http) = &server
                            && let Err(e) = crate::mcp_auth::authenticate(http).await
                        {
                            let _ = events.send(McpCatalogEvent::Failed {
                                agent,
                                server_name: server.name().to_string(),
                                message: format!("{e:#}"),
                                can_auth: true,
                            });
                            return;
                        }
                        spawn_fetch(agent, server, oauth, events);
                    });
                }
            }
        }
    });
}

fn find_server(
    agent: McpAgent,
    server_name: &str,
    claude_servers: &[McpServer],
    codex_servers: &[McpServer],
) -> Option<McpServer> {
    let servers = match agent {
        McpAgent::Claude => claude_servers,
        McpAgent::Codex => codex_servers,
    };
    servers
        .iter()
        .find(|server| server.name() == server_name)
        .cloned()
}

fn spawn_fetch(
    agent: McpAgent,
    server: McpServer,
    oauth: Arc<OAuthStore>,
    events: mpsc::Sender<McpCatalogEvent>,
) {
    tokio::spawn(async move {
        let server_name = server.name().to_string();
        let can_auth = matches!(server, McpServer::Http(_));
        let _ = events.send(McpCatalogEvent::Loading {
            agent,
            server_name: server_name.clone(),
        });
        match crate::mcp_client::fetch_tools_any_with_timeout(
            &server,
            &oauth,
            crate::mcp_client::DEFAULT_FETCH_TIMEOUT,
        )
        .await
        {
            Ok(tools) => {
                let entries = tools
                    .into_iter()
                    .map(|tool| {
                        let read_only_hint = tool.read_only_hint();
                        ToolEntry {
                            server_name: server_name.clone(),
                            tool_name: tool.name,
                            description: tool.description.unwrap_or_default(),
                            read_only_hint,
                        }
                    })
                    .collect();
                let _ = events.send(McpCatalogEvent::Loaded {
                    agent,
                    server_name,
                    tools: entries,
                });
            }
            Err(e) => {
                let _ = events.send(McpCatalogEvent::Failed {
                    agent,
                    server_name,
                    message: format!("{e:#}"),
                    can_auth,
                });
            }
        }
    });
}

/// Strip task entries from `final_tasks` whose value matches what the
/// scope would inherit from the `base` layer. Keeps the target scope's
/// `[task_runner.tasks]` sparse — workspace files only carry overrides,
/// never redundant copies of global tasks.
fn minimise_tasks_against_base(
    mut final_tasks: BTreeMap<String, TaskDefinition>,
    base: &BTreeMap<String, TaskDefinition>,
) -> BTreeMap<String, TaskDefinition> {
    final_tasks.retain(|name, task| base.get(name).map(|b| b != task).unwrap_or(true));
    final_tasks
}

/// Strip per-tool entries from `target` that match what the scope would
/// inherit from `base` (`McpPolicy::default()` for Global; the global
/// policy when saving Workspace). Then drop servers whose `tools` map
/// is empty *and* whose `enabled` field also matches the base, so the
/// scope file stays as sparse as possible.
fn minimise_policy_against_base(target: &mut McpPolicy, base: &McpPolicy, catalog: &[ToolEntry]) {
    for entry in catalog {
        let Some(sp) = target.servers.get_mut(&entry.server_name) else {
            continue;
        };
        let Some(target_state) = sp.tools.get(&entry.tool_name).copied() else {
            continue;
        };
        let base_state =
            base.tool_allowed(&entry.server_name, &entry.tool_name, entry.read_only_hint);
        if target_state == base_state {
            sp.tools.remove(&entry.tool_name);
        }
    }
    target.servers.retain(|name, sp| {
        if !sp.tools.is_empty() {
            return true;
        }
        let base_enabled = base.servers.get(name).map(|b| b.enabled).unwrap_or(true);
        sp.enabled != base_enabled
    });
}
