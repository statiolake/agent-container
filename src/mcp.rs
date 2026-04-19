//! Read the host's MCP server declarations and classify them by transport.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct HttpMcpServer {
    pub name: String,
    /// `http` or `sse` (kept verbatim so the injected container config
    /// matches what Claude Code on the host sees).
    pub transport: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct StdioMcpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub enum McpServer {
    Http(HttpMcpServer),
    Stdio(StdioMcpServer),
}

impl McpServer {
    pub fn name(&self) -> &str {
        match self {
            McpServer::Http(s) => &s.name,
            McpServer::Stdio(s) => &s.name,
        }
    }

    pub fn transport_label(&self) -> &str {
        match self {
            McpServer::Http(s) => s.transport.as_str(),
            McpServer::Stdio(_) => "stdio",
        }
    }
}

/// Read every MCP server definition out of the top-level `mcpServers` key
/// of `~/.claude.json`. Entries the parser cannot classify are logged and
/// skipped rather than returned as errors.
pub fn load_servers(claude_json: &Path) -> Result<Vec<McpServer>> {
    if !claude_json.is_file() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(claude_json)
        .with_context(|| format!("failed to read {}", claude_json.display()))?;
    let cfg: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {} as JSON", claude_json.display()))?;

    let Some(map) = cfg.get("mcpServers").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    for (name, value) in map {
        match parse_entry(name, value) {
            Ok(Some(server)) => out.push(server),
            Ok(None) => {
                tracing::debug!(name, "skipping unrecognised MCP server entry");
            }
            Err(e) => {
                tracing::warn!(name, error = %e, "failed to parse MCP server entry; skipping");
            }
        }
    }
    out.sort_by(|a, b| a.name().cmp(b.name()));
    Ok(out)
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(default, rename = "type")]
    transport: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

fn parse_entry(name: &str, value: &Value) -> Result<Option<McpServer>> {
    let entry: RawEntry =
        serde_json::from_value(value.clone()).context("entry is not a valid MCP server object")?;

    // Decide transport. Claude Code infers stdio when `command` is present
    // and no `type` is set; http/sse require a URL.
    let transport = match entry.transport.as_deref() {
        Some(t) => t.to_ascii_lowercase(),
        None => {
            if entry.command.is_some() {
                "stdio".to_string()
            } else {
                "http".to_string()
            }
        }
    };

    match transport.as_str() {
        "stdio" => {
            let Some(command) = entry.command else {
                return Ok(None);
            };
            Ok(Some(McpServer::Stdio(StdioMcpServer {
                name: name.to_string(),
                command,
                args: entry.args,
                env: entry.env,
            })))
        }
        "http" | "sse" => {
            let Some(url) = entry.url else {
                return Ok(None);
            };
            if url.is_empty() {
                return Ok(None);
            }
            Ok(Some(McpServer::Http(HttpMcpServer {
                name: name.to_string(),
                transport,
                url,
                headers: entry.headers,
            })))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(json: &str) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        fs::write(f.path(), json).unwrap();
        f
    }

    #[test]
    fn loads_both_http_and_stdio_servers() {
        let f = write(
            r#"{
              "mcpServers": {
                "web": {"type": "http", "url": "https://example.com/mcp",
                         "headers": {"Authorization": "Bearer xxx"}},
                "fs": {"type": "stdio", "command": "node", "args": ["srv.js"]},
                "legacy-sse": {"type": "sse", "url": "https://old.example/mcp"},
                "implicit-stdio": {"command": "ls", "args": ["/tmp"]},
                "broken": {}
              }
            }"#,
        );
        let servers = load_servers(f.path()).unwrap();
        let pairs: Vec<_> = servers
            .iter()
            .map(|s| (s.name(), s.transport_label()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("fs", "stdio"),
                ("implicit-stdio", "stdio"),
                ("legacy-sse", "sse"),
                ("web", "http"),
            ]
        );
    }

#[test]
    fn empty_when_no_mcp_servers() {
        let f = write(r#"{"hasCompletedOnboarding": true}"#);
        assert!(load_servers(f.path()).unwrap().is_empty());
    }

    #[test]
    fn missing_file_is_fine() {
        let p = std::env::temp_dir().join("definitely-missing-claude.json");
        assert!(load_servers(&p).unwrap().is_empty());
    }
}
