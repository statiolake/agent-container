//! Sync a filtered subset of the host's Claude Code state into the
//! container's persistent `$HOME` before each run.
//!
//! What moves across:
//! - `~/.claude.json` — top-level preferences, onboarding flags, per-project
//!   settings for the current workspace. MCP server definitions are stripped
//!   (Phase 3 will reintroduce them via an in-container proxy). Other
//!   `projects.<path>` entries are dropped so the container only sees its
//!   own workspace, with its path rewritten to `/workspace`.
//! - `~/.claude/settings.json` — user-level settings, copied as-is.
//! - `~/.claude/skills/` — copied recursively so the container's Claude Code
//!   can load the same skills the host has.
//!
//! Not copied: hooks, plugins, raw MCP configuration, other projects.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::mcp::HttpMcpServer;
use crate::paths::HostPaths;

const CONTAINER_WORKSPACE: &str = "/workspace";

/// Keys stripped from every object we copy over (top-level of `.claude.json`,
/// per-project entries, and `settings.json`). Each of these either references
/// host-only state, holds policy that stops making sense inside the
/// container, or would be bypassed regardless:
/// - `mcpServers` + friends: handled separately by the container's proxy path.
/// - `env`: exports can reference host tool paths that don't exist here.
/// - `hooks`: shell commands that typically shell out to host binaries.
/// - `permissions`: we run with `--dangerously-skip-permissions` anyway.
/// - `sandbox`: Claude Code's in-process sandbox is redundant (and bypassed)
///   inside the container.
const COMMON_STRIP: &[&str] = &[
    "mcpServers",
    "mcpContextUris",
    "enabledMcpjsonServers",
    "disabledMcpjsonServers",
    "enabledMcpServers",
    "disabledMcpServers",
    "env",
    "hooks",
    "permissions",
    "sandbox",
];

pub struct SyncOptions<'a> {
    pub bedrock: bool,
    /// `http://host.docker.internal:<broker port>` as seen from the container.
    pub broker_url_from_container: &'a str,
    pub mcp_servers: &'a [HttpMcpServer],
}

pub fn sync_host_state(host: &HostPaths, opts: SyncOptions<'_>) -> Result<()> {
    fs::create_dir_all(&host.container_home).with_context(|| {
        format!(
            "failed to ensure container home {}",
            host.container_home.display()
        )
    })?;

    sync_claude_json(host, &opts).context("failed to sync .claude.json")?;
    sync_settings_json(host).context("failed to sync .claude/settings.json")?;
    sync_skills(host).context("failed to sync .claude/skills")?;
    if opts.bedrock {
        ensure_dummy_aws_profile(host).context("failed to prepare dummy AWS bedrock profile")?;
    }
    Ok(())
}

