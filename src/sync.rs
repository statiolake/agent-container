//! Sync a filtered subset of the host's Claude Code state into the
//! a per-run staging tree before each container start.
//!
//! What moves across:
//! - `~/.claude.json` — top-level preferences, onboarding flags, per-project
//!   settings for the current workspace. MCP server definitions from both
//!   top-level and the current project are collected separately and
//!   reintroduced via the broker; raw MCP definitions are stripped from the
//!   staged copy. Other `projects.<path>` entries are dropped so the
//!   container only sees its own workspace. The container uses the same
//!   absolute workspace path as the host so Claude Code resume keys stay
//!   stable across native and containerised runs.
//! - `~/.claude/settings.json` — user-level settings with host-bound values
//!   filtered; hook definitions are preserved.
//! - `~/.claude/skills/`, `~/.claude/commands/`, `~/.claude/agents/` — user-
//!   authored extensions (custom skills, slash commands, subagents).
//! - Plugin-provided `skills/` and `commands/` are flattened into the same
//!   top-level extension directories. The plugin marketplace/cache tree
//!   itself is not copied, because Claude Code treats that tree as managed
//!   marketplace state and may try to refresh it over the network.
//!
//! Hooks are copied as configured. A hook that invokes a host-only command may
//! fail inside the container, but preserving the configuration lets hooks
//! that are container-compatible continue to run.
//!
//! Not copied: the raw MCP configuration, other projects, or anything under
//! `~/.claude/` not listed above.

use std::fs;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::mcp::McpServer;
use crate::paths::HostPaths;

/// Keys stripped from every object we copy over (top-level of `.claude.json`,
/// per-project entries, and `settings.json`). Each of these either references
/// host-only state, holds policy that stops making sense inside the
/// container, or would be bypassed regardless:
/// - `mcpServers` + friends: handled separately by the container's proxy path.
/// - `env`: exports can reference host tool paths that don't exist here.
/// - `permissions`: we run Claude Code in bypass-permissions mode anyway.
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
    "permissions",
    "sandbox",
];

pub struct SyncOptions<'a> {
    pub bedrock: Option<&'a crate::aws::BedrockSetup>,
    /// `http://<engine-specific host>:<broker port>` as seen from the
    /// container. The hostname is chosen by `host_kind::HostKind` at
    /// startup — `host.docker.internal` on Docker Desktop and native
    /// Linux Docker, `host.lima.internal` on Rancher Desktop.
    pub broker_url_from_container: &'a str,
    pub mcp_servers: &'a [McpServer],
    /// When true, inject an `mcpServers.task-runner` entry pointing at
    /// the broker's built-in route so Claude Code inside the container
    /// can call the host-side task commands.
    pub task_runner_enabled: bool,
    /// When true, inject the broker's built-in host filesystem MCP.
    pub host_fs_enabled: bool,
    /// When true, pre-acknowledge Claude Code's bypass-permissions warning in
    /// the staged container settings. Defaults false so Claude Code can show
    /// its normal confirmation dialog.
    pub skip_bypass_permissions_warning: bool,
}

impl SyncOptions<'_> {
    fn is_bedrock(&self) -> bool {
        self.bedrock.is_some()
    }
}

