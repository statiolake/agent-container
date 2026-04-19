//! `agent-container config mcp` — fetch every declared MCP server's
//! tools/list (HTTP/SSE/stdio alike) and hand the full catalogue to the
//! ratatui-based allowlist UI.

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::mcp::{self, McpServer};
use crate::mcp_client::{Tool, fetch_tools, fetch_tools_stdio};
use crate::oauth::{OAuthStore, load_from_keychain};
use crate::paths::HostPaths;
use crate::policy::McpPolicy;
use crate::tui::{self, Outcome, ToolEntry};

pub async fn run() -> Result<()> {
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
    let policy = McpPolicy::load().context("failed to load existing MCP allowlist")?;

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
            let mut policy = policy;
            apply_entries(&mut policy, &entries);
            let path = policy.save().context("failed to save MCP allowlist")?;
            println!("Saved to {}", path.display());
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
            println!("Cancelled; policy file unchanged.");
        }
    }

    Ok(())
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

// Touch the unused symbol so the config module continues to pull in
// policy::config_path (documented contract for callers who want to know
// where the allowlist lives).
#[allow(dead_code)]
fn _hint_config_path() -> anyhow::Result<std::path::PathBuf> {
    crate::policy::config_path()
}
