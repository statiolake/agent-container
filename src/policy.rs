//! Per-tool MCP allowlist. The authoritative copy lives at
//! `$XDG_CONFIG/agent-container/mcp.toml` and is read/written by both the
//! broker (which enforces it at proxy time) and the `config mcp` TUI
//! (which toggles individual entries).
//!
//! Shape:
//!
//! ```toml
//! [servers.github]
//! enabled = true
//! [servers.github.tools]
//! list_issues = true
//! create_issue = false
//!
//! [servers.fs]
//! enabled = false   # hides the whole server
//! ```
//!
//! Tools that are not listed under `[servers.<name>.tools]` fall back to
//! the upstream's `annotations.readOnlyHint`: tools declared read-only
//! are allowed, everything else is denied.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpPolicy {
    #[serde(default)]
    pub servers: BTreeMap<String, ServerPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerPolicy {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, bool>,
}

impl Default for ServerPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            tools: BTreeMap::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

impl McpPolicy {
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("invalid TOML at {}", path.display()))
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = config_path()?;
        self.save_to(&path)?;
        Ok(path)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("failed to serialize MCP policy")?;
        fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    /// Decide whether a tool should be visible to the container.
    ///
    /// - A disabled server hides every tool it owns.
    /// - An explicit per-tool entry wins over the default.
    /// - Otherwise defaults to `read_only_hint` (so destructive-by-default
    ///   tools stay hidden until the operator opts them in).
    pub fn tool_allowed(&self, server: &str, tool: &str, read_only_hint: Option<bool>) -> bool {
        let Some(server_policy) = self.servers.get(server) else {
            return read_only_hint.unwrap_or(false);
        };
        if !server_policy.enabled {
            return false;
        }
        if let Some(explicit) = server_policy.tools.get(tool) {
            return *explicit;
        }
        read_only_hint.unwrap_or(false)
    }

    pub fn set_tool(&mut self, server: &str, tool: &str, enabled: bool) {
        self.servers
            .entry(server.to_string())
            .or_default()
            .tools
            .insert(tool.to_string(), enabled);
    }

    pub fn set_server_enabled(&mut self, server: &str, enabled: bool) {
        self.servers
            .entry(server.to_string())
            .or_default()
            .enabled = enabled;
    }
}

pub fn config_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "agent-container")
        .context("failed to resolve XDG project directories")?;
    Ok(dirs.config_dir().join("mcp.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_default() {
        let path = std::env::temp_dir().join("agent-container-definitely-missing.toml");
        let policy = McpPolicy::load_from(&path).unwrap();
        assert!(policy.servers.is_empty());
    }

    #[test]
    fn default_follows_read_only_hint() {
        let policy = McpPolicy::default();
        assert!(policy.tool_allowed("any", "ro", Some(true)));
        assert!(!policy.tool_allowed("any", "rw", Some(false)));
        // Unknown/ missing annotation is treated conservatively as not-read-only.
        assert!(!policy.tool_allowed("any", "unknown", None));
    }

    #[test]
    fn disabled_server_hides_everything() {
        let mut policy = McpPolicy::default();
        policy.set_server_enabled("bad", false);
        // Even a readonly tool under a disabled server is hidden.
        assert!(!policy.tool_allowed("bad", "safe", Some(true)));
    }

    #[test]
    fn explicit_override_beats_annotation() {
        let mut policy = McpPolicy::default();
        policy.set_tool("github", "create_issue", true);
        policy.set_tool("github", "get_issue", false);
        assert!(policy.tool_allowed("github", "create_issue", Some(false)));
        assert!(!policy.tool_allowed("github", "get_issue", Some(true)));
    }

    #[test]
    fn roundtrip_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.toml");

        let mut written = McpPolicy::default();
        written.set_server_enabled("github", true);
        written.set_tool("github", "list_issues", true);
        written.set_tool("github", "create_issue", false);
        written.set_server_enabled("evil", false);

        written.save_to(&path).unwrap();
        let reloaded = McpPolicy::load_from(&path).unwrap();

        assert_eq!(
            reloaded.tool_allowed("github", "list_issues", Some(false)),
            true
        );
        assert_eq!(
            reloaded.tool_allowed("github", "create_issue", Some(true)),
            false
        );
        assert_eq!(reloaded.tool_allowed("evil", "anything", Some(true)), false);
    }
}