pub fn sync_host_state(host: &HostPaths, opts: SyncOptions<'_>) -> Result<()> {
    fs::create_dir_all(&host.staged_home).with_context(|| {
        format!(
            "failed to ensure staged home {}",
            host.staged_home.display()
        )
    })?;

    sync_claude_json(host, &opts).context("failed to sync .claude.json")?;
    sync_settings_json(host, &opts).context("failed to sync .claude/settings.json")?;
    sync_claude_md(host).context("failed to stage .claude/CLAUDE.md")?;
    sync_claude_extensions(host).context("failed to sync Claude skills/commands/agents")?;
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
    write_container_gitconfig(&host.staged_home, name.as_deref(), email.as_deref())
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
            fs::write(&dest, "")
                .with_context(|| format!("failed to write empty {}", dest.display()))?;
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
        .stdin(Stdio::null())
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

        // Keep only the current workspace's entry, preserving the same
        // absolute path inside the container so Claude Code can resume
        // sessions created by a native host-side run.
        if let Some(Value::Object(projects)) = obj.get_mut("projects") {
            let workspace_key = host.workspace.display().to_string();
            let container_key = host.container_workspace().display().to_string();
            let surviving = projects.remove(&workspace_key).unwrap_or(Value::Null);
            let mut filtered = serde_json::Map::new();
            if let Value::Object(mut entry) = surviving {
                strip_keys(&mut entry);
                filtered.insert(container_key, Value::Object(entry));
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
        // Always strip the older awsAuthRefresh key we used to inject.
        obj.remove("awsAuthRefresh");
        inject_agent_teams_env(obj);

        if !opts.mcp_servers.is_empty() || opts.task_runner_enabled || opts.host_fs_enabled {
            obj.insert(
                "mcpServers".to_string(),
                Value::Object(build_proxy_mcp_map(
                    opts.broker_url_from_container,
                    opts.mcp_servers,
                    opts.task_runner_enabled,
                    opts.host_fs_enabled,
                )),
            );
        }
    }

    let dest = host.staged_home.join(".claude.json");
    let pretty = serde_json::to_string_pretty(&cfg)?;
    fs::write(&dest, pretty).with_context(|| format!("failed to write {}", dest.display()))?;
    Ok(())
}

fn build_proxy_mcp_map(
    broker_url: &str,
    servers: &[McpServer],
    task_runner_enabled: bool,
    host_fs_enabled: bool,
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
                Value::String(format!("{}/mcp/{}", broker_url.trim_end_matches('/'), name)),
            );
            map.insert(name.to_string(), Value::Object(entry));
        }
    }
    if host_fs_enabled {
        let name = crate::host_fs::NAME;
        if !map.contains_key(name) {
            let mut entry = serde_json::Map::new();
            entry.insert("type".into(), Value::String("http".into()));
            entry.insert(
                "url".into(),
                Value::String(format!("{}/mcp/{}", broker_url.trim_end_matches('/'), name)),
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
        if opts.skip_bypass_permissions_warning {
            // Skip Claude Code's bypass-permissions warning in the staged
            // user settings only, so the host profile is not modified. This
            // is intentionally written to ~/.claude/settings.json inside the
            // container; Claude Code ignores this key in project settings.
            obj.insert(
                "skipDangerousModePermissionPrompt".into(),
                Value::Bool(true),
            );
        } else {
            obj.remove("skipDangerousModePermissionPrompt");
        }
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
            env.insert("AWS_PROFILE".into(), Value::String(bedrock.profile.clone()));
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
        inject_agent_teams_env(obj);
    }
    let dest_dir = host.staged_home.join(".claude");
    fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join("settings.json");
    let pretty = serde_json::to_string_pretty(&settings)?;
    fs::write(&dest, pretty).with_context(|| format!("failed to write {}", dest.display()))?;
    Ok(())
}

/// Build the `awsCredentialExport` shell command for the container.
///
/// Three things to notice:
/// - The broker URL is interpolated at sync time rather than referencing
///   `$AGENT_CONTAINER_HOST_ENDPOINT`, because Claude Code may spawn the
///   hook without a shell that expands env vars.
/// - `-x http://proxy:8888` forces the curl through the compose
///   `proxy` service. The agent container is on a `--internal` network
///   and has no `extra_hosts`, so the broker host only resolves
///   (and routes) when we go via the proxy. We cannot rely on
///   `HTTP_PROXY` being inherited by the hook subprocess.
/// - The hostname inside `broker_url` is not always `host.docker.internal`
///   — it depends on which Docker engine is hosting us (see
///   `host_kind::HostKind`). Rancher Desktop, for instance, gets
///   `host.lima.internal`.
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

pub fn sanitize_claude_config_for_container(raw: &str) -> Result<String> {
    let mut value: Value =
        serde_json::from_str(raw).context("failed to parse Claude JSON config")?;
    if let Some(obj) = value.as_object_mut() {
        strip_keys(obj);
    }
    Ok(serde_json::to_string_pretty(&value)?)
}

fn inject_agent_teams_env(obj: &mut serde_json::Map<String, Value>) {
    let env = obj
        .entry("env".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !env.is_object() {
        *env = Value::Object(serde_json::Map::new());
    }
    let env = env.as_object_mut().expect("env must be an object");
    env.insert(
        "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS".to_string(),
        Value::String("1".to_string()),
    );
}

fn sync_claude_md(host: &HostPaths) -> Result<()> {
    let src = host.host_claude_md();
    let dest = host.staged_home.join(".claude/CLAUDE.md");
    clear_path(&dest)?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let body = if src.is_file() {
        fs::read_to_string(&src).with_context(|| format!("failed to read {}", src.display()))?
    } else {
        String::new()
    };
    fs::write(&dest, crate::container_notice::append_to(&body))
        .with_context(|| format!("failed to write {}", dest.display()))?;
    Ok(())
}

fn sync_claude_extensions(host: &HostPaths) -> Result<()> {
    // User-authored extensions keep their native top-level shape. Plugin
    // marketplaces are a different ownership model: Claude Code manages
    // them as remote state, so the container receives only their portable
    // skills/commands payloads merged into the top-level extension dirs.
    for name in ["skills", "commands"] {
        let src = host.claude_root.join(name);
        let dest = host.staged_home.join(".claude").join(name);
        mirror_or_clear(&src, &dest)?;
        merge_plugin_extension_dirs(host, name)
            .with_context(|| format!("failed to merge plugin {name}"))?;
    }

    let src = host.claude_root.join("agents");
    let dest = host.staged_home.join(".claude").join("agents");
    mirror_or_clear(&src, &dest)?;

    // Never stage the plugin marketplace/cache tree; only flattened portable
    // commands and skills belong in the container.
    let plugin_dest = host.staged_home.join(".claude").join("plugins");
    clear_path(&plugin_dest)?;
    Ok(())
}

/// Mirror `src` → `dest`, wiping any pre-existing container copy first.
fn mirror_or_clear(src: &Path, dest: &Path) -> Result<()> {
    clear_path(dest)?;
    if !src.is_dir() {
        fs::create_dir_all(dest)
            .with_context(|| format!("failed to create empty {}", dest.display()))?;
        return Ok(());
    }
    copy_dir_recursive(src, dest)
        .with_context(|| format!("failed to copy {} to {}", src.display(), dest.display()))?;
    Ok(())
}

fn merge_plugin_extension_dirs(host: &HostPaths, name: &str) -> Result<()> {
    let plugin_root = host.claude_root.join("plugins");
    let mut extension_dirs = Vec::new();
    collect_extension_dirs(&plugin_root, name, &mut extension_dirs)?;
    extension_dirs.sort();

    let dest = host.staged_home.join(".claude").join(name);
    for src in extension_dirs {
        copy_children_without_overwrite(&src, &dest)?;
    }
    Ok(())
}

fn collect_extension_dirs(
    root: &Path,
    name: &str,
    out: &mut Vec<std::path::PathBuf>,
) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }

    let mut entries = fs::read_dir(root)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        if entry.file_name() == name {
            out.push(path);
            continue;
        }
        collect_extension_dirs(&path, name, out)?;
    }
    Ok(())
}