fn sync_claude_json(host: &HostPaths, opts: &SyncOptions<'_>) -> Result<()> {
    let src = host.home.join(".claude.json");
    let mut cfg: Value = if src.is_file() {
        let raw = fs::read_to_string(&src)
            .with_context(|| format!("failed to read {}", src.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {} as JSON", src.display()))?
    } else {
        Value::Object(serde_json::Map::new())
    };

    if let Some(obj) = cfg.as_object_mut() {
        strip_keys(obj);

        // Keep only the current workspace's entry, rewritten to /workspace.
        if let Some(Value::Object(projects)) = obj.get_mut("projects") {
            let workspace_key = host.workspace.display().to_string();
            let surviving = projects.remove(&workspace_key).unwrap_or(Value::Null);
            let mut filtered = serde_json::Map::new();
            if let Value::Object(mut entry) = surviving {
                strip_keys(&mut entry);
                filtered.insert(CONTAINER_WORKSPACE.to_string(), Value::Object(entry));
            }
            *projects = filtered;
        }

        if opts.bedrock {
            obj.insert(
                "awsAuthRefresh".to_string(),
                Value::String("/usr/local/bin/agent-container-aws-refresh".to_string()),
            );
        } else {
            obj.remove("awsAuthRefresh");
        }

        if !opts.mcp_servers.is_empty() {
            obj.insert(
                "mcpServers".to_string(),
                Value::Object(build_proxy_mcp_map(
                    opts.broker_url_from_container,
                    opts.mcp_servers,
                )),
            );
        }
    }

    let dest = host.container_home.join(".claude.json");
    let pretty = serde_json::to_string_pretty(&cfg)?;
    fs::write(&dest, pretty).with_context(|| format!("failed to write {}", dest.display()))?;
    Ok(())
}

fn build_proxy_mcp_map(
    broker_url: &str,
    servers: &[HttpMcpServer],
) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    for s in servers {
        let mut entry = serde_json::Map::new();
        entry.insert("type".into(), Value::String(s.transport.clone()));
        entry.insert(
            "url".into(),
            Value::String(format!(
                "{}/mcp/{}",
                broker_url.trim_end_matches('/'),
                s.name
            )),
        );
        map.insert(s.name.clone(), Value::Object(entry));
    }
    map
}

/// Initialise a dummy `~/.aws/credentials` with a `[bedrock]` section if
/// one does not already exist, so Claude Code has something to point
/// `AWS_PROFILE=bedrock` at on first use. The placeholder values will be
/// rejected by Bedrock, which triggers `awsAuthRefresh` to fetch real ones.
fn ensure_dummy_aws_profile(host: &HostPaths) -> Result<()> {
    let aws_dir = host.container_home.join(".aws");
    fs::create_dir_all(&aws_dir)
        .with_context(|| format!("failed to create {}", aws_dir.display()))?;
    let creds_path = aws_dir.join("credentials");
    if creds_path.exists() {
        return Ok(());
    }
    fs::write(
        &creds_path,
        "[bedrock]\n\
         aws_access_key_id = PLACEHOLDER\n\
         aws_secret_access_key = PLACEHOLDER\n",
    )
    .with_context(|| format!("failed to write {}", creds_path.display()))?;
    Ok(())
}

fn sync_settings_json(host: &HostPaths) -> Result<()> {
    let src = host.claude_root.join("settings.json");
    if !src.is_file() {
        return Ok(());
    }
    let raw = fs::read_to_string(&src)
        .with_context(|| format!("failed to read {}", src.display()))?;
    let mut settings: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {} as JSON", src.display()))?;
    if let Some(obj) = settings.as_object_mut() {
        strip_keys(obj);
    }
    let dest_dir = host.container_home.join(".claude");
    fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join("settings.json");
    let pretty = serde_json::to_string_pretty(&settings)?;
    fs::write(&dest, pretty)
        .with_context(|| format!("failed to write {}", dest.display()))?;
    Ok(())
}

fn strip_keys(obj: &mut serde_json::Map<String, Value>) {
    for key in COMMON_STRIP {
        obj.remove(*key);
    }
}

fn sync_skills(host: &HostPaths) -> Result<()> {
    let src = host.claude_root.join("skills");
    let dest = host.container_home.join(".claude").join("skills");
    if !src.is_dir() {
        // Remove stale container-side skills if host no longer has any.
        if dest.is_dir() {
            fs::remove_dir_all(&dest).ok();
        }
        return Ok(());
    }
    if dest.is_dir() {
        fs::remove_dir_all(&dest)
            .with_context(|| format!("failed to clear {}", dest.display()))?;
    }
    copy_dir_recursive(&src, &dest)
        .with_context(|| format!("failed to copy {} to {}", src.display(), dest.display()))?;
    Ok(())
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        let target = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else if file_type.is_symlink() {
            let link_target = fs::read_link(&path)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(link_target, &target)?;
        } else {
            fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtering_drops_mcp_and_rewrites_workspace() {
        let tmp_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let workspace = tmp_home.path().join("work/repo");
        fs::create_dir_all(&workspace).unwrap();

        let workspace_key = workspace.display().to_string();
        let synthetic = format!(
            r#"{{
              "mcpServers": {{"github": {{"command": "foo"}}}},
              "env": {{"HOST_ONLY": "/opt/host/bin"}},
              "hooks": {{"PreToolUse": ["echo hi"]}},
              "permissions": {{"allow": ["*"]}},
              "sandbox": {{"mode": "strict"}},
              "hasCompletedOnboarding": true,
              "projects": {{
                "{ws}": {{
                  "allowedTools": ["bash"],
                  "mcpServers": {{"x": {{}}}},
                  "env": {{"ANOTHER": "/host/path"}},
                  "hooks": {{"SessionStart": ["tool"]}},
                  "permissions": {{"deny": ["git push"]}},
                  "sandbox": {{"enabled": true}},
                  "lastCost": 1.23
                }},
                "{ws}-other": {{ "allowedTools": [] }}
              }}
            }}"#,
            ws = workspace_key
        );
        fs::write(tmp_home.path().join(".claude.json"), synthetic).unwrap();

        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root: tmp_home.path().join(".claude"),
            workspace,
            container_home: container_home.path().to_path_buf(),
        };

        sync_claude_json(
            &host,
            &SyncOptions {
                bedrock: false,
                broker_url_from_container: "http://host.docker.internal:0",
                mcp_servers: &[],
            },
        )
        .unwrap();

        let out: Value = serde_json::from_str(
            &fs::read_to_string(container_home.path().join(".claude.json")).unwrap(),
        )
        .unwrap();
        for key in ["mcpServers", "env", "hooks", "permissions", "sandbox"] {
            assert!(
                out.get(key).is_none(),
                "top-level {key} must be removed"
            );
        }
        assert_eq!(out["hasCompletedOnboarding"], serde_json::json!(true));
        let projects = out["projects"].as_object().unwrap();
        assert_eq!(projects.len(), 1, "only current workspace survives");
        let entry = &projects["/workspace"];
        for key in ["mcpServers", "env", "hooks", "permissions", "sandbox"] {
            assert!(
                entry.get(key).is_none(),
                "per-project {key} must be removed"
            );
        }
        assert_eq!(entry["allowedTools"], serde_json::json!(["bash"]));
        assert_eq!(entry["lastCost"], serde_json::json!(1.23));
        assert!(out.get("awsAuthRefresh").is_none());
    }

    #[test]
    fn bedrock_mode_injects_aws_auth_refresh() {
        let tmp_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let workspace = tmp_home.path().join("work");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(tmp_home.path().join(".claude.json"), "{}").unwrap();

        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root: tmp_home.path().join(".claude"),
            workspace,
            container_home: container_home.path().to_path_buf(),
        };
        sync_claude_json(
            &host,
            &SyncOptions {
                bedrock: true,
                broker_url_from_container: "http://host.docker.internal:0",
                mcp_servers: &[],
            },
        )
        .unwrap();

        let out: Value = serde_json::from_str(
            &fs::read_to_string(container_home.path().join(".claude.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            out["awsAuthRefresh"].as_str(),
            Some("/usr/local/bin/agent-container-aws-refresh")
        );
    }

    #[test]
    fn mcp_servers_are_rewritten_to_proxy_urls() {
        let tmp_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let workspace = tmp_home.path().join("work");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(tmp_home.path().join(".claude.json"), "{}").unwrap();

        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root: tmp_home.path().join(".claude"),
            workspace,
            container_home: container_home.path().to_path_buf(),
        };
        let servers = vec![
            HttpMcpServer {
                name: "github".to_string(),
                transport: "http".to_string(),
                url: "https://upstream/mcp".to_string(),
                headers: Default::default(),
            },
            HttpMcpServer {
                name: "legacy".to_string(),
                transport: "sse".to_string(),
                url: "https://old/mcp".to_string(),
                headers: Default::default(),
            },
        ];
        sync_claude_json(
            &host,
            &SyncOptions {
                bedrock: false,
                broker_url_from_container: "http://host.docker.internal:9999",
                mcp_servers: &servers,
            },
        )
        .unwrap();

        let out: Value = serde_json::from_str(
            &fs::read_to_string(container_home.path().join(".claude.json")).unwrap(),
        )
        .unwrap();
        let mcp = out["mcpServers"].as_object().unwrap();
        assert_eq!(
            mcp["github"]["url"].as_str(),
            Some("http://host.docker.internal:9999/mcp/github")
        );
        assert_eq!(mcp["github"]["type"].as_str(), Some("http"));
        assert_eq!(mcp["legacy"]["type"].as_str(), Some("sse"));
        // auth headers must never end up in the container copy
        assert!(mcp["github"].get("headers").is_none());
    }

    #[test]
    fn ensures_dummy_aws_profile_when_missing() {
        let tmp_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root: tmp_home.path().join(".claude"),
            workspace: tmp_home.path().join("work"),
            container_home: container_home.path().to_path_buf(),
        };
        ensure_dummy_aws_profile(&host).unwrap();
        let creds = fs::read_to_string(container_home.path().join(".aws/credentials")).unwrap();
        assert!(creds.contains("[bedrock]"));
        assert!(creds.contains("PLACEHOLDER"));
    }

    #[test]
    fn preserves_existing_aws_credentials() {
        let tmp_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let aws_dir = container_home.path().join(".aws");
        fs::create_dir_all(&aws_dir).unwrap();
        fs::write(
            aws_dir.join("credentials"),
            "[bedrock]\naws_access_key_id = REAL\naws_secret_access_key = REAL_SECRET\n",
        )
        .unwrap();
        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root: tmp_home.path().join(".claude"),
            workspace: tmp_home.path().join("work"),
            container_home: container_home.path().to_path_buf(),
        };
        ensure_dummy_aws_profile(&host).unwrap();
        let creds = fs::read_to_string(aws_dir.join("credentials")).unwrap();
        assert!(creds.contains("REAL"));
        assert!(!creds.contains("PLACEHOLDER"));
    }

    #[test]
    fn settings_json_is_filtered() {
        let tmp_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let workspace = tmp_home.path().join("work");
        fs::create_dir_all(&workspace).unwrap();
        let claude_root = tmp_home.path().join(".claude");
        fs::create_dir_all(&claude_root).unwrap();
        fs::write(
            claude_root.join("settings.json"),
            r#"{
              "theme": "dark",
              "env": {"FOO": "bar"},
              "hooks": {"PreToolUse": ["echo"]},
              "permissions": {"allow": ["*"]},
              "sandbox": {"mode": "strict"},
              "mcpServers": {"x": {}}
            }"#,
        )
        .unwrap();

        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root,
            workspace,
            container_home: container_home.path().to_path_buf(),
        };
        sync_settings_json(&host).unwrap();

        let out: Value = serde_json::from_str(
            &fs::read_to_string(container_home.path().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(out["theme"], serde_json::json!("dark"));
        for key in ["env", "hooks", "permissions", "sandbox", "mcpServers"] {
            assert!(out.get(key).is_none(), "{key} should be stripped");
        }
    }
}
