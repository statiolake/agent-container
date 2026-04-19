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
pub struct BedrockCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub region: Option<String>,
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

/// Invoke `aws configure export-credentials` on the host; works uniformly
/// for static keys, SSO sessions, and assume-role profiles.
pub fn resolve_credentials(setup: &BedrockSetup) -> Result<BedrockCredentials> {
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
            "`aws configure export-credentials --profile {}` failed. If you use SSO, run `aws sso login --profile {}` on the host first.\n{}",
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

    let region = setup.region.clone().or_else(|| lookup_profile_region(&setup.profile));

    Ok(BedrockCredentials {
        access_key_id: parsed.access_key_id,
        secret_access_key: parsed.secret_access_key,
        session_token: parsed.session_token,
        region,
    })
}

fn lookup_profile_region(profile: &str) -> Option<String> {
    let output = Command::new("aws")
        .args(["configure", "get", "region", "--profile", profile])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let region = String::from_utf8(output.stdout).ok()?;
    let trimmed = region.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
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
}