fn copy_children_without_overwrite(src: &Path, dest: &Path) -> Result<()> {
    let mut entries = fs::read_dir(src)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());
    if entries.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dest)?;

    for entry in entries {
        let target = dest.join(entry.file_name());
        if target.exists() {
            continue;
        }
        copy_entry(&entry.path(), &target)?;
    }
    Ok(())
}

fn clear_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path).with_context(|| format!("failed to clear {}", path.display()))?;
    } else if path.exists() {
        fs::remove_file(path).with_context(|| format!("failed to clear {}", path.display()))?;
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    copy_dir_recursive_excluding_names(src, dest, &[])
}

fn copy_entry(src: &Path, dest: &Path) -> Result<()> {
    let mut stack = Vec::new();
    copy_entry_inner(src, dest, &[], &mut stack)
}

fn copy_dir_recursive_excluding_names(
    src: &Path,
    dest: &Path,
    excluded_names: &[&str],
) -> Result<()> {
    let mut stack = Vec::new();
    copy_dir_recursive_inner(src, dest, excluded_names, &mut stack)
}

fn copy_dir_recursive_inner(
    src: &Path,
    dest: &Path,
    excluded_names: &[&str],
    stack: &mut Vec<std::path::PathBuf>,
) -> Result<()> {
    let canonical = src
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", src.display()))?;
    if stack.contains(&canonical) {
        anyhow::bail!("refusing to copy symlink cycle at {}", src.display());
    }
    stack.push(canonical);

    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        if excluded_names
            .iter()
            .any(|excluded| entry.file_name() == *excluded)
        {
            continue;
        }
        let path = entry.path();
        let target = dest.join(entry.file_name());
        copy_entry_inner(&path, &target, excluded_names, stack)?;
    }
    stack.pop();
    Ok(())
}

