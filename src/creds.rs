//! Claude Code credential preparation.
//!
//! Backed by [`crate::shared_cred`]: every concurrent agent-container
//! shares one credentials file under `$XDG_DATA/agent-container/shared/`
//! so that an OAuth refresh in one container is visible to the others
//! and to the host. The file is materialised from the host on first
//! use (Keychain on macOS, `~/.claude/.credentials.json` on Linux) and
//! written back to the host when the last container exits.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::keychain::CLAUDE_CODE_CREDENTIALS_SERVICE;
use crate::shared_cred::{HostSync, SharedCredFile, shared_dir};

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
    let parsed: Envelope =
        serde_json::from_str(raw.trim()).context("failed to parse Claude Code credentials JSON")?;
    Ok(CredentialFile {
        path: shared.path.clone(),
        expires_at: parsed.oauth.expires_at,
        _shared: shared,
    })
}

#[cfg(target_os = "macos")]
fn host_sync_target(_claude_root: &Path) -> HostSync {
    HostSync::Keychain {
        service: CLAUDE_CODE_CREDENTIALS_SERVICE.to_string(),
        account: crate::keychain::read_generic_password_account(CLAUDE_CODE_CREDENTIALS_SERVICE)
            .ok()
            .flatten(),
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
    crate::keychain::read_generic_password(CLAUDE_CODE_CREDENTIALS_SERVICE)?
        .context("Claude Code credentials not found in Keychain")
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
