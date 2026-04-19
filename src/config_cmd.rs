//! `agent-container config mcp` — interactively review the host's MCP
//! servers and toggle which of their tools the container may see.

use anyhow::{Context, Result};
use inquire::MultiSelect;

use crate::mcp::HttpMcpServer;
use crate::mcp_client::{Tool, fetch_tools};
use crate::paths::HostPaths;
use crate::policy::{McpPolicy, config_path};

pub async fn run() -> Result<()> {
    let host = HostPaths::detect()?;
    let servers = crate::mcp::load_http_servers(&host.home.join(".claude.json"))
        .context("failed to load MCP servers from ~/.claude.json")?;

    if servers.is_empty() {
        println!(
            "No HTTP/SSE MCP servers are declared in ~/.claude.json; nothing to configure."
        );
        return Ok(());
    }

    let mut policy = McpPolicy::load().context("failed to load existing MCP allowlist")?;

    for server in &servers {
        configure_server(server, &mut policy).await?;
    }

    let path = policy.save().context("failed to save MCP allowlist")?;
    println!();
    println!("Saved to {}", path.display());
    println!(
        "Broker checks this on startup; re-run `agent-container run` to pick up changes."
    );
    Ok(())
}

async fn configure_server(server: &HttpMcpServer, policy: &mut McpPolicy) -> Result<()> {
    println!();
    println!("── {} ──", server.name);
    println!("   upstream: {}", server.url);

    println!("   fetching tools/list from upstream...");
    let tools = match fetch_tools(server).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "   ✗ could not fetch tools from '{}': {e:#}\n     skipping; existing policy kept intact.",
                server.name
            );
            return Ok(());
        }
    };
    if tools.is_empty() {
        println!("   (no tools advertised)");
        return Ok(());
    }

    let options: Vec<ToolRow> = tools
        .iter()
        .map(|t| ToolRow::new(&server.name, t, policy))
        .collect();
    let defaults: Vec<usize> = options
        .iter()
        .enumerate()
        .filter_map(|(i, row)| row.currently_enabled.then_some(i))
        .collect();

    let selection = MultiSelect::new(
        &format!("tools to expose from '{}'", server.name),
        options.clone(),
    )
    .with_default(&defaults)
    .with_help_message("↑/↓ move, space to toggle, enter to accept, esc to skip")
    .prompt();

    let chosen = match selection {
        Ok(rows) => rows,
        Err(inquire::InquireError::OperationCanceled)
        | Err(inquire::InquireError::OperationInterrupted) => {
            println!("   (skipped)");
            return Ok(());
        }
        Err(e) => return Err(anyhow::anyhow!(e).context("MCP tool selection failed")),
    };

    let chosen_names: std::collections::BTreeSet<&str> =
        chosen.iter().map(|r| r.tool.as_str()).collect();

    for row in &options {
        let enabled = chosen_names.contains(row.tool.as_str());
        if enabled == row.annotation_default {
            // Matches the annotation's default — drop any explicit entry so
            // the config file only records real deviations.
            if let Some(server_policy) = policy.servers.get_mut(&server.name) {
                server_policy.tools.remove(&row.tool);
            }
        } else {
            policy.set_tool(&server.name, &row.tool, enabled);
        }
    }
    policy.set_server_enabled(&server.name, true);

    Ok(())
}

#[derive(Clone)]
struct ToolRow {
    tool: String,
    description: String,
    annotation: String,
    currently_enabled: bool,
    annotation_default: bool,
}

impl ToolRow {
    fn new(server: &str, tool: &Tool, policy: &McpPolicy) -> Self {
        let ro = tool.read_only_hint();
        let annotation = match ro {
            Some(true) => "read-only",
            Some(false) => "write/destructive",
            None => "no annotation",
        };
        let annotation_default = ro.unwrap_or(false);
        let currently_enabled = policy.tool_allowed(server, &tool.name, ro);
        Self {
            tool: tool.name.clone(),
            description: tool
                .description
                .clone()
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string(),
            annotation: annotation.to_string(),
            currently_enabled,
            annotation_default,
        }
    }
}

impl std::fmt::Display for ToolRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.description.is_empty() {
            write!(f, "{:<40}  [{}]", self.tool, self.annotation)
        } else {
            let desc = if self.description.len() > 60 {
                format!("{}…", &self.description[..60])
            } else {
                self.description.clone()
            };
            write!(f, "{:<40}  [{}]  {}", self.tool, self.annotation, desc)
        }
    }
}

#[allow(dead_code)]
fn _config_path_fn() -> anyhow::Result<std::path::PathBuf> {
    config_path()
}
