//! Helpers for the Codex pathway: ship the host's ChatGPT-subscription
//! auth token into the container through a short-lived 0600 temp file so
//! the container can run `@openai/codex` without inheriting anything else
//! from `~/.codex` (trust_level lists, session history, …), and pin a
//! minimal `config.toml` inside the container so Codex does not try to
//! nest its own bubblewrap sandbox (which fails inside docker because
//! user namespaces cannot be recreated).

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub struct CodexAuthFile {
    pub path: PathBuf,
    cleanup: bool,
}

impl Drop for CodexAuthFile {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Copy `~/.codex/auth.json` into a fresh temp file with 0600 permissions.
///
/// We do not bind-mount the host file directly: Codex may rewrite the file
/// on token refresh, and mirroring that back to the host would mean a
/// stale in-container session could overwrite a freshly-logged-in token.
/// Instead, every run starts from the host's current token and discards
/// any refreshed copy on exit.
pub fn prepare_auth(host_home: &Path) -> Result<CodexAuthFile> {
    let src = host_home.join(".codex/auth.json");
    let raw = fs::read_to_string(&src).with_context(|| {
        format!(
            "failed to read Codex auth at {}; run `codex login` on the host first",
            src.display()
        )
    })?;

    let dir = std::env::temp_dir().join("agent-container");
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to prepare temp dir at {}", dir.display()))?;
    let path = dir.join(format!("codex-auth-{}.json", std::process::id()));

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("failed to create codex auth file at {}", path.display()))?;
    file.write_all(raw.as_bytes())
        .context("failed to write codex auth file")?;
    file.flush().ok();

    Ok(CodexAuthFile { path, cleanup: true })
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
