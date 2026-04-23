//! Bedrock pathway: detect the host's Bedrock configuration from
//! `~/.claude/settings.json`, resolve AWS credentials for the named profile
//! on the host (static keys, SSO, assume-role — all via `aws configure
//! export-credentials`), and surface them as env vars for the container.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

/// Bedrock declaration from the host's settings.json env section.
#[derive(Debug, Clone)]
pub struct BedrockSetup {
    pub profile: String,
    pub model: Option<String>,
    pub region: Option<String>,
}

/// Credentials materialised via `aws configure export-credentials`.
///
/// Region is deliberately absent: the AWS credential-process JSON format
/// has no region field, and Claude Code picks up the region from the env
/// section of settings.json (which we also keep in sync).
pub struct BedrockCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// Parse `~/.claude/settings.json` and return a BedrockSetup if the user
/// has declared Bedrock usage in the env section.
pub fn detect_setup(settings_path: &Path) -> Result<Option<BedrockSetup>> {
    if !settings_path.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(settings_path)
        .with_context(|| format!("failed to read {}", settings_path.display()))?;
    let cfg: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {} as JSON", settings_path.display()))?;
    let Some(env) = cfg.get("env").and_then(Value::as_object) else {
        return Ok(None);
    };

    if !truthy(env.get("CLAUDE_CODE_USE_BEDROCK")) {
        return Ok(None);
    }

    let Some(profile) = env
        .get("AWS_PROFILE")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(None);
    };

    let model = env
        .get("ANTHROPIC_MODEL")
        .and_then(Value::as_str)
        .map(str::to_string);
    let region = env
        .get("AWS_REGION")
        .or_else(|| env.get("AWS_DEFAULT_REGION"))
        .and_then(Value::as_str)
        .map(str::to_string);

    Ok(Some(BedrockSetup {
        profile,
        model,
        region,
    }))
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::String(s)) => {
            matches!(s.as_str(), "1" | "true" | "TRUE" | "True" | "yes" | "YES")
        }
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().is_some_and(|v| v != 0),
        _ => false,
    }
}

/// Look up an optional `awsAuthRefresh` shell command from either
/// `~/.claude/settings.json` or `~/.claude.json`. Either is valid —
/// Claude Code itself reads the former (user settings) and we also fall
/// back to the latter for compatibility.
///
/// When present, it is invoked on the host before retrying AWS credential
/// resolution — typically `aws sso login --profile XXX` or a thin wrapper.
pub fn detect_refresh_command(
    settings_json: &Path,
    claude_json: &Path,
) -> Result<Option<String>> {
    for path in [settings_json, claude_json] {
        if let Some(cmd) = read_refresh_from(path)? {
            return Ok(Some(cmd));
        }
    }
    Ok(None)
}

fn read_refresh_from(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let cfg: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {} as JSON", path.display()))?;
    Ok(cfg
        .get("awsAuthRefresh")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()))
}

/// Invoke `aws configure export-credentials` on the host; works uniformly
/// for static keys, SSO sessions, and assume-role profiles. If the first
/// attempt fails and `refresh` is provided, the refresh command is run
/// (stdio inherited so SSO prompts are visible) and the resolution is
/// retried once.
pub fn resolve_credentials(
    setup: &BedrockSetup,
    refresh: Option<&str>,
) -> Result<BedrockCredentials> {
    match try_export(setup) {
        Ok(c) => Ok(c),
        Err(first_err) => {
            let Some(cmd) = refresh else {
                return Err(first_err);
            };
            eprintln!(
                "[agent-container] AWS credentials resolution failed; running awsAuthRefresh: {cmd}"
            );
            run_refresh(cmd)
                .context("awsAuthRefresh command failed; the original credential resolution error still stands")?;
            try_export(setup).with_context(|| {
                format!(
                    "credential resolution still failed after awsAuthRefresh (original error: {first_err:#})"
                )
            })
        }
    }
}

