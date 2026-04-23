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

use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::mcp::{self, McpServer};
use crate::mcp_client::{Tool, fetch_tools, fetch_tools_stdio};
use crate::oauth::{OAuthStore, load_from_keychain};
use crate::paths::HostPaths;
use crate::policy::McpPolicy;
use crate::settings::{self, Scope, Settings};
use crate::tui::{self, Outcome, ToolEntry};

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
pub async fn run_editor(scope: Scope) -> Result<()> {
    let host = HostPaths::detect()?;

    let servers = mcp::load_servers(&host.home.join(".claude.json"))
        .context("failed to load MCP servers from ~/.claude.json")?;

    if servers.is_empty() {
        println!("No MCP servers declared in ~/.claude.json; nothing to configure.");
        return Ok(());
    }

    let oauth = Arc::new(OAuthStore::new(
        load_from_keychain().context("failed to load MCP OAuth entries from Keychain")?,
    ));

    // Display starts from the merged view so users see what is effectively
    // active; writes land in the chosen scope only.
    let policy: McpPolicy = Settings::load_merged(&host.workspace)
        .context("failed to load agent-container settings")?
        .mcp;

    println!("Fetching tools from {} MCP server(s)...", servers.len());
    let mut entries: Vec<ToolEntry> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
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
                    let enabled = policy.tool_allowed(&name, &tool.name, read_only_hint);
                    entries.push(ToolEntry {
                        server_name: name.clone(),
                        tool_name: tool.name,
                        description: tool.description.unwrap_or_default(),
                        read_only_hint,
                        enabled,
                    });
                }
            }
            Err(e) => {
                println!(" FAILED ({e:#})");
                skipped.push((name, format!("{e:#}")));
            }
        }
    }

    if entries.is_empty() {
        eprintln!("No tools to configure.");
        return Ok(());
    }

    entries.sort_by(|a, b| {
        a.server_name
            .cmp(&b.server_name)
            .then_with(|| a.tool_name.cmp(&b.tool_name))
    });

    match tui::run_selection(entries)? {
        Outcome::Save(entries) => {
            // Load the *target scope* (not merged) so we don't accidentally
            // promote a global entry into the workspace file just because
            // it happened to be the effective value.
            let mut scoped = Settings::load_scope(scope, &host.workspace)
                .context("failed to load target-scope settings for save")?;
            apply_entries(&mut scoped.mcp, &entries);
            let path = settings::path(scope, &host.workspace)?;
            scoped.save_to(&path).context("failed to save settings")?;
            println!("Saved to {} ({:?} scope)", path.display(), scope);
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

fn apply_entries(policy: &mut McpPolicy, entries: &[ToolEntry]) {
    use std::collections::BTreeSet;
    let servers: BTreeSet<&str> = entries.iter().map(|e| e.server_name.as_str()).collect();
    for server in &servers {
        policy.set_server_enabled(server, true);
    }

    for entry in entries {
        let annotation_default = entry.read_only_hint.unwrap_or(false);
        if entry.enabled == annotation_default {
            // Matches the annotation default; leave no explicit entry so
            // the toml stays minimal.
            if let Some(sp) = policy.servers.get_mut(&entry.server_name) {
                sp.tools.remove(&entry.tool_name);
            }
        } else {
            policy.set_tool(&entry.server_name, &entry.tool_name, entry.enabled);
        }
    }
}
