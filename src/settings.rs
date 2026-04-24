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
//!
//! [task_runner.tasks]
//! lint = "cargo check"
//! build = "cargo build --release"
//! ```

use std::collections::BTreeMap;
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
    #[serde(default, skip_serializing_if = "TaskRunnerPolicy::is_empty")]
    pub task_runner: TaskRunnerPolicy,
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

/// User-defined shell commands surfaced to the container as MCP tools by
/// the built-in `task-runner` server. Each key becomes a tool name; the
/// value is the command line executed on the host when the tool is
/// invoked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRunnerPolicy {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tasks: BTreeMap<String, String>,
}

impl TaskRunnerPolicy {
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
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

    /// Default global configuration — the one we would have shipped if
    /// the user ran `agent-container` before writing any settings. Seeds
    /// the proxy allow list with the defaults baked into the CLI so
    /// they behave exactly like user-authored entries: visible in
    /// `config show`, editable in the TUI, and actively read on save.
    ///
    /// Workspace files don't use this — their own `Self::default()`
    /// (everything empty) is the right starting point.
    pub fn default_global() -> Self {
        Self {
            proxy: ProxyPolicy {
                allow: crate::proxy_allowlist::default_allow_entries(),
            },
            mcp: McpPolicy::default(),
            task_runner: TaskRunnerPolicy::default(),
        }
    }

    /// Load the global settings — or materialise [`Self::default_global`]
    /// when the file does not yet exist. Once the user saves anything,
    /// the file is authoritative and the bundled defaults are no longer
    /// consulted.
    pub fn load_global() -> Result<Self> {
        Self::load_from_or(&global_path()?, Self::default_global)
    }

    pub fn load_workspace(workspace: &Path) -> Result<Self> {
        Self::load_from(&workspace_path(workspace))
    }

    /// Like [`Self::load_from`], but the caller supplies the fallback
    /// to use when the file is missing. Lets `load_global` inject the
    /// bundled defaults while `load_workspace` keeps using the
    /// everything-empty `Default::default()`.
    pub fn load_from_or(path: &Path, fallback: impl FnOnce() -> Self) -> Result<Self> {
        if path.is_file() {
            Self::load_from(path)
        } else {
            Ok(fallback())
        }
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
    /// - `task_runner.tasks.<name>`: same as MCP — overlay's same-named
    ///   task replaces the base's, others pass through.
    pub fn merge_in_place(&mut self, overlay: Self) {
        for pat in overlay.proxy.allow {
            if !self.proxy.allow.contains(&pat) {
                self.proxy.allow.push(pat);
            }
        }
        for (name, sp) in overlay.mcp.servers {
            self.mcp.servers.insert(name, sp);
        }
        for (name, cmd) in overlay.task_runner.tasks {
            self.task_runner.tasks.insert(name, cmd);
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
    // Seed with the bundled defaults so the new file is a complete
    // global config, then overlay whatever the legacy mcp.toml said.
    let settings = Settings {
        mcp,
        ..Settings::default_global()
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
    fn default_global_includes_bundled_proxy_defaults() {
        let g = Settings::default_global();
        assert!(
            !g.proxy.allow.is_empty(),
            "bundled defaults should seed proxy.allow"
        );
        assert!(g.proxy.allow.iter().any(|p| p.contains("anthropic")));
    }

    #[test]
    fn load_from_or_falls_back_when_file_is_missing() {
        let p = std::env::temp_dir().join("agent-container-never-here-global.toml");
        let s = Settings::load_from_or(&p, Settings::default_global).unwrap();
        assert_eq!(s, Settings::default_global());
    }

    #[test]
    fn load_from_or_reads_on_disk_file_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        // Sparse file — no proxy section. Defaults must NOT fill it
        // back in; the on-disk file is authoritative once it exists.
        std::fs::write(&path, "[mcp.servers.gh]\nenabled = true\n").unwrap();
        let s = Settings::load_from_or(&path, Settings::default_global).unwrap();
        assert!(
            s.proxy.allow.is_empty(),
            "existing file should not be padded with bundled defaults"
        );
        assert!(s.mcp.servers.contains_key("gh"));
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
        // Sparse configs stay sparse — no empty `[mcp]` or `[task_runner]`
        // header on disk when the corresponding policy is empty.
        let settings = Settings {
            proxy: ProxyPolicy {
                allow: vec!["^example\\.com$".into()],
            },
            ..Default::default()
        };
        let raw = toml::to_string_pretty(&settings).unwrap();
        assert!(raw.contains("[proxy]"));
        assert!(!raw.contains("[mcp"));
        assert!(!raw.contains("[task_runner"));
    }

    #[test]
    fn merge_appends_proxy_allow_and_dedupes() {
        let mut base = Settings {
            proxy: ProxyPolicy {
                allow: vec!["a".into(), "b".into()],
            },
            ..Default::default()
        };
        let overlay = Settings {
            proxy: ProxyPolicy {
                allow: vec!["b".into(), "c".into()],
            },
            ..Default::default()
        };
        base.merge_in_place(overlay);
        assert_eq!(
            base.proxy.allow,
            vec!["a".to_string(), "b".into(), "c".into()]
        );
    }

    #[test]
    fn merge_workspace_task_replaces_global_same_name() {
        let mut base = Settings::default();
        base.task_runner
            .tasks
            .insert("lint".into(), "cargo check".into());
        base.task_runner
            .tasks
            .insert("test".into(), "cargo test".into());

        let mut overlay = Settings::default();
        overlay
            .task_runner
            .tasks
            .insert("lint".into(), "cargo clippy".into());
        overlay
            .task_runner
            .tasks
            .insert("build".into(), "cargo build --release".into());

        base.merge_in_place(overlay);
        assert_eq!(
            base.task_runner.tasks.get("lint").map(String::as_str),
            Some("cargo clippy"),
            "overlay overrides same-named task"
        );
        assert_eq!(
            base.task_runner.tasks.get("test").map(String::as_str),
            Some("cargo test"),
            "untouched task survives"
        );
        assert_eq!(
            base.task_runner.tasks.get("build").map(String::as_str),
            Some("cargo build --release"),
            "new task from overlay is added"
        );
    }

    #[test]
    fn task_runner_roundtrips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let mut written = Settings::default();
        written
            .task_runner
            .tasks
            .insert("lint".into(), "cargo check".into());
        written.save_to(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[task_runner.tasks]"));
        assert!(raw.contains("lint"));
        let read = Settings::load_from(&path).unwrap();
        assert_eq!(read, written);
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
