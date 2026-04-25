//! Claude Code credential preparation.
//!
//! Backed by [`crate::shared_cred`]: every concurrent agent-container
//! shares one credentials file under `$XDG_DATA/agent-container/shared/`
//! so that an OAuth refresh in one container is visible to the others
//! and to the host. The file is materialised from the host on first
//! use (Keychain on macOS, `~/.claude/.credentials.json` on Linux) and
//! written back to the host when the last container exits.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::shared_cred::{HostSync, SharedCredFile, shared_dir};

const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

pub struct CredentialFile {
    pub path: PathBuf,
    pub expires_at: Option<i64>,
    /// Owns the shared lock: drop releases it and triggers the
    /// last-out write-back.
    _shared: SharedCredFile,
}

impl CredentialFile {
    pub fn is_expired(&self) -> bool {
        let Some(expires_at) = self.expires_at else {
            return false;
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        now_ms >= expires_at
    }
}

/// Prepare a credentials JSON file that can be bind-mounted at
/// `~/.claude/.credentials.json` inside the container.
pub fn prepare(claude_root: &Path) -> Result<CredentialFile> {
    let shared_path = shared_dir()?.join("claude-credentials.json");
    let host_sync = host_sync_target(claude_root);
    let claude_root = claude_root.to_path_buf();
    let (shared, raw) = SharedCredFile::open(shared_path, host_sync, move || {
        read_raw_credentials_from_host(&claude_root)
    })?;
    let parsed: Envelope = serde_json::from_str(raw.trim())
        .context("failed to parse Claude Code credentials JSON")?;
    Ok(CredentialFile {
        path: shared.path.clone(),
        expires_at: parsed.oauth.expires_at,
        _shared: shared,
    })
}

#[cfg(target_os = "macos")]
fn host_sync_target(_claude_root: &Path) -> HostSync {
    HostSync::Keychain {
        service: KEYCHAIN_SERVICE.to_string(),
        account: read_keychain_account().ok(),
    }
}

#[cfg(not(target_os = "macos"))]
fn host_sync_target(claude_root: &Path) -> HostSync {
    HostSync::File(claude_root.join(".credentials.json"))
}

fn read_raw_credentials_from_host(claude_root: &Path) -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        match read_from_keychain() {
            Ok(s) => return Ok(s),
            Err(e) => tracing::debug!(%e, "keychain lookup failed, falling back to file"),
        }
    }
    let path = claude_root.join(".credentials.json");
    std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read credentials file at {}", path.display()))
}

#[cfg(target_os = "macos")]
fn read_from_keychain() -> Result<String> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-w", "-s", KEYCHAIN_SERVICE])
        .output()
        .context("failed to invoke `security` command")?;
    if !output.status.success() {
        bail!(
            "security command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).context("keychain entry was not valid UTF-8")
}

/// Read the `acct` (account) field from the Keychain entry so the
/// write-back path can target the same item with `security
/// add-generic-password -U`.
#[cfg(target_os = "macos")]
fn read_keychain_account() -> Result<String> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE])
        .output()
        .context("failed to invoke `security`")?;
    if !output.status.success() {
        bail!(
            "security find-generic-password failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8(output.stdout).context("keychain entry not utf-8")?;
    for line in stdout.lines() {
        // Format: `    "acct"<blob>="me@example.com"`
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(r#""acct"<blob>=""#) {
            if let Some(end) = rest.rfind('"') {
                return Ok(rest[..end].to_string());
            }
        }
    }
    bail!("acct attribute not found in keychain entry");
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "claudeAiOauth")]
    oauth: OAuth,
}

#[derive(Deserialize)]
struct OAuth {
    #[serde(default, rename = "expiresAt")]
    expires_at: Option<i64>,
}
