//! Per-tool MCP allowlist. Serialised as the `[mcp]` section of
//! `settings.toml` (see [`crate::settings`]) and read by both the broker
//! (which enforces it at proxy time) and the `config` TUI (which toggles
//! individual entries).
//!
//! Shape:
//!
//! ```toml
//! [mcp.servers.github]
//! enabled = true
//! [mcp.servers.github.tools]
//! list_issues = true
//! create_issue = false
//!
//! [mcp.servers.fs]
//! enabled = false   # hides the whole server
//! ```
//!
//! Tools that are not listed under `[mcp.servers.<name>.tools]` fall back
//! to the upstream's `annotations.readOnlyHint`: tools declared read-only
//! are allowed, everything else is denied.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPolicy {
    #[serde(default)]
    pub servers: BTreeMap<String, ServerPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut written = McpPolicy::default();
        written.set_server_enabled("github", true);
        written.set_tool("github", "list_issues", true);
        written.set_tool("github", "create_issue", false);
        written.set_server_enabled("evil", false);

        let raw = toml::to_string_pretty(&written).unwrap();
        let reloaded: McpPolicy = toml::from_str(&raw).unwrap();

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
