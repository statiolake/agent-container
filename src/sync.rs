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
//! - `~/.claude/skills/`, `~/.claude/commands/`, `~/.claude/agents/` — user-
//!   authored extensions (custom skills, slash commands, subagents).
//! - `~/.claude/plugins/` — plugin-provided skills, slash commands, and
//!   subagents. Copied verbatim (including `hooks/`, `scripts/`, `.git/`
//!   inside each plugin) because Claude Code re-syncs marketplaces via
//!   `git` on start anyway if they look stale, so pruning subtrees only
//!   thrashes. Hooks declared inside plugins are dormant until a plugin
//!   is explicitly installed, and nothing gets installed automatically.
//!
//! Not copied: user-level hooks, the raw MCP configuration, other projects,
//! or anything under `~/.claude/` not listed above.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::mcp::McpServer;
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
/// - `sandbox`: Claude Code's in-process sandbox is redundant (and noisy)
///   inside the container. The top-level settings.json gets an explicit
///   `{"enabled": false}` re-injected after the strip — Claude Code
///   defaults to sandbox-enabled when the key is absent, and the docker
///   container is already our isolation boundary.
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
    pub bedrock: Option<&'a crate::aws::BedrockSetup>,
    /// `http://host.docker.internal:<broker port>` as seen from the container.
    pub broker_url_from_container: &'a str,
    pub mcp_servers: &'a [McpServer],
    /// When true, inject an `mcpServers.task-runner` entry pointing at
    /// the broker's built-in route so Claude Code inside the container
    /// can call the host-side task commands.
    pub task_runner_enabled: bool,
}

impl SyncOptions<'_> {
    fn is_bedrock(&self) -> bool {
        self.bedrock.is_some()
    }
}

pub fn sync_host_state(host: &HostPaths, opts: SyncOptions<'_>) -> Result<()> {
    fs::create_dir_all(&host.container_home).with_context(|| {
        format!(
            "failed to ensure container home {}",
            host.container_home.display()
        )
    })?;

    sync_claude_json(host, &opts).context("failed to sync .claude.json")?;
    sync_settings_json(host, &opts).context("failed to sync .claude/settings.json")?;
    sync_user_extensions(host).context("failed to sync user skills/commands/agents")?;
    sync_plugin_marketplaces(host).context("failed to sync plugin marketplaces")?;
    sync_git_identity(host).context("failed to sync git identity")?;
    Ok(())
}

/// Query the host's git identity for the current workspace and write it
/// into the container's `~/.gitconfig`. Using `git -C <workspace> config
/// --get` resolves global, local, and any `includeIf` config the host
/// would use in that directory, so the container commits with the same
/// author the host would.
///
/// We write to the container HOME's gitconfig rather than touching
/// `<workspace>/.git/config` directly — the workspace is bind-mounted,
/// so writes there would leak back into the host's repo.
fn sync_git_identity(host: &HostPaths) -> Result<()> {
    let name = host_git_config(&host.workspace, "user.name");
    let email = host_git_config(&host.workspace, "user.email");
    write_container_gitconfig(&host.container_home, name.as_deref(), email.as_deref())
}

fn write_container_gitconfig(
    container_home: &Path,
    name: Option<&str>,
    email: Option<&str>,
) -> Result<()> {
    let dest = container_home.join(".gitconfig");
    match (name, email) {
        (Some(n), Some(e)) => {
            let body = format!("[user]\n\tname = {n}\n\temail = {e}\n");
            fs::write(&dest, body)
                .with_context(|| format!("failed to write {}", dest.display()))?;
        }
        _ => {
            if dest.exists() {
                let _ = fs::remove_file(&dest);
            }
            eprintln!(
                "[agent-container] warning: host has no git user.name / user.email configured for this workspace; `git commit` inside the container will fail until you set them."
            );
        }
    }
    Ok(())
}

