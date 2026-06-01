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
//! [claude_code.mcp.servers.github]
//! enabled = true
//!
//! [codex.mcp.servers.github]
//! enabled = true
//!
//! [general]
//! default_agent = "claude"
//!
//! [task_runner.tasks]
//! lint = "cargo check"
//! build = "cargo build --release"
//!
//! [filesystem]
//! mounts = ["/Users/me/project-notes"]
//! hide = ["(^|/)\\.env(\\..*)?$"]
//! readonly = ["(^|/)\\.claude(/|$)"]
//!
//! [claude]
//! tmux_prefix = "C-b"
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
    #[serde(default, skip_serializing_if = "GeneralPolicy::is_empty")]
    pub general: GeneralPolicy,
    #[serde(default, skip_serializing_if = "ProxyPolicy::is_empty")]
    pub proxy: ProxyPolicy,
    #[serde(default, rename = "mcp", skip_serializing)]
    pub mcp: McpPolicy,
    #[serde(default, skip_serializing_if = "ClaudeCodePolicy::is_empty")]
    pub claude_code: ClaudeCodePolicy,
    #[serde(default, skip_serializing_if = "CodexPolicy::is_empty")]
    pub codex: CodexPolicy,
    #[serde(default, skip_serializing_if = "TaskRunnerPolicy::is_empty")]
    pub task_runner: TaskRunnerPolicy,
    #[serde(default, skip_serializing_if = "FilesystemPolicy::is_empty")]
    pub filesystem: FilesystemPolicy,
    #[serde(default, skip_serializing_if = "ClaudePolicy::is_empty")]
    pub claude: ClaudePolicy,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneralPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<DefaultAgent>,
}

impl GeneralPolicy {
    pub fn is_empty(&self) -> bool {
        self.default_agent.is_none()
    }

    pub fn default_agent(&self) -> DefaultAgent {
        self.default_agent.unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DefaultAgent {
    #[default]
    Claude,
    Codex,
}

impl DefaultAgent {
    pub fn toggle(self) -> Self {
        match self {
            DefaultAgent::Claude => DefaultAgent::Codex,
            DefaultAgent::Codex => DefaultAgent::Claude,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DefaultAgent::Claude => "Claude Code",
            DefaultAgent::Codex => "Codex",
        }
    }
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

/// Host filesystem roots and filters shared by bind mounts and the
/// built-in `host-fs` MCP server. The current workspace is always a
/// mounted root; `mounts` adds more absolute host directories. `hide`
/// and `readonly` are regular expressions matched against paths
/// relative to each root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemPolicy {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hide: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readonly: Vec<String>,
}

impl FilesystemPolicy {
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty() && self.hide.is_empty() && self.readonly.is_empty()
    }
}

pub fn default_filesystem_hide() -> Vec<String> {
    [
        r"(^|/)\.env(\..*)?$",
        r"(^|/).*\.env(\..*)?$",
        r"(^|/).*\.pem$",
        r"(^|/).*\.key$",
        r"(^|/).*\.p12$",
        r"(^|/).*\.pfx$",
        r"(^|/)id_rsa$",
        r"(^|/)id_ecdsa$",
        r"(^|/)id_ed25519$",
        r"(^|/)\.npmrc$",
        r"(^|/)\.pypirc$",
        r"(^|/)\.netrc$",
        r"(^|/)credentials$",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

pub fn default_filesystem_readonly() -> Vec<String> {
    [r"(^|/)\.claude(/|$)", r"(^|/)\.codex(/|$)"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// Claude Code MCP policy. Legacy top-level `[mcp]` is still accepted on
/// read, then saved back as `[claude_code.mcp]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeCodePolicy {
    #[serde(default, skip_serializing_if = "McpPolicy::is_empty_policy")]
    pub mcp: McpPolicy,
}

impl ClaudeCodePolicy {
    pub fn is_empty(&self) -> bool {
        self.mcp.is_empty_policy()
    }
}

/// Codex runtime options controlled by agent-container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexPolicy {
    #[serde(default, skip_serializing_if = "McpPolicy::is_empty_policy")]
    pub mcp: McpPolicy,
}

impl CodexPolicy {
    pub fn is_empty(&self) -> bool {
        self.mcp.is_empty_policy()
    }
}

/// Claude Code runtime options controlled by agent-container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudePolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_prefix: Option<String>,
}

impl ClaudePolicy {
    pub const DEFAULT_TMUX_PREFIX: &'static str = "C-b";

    pub fn is_empty(&self) -> bool {
        self.tmux_prefix.is_none()
    }

    pub fn tmux_prefix(&self) -> &str {
        self.tmux_prefix
            .as_deref()
            .unwrap_or(Self::DEFAULT_TMUX_PREFIX)
    }
}

impl McpPolicy {
    /// Convenience for `skip_serializing_if` so an empty MCP section
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
        let mut settings: Self =
            toml::from_str(&raw).with_context(|| format!("invalid TOML at {}", path.display()))?;
        settings.migrate_legacy_mcp();
        Ok(settings)
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
            general: GeneralPolicy::default(),
            proxy: ProxyPolicy {
                allow: crate::proxy_allowlist::default_allow_entries(),
            },
            mcp: McpPolicy::default(),
            claude_code: ClaudeCodePolicy::default(),
            codex: CodexPolicy::default(),
            task_runner: TaskRunnerPolicy::default(),
            filesystem: FilesystemPolicy {
                mounts: Vec::new(),
                hide: default_filesystem_hide(),
                readonly: default_filesystem_readonly(),
            },
            claude: ClaudePolicy::default(),
        }
    }

