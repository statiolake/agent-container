//! Helpers for the Codex pathway: ship the host's ChatGPT-subscription
//! auth token into the container through a short-lived 0600 temp file so
//! the container can run `@openai/codex` without inheriting anything else
//! from `~/.codex` (trust_level lists, session history, …).

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