fn host_git_config(workspace: &Path, key: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C"])
        .arg(workspace)
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
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

        if opts.is_bedrock() {
            obj.insert(
                "awsCredentialExport".to_string(),
                Value::String(aws_credential_export_command(
                    opts.broker_url_from_container,
                )),
            );
        } else {
            obj.remove("awsCredentialExport");
        }
        // Always strip the older awsAuthRefresh key we used to inject in
        // case a stale persistent home still has it.
        obj.remove("awsAuthRefresh");

        if !opts.mcp_servers.is_empty() || opts.task_runner_enabled {
            obj.insert(
                "mcpServers".to_string(),
                Value::Object(build_proxy_mcp_map(
                    opts.broker_url_from_container,
                    opts.mcp_servers,
                    opts.task_runner_enabled,
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
    servers: &[McpServer],
    task_runner_enabled: bool,
) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    for s in servers {
        let mut entry = serde_json::Map::new();
        // The broker always exposes everything as plain HTTP, even when the
        // original server is SSE or stdio. Pick the closest `type` the
        // Claude Code client understands: keep `sse` for native SSE (so the
        // streaming semantics match), otherwise call it `http`.
        let transport = match s {
            McpServer::Http(h) if h.transport == "sse" => "sse",
            _ => "http",
        };
        entry.insert("type".into(), Value::String(transport.into()));
        entry.insert(
            "url".into(),
            Value::String(format!(
                "{}/mcp/{}",
                broker_url.trim_end_matches('/'),
                s.name()
            )),
        );
        map.insert(s.name().to_string(), Value::Object(entry));
    }
    if task_runner_enabled {
        let name = crate::task_runner::NAME;
        // Skip if the user already has an entry by this name — main.rs's
        // build_task_runner() drops the built-in in that case, and this
        // guard keeps sync in step with that decision.
        if !map.contains_key(name) {
            let mut entry = serde_json::Map::new();
            entry.insert("type".into(), Value::String("http".into()));
            entry.insert(
                "url".into(),
                Value::String(format!(
                    "{}/mcp/{}",
                    broker_url.trim_end_matches('/'),
                    name
                )),
            );
            map.insert(name.to_string(), Value::Object(entry));
        }
    }
    map
}

fn sync_settings_json(host: &HostPaths, opts: &SyncOptions<'_>) -> Result<()> {
    let src = host.claude_root.join("settings.json");
    let mut settings: Value = if src.is_file() {
        let raw = fs::read_to_string(&src)
            .with_context(|| format!("failed to read {}", src.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {} as JSON", src.display()))?
    } else {
        Value::Object(serde_json::Map::new())
    };
    if let Some(obj) = settings.as_object_mut() {
        strip_keys(obj);
        // Positively disable Claude Code's Bash sandbox inside the
        // container. The key was just stripped above; without a positive
        // re-injection Claude Code falls back to its default of
        // sandbox-enabled, which would then second-guess writes and
        // network egress that the docker boundary is already mediating.
        obj.insert(
            "sandbox".to_string(),
            serde_json::json!({ "enabled": false }),
        );
        // Mirror the awsCredentialExport injection we do for .claude.json
        // — Claude Code looks in settings.json first for user-level
        // configuration, which is where the operator most naturally puts
        // it. The command returns JSON on stdout; the broker bridges
        // through the forward proxy so the container never touches its
        // own ~/.aws/credentials.
        if let Some(bedrock) = opts.bedrock {
            obj.insert(
                "awsCredentialExport".to_string(),
                Value::String(aws_credential_export_command(
                    opts.broker_url_from_container,
                )),
            );
            // Strip of `env` happened above via COMMON_STRIP; rebuild a
            // minimal one so Claude Code sees CLAUDE_CODE_USE_BEDROCK
            // (and the model/region it picked) wherever it looks —
            // process env and settings.json env both match.
            let mut env = serde_json::Map::new();
            env.insert("CLAUDE_CODE_USE_BEDROCK".into(), Value::String("1".into()));
            if let Some(model) = &bedrock.model {
                env.insert("ANTHROPIC_MODEL".into(), Value::String(model.clone()));
            }
            if let Some(region) = &bedrock.region {
                env.insert("AWS_REGION".into(), Value::String(region.clone()));
                env.insert("AWS_DEFAULT_REGION".into(), Value::String(region.clone()));
            }
            obj.insert("env".into(), Value::Object(env));
        } else {
            obj.remove("awsCredentialExport");
        }
        obj.remove("awsAuthRefresh");
    }
    let dest_dir = host.container_home.join(".claude");
    fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join("settings.json");
    let pretty = serde_json::to_string_pretty(&settings)?;
    fs::write(&dest, pretty)
        .with_context(|| format!("failed to write {}", dest.display()))?;
    Ok(())
}

/// Build the `awsCredentialExport` shell command for the container.
///
/// Two things to notice:
/// - The broker URL is interpolated at sync time rather than referencing
///   `$AGENT_CONTAINER_HOST_ENDPOINT`, because Claude Code may spawn the
///   hook without a shell that expands env vars.
/// - `-x http://proxy:8888` forces the curl through the compose
///   `proxy` service. The agent container is on a `--internal` network
///   and has no `extra_hosts`, so `host.docker.internal` only resolves
///   (and routes) when we go via the proxy. We cannot rely on
///   `HTTP_PROXY` being inherited by the hook subprocess.
fn aws_credential_export_command(broker_url: &str) -> String {
    format!(
        "curl -fsS --max-time 15 -x http://proxy:8888 {}/aws/credentials",
        broker_url.trim_end_matches('/')
    )
}

fn strip_keys(obj: &mut serde_json::Map<String, Value>) {
    for key in COMMON_STRIP {
        obj.remove(*key);
    }
}

fn sync_user_extensions(host: &HostPaths) -> Result<()> {
    // Custom user skills / slash commands / subagents live under these
    // directories. They're markdown data; mirror them verbatim.
    for name in ["skills", "commands", "agents"] {
        let src = host.claude_root.join(name);
        let dest = host.container_home.join(".claude").join(name);
        mirror_or_clear(&src, &dest)?;
    }
    Ok(())
}

fn sync_plugin_marketplaces(host: &HostPaths) -> Result<()> {
    // Mirror the entire host plugins tree — marketplaces, cache, installed
    // manifest, everything. Trying to prune hooks/scripts inside plugin
    // dirs is pointless because Claude Code re-syncs marketplaces via git
    // whenever the copy looks incomplete; those files come back every
    // run. Plugin-internal hooks stay dormant until a plugin is installed
    // via `installed_plugins.json`, which we do not do automatically.
    let src = host.claude_root.join("plugins");
    let dest = host.container_home.join(".claude").join("plugins");
    mirror_or_clear(&src, &dest)
}

/// Mirror `src` → `dest`, wiping any pre-existing container copy first.
fn mirror_or_clear(src: &Path, dest: &Path) -> Result<()> {
    if dest.is_dir() {
        fs::remove_dir_all(dest)
            .with_context(|| format!("failed to clear {}", dest.display()))?;
    }
    if !src.is_dir() {
        return Ok(());
    }
    copy_dir_recursive(src, dest)
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

    fn sample_bedrock() -> crate::aws::BedrockSetup {
        crate::aws::BedrockSetup {
            profile: "bedrock".into(),
            model: Some("anthropic.claude-sonnet-4-20250514-v1:0".into()),
            region: Some("us-west-2".into()),
        }
    }

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
                bedrock: None,
                broker_url_from_container: "http://host.docker.internal:0",
                mcp_servers: &[],
                task_runner_enabled: false,
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
    fn bedrock_mode_injects_aws_credential_export_and_clears_auth_refresh() {
        let tmp_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let workspace = tmp_home.path().join("work");
        fs::create_dir_all(&workspace).unwrap();
        // A stale persistent home may carry the legacy key from an older
        // agent-container version — sync should remove it unconditionally.
        fs::write(
            tmp_home.path().join(".claude.json"),
            r#"{"awsAuthRefresh": "stale"}"#,
        )
        .unwrap();

        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root: tmp_home.path().join(".claude"),
            workspace,
            container_home: container_home.path().to_path_buf(),
        };
        sync_claude_json(
            &host,
            &SyncOptions {
                bedrock: Some(&sample_bedrock()),
                broker_url_from_container: "http://host.docker.internal:0",
                mcp_servers: &[],
                task_runner_enabled: false,
            },
        )
        .unwrap();

        let out: Value = serde_json::from_str(
            &fs::read_to_string(container_home.path().join(".claude.json")).unwrap(),
        )
        .unwrap();
        assert!(out.get("awsAuthRefresh").is_none(), "awsAuthRefresh must be cleared");
        let export = out["awsCredentialExport"].as_str().unwrap();
        assert!(
            export.contains("http://host.docker.internal:0/aws/credentials"),
            "awsCredentialExport should curl the broker directly (got {export})"
        );
        assert!(
            export.contains("-x http://proxy:8888"),
            "awsCredentialExport must route through the compose proxy (got {export})"
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
        use crate::mcp::HttpMcpServer;
        let servers = vec![
            McpServer::Http(HttpMcpServer {
                name: "github".to_string(),
                transport: "http".to_string(),
                url: "https://upstream/mcp".to_string(),
                headers: Default::default(),
            }),
            McpServer::Http(HttpMcpServer {
                name: "legacy".to_string(),
                transport: "sse".to_string(),
                url: "https://old/mcp".to_string(),
                headers: Default::default(),
            }),
            McpServer::Stdio(crate::mcp::StdioMcpServer {
                name: "local-fs".to_string(),
                command: "node".to_string(),
                args: vec!["srv.js".to_string()],
                env: Default::default(),
            }),
        ];
        sync_claude_json(
            &host,
            &SyncOptions {
                bedrock: None,
                broker_url_from_container: "http://host.docker.internal:9999",
                mcp_servers: &servers,
                task_runner_enabled: false,
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
        // stdio MCP servers get proxied as HTTP in the container view.
        assert_eq!(mcp["local-fs"]["type"].as_str(), Some("http"));
        assert_eq!(
            mcp["local-fs"]["url"].as_str(),
            Some("http://host.docker.internal:9999/mcp/local-fs")
        );
        // auth headers must never end up in the container copy
        assert!(mcp["github"].get("headers").is_none());
    }

    #[test]
    fn task_runner_enabled_adds_builtin_server_to_claude_json() {
        let tmp_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let workspace = tmp_home.path().join("work");
        fs::create_dir_all(&workspace).unwrap();
        let claude_root = tmp_home.path().join(".claude");
        fs::create_dir_all(&claude_root).unwrap();
        fs::write(tmp_home.path().join(".claude.json"), r#"{}"#).unwrap();

        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root,
            workspace,
            container_home: container_home.path().to_path_buf(),
        };
        sync_claude_json(
            &host,
            &SyncOptions {
                bedrock: None,
                broker_url_from_container: "http://host.docker.internal:7000",
                mcp_servers: &[],
                task_runner_enabled: true,
            },
        )
        .unwrap();

        let out: Value = serde_json::from_str(
            &fs::read_to_string(container_home.path().join(".claude.json")).unwrap(),
        )
        .unwrap();
        let tr = &out["mcpServers"]["task-runner"];
        assert_eq!(tr["type"].as_str(), Some("http"));
        assert_eq!(
            tr["url"].as_str(),
            Some("http://host.docker.internal:7000/mcp/task-runner")
        );
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
        sync_settings_json(
            &host,
            &SyncOptions {
                bedrock: None,
                broker_url_from_container: "http://unused",
                mcp_servers: &[],
                task_runner_enabled: false,
            },
        )
        .unwrap();

        let out: Value = serde_json::from_str(
            &fs::read_to_string(container_home.path().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(out["theme"], serde_json::json!("dark"));
        for key in ["env", "hooks", "permissions", "mcpServers"] {
            assert!(out.get(key).is_none(), "{key} should be stripped");
        }
        // The host's `{"mode": "strict"}` must not survive; the container
        // gets an explicit `enabled: false` injection instead.
        assert_eq!(
            out["sandbox"],
            serde_json::json!({ "enabled": false }),
            "sandbox should be forced off inside the container",
        );
        assert!(out.get("awsAuthRefresh").is_none());
    }

    #[test]
    fn bedrock_mode_injects_aws_auth_refresh_into_settings_too() {
        let tmp_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let workspace = tmp_home.path().join("work");
        fs::create_dir_all(&workspace).unwrap();
        let claude_root = tmp_home.path().join(".claude");
        fs::create_dir_all(&claude_root).unwrap();
        fs::write(claude_root.join("settings.json"), r#"{"theme": "dark"}"#).unwrap();

        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root,
            workspace,
            container_home: container_home.path().to_path_buf(),
        };
        sync_settings_json(
            &host,
            &SyncOptions {
                bedrock: Some(&sample_bedrock()),
                broker_url_from_container: "http://unused",
                mcp_servers: &[],
                task_runner_enabled: false,
            },
        )
        .unwrap();

        let out: Value = serde_json::from_str(
            &fs::read_to_string(container_home.path().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        assert!(out.get("awsAuthRefresh").is_none());
        let export = out["awsCredentialExport"].as_str().unwrap();
        assert!(export.contains("http://unused/aws/credentials"));
        assert!(export.contains("-x http://proxy:8888"));
        // env is rebuilt for Claude Code that reads it from settings.json
        let env = out["env"].as_object().expect("env object injected");
        assert_eq!(env["CLAUDE_CODE_USE_BEDROCK"].as_str(), Some("1"));
        assert_eq!(
            env["ANTHROPIC_MODEL"].as_str(),
            Some("anthropic.claude-sonnet-4-20250514-v1:0")
        );
        assert_eq!(env["AWS_REGION"].as_str(), Some("us-west-2"));
        assert_eq!(env["AWS_DEFAULT_REGION"].as_str(), Some("us-west-2"));
    }

    #[test]
    fn gitconfig_written_when_both_values_present() {
        let container_home = tempfile::tempdir().unwrap();
        write_container_gitconfig(
            container_home.path(),
            Some("Example User"),
            Some("user@example.com"),
        )
        .unwrap();
        let body = fs::read_to_string(container_home.path().join(".gitconfig")).unwrap();
        assert!(body.contains("name = Example User"));
        assert!(body.contains("email = user@example.com"));
    }

    #[test]
    fn gitconfig_removed_when_values_missing() {
        let container_home = tempfile::tempdir().unwrap();
        let dest = container_home.path().join(".gitconfig");
        fs::write(&dest, "[user]\n\tname = stale\n").unwrap();

        write_container_gitconfig(container_home.path(), None, Some("only@example.com"))
            .unwrap();
        assert!(!dest.exists(), "stale gitconfig should be removed");
    }
}
