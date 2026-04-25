//! Helpers for the Codex pathway: ship the host's ChatGPT-subscription
//! auth token into the container through a short-lived 0600 temp file so
//! the container can run `@openai/codex` without inheriting anything else
//! from `~/.codex` (trust_level lists, session history, …), and pin a
//! minimal `config.toml` inside the container so Codex does not try to
//! nest its own bubblewrap sandbox (which fails inside docker because
//! user namespaces cannot be recreated).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::shared_cred::{HostSync, SharedCredFile, shared_dir};

pub struct CodexAuthFile {
    pub path: PathBuf,
    /// Owns the shared lock; see [`crate::shared_cred`]. The last
    /// agent-container to drop this writes the (possibly refreshed)
    /// auth.json back to `~/.codex/auth.json` on the host.
    _shared: SharedCredFile,
}

/// Open `~/.codex/auth.json` through the shared-credential machinery.
///
/// All concurrent agent-container processes on this host see the same
/// `auth.json`, so a token refresh in one container is observable by
/// the others via the bind-mounted shared file. The host copy is
/// updated only when the last container exits.
pub fn prepare_auth(host_home: &Path) -> Result<CodexAuthFile> {
    let src = host_home.join(".codex/auth.json");
    let shared_path = shared_dir()?.join("codex-auth.json");
    let host_sync = HostSync::File(src.clone());
    let (shared, _raw) = SharedCredFile::open(shared_path, host_sync, move || {
        fs::read_to_string(&src).with_context(|| {
            format!(
                "failed to read Codex auth at {}; run `codex login` on the host first",
                src.display()
            )
        })
    })?;
    Ok(CodexAuthFile {
        path: shared.path.clone(),
        _shared: shared,
    })
}

/// Top-level scalar keys we inherit from the host's `~/.codex/config.toml`
/// so the containerised Codex runs with the same model / effort / persona
/// the user picked on the host.
const INHERITED_SCALAR_KEYS: &[&str] = &[
    "model",
    "model_provider",
    "model_reasoning_effort",
    "model_reasoning_summary",
    "personality",
];

/// Write `~/.codex/config.toml` into the container's persistent home.
///
/// The file is composed from two sources:
/// - Carry over the user's model / reasoning-effort / personality choices
///   from the host's `~/.codex/config.toml` so the container follows the
///   same behaviour. Host-absolute `[projects.*]` trust entries and any
///   sandbox-related toggles are dropped.
/// - Pin `approval_policy = "never"` and `sandbox_mode = "danger-full-access"`
///   because the docker container itself is the sandbox; Codex's bubblewrap
///   cannot recreate user namespaces inside docker and would otherwise
///   fail every nested shell exec.
pub fn write_container_config(host_home: &Path, container_home: &Path) -> Result<()> {
    let mut table = toml::value::Table::new();

    let host_config = host_home.join(".codex/config.toml");
    if host_config.is_file() {
        let raw = fs::read_to_string(&host_config)
            .with_context(|| format!("failed to read {}", host_config.display()))?;
        let parsed: toml::Value = toml::from_str(&raw)
            .with_context(|| format!("failed to parse {} as TOML", host_config.display()))?;
        if let Some(host_table) = parsed.as_table() {
            for key in INHERITED_SCALAR_KEYS {
                if let Some(v) = host_table.get(*key).cloned() {
                    table.insert((*key).to_string(), v);
                }
            }
        }
    }

    // Always pin the sandbox/approval defaults — they are the whole reason
    // this file exists inside the container.
    table.insert(
        "approval_policy".to_string(),
        toml::Value::String("never".into()),
    );
    table.insert(
        "sandbox_mode".to_string(),
        toml::Value::String("danger-full-access".into()),
    );

    let dir = container_home.join(".codex");
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join("config.toml");
    let header = "# Written by agent-container. The container itself is the sandbox,\n\
                  # so Codex's internal sandbox is disabled here; the other values\n\
                  # are inherited from the host's ~/.codex/config.toml.\n";
    let body = toml::to_string_pretty(&toml::Value::Table(table))
        .context("serialising codex config.toml")?;
    fs::write(&path, format!("{header}{body}"))
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherits_model_and_effort_and_pins_sandbox() {
        let host_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        fs::create_dir_all(host_home.path().join(".codex")).unwrap();
        fs::write(
            host_home.path().join(".codex/config.toml"),
            r#"
model = "gpt-5.4"
model_reasoning_effort = "xhigh"
personality = "pragmatic"
approval_policy = "on-request"
sandbox_mode = "workspace-write"

[projects."/home/user/projects/sample"]
trust_level = "trusted"
"#,
        )
        .unwrap();

        write_container_config(host_home.path(), container_home.path()).unwrap();
        let out =
            fs::read_to_string(container_home.path().join(".codex/config.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        let t = parsed.as_table().unwrap();
        assert_eq!(t["model"].as_str(), Some("gpt-5.4"));
        assert_eq!(t["model_reasoning_effort"].as_str(), Some("xhigh"));
        assert_eq!(t["personality"].as_str(), Some("pragmatic"));
        assert_eq!(t["approval_policy"].as_str(), Some("never"));
        assert_eq!(t["sandbox_mode"].as_str(), Some("danger-full-access"));
        assert!(t.get("projects").is_none(), "projects must be dropped");
    }

    #[test]
    fn works_without_host_config() {
        let host_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        write_container_config(host_home.path(), container_home.path()).unwrap();
        let out =
            fs::read_to_string(container_home.path().join(".codex/config.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        let t = parsed.as_table().unwrap();
        assert_eq!(t["approval_policy"].as_str(), Some("never"));
        assert_eq!(t["sandbox_mode"].as_str(), Some("danger-full-access"));
        assert!(t.get("model").is_none());
    }
}
