//! Read the host's MCP server declarations, classify them by transport,
//! and expose the HTTP/SSE ones via the broker.

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

/// Read HTTP/SSE MCP server definitions from the top-level `mcpServers`
/// in `~/.claude.json`. stdio servers are ignored for now.
pub fn load_http_servers(claude_json: &Path) -> Result<Vec<HttpMcpServer>> {
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
                tracing::debug!(name, "skipping non-HTTP MCP server");
            }
            Err(e) => {
                tracing::warn!(name, error = %e, "failed to parse MCP server entry; skipping");
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
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
}

fn parse_entry(name: &str, value: &Value) -> Result<Option<HttpMcpServer>> {
    let entry: RawEntry =
        serde_json::from_value(value.clone()).context("entry is not a valid MCP server object")?;

    // Decide transport. Claude Code infers stdio when `command` is present
    // and no `type` is set; http/sse require a URL.
    let transport = match entry.transport.as_deref() {
        Some(t) => t.to_ascii_lowercase(),
        None => {
            if entry.command.is_some() {
                return Ok(None);
            }
            "http".to_string()
        }
    };

    if transport != "http" && transport != "sse" {
        return Ok(None);
    }
    let Some(url) = entry.url else {
        return Ok(None);
    };
    if url.is_empty() {
        return Ok(None);
    }

    Ok(Some(HttpMcpServer {
        name: name.to_string(),
        transport,
        url,
        headers: entry.headers,
    }))
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
    fn loads_http_servers_skipping_stdio() {
        let f = write(
            r#"{
              "mcpServers": {
                "web": {"type": "http", "url": "https://example.com/mcp",
                         "headers": {"Authorization": "Bearer xxx"}},
                "fs": {"type": "stdio", "command": "node", "args": ["srv.js"]},
                "legacy-sse": {"type": "sse", "url": "https://old.example/mcp"},
                "implicit-stdio": {"command": "ls"},
                "broken": {}
              }
            }"#,
        );
        let servers = load_http_servers(f.path()).unwrap();
        let names: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["legacy-sse", "web"]);
        let web = servers.iter().find(|s| s.name == "web").unwrap();
        assert_eq!(web.transport, "http");
        assert_eq!(web.url, "https://example.com/mcp");
        assert_eq!(web.headers.get("Authorization").map(String::as_str), Some("Bearer xxx"));
    }

    #[test]
    fn empty_when_no_mcp_servers() {
        let f = write(r#"{"hasCompletedOnboarding": true}"#);
        assert!(load_http_servers(f.path()).unwrap().is_empty());
    }

    #[test]
    fn missing_file_is_fine() {
        let p = std::env::temp_dir().join("definitely-missing-claude.json");
        assert!(load_http_servers(&p).unwrap().is_empty());
    }
}
