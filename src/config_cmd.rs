//! `agent-container config …` — scope-aware settings editor.
//!
//! - `config show [--global|--workspace]` prints TOML.
//! - `config [--global|--workspace]` opens the ratatui editor.
//! - `config [--global|--workspace] --editor` opens `$EDITOR` on the
//!    settings file directly.
//!
//! Scope flags select the file to *write* (or, for `show`, the file to
//! read in isolation). Without flags, writes default to workspace and
//! `show` defaults to the merged view — matching VS Code semantics where
//! the workspace is the usual place to pin project-specific overrides.

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::mcp::{self, McpServer};
use crate::mcp_client::{Tool, fetch_tools, fetch_tools_stdio};
use crate::oauth::{OAuthStore, load_from_keychain};
use crate::paths::HostPaths;
use crate::policy::McpPolicy;
use crate::settings::{self, Scope, Settings};
use crate::tui::{self, Outcome, ToolEntry, TuiInput};

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

    let servers = mcp::load_servers(&host.home.join(".claude.json"))
        .context("failed to load MCP servers from ~/.claude.json")?;

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

    let mut entries: Vec<ToolEntry> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    if servers.is_empty() {
        eprintln!("[agent-container] note: no MCP servers declared in ~/.claude.json; the MCP tab will be empty.");
    } else {
        println!("Fetching tools from {} MCP server(s)...", servers.len());
        for server in &servers {
            let name = server.name().to_string();
            use std::io::Write;
            print!("  {} ({})...", name, server.transport_label());
            std::io::stdout().flush().ok();
            match fetch_any(server, &oauth).await {
                Ok(tools) => {
                    println!(" {} tool(s)", tools.len());
                    for tool in tools {
                        let read_only_hint = tool.read_only_hint();
                        entries.push(ToolEntry {
                            server_name: name.clone(),
                            tool_name: tool.name,
                            description: tool.description.unwrap_or_default(),
                            read_only_hint,
                        });
                    }
                }
                Err(e) => {
                    println!(" FAILED ({e:#})");
                    skipped.push((name, format!("{e:#}")));
                }
            }
        }
        entries.sort_by(|a, b| {
            a.server_name
                .cmp(&b.server_name)
                .then_with(|| a.tool_name.cmp(&b.tool_name))
        });
    }

    // The TUI keeps two complete McpPolicy / tasks views in memory and
    // edits the active scope's view directly. Keep a copy of the catalog
    // here so the post-save minimisation can inspect every (server,
    // tool) pair regardless of which scope the user wound up saving.
    let catalog = entries.clone();
    let input = TuiInput {
        initial_scope,
        proxy_allow_global: global_settings.proxy.allow.clone(),
        proxy_allow_workspace: workspace_settings.proxy.allow.clone(),
        tool_catalog: entries,
        mcp_global: global_settings.mcp.clone(),
        mcp_workspace: workspace_settings.mcp.clone(),
        tasks_global: global_settings.task_runner.tasks.clone(),
        tasks_workspace: workspace_settings.task_runner.tasks.clone(),
    };
    let _ = merged; // formerly drove the per-row enabled bit; now per-scope.

    match tui::run_selection(input)? {
        Outcome::Save(out) => {
            let saved_scope = out.saved_scope;
            // Base for MCP/task minimisation is the *other* scope. For
            // Global there is no lower layer, so base falls back to the
            // policy default.
            let (base_mcp, base_tasks) = match saved_scope {
                Scope::Workspace => (
                    global_settings.mcp.clone(),
                    global_settings.task_runner.tasks.clone(),
                ),
                Scope::Global => (McpPolicy::default(), BTreeMap::new()),
            };
            // Load the target scope fresh (not merged) so untouched sections
            // of its settings.toml survive this save verbatim.
            let mut target = Settings::load_scope(saved_scope, &host.workspace)
                .context("failed to reload target-scope settings for save")?;
            target.proxy.allow = match saved_scope {
                Scope::Global => out.proxy_allow_global,
                Scope::Workspace => out.proxy_allow_workspace,
            };
            target.mcp = match saved_scope {
                Scope::Global => out.mcp_global,
                Scope::Workspace => out.mcp_workspace,
            };
            minimise_policy_against_base(&mut target.mcp, &base_mcp, &catalog);
            let edited_tasks = match saved_scope {
                Scope::Global => out.tasks_global,
                Scope::Workspace => out.tasks_workspace,
            };
            target.task_runner.tasks = minimise_tasks_against_base(edited_tasks, &base_tasks);
            let path = settings::path(saved_scope, &host.workspace)?;
            target.save_to(&path).context("failed to save settings")?;
            println!("Saved to {} ({:?} scope)", path.display(), saved_scope);
            if !skipped.is_empty() {
                println!(
                    "Skipped {} server(s); their existing policy entries were not touched:",
                    skipped.len()
                );
                for (name, err) in &skipped {
                    println!("  {name}: {err}");
                }
            }
            println!("Re-run `agent-container run` to pick up changes.");
        }
        Outcome::Cancel => {
            println!("Cancelled; settings file unchanged.");
        }
    }

    Ok(())
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
        Scope::Global => "# agent-container global settings\n# Applies to every workspace unless overridden locally.\n",
        Scope::Workspace => "# agent-container workspace settings\n# Merged on top of the global settings at runtime.\n",
    };
    format!(
        "{header}\n# Uncomment examples below.\n# [proxy]\n# allow = [\"^my-internal\\\\.example$\"]\n\n# [mcp.servers.github]\n# enabled = true\n# [mcp.servers.github.tools]\n# list_issues = true\n# create_issue = false\n"
    )
}

async fn fetch_any(server: &McpServer, oauth: &OAuthStore) -> Result<Vec<Tool>> {
    match server {
        McpServer::Http(h) => {
            let bearer = oauth.access_token(&h.name).await.unwrap_or(None);
            fetch_tools(h, bearer.as_deref()).await
        }
        McpServer::Stdio(s) => fetch_tools_stdio(s).await,
    }
}

/// Strip task entries from `final_tasks` whose value matches what the
/// scope would inherit from the `base` layer. Keeps the target scope's
/// `[task_runner.tasks]` sparse — workspace files only carry overrides,
/// never redundant copies of global tasks.
fn minimise_tasks_against_base(
    mut final_tasks: BTreeMap<String, String>,
    base: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    final_tasks.retain(|name, cmd| base.get(name).map(|b| b != cmd).unwrap_or(true));
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
        let base_state = base.tool_allowed(&entry.server_name, &entry.tool_name, entry.read_only_hint);
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
