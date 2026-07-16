#[cfg(target_os = "macos")]
use std::process::Stdio;

use anyhow::{Context, Result, bail};

pub const CLAUDE_CODE_CREDENTIALS_SERVICE: &str = "Claude Code-credentials";

#[cfg(target_os = "macos")]
pub fn read_generic_password(service: &str) -> Result<Option<String>> {
    read_generic_password_for_account(service, None)
}

#[cfg(target_os = "macos")]
pub fn read_generic_password_for_account(
    service: &str,
    account: Option<&str>,
) -> Result<Option<String>> {
    let mut cmd = std::process::Command::new("security");
    cmd.args(["find-generic-password", "-w", "-s", service]);
    if let Some(account) = account {
        cmd.args(["-a", account]);
    }
    cmd.stdin(Stdio::null());
    let output = cmd
        .output()
        .context("failed to invoke `security` command")?;
    if !output.status.success() {
        return Ok(None);
    }
    let raw = String::from_utf8(output.stdout).context("keychain entry was not valid UTF-8")?;
    Ok(Some(raw))
}

#[cfg(target_os = "macos")]
pub fn read_generic_password_account(service: &str) -> Result<Option<String>> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", service])
        .stdin(Stdio::null())
        .output()
        .context("failed to invoke `security` command")?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8(output.stdout).context("keychain entry not utf-8")?;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(r#""acct"<blob>=""#)
            && let Some(end) = rest.rfind('"')
        {
            return Ok(Some(rest[..end].to_string()));
        }
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
pub fn write_generic_password(service: &str, account: Option<&str>, raw: &str) -> Result<()> {
    let mut cmd = std::process::Command::new("security");
    cmd.args(["add-generic-password", "-U", "-s", service, "-w", raw]);
    if let Some(account) = account {
        cmd.args(["-a", account]);
    }
    cmd.stdin(Stdio::null());
    let status = cmd
        .status()
        .context("failed to invoke `security` command")?;
    if !status.success() {
        bail!("security add-generic-password exited with {status}");
    }
    Ok(())
}
