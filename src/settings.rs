//! Two-layer user configuration.
//!
//! - **Global**: `$XDG_CONFIG/agent-container/settings.toml`
//! - **Workspace**: `<workspace>/.agent-container/settings.toml`
//!
//! At runtime both are loaded and merged into a single [`Settings`] — the
//! workspace layer takes precedence per the rules on
//! [`Settings::merge_in_place`].
//!
//! Shape:
//!
//! ```toml
//! [proxy]
//! allow = ["^my-internal-host\\.example$"]
//!
//! [mcp.servers.github]
//! enabled = true
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::policy::McpPolicy;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "ProxyPolicy::is_empty")]
    pub proxy: ProxyPolicy,
    #[serde(default, skip_serializing_if = "McpPolicy::is_empty_policy")]
    pub mcp: McpPolicy,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyPolicy {
    /// Additional allow patterns (extended regex) appended to the bundled
    /// base allowlist. tinyproxy matches these case-insensitively against
    /// the request host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
}

impl ProxyPolicy {
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty()
    }
}

impl McpPolicy {
    /// Convenience for `skip_serializing_if` so an empty `[mcp]` section
    /// doesn't round-trip back as a stray header.
    pub fn is_empty_policy(&self) -> bool {
        self.servers.is_empty()
    }
}

/// Scope selector for commands that read or write a single layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Workspace,
}

impl Settings {
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("invalid TOML at {}", path.display()))
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("failed to serialize settings")?;
        fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    pub fn load_global() -> Result<Self> {
        Self::load_from(&global_path()?)
    }

    pub fn load_workspace(workspace: &Path) -> Result<Self> {
        Self::load_from(&workspace_path(workspace))
    }

    pub fn load_scope(scope: Scope, workspace: &Path) -> Result<Self> {
        match scope {
            Scope::Global => Self::load_global(),
            Scope::Workspace => Self::load_workspace(workspace),
        }
    }

    /// Load global + workspace and return the merged view.
    pub fn load_merged(workspace: &Path) -> Result<Self> {
        let mut base = Self::load_global()?;
        let overlay = Self::load_workspace(workspace)?;
        base.merge_in_place(overlay);
        Ok(base)
    }

    /// Merge `overlay` on top of `self`.
    ///
    /// - `proxy.allow`: overlay entries are appended to the base list,
    ///   preserving order and removing exact duplicates.
    /// - `mcp.servers.<server>`: if overlay declares a server, the whole
    ///   entry replaces the base entry (matching VS Code's "workspace
    ///   setting wins at the key" semantics). Servers unmentioned by
    ///   overlay keep their base definition.
    pub fn merge_in_place(&mut self, overlay: Self) {
        for pat in overlay.proxy.allow {
            if !self.proxy.allow.contains(&pat) {
                self.proxy.allow.push(pat);
            }
        }
        for (name, sp) in overlay.mcp.servers {
            self.mcp.servers.insert(name, sp);
        }
    }
}

pub fn path(scope: Scope, workspace: &Path) -> Result<PathBuf> {
    match scope {
        Scope::Global => global_path(),
        Scope::Workspace => Ok(workspace_path(workspace)),
    }
}

pub fn global_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "agent-container")
        .context("failed to resolve XDG project directories")?;
    Ok(dirs.config_dir().join("settings.toml"))
}

pub fn workspace_path(workspace: &Path) -> PathBuf {
    workspace.join(".agent-container").join("settings.toml")
}

/// One-shot migration from the legacy standalone `mcp.toml` into the
/// unified global `settings.toml`. Runs only when the new file does not
/// yet exist; leaves things alone on any subsequent invocation.
pub fn migrate_legacy_global_if_needed() -> Result<()> {
    let new_path = global_path()?;
    if new_path.is_file() {
        return Ok(());
    }
    let legacy = legacy_mcp_path()?;
    if !legacy.is_file() {
        return Ok(());
    }
    let raw = fs::read_to_string(&legacy)
        .with_context(|| format!("failed to read {}", legacy.display()))?;
    let mcp: McpPolicy = toml::from_str(&raw)
        .with_context(|| format!("invalid TOML at {}", legacy.display()))?;
    let settings = Settings {
        proxy: ProxyPolicy::default(),
        mcp,
    };
    settings.save_to(&new_path)?;
    fs::remove_file(&legacy).ok();
    eprintln!(
        "[agent-container] migrated {} -> {} (legacy file removed)",
        legacy.display(),
        new_path.display(),
    );
    Ok(())
}

fn legacy_mcp_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "agent-container")
        .context("failed to resolve XDG project directories")?;
    Ok(dirs.config_dir().join("mcp.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_is_default() {
        let p = std::env::temp_dir().join("agent-container-never-here-settings.toml");
        let s = Settings::load_from(&p).unwrap();
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn roundtrip_preserves_both_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");

        let mut written = Settings::default();
        written.proxy.allow.push("^example\\.com$".into());
        written.mcp.set_server_enabled("gh", true);
        written.mcp.set_tool("gh", "list", true);
        written.save_to(&path).unwrap();

        let read = Settings::load_from(&path).unwrap();
        assert_eq!(read, written);
    }

    #[test]
    fn empty_sections_are_not_emitted() {
        // Sparse configs stay sparse — no empty `[mcp]` header on disk.
        let settings = Settings {
            proxy: ProxyPolicy {
                allow: vec!["^example\\.com$".into()],
            },
            mcp: McpPolicy::default(),
        };
        let raw = toml::to_string_pretty(&settings).unwrap();
        assert!(raw.contains("[proxy]"));
        assert!(!raw.contains("[mcp"));
    }

    #[test]
    fn merge_appends_proxy_allow_and_dedupes() {
        let mut base = Settings {
            proxy: ProxyPolicy {
                allow: vec!["a".into(), "b".into()],
            },
            mcp: McpPolicy::default(),
        };
        let overlay = Settings {
            proxy: ProxyPolicy {
                allow: vec!["b".into(), "c".into()],
            },
            mcp: McpPolicy::default(),
        };
        base.merge_in_place(overlay);
        assert_eq!(
            base.proxy.allow,
            vec!["a".to_string(), "b".into(), "c".into()]
        );
    }

    #[test]
    fn merge_workspace_server_entry_replaces_global() {
        let mut base = Settings::default();
        base.mcp.set_server_enabled("github", true);
        base.mcp.set_tool("github", "list_issues", true);

        let mut overlay = Settings::default();
        overlay.mcp.set_server_enabled("github", false);

        base.merge_in_place(overlay);
        let sp = base.mcp.servers.get("github").unwrap();
        assert!(!sp.enabled);
        // Workspace replaced the whole entry — no inherited tool overrides.
        assert!(sp.tools.is_empty());
    }

    #[test]
    fn merge_keeps_global_servers_untouched_by_overlay() {
        let mut base = Settings::default();
        base.mcp.set_server_enabled("a", true);
        base.mcp.set_server_enabled("b", true);

        let mut overlay = Settings::default();
        overlay.mcp.set_server_enabled("b", false);

        base.merge_in_place(overlay);
        assert!(base.mcp.servers.get("a").unwrap().enabled);
        assert!(!base.mcp.servers.get("b").unwrap().enabled);
    }
}
