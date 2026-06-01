//! Read the host's MCP server declarations and classify them by transport.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
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

/// Read every MCP server definition out of Codex's `~/.codex/config.toml`
/// `[mcp_servers.<name>]` table. The shape mirrors `codex mcp add` output:
/// HTTP servers usually have `url`, while stdio servers have `command`,
/// `args`, and optional `env`.
pub fn load_codex_servers(codex_config: &Path) -> Result<Vec<McpServer>> {
    if !codex_config.is_file() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(codex_config)
        .with_context(|| format!("failed to read {}", codex_config.display()))?;
    let cfg: toml::Value = toml::from_str(&raw)
        .with_context(|| format!("failed to parse {} as TOML", codex_config.display()))?;

    let Some(map) = cfg.get("mcp_servers").and_then(toml::Value::as_table) else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    for (name, value) in map {
        match parse_toml_entry(name, value) {
            Ok(Some(server)) => out.push(server),
            Ok(None) => {
                tracing::debug!(name, "skipping unrecognised Codex MCP server entry");
            }
            Err(e) => {
                tracing::warn!(name, error = %e, "failed to parse Codex MCP server entry; skipping");
            }
        }
    }
    out.sort_by(|a, b| a.name().cmp(b.name()));
    Ok(out)
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(default, rename = "type", alias = "transport")]
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
    let mut entry: RawEntry =
        serde_json::from_value(value.clone()).context("entry is not a valid MCP server object")?;
    parse_raw_entry(name, &mut entry)
}

fn parse_toml_entry(name: &str, value: &toml::Value) -> Result<Option<McpServer>> {
    let mut entry: RawEntry = value
        .clone()
        .try_into()
        .context("entry is not a valid MCP server object")?;
    parse_raw_entry(name, &mut entry)
}

fn parse_raw_entry(name: &str, entry: &mut RawEntry) -> Result<Option<McpServer>> {
    expand_entry_env(entry).context("failed to expand environment variables")?;

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
            let Some(command) = entry.command.take() else {
                return Ok(None);
            };
            Ok(Some(McpServer::Stdio(StdioMcpServer {
                name: name.to_string(),
                command,
                args: std::mem::take(&mut entry.args),
                env: std::mem::take(&mut entry.env),
            })))
        }
        "http" | "sse" => {
            let Some(url) = entry.url.take() else {
                return Ok(None);
            };
            if url.is_empty() {
                return Ok(None);
            }
            Ok(Some(McpServer::Http(HttpMcpServer {
                name: name.to_string(),
                transport,
                url,
                headers: std::mem::take(&mut entry.headers),
            })))
        }
        _ => Ok(None),
    }
}

fn expand_entry_env(entry: &mut RawEntry) -> Result<()> {
    if let Some(command) = &mut entry.command {
        *command = expand_env_vars(command)?;
    }
    if let Some(url) = &mut entry.url {
        *url = expand_env_vars(url)?;
    }
    for arg in &mut entry.args {
        *arg = expand_env_vars(arg)?;
    }
    for value in entry.env.values_mut() {
        *value = expand_env_vars(value)?;
    }
    for value in entry.headers.values_mut() {
        *value = expand_env_vars(value)?;
    }
    Ok(())
}

fn expand_env_vars(input: &str) -> Result<String> {
    expand_env_vars_with(input, |name| std::env::var(name).ok())
}

fn expand_env_vars_with<F>(input: &str, lookup: F) -> Result<String>
where
    F: Fn(&str) -> Option<String>,
{
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let expr = &after[..end];
        let (name, default) = expr
            .split_once(":-")
            .map(|(name, default)| (name, Some(default)))
            .unwrap_or((expr, None));
        if !is_valid_env_name(name) {
            bail!("invalid environment variable reference `${{{expr}}}`");
        }
        let value = lookup(name)
            .or_else(|| default.map(str::to_string))
            .ok_or_else(|| anyhow::anyhow!("environment variable `{name}` is not set"))?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(json: &str) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        fs::write(f.path(), json).unwrap();
        f
    }

    fn write_toml(toml: &str) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        fs::write(f.path(), toml).unwrap();
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
    fn loads_codex_mcp_servers_from_toml() {
        let f = write_toml(
            r#"
[mcp_servers.web]
url = "https://example.com/mcp"

[mcp_servers.fs]
command = "node"
args = ["server.js"]
"#,
        );
        let servers = load_codex_servers(f.path()).unwrap();
        let pairs: Vec<_> = servers
            .iter()
            .map(|s| (s.name(), s.transport_label()))
            .collect();
        assert_eq!(pairs, vec![("fs", "stdio"), ("web", "http")]);
    }

    #[test]
    fn accepts_transport_alias_and_expands_env_placeholders() {
        let f = write(
            r#"{
              "mcpServers": {
                "aws-mcp": {
                  "transport": "stdio",
                  "command": "uvx",
                  "args": [
                    "mcp-proxy-for-aws@latest",
                    "${AWS_MCP_ENDPOINT:-https://aws-mcp.us-east-1.api.aws/mcp}",
                    "--metadata",
                    "AWS_REGION=${AWS_REGION:-us-west-2}"
                  ],
                  "env": {"AWS_PROFILE": "${AWS_PROFILE:-sandbox-bedrock}"}
                }
              }
            }"#,
        );
        let servers = load_servers(f.path()).unwrap();
        let McpServer::Stdio(server) = &servers[0] else {
            panic!("expected stdio server");
        };
        assert_eq!(server.command, "uvx");
        assert_eq!(server.args[1], "https://aws-mcp.us-east-1.api.aws/mcp");
        assert_eq!(server.args[3], "AWS_REGION=us-west-2");
        assert_eq!(server.env["AWS_PROFILE"], "sandbox-bedrock");
    }

    #[test]
    fn env_expansion_requires_value_without_default() {
        let err = expand_env_vars_with("${MISSING}", |_| None).unwrap_err();
        assert!(format!("{err:#}").contains("environment variable `MISSING` is not set"));
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
