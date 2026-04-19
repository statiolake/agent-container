use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// A handle to a credentials file on disk prepared for bind-mounting into the
/// container. Removes the file on drop when it was created in a temp location.
pub struct CredentialFile {
    pub path: PathBuf,
    pub expires_at: Option<i64>,
    cleanup: bool,
}

impl Drop for CredentialFile {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = fs::remove_file(&self.path);
        }
    }
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
///
/// - macOS: extract the Keychain entry into a fresh temp file (0600).
/// - Linux: copy the user's existing `~/.claude/.credentials.json` into a temp
///   file so Claude Code can write refreshed tokens without touching the host
///   canonical copy.
pub fn prepare(claude_root: &Path) -> Result<CredentialFile> {
    let raw = read_raw_credentials(claude_root)?;
    let parsed: Envelope = serde_json::from_str(raw.trim())
        .context("failed to parse Claude Code credentials JSON")?;

    let dir = std::env::temp_dir().join("agent-container");
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to prepare temp dir at {}", dir.display()))?;
    let path = dir.join(format!("creds-{}.json", std::process::id()));

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("failed to create credentials file at {}", path.display()))?;
    file.write_all(raw.trim().as_bytes())
        .context("failed to write credentials file")?;
    file.flush().ok();

    Ok(CredentialFile {
        path,
        expires_at: parsed.oauth.expires_at,
        cleanup: true,
    })
}

fn read_raw_credentials(claude_root: &Path) -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        match read_from_keychain() {
            Ok(s) => return Ok(s),
            Err(e) => tracing::debug!(%e, "keychain lookup failed, falling back to file"),
        }
    }
    let path = claude_root.join(".credentials.json");
    fs::read_to_string(&path)
        .with_context(|| format!("failed to read credentials file at {}", path.display()))
}

#[cfg(target_os = "macos")]
fn read_from_keychain() -> Result<String> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-w",
            "-s",
            "Claude Code-credentials",
        ])
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