fn copy_entry_inner(
    src: &Path,
    dest: &Path,
    excluded_names: &[&str],
    stack: &mut Vec<std::path::PathBuf>,
) -> Result<()> {
    // `metadata` follows symlinks. Staged Claude extensions must be portable
    // inside the container, so a host symlink is copied as the target's actual
    // file or directory rather than recreated as a link to a host-only path.
    let metadata =
        fs::metadata(src).with_context(|| format!("failed to stat {}", src.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        copy_dir_recursive_inner(src, dest, excluded_names, stack)?;
    } else {
        fs::copy(src, dest)?;
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
    fn filtering_drops_host_bound_config_and_preserves_hooks() {
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
            staged_home: container_home.path().to_path_buf(),
        };

        sync_claude_json(
            &host,
            &SyncOptions {
                bedrock: None,
                broker_url_from_container: "http://host.docker.internal:0",
                mcp_servers: &[],
                task_runner_enabled: false,
                host_fs_enabled: false,
                skip_bypass_permissions_warning: false,
            },
        )
        .unwrap();

        let out: Value = serde_json::from_str(
            &fs::read_to_string(container_home.path().join(".claude.json")).unwrap(),
        )
        .unwrap();
        for key in ["mcpServers", "permissions", "sandbox"] {
            assert!(out.get(key).is_none(), "top-level {key} must be removed");
        }
        assert_eq!(out["hooks"], serde_json::json!({"PreToolUse": ["echo hi"]}));
        let env = out["env"].as_object().expect("agent env injected");
        assert_eq!(
            env["CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS"].as_str(),
            Some("1")
        );
        assert!(
            env.get("HOST_ONLY").is_none(),
            "host env values must not survive"
        );
        assert_eq!(out["hasCompletedOnboarding"], serde_json::json!(true));
        let projects = out["projects"].as_object().unwrap();
        assert_eq!(projects.len(), 1, "only current workspace survives");
        let entry = &projects[&host.container_workspace().display().to_string()];
        for key in ["mcpServers", "env", "permissions", "sandbox"] {
            assert!(
                entry.get(key).is_none(),
                "per-project {key} must be removed"
            );
        }
        assert_eq!(
            entry["hooks"],
            serde_json::json!({"SessionStart": ["tool"]})
        );
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
        // A stale host config may carry the legacy key from an older
        // agent-container version; sync should remove it unconditionally.
        fs::write(
            tmp_home.path().join(".claude.json"),
            r#"{"awsAuthRefresh": "stale"}"#,
        )
        .unwrap();

        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root: tmp_home.path().join(".claude"),
            workspace,
            staged_home: container_home.path().to_path_buf(),
        };
        sync_claude_json(
            &host,
            &SyncOptions {
                bedrock: Some(&sample_bedrock()),
                broker_url_from_container: "http://host.docker.internal:0",
                mcp_servers: &[],
                task_runner_enabled: false,
                host_fs_enabled: false,
                skip_bypass_permissions_warning: false,
            },
        )
        .unwrap();

        let out: Value = serde_json::from_str(
            &fs::read_to_string(container_home.path().join(".claude.json")).unwrap(),
        )
        .unwrap();
        assert!(
            out.get("awsAuthRefresh").is_none(),
            "awsAuthRefresh must be cleared"
        );
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
            staged_home: container_home.path().to_path_buf(),
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
                host_fs_enabled: false,
                skip_bypass_permissions_warning: false,
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
            staged_home: container_home.path().to_path_buf(),
        };
        sync_claude_json(
            &host,
            &SyncOptions {
                bedrock: None,
                broker_url_from_container: "http://host.docker.internal:7000",
                mcp_servers: &[],
                task_runner_enabled: true,
                host_fs_enabled: false,
                skip_bypass_permissions_warning: false,
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
    fn host_fs_enabled_adds_builtin_server_to_claude_json() {
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
            staged_home: container_home.path().to_path_buf(),
        };
        sync_claude_json(
            &host,
            &SyncOptions {
                bedrock: None,
                broker_url_from_container: "http://host.docker.internal:7000",
                mcp_servers: &[],
                task_runner_enabled: false,
                host_fs_enabled: true,
                skip_bypass_permissions_warning: false,
            },
        )
        .unwrap();

        let out: Value = serde_json::from_str(
            &fs::read_to_string(container_home.path().join(".claude.json")).unwrap(),
        )
        .unwrap();
        let host_fs = &out["mcpServers"]["host-fs"];
        assert_eq!(host_fs["type"].as_str(), Some("http"));
        assert_eq!(
            host_fs["url"].as_str(),
            Some("http://host.docker.internal:7000/mcp/host-fs")
        );
    }

    #[test]
    fn settings_json_filters_host_bound_values_but_preserves_hooks() {
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
            staged_home: container_home.path().to_path_buf(),
        };
        sync_settings_json(
            &host,
            &SyncOptions {
                bedrock: None,
                broker_url_from_container: "http://unused",
                mcp_servers: &[],
                task_runner_enabled: false,
                host_fs_enabled: false,
                skip_bypass_permissions_warning: false,
            },
        )
        .unwrap();

        let out: Value = serde_json::from_str(
            &fs::read_to_string(container_home.path().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(out["theme"], serde_json::json!("dark"));
        for key in ["permissions", "mcpServers"] {
            assert!(out.get(key).is_none(), "{key} should be stripped");
        }
        assert_eq!(out["hooks"], serde_json::json!({"PreToolUse": ["echo"]}));
        let env = out["env"].as_object().expect("agent env injected");
        assert_eq!(
            env["CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS"].as_str(),
            Some("1")
        );
        assert!(env.get("FOO").is_none(), "host env values must not survive");
        // The host's `{"mode": "strict"}` must not survive; the container
        // gets an explicit `enabled: false` injection instead.
        assert_eq!(
            out["sandbox"],
            serde_json::json!({ "enabled": false }),
            "sandbox should be forced off inside the container",
        );
        assert!(
            out.get("skipDangerousModePermissionPrompt").is_none(),
            "bypass-permissions warning should be confirmed by default",
        );
        assert!(out.get("awsAuthRefresh").is_none());
    }

    #[test]
    fn settings_json_can_skip_bypass_permissions_warning() {
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
            staged_home: container_home.path().to_path_buf(),
        };
        sync_settings_json(
            &host,
            &SyncOptions {
                bedrock: None,
                broker_url_from_container: "http://unused",
                mcp_servers: &[],
                task_runner_enabled: false,
                host_fs_enabled: false,
                skip_bypass_permissions_warning: true,
            },
        )
        .unwrap();

        let out: Value = serde_json::from_str(
            &fs::read_to_string(container_home.path().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            out["skipDangerousModePermissionPrompt"],
            serde_json::json!(true),
            "enabled setting should acknowledge the container-only warning",
        );
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
            staged_home: container_home.path().to_path_buf(),
        };
        sync_settings_json(
            &host,
            &SyncOptions {
                bedrock: Some(&sample_bedrock()),
                broker_url_from_container: "http://unused",
                mcp_servers: &[],
                task_runner_enabled: false,
                host_fs_enabled: false,
                skip_bypass_permissions_warning: false,
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
        assert_eq!(
            env["CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS"].as_str(),
            Some("1")
        );
        assert_eq!(env["CLAUDE_CODE_USE_BEDROCK"].as_str(), Some("1"));
        assert_eq!(env["AWS_PROFILE"].as_str(), Some("bedrock"));
        assert_eq!(
            env["ANTHROPIC_MODEL"].as_str(),
            Some("anthropic.claude-sonnet-4-20250514-v1:0")
        );
        assert_eq!(env["AWS_REGION"].as_str(), Some("us-west-2"));
        assert_eq!(env["AWS_DEFAULT_REGION"].as_str(), Some("us-west-2"));
    }

    #[test]
    fn claude_md_is_staged_with_container_notice() {
        let tmp_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let workspace = tmp_home.path().join("work");
        let claude_root = tmp_home.path().join(".claude");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&claude_root).unwrap();
        fs::write(claude_root.join("CLAUDE.md"), "host instructions").unwrap();
        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root,
            workspace,
            staged_home: container_home.path().to_path_buf(),
        };

        sync_claude_md(&host).unwrap();

        let out = fs::read_to_string(container_home.path().join(".claude/CLAUDE.md")).unwrap();
        assert!(out.starts_with("host instructions\n\n"));
        assert!(out.contains(crate::container_notice::MARKER));
        assert!(out.contains("Network access from this container is restricted."));
        assert!(out.contains(crate::task_runner::CLI_GUIDANCE));
    }

    #[test]
    fn claude_md_notice_is_staged_even_without_host_file() {
        let tmp_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let workspace = tmp_home.path().join("work");
        let claude_root = tmp_home.path().join(".claude");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&claude_root).unwrap();
        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root,
            workspace,
            staged_home: container_home.path().to_path_buf(),
        };

        sync_claude_md(&host).unwrap();

        let out = fs::read_to_string(container_home.path().join(".claude/CLAUDE.md")).unwrap();
        assert!(out.starts_with(crate::container_notice::MARKER));
        assert!(out.contains("HostRead, HostList, HostWrite, and HostSearch"));
        assert!(out.contains(crate::task_runner::CLI_GUIDANCE));
    }

    #[test]
    fn claude_extensions_flatten_plugin_skills_and_commands() {
        let tmp_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let workspace = tmp_home.path().join("work");
        fs::create_dir_all(&workspace).unwrap();
        let claude_root = tmp_home.path().join(".claude");
        fs::create_dir_all(claude_root.join("skills/user-skill")).unwrap();
        fs::write(claude_root.join("skills/user-skill/SKILL.md"), "user skill").unwrap();
        fs::create_dir_all(claude_root.join("commands")).unwrap();
        fs::write(claude_root.join("commands/user.md"), "user command").unwrap();
        fs::create_dir_all(claude_root.join("agents")).unwrap();
        fs::write(claude_root.join("agents/helper.md"), "agent").unwrap();

        let plugin_root = claude_root.join("plugins/cache/vendor/plugin-a");
        fs::create_dir_all(plugin_root.join("skills/plugin-skill")).unwrap();
        fs::write(
            plugin_root.join("skills/plugin-skill/SKILL.md"),
            "plugin skill",
        )
        .unwrap();
        fs::create_dir_all(plugin_root.join("commands")).unwrap();
        fs::write(plugin_root.join("commands/plugin.md"), "plugin command").unwrap();
        fs::create_dir_all(plugin_root.join(".git")).unwrap();
        fs::write(plugin_root.join(".git/config"), "[remote]\n").unwrap();

        let stale_plugins = container_home.path().join(".claude/plugins/cache/stale");
        fs::create_dir_all(&stale_plugins).unwrap();
        fs::write(stale_plugins.join("manifest.json"), "{}").unwrap();

        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root,
            workspace,
            staged_home: container_home.path().to_path_buf(),
        };
        sync_claude_extensions(&host).unwrap();

        let out_root = container_home.path().join(".claude");
        assert_eq!(
            fs::read_to_string(out_root.join("skills/user-skill/SKILL.md")).unwrap(),
            "user skill"
        );
        assert_eq!(
            fs::read_to_string(out_root.join("commands/user.md")).unwrap(),
            "user command"
        );
        assert_eq!(
            fs::read_to_string(out_root.join("agents/helper.md")).unwrap(),
            "agent"
        );
        assert_eq!(
            fs::read_to_string(out_root.join("skills/plugin-skill/SKILL.md")).unwrap(),
            "plugin skill"
        );
        assert_eq!(
            fs::read_to_string(out_root.join("commands/plugin.md")).unwrap(),
            "plugin command"
        );
        assert!(
            !out_root.join("plugins").exists(),
            "plugin marketplace/cache tree should not be staged"
        );
    }

    #[test]
    fn user_extensions_win_when_plugin_flattening_collides() {
        let tmp_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let workspace = tmp_home.path().join("work");
        fs::create_dir_all(&workspace).unwrap();
        let claude_root = tmp_home.path().join(".claude");
        fs::create_dir_all(claude_root.join("skills/shared")).unwrap();
        fs::write(claude_root.join("skills/shared/SKILL.md"), "user").unwrap();

        let plugin_root = claude_root.join("plugins/cache/vendor/plugin-a");
        fs::create_dir_all(plugin_root.join("skills/shared")).unwrap();
        fs::write(plugin_root.join("skills/shared/SKILL.md"), "plugin").unwrap();

        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root,
            workspace,
            staged_home: container_home.path().to_path_buf(),
        };
        sync_claude_extensions(&host).unwrap();

        assert_eq!(
            fs::read_to_string(container_home.path().join(".claude/skills/shared/SKILL.md"))
                .unwrap(),
            "user"
        );
        assert!(
            !container_home.path().join(".claude/plugins").exists(),
            "plugin tree is intentionally not staged"
        );
    }

    #[test]
    fn marketplace_plugin_skills_are_flattened_without_marketplace_state() {
        let tmp_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let workspace = tmp_home.path().join("work");
        fs::create_dir_all(&workspace).unwrap();
        let claude_root = tmp_home.path().join(".claude");
        let plugins = claude_root.join("plugins");
        fs::create_dir_all(plugins.join("marketplaces/example/plugins/demo/skills/demo")).unwrap();
        fs::write(
            plugins.join("marketplaces/example/plugins/demo/skills/demo/SKILL.md"),
            "demo skill",
        )
        .unwrap();

        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root,
            workspace,
            staged_home: container_home.path().to_path_buf(),
        };
        sync_claude_extensions(&host).unwrap();

        assert_eq!(
            fs::read_to_string(container_home.path().join(".claude/skills/demo/SKILL.md")).unwrap(),
            "demo skill"
        );
        assert!(
            !container_home.path().join(".claude/plugins").exists(),
            "marketplace state must not be copied into the container"
        );
    }

    #[test]
    #[cfg(unix)]
    fn claude_extension_symlinks_are_copied_as_real_files() {
        let tmp_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let workspace = tmp_home.path().join("work");
        fs::create_dir_all(&workspace).unwrap();
        let claude_root = tmp_home.path().join(".claude");
        fs::create_dir_all(claude_root.join("skills")).unwrap();

        let external = tmp_home.path().join("external-skills");
        fs::create_dir_all(external.join("linked-skill")).unwrap();
        fs::write(external.join("linked-skill/SKILL.md"), "linked skill").unwrap();
        fs::write(external.join("linked-command.md"), "linked command").unwrap();

        std::os::unix::fs::symlink(
            external.join("linked-skill"),
            claude_root.join("skills/linked-skill"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            external.join("linked-command.md"),
            claude_root.join("skills/linked-command.md"),
        )
        .unwrap();

        let host = HostPaths {
            home: tmp_home.path().to_path_buf(),
            claude_root,
            workspace,
            staged_home: container_home.path().to_path_buf(),
        };
        sync_claude_extensions(&host).unwrap();

        let out_root = container_home.path().join(".claude/skills");
        assert_eq!(
            fs::read_to_string(out_root.join("linked-skill/SKILL.md")).unwrap(),
            "linked skill"
        );
        assert_eq!(
            fs::read_to_string(out_root.join("linked-command.md")).unwrap(),
            "linked command"
        );
        assert!(
            !fs::symlink_metadata(out_root.join("linked-skill"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            !fs::symlink_metadata(out_root.join("linked-command.md"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
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
    fn gitconfig_emptied_when_values_missing() {
        let container_home = tempfile::tempdir().unwrap();
        let dest = container_home.path().join(".gitconfig");
        fs::write(&dest, "[user]\n\tname = stale\n").unwrap();

        write_container_gitconfig(container_home.path(), None, Some("only@example.com")).unwrap();
        assert_eq!(
            fs::read_to_string(&dest).unwrap(),
            "",
            "stale gitconfig should be emptied"
        );
    }
}