fn try_export(setup: &BedrockSetup) -> Result<BedrockCredentials> {
    let output = Command::new("aws")
        .args([
            "configure",
            "export-credentials",
            "--profile",
            &setup.profile,
            "--format",
            "process",
        ])
        .output()
        .context("failed to invoke `aws` CLI; is it installed and on PATH?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        bail!(
            "`aws configure export-credentials --profile {}` failed. If you use SSO, run `aws sso login --profile {}` on the host (or set awsAuthRefresh in ~/.claude.json) first.\n{}",
            setup.profile,
            setup.profile,
            stderr
        );
    }

    #[derive(Deserialize)]
    struct ExportedCreds {
        #[serde(rename = "AccessKeyId")]
        access_key_id: String,
        #[serde(rename = "SecretAccessKey")]
        secret_access_key: String,
        #[serde(default, rename = "SessionToken")]
        session_token: Option<String>,
    }

    let parsed: ExportedCreds = serde_json::from_slice(&output.stdout)
        .context("failed to parse aws configure export-credentials JSON")?;

    Ok(BedrockCredentials {
        access_key_id: parsed.access_key_id,
        secret_access_key: parsed.secret_access_key,
        session_token: parsed.session_token,
    })
}

fn run_refresh(cmd: &str) -> Result<()> {
    let status = Command::new("sh")
        .args(["-c", cmd])
        .status()
        .with_context(|| format!("failed to spawn `sh -c {cmd}`"))?;
    if !status.success() {
        bail!("awsAuthRefresh exited with status {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_settings(json: &str) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        fs::write(f.path(), json).unwrap();
        f
    }

    #[test]
    fn detects_bedrock_with_claude_code_use_bedrock_flag() {
        let f = write_settings(
            r#"{"env": {"CLAUDE_CODE_USE_BEDROCK": "1", "AWS_PROFILE": "dev",
                "ANTHROPIC_MODEL": "anthropic.claude-3-5-sonnet-20241022-v1:0",
                "AWS_REGION": "us-west-2"}}"#,
        );
        let s = detect_setup(f.path()).unwrap().unwrap();
        assert_eq!(s.profile, "dev");
        assert_eq!(s.model.as_deref(), Some("anthropic.claude-3-5-sonnet-20241022-v1:0"));
        assert_eq!(s.region.as_deref(), Some("us-west-2"));
    }

    #[test]
    fn accepts_boolean_true_for_flag() {
        let f = write_settings(
            r#"{"env": {"CLAUDE_CODE_USE_BEDROCK": "true", "AWS_PROFILE": "prod"}}"#,
        );
        let s = detect_setup(f.path()).unwrap().unwrap();
        assert_eq!(s.profile, "prod");
    }

    #[test]
    fn no_bedrock_when_profile_missing() {
        let f = write_settings(r#"{"env": {"CLAUDE_CODE_USE_BEDROCK": "1"}}"#);
        assert!(detect_setup(f.path()).unwrap().is_none());
    }

    #[test]
    fn no_bedrock_when_flag_falsy() {
        let f = write_settings(
            r#"{"env": {"CLAUDE_CODE_USE_BEDROCK": "0", "AWS_PROFILE": "dev"}}"#,
        );
        assert!(detect_setup(f.path()).unwrap().is_none());
    }

    #[test]
    fn no_bedrock_when_settings_missing() {
        let p = std::env::temp_dir().join("definitely-not-here-agent-container.json");
        assert!(detect_setup(&p).unwrap().is_none());
    }

    #[test]
    fn reads_refresh_from_settings_json_first() {
        let settings = write_settings(r#"{"awsAuthRefresh": "from-settings"}"#);
        let claude_json = write_settings(r#"{"awsAuthRefresh": "from-claude-json"}"#);
        assert_eq!(
            detect_refresh_command(settings.path(), claude_json.path())
                .unwrap()
                .as_deref(),
            Some("from-settings")
        );
    }

    #[test]
    fn falls_back_to_claude_json() {
        let missing =
            std::env::temp_dir().join("agent-container-definitely-no-settings.json");
        let claude_json = write_settings(r#"{"awsAuthRefresh": "aws sso login --profile dev"}"#);
        assert_eq!(
            detect_refresh_command(&missing, claude_json.path())
                .unwrap()
                .as_deref(),
            Some("aws sso login --profile dev")
        );
    }

    #[test]
    fn no_refresh_when_neither_has_it() {
        let settings = write_settings(r#"{"hasCompletedOnboarding": true}"#);
        let claude_json = write_settings(r#"{"theme": "dark"}"#);
        assert!(
            detect_refresh_command(settings.path(), claude_json.path())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn ignores_blank_refresh_command() {
        let settings = write_settings(r#"{"awsAuthRefresh": "   "}"#);
        let claude_json = write_settings(r#"{"awsAuthRefresh": ""}"#);
        assert!(
            detect_refresh_command(settings.path(), claude_json.path())
                .unwrap()
                .is_none()
        );
    }
}