    /// Load the global settings — or materialise [`Self::default_global`]
    /// when the file does not yet exist. Security-oriented filesystem
    /// defaults are always appended so older settings files do not
    /// accidentally expose secrets after upgrading.
    pub fn load_global() -> Result<Self> {
        let path = global_path()?;
        let mut settings = Self::load_from_or(&path, Self::default_global)?;
        append_unique(&mut settings.filesystem.hide, default_filesystem_hide());
        append_unique(
            &mut settings.filesystem.readonly,
            default_filesystem_readonly(),
        );
        Ok(settings)
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
    /// - `general.default_agent`: workspace overrides global when set.
    /// - `claude_code.mcp.servers.<server>` and
    ///   `codex.mcp.servers.<server>`: if overlay declares a server, the
    ///   whole entry replaces the base entry (matching VS Code's
    ///   "workspace setting wins at the key" semantics). Servers
    ///   unmentioned by overlay keep their base definition.
    /// - `task_runner.tasks.<name>`: same as MCP — overlay's same-named
    ///   task replaces the base's, others pass through.
    /// - `filesystem.mounts`, `filesystem.hide`, `filesystem.readonly`:
    ///   overlay entries are appended to the base list, preserving order
    ///   and removing exact duplicates.
    pub fn merge_in_place(&mut self, overlay: Self) {
        self.migrate_legacy_mcp();
        let mut overlay = overlay;
        overlay.migrate_legacy_mcp();
        if overlay.general.default_agent.is_some() {
            self.general.default_agent = overlay.general.default_agent;
        }
        for pat in overlay.proxy.allow {
            if !self.proxy.allow.contains(&pat) {
                self.proxy.allow.push(pat);
            }
        }
        for (name, sp) in overlay.claude_code.mcp.servers {
            self.claude_code.mcp.servers.insert(name, sp);
        }
        for (name, sp) in overlay.codex.mcp.servers {
            self.codex.mcp.servers.insert(name, sp);
        }
        for (name, cmd) in overlay.task_runner.tasks {
            self.task_runner.tasks.insert(name, cmd);
        }
        append_unique(&mut self.filesystem.mounts, overlay.filesystem.mounts);
        append_unique(&mut self.filesystem.hide, overlay.filesystem.hide);
        append_unique(&mut self.filesystem.readonly, overlay.filesystem.readonly);
        if overlay.claude.tmux_prefix.is_some() {
            self.claude.tmux_prefix = overlay.claude.tmux_prefix;
        }
    }

    fn migrate_legacy_mcp(&mut self) {
        if self.claude_code.mcp.is_empty_policy() && !self.mcp.is_empty_policy() {
            self.claude_code.mcp = self.mcp.clone();
        }
        self.mcp = McpPolicy::default();
    }
}

fn append_unique(target: &mut Vec<String>, overlay: Vec<String>) {
    for value in overlay {
        if !target.contains(&value) {
            target.push(value);
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

/// Content fingerprint for the settings files that affect a running
/// session. Missing/unreadable files are represented explicitly so create,
/// delete, and edit operations all become observable by simple comparison.
pub fn watched_file_fingerprint(workspace: &Path) -> Vec<Option<Vec<u8>>> {
    let global = global_path().ok().and_then(|path| std::fs::read(path).ok());
    let workspace = std::fs::read(workspace_path(workspace)).ok();
    vec![global, workspace]
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
        assert!(s.claude_code.mcp.servers.contains_key("gh"));
    }

    #[test]
    fn legacy_mcp_reads_as_claude_code_and_saves_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "[mcp.servers.gh.tools]\nlist = true\n").unwrap();

        let settings = Settings::load_from(&path).unwrap();
        assert!(
            settings
                .claude_code
                .mcp
                .tool_allowed("gh", "list", Some(false))
        );

        settings.save_to(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[claude_code.mcp.servers.gh.tools]"));
        assert!(!raw.contains("[mcp."));
    }

    #[test]
    fn roundtrip_preserves_both_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");

        let mut written = Settings::default();
        written.proxy.allow.push("^example\\.com$".into());
        written.claude_code.mcp.set_server_enabled("gh", true);
        written.claude_code.mcp.set_tool("gh", "list", true);
        written.save_to(&path).unwrap();

        let read = Settings::load_from(&path).unwrap();
        assert_eq!(read, written);
    }

    #[test]
    fn empty_sections_are_not_emitted() {
        // Sparse configs stay sparse — no empty MCP or `[task_runner]`
        // header on disk when the corresponding policy is empty.
        let settings = Settings {
            proxy: ProxyPolicy {
                allow: vec!["^example\\.com$".into()],
            },
            ..Default::default()
        };
        let raw = toml::to_string_pretty(&settings).unwrap();
        assert!(raw.contains("[proxy]"));
        assert!(!raw.contains("mcp"));
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
    fn default_agent_roundtrips_and_workspace_overrides_global() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let written = Settings {
            general: GeneralPolicy {
                default_agent: Some(DefaultAgent::Codex),
            },
            ..Default::default()
        };
        written.save_to(&path).unwrap();

        let read = Settings::load_from(&path).unwrap();
        assert_eq!(read.general.default_agent(), DefaultAgent::Codex);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[general]"));
        assert!(raw.contains("default_agent = \"codex\""));

        let mut base = Settings {
            general: GeneralPolicy {
                default_agent: Some(DefaultAgent::Claude),
            },
            ..Default::default()
        };
        base.merge_in_place(read);
        assert_eq!(base.general.default_agent(), DefaultAgent::Codex);
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
    fn claude_tmux_prefix_defaults_and_roundtrips() {
        assert_eq!(ClaudePolicy::default().tmux_prefix(), "C-b");

        let raw = r#"
          [claude]
          tmux_prefix = "C-q"
        "#;
        let settings: Settings = toml::from_str(raw).unwrap();
        assert_eq!(settings.claude.tmux_prefix(), "C-q");

        let serialized = toml::to_string_pretty(&settings).unwrap();
        assert!(serialized.contains("[claude]"));
        assert!(serialized.contains("tmux_prefix = \"C-q\""));
    }

    #[test]
    fn workspace_claude_tmux_prefix_overrides_global() {
        let mut base = Settings::default();
        base.claude.tmux_prefix = Some("C-b".into());

        let mut overlay = Settings::default();
        overlay.claude.tmux_prefix = Some("C-q".into());

        base.merge_in_place(overlay);
        assert_eq!(base.claude.tmux_prefix(), "C-q");
    }

    #[test]
    fn merge_workspace_server_entry_replaces_global() {
        let mut base = Settings::default();
        base.claude_code.mcp.set_server_enabled("github", true);
        base.claude_code.mcp.set_tool("github", "list_issues", true);

        let mut overlay = Settings::default();
        overlay.claude_code.mcp.set_server_enabled("github", false);

        base.merge_in_place(overlay);
        let sp = base.claude_code.mcp.servers.get("github").unwrap();
        assert!(!sp.enabled);
        // Workspace replaced the whole entry — no inherited tool overrides.
        assert!(sp.tools.is_empty());
    }

    #[test]
    fn merge_keeps_global_servers_untouched_by_overlay() {
        let mut base = Settings::default();
        base.claude_code.mcp.set_server_enabled("a", true);
        base.claude_code.mcp.set_server_enabled("b", true);

        let mut overlay = Settings::default();
        overlay.claude_code.mcp.set_server_enabled("b", false);

        base.merge_in_place(overlay);
        assert!(base.claude_code.mcp.servers.get("a").unwrap().enabled);
        assert!(!base.claude_code.mcp.servers.get("b").unwrap().enabled);
    }
}
