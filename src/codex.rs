//! Helpers for the Codex pathway: mount the host's `~/.codex/auth.json`
//! directly and mount the host-side Codex history paths needed for
//! resume/history. The auth file deliberately does not go through
//! `shared_cred`: Codex first-party auth already lives in a portable
//! host file, and direct bind-mounting keeps host-side `codex login`
//! changes visible even while older agent-container sessions are still
//! running. The rest of `~/.codex` (trust_level lists, plugins, caches,
//! …) stays outside the container. We also pin a minimal `config.toml`
//! inside the container so Codex does not try to nest its own bubblewrap
//! sandbox (which fails inside docker because user namespaces cannot be
//! recreated).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub struct CodexAuthFile {
    pub path: PathBuf,
}

pub struct CodexHistoryMounts {
    pub sessions_dir: PathBuf,
    pub archived_sessions_dir: PathBuf,
    pub shell_snapshots_dir: PathBuf,
    pub session_index_path: PathBuf,
    pub history_path: PathBuf,
}

/// Return the host `~/.codex/auth.json` path for direct bind-mounting.
/// Codex stores its first-party auth in this file rather than macOS
/// Keychain, so direct mounting keeps host and container on the same
/// refresh-token lineage.
pub fn prepare_auth(host_home: &Path) -> Result<CodexAuthFile> {
    let src = host_home.join(".codex/auth.json");
    let metadata = fs::metadata(&src).with_context(|| {
        format!(
            "failed to read Codex auth at {}; run `codex login` on the host first",
            src.display()
        )
    })?;
    if !metadata.is_file() {
        anyhow::bail!(
            "Codex auth path is not a file at {}; run `codex login` on the host first",
            src.display()
        );
    }
    let path = std::fs::canonicalize(&src)
        .with_context(|| format!("failed to resolve Codex auth at {}", src.display()))?;
    Ok(CodexAuthFile { path })
}

/// Prepare the host Codex history paths mounted into the container.
pub fn prepare_history_mounts(host_home: &Path) -> Result<CodexHistoryMounts> {
    let codex_root = host_home.join(".codex");
    let sessions_dir = ensure_dir(&codex_root.join("sessions"))?;
    let archived_sessions_dir = ensure_dir(&codex_root.join("archived_sessions"))?;
    let shell_snapshots_dir = ensure_dir(&codex_root.join("shell_snapshots"))?;
    let session_index_path = ensure_file(&codex_root.join("session_index.jsonl"))?;
    let history_path = ensure_file(&codex_root.join("history.jsonl"))?;

    Ok(CodexHistoryMounts {
        sessions_dir,
        archived_sessions_dir,
        shell_snapshots_dir,
        session_index_path,
        history_path,
    })
}

fn ensure_dir(path: &Path) -> Result<PathBuf> {
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    std::fs::canonicalize(path).with_context(|| format!("failed to resolve {}", path.display()))
}

fn ensure_file(path: &Path) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if !path.exists() {
        fs::write(path, "").with_context(|| format!("failed to create {}", path.display()))?;
    }
    std::fs::canonicalize(path).with_context(|| format!("failed to resolve {}", path.display()))
}

/// Top-level scalar keys we inherit from the host's `~/.codex/config.toml`
/// so the containerised Codex runs with the same model / effort / persona
/// the user picked on the host.
const INHERITED_SCALAR_KEYS: &[&str] = &[
    "model",
    "model_provider",
    "model_reasoning_effort",
    "model_reasoning_summary",
    "personality",
];

/// Write `~/.codex/config.toml` into the staged config tree that will be
/// mounted into the container.
///
/// The file is composed from two sources:
/// - Carry over the user's model / reasoning-effort / personality choices
///   from the host's `~/.codex/config.toml` so the container follows the
///   same behaviour. Host-absolute `[projects.*]` trust entries and any
///   sandbox-related toggles are dropped.
/// - Rewrite host-declared `[mcp_servers.*]` entries to the in-container
///   broker URL so Codex gets the same proxying and policy controls as
///   Claude Code.
/// - Pin `approval_policy = "never"` and `sandbox_mode = "danger-full-access"`
///   because the docker container itself is the sandbox; Codex's bubblewrap
///   cannot recreate user namespaces inside docker and would otherwise
///   fail every nested shell exec.
pub fn write_container_config(
    host_home: &Path,
    container_home: &Path,
    broker_url_from_container: &str,
    mcp_servers: &[crate::mcp::McpServer],
    task_runner_enabled: bool,
    host_fs_enabled: bool,
) -> Result<()> {
    let mut table = toml::value::Table::new();

    let host_config = host_home.join(".codex/config.toml");
    if host_config.is_file() {
        let raw = fs::read_to_string(&host_config)
            .with_context(|| format!("failed to read {}", host_config.display()))?;
        let parsed: toml::Value = toml::from_str(&raw)
            .with_context(|| format!("failed to parse {} as TOML", host_config.display()))?;
        if let Some(host_table) = parsed.as_table() {
            for key in INHERITED_SCALAR_KEYS {
                if let Some(v) = host_table.get(*key).cloned() {
                    table.insert((*key).to_string(), v);
                }
            }
        }
    }

    // Always pin the sandbox/approval defaults — they are the whole reason
    // this file exists inside the container.
    table.insert(
        "approval_policy".to_string(),
        toml::Value::String("never".into()),
    );
    table.insert(
        "sandbox_mode".to_string(),
        toml::Value::String("danger-full-access".into()),
    );
    inject_builtin_mcp_servers(
        &mut table,
        broker_url_from_container,
        mcp_servers,
        task_runner_enabled,
        host_fs_enabled,
    );

    let dir = container_home.join(".codex");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join("config.toml");
    let header = "# Written by agent-container. The container itself is the sandbox,\n\
                  # so Codex's internal sandbox is disabled here; the other values\n\
                  # are inherited from the host's ~/.codex/config.toml.\n";
    let body = toml::to_string_pretty(&toml::Value::Table(table))
        .context("serialising codex config.toml")?;
    fs::write(&path, format!("{header}{body}"))
        .with_context(|| format!("failed to write {}", path.display()))?;
    write_container_agents_md(host_home, container_home)?;
    Ok(())
}

fn write_container_agents_md(host_home: &Path, container_home: &Path) -> Result<()> {
    let src = host_home.join(".codex/AGENTS.md");
    let dir = container_home.join(".codex");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let body = if src.is_file() {
        fs::read_to_string(&src).with_context(|| format!("failed to read {}", src.display()))?
    } else {
        String::new()
    };
    let dest = dir.join("AGENTS.md");
    fs::write(&dest, crate::container_notice::append_to(&body))
        .with_context(|| format!("failed to write {}", dest.display()))?;
    Ok(())
}

fn inject_builtin_mcp_servers(
    table: &mut toml::value::Table,
    broker_url: &str,
    mcp_servers: &[crate::mcp::McpServer],
    task_runner_enabled: bool,
    host_fs_enabled: bool,
) {
    if mcp_servers.is_empty() && !task_runner_enabled && !host_fs_enabled {
        return;
    }
    let mut servers = toml::value::Table::new();
    for server in mcp_servers {
        servers.insert(
            server.name().to_string(),
            codex_http_mcp_entry(broker_url, server.name()),
        );
    }
    if task_runner_enabled && !servers.contains_key(crate::task_runner::NAME) {
        servers.insert(
            crate::task_runner::NAME.to_string(),
            codex_http_mcp_entry(broker_url, crate::task_runner::NAME),
        );
    }
    if host_fs_enabled && !servers.contains_key(crate::host_fs::NAME) {
        servers.insert(
            crate::host_fs::NAME.to_string(),
            codex_http_mcp_entry(broker_url, crate::host_fs::NAME),
        );
    }
    if !servers.is_empty() {
        table.insert("mcp_servers".to_string(), toml::Value::Table(servers));
    }
}

fn codex_http_mcp_entry(broker_url: &str, name: &str) -> toml::Value {
    let mut entry = toml::value::Table::new();
    entry.insert(
        "url".to_string(),
        toml::Value::String(format!("{}/mcp/{}", broker_url.trim_end_matches('/'), name)),
    );
    toml::Value::Table(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherits_model_and_effort_and_pins_sandbox() {
        let host_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        fs::create_dir_all(host_home.path().join(".codex")).unwrap();
        fs::write(
            host_home.path().join(".codex/config.toml"),
            r#"
model = "gpt-5.4"
model_reasoning_effort = "xhigh"
personality = "pragmatic"
approval_policy = "on-request"
sandbox_mode = "workspace-write"

[projects."/home/user/projects/sample"]
trust_level = "trusted"
"#,
        )
        .unwrap();

        write_container_config(
            host_home.path(),
            container_home.path(),
            "http://broker",
            &[],
            false,
            false,
        )
        .unwrap();
        let out = fs::read_to_string(container_home.path().join(".codex/config.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        let t = parsed.as_table().unwrap();
        assert_eq!(t["model"].as_str(), Some("gpt-5.4"));
        assert_eq!(t["model_reasoning_effort"].as_str(), Some("xhigh"));
        assert_eq!(t["personality"].as_str(), Some("pragmatic"));
        assert_eq!(t["approval_policy"].as_str(), Some("never"));
        assert_eq!(t["sandbox_mode"].as_str(), Some("danger-full-access"));
        assert!(t.get("projects").is_none(), "projects must be dropped");
        let agents = fs::read_to_string(container_home.path().join(".codex/AGENTS.md")).unwrap();
        assert!(agents.contains(crate::container_notice::MARKER));
    }

    #[test]
    fn works_without_host_config() {
        let host_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        write_container_config(
            host_home.path(),
            container_home.path(),
            "http://broker",
            &[],
            false,
            false,
        )
        .unwrap();
        let out = fs::read_to_string(container_home.path().join(".codex/config.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        let t = parsed.as_table().unwrap();
        assert_eq!(t["approval_policy"].as_str(), Some("never"));
        assert_eq!(t["sandbox_mode"].as_str(), Some("danger-full-access"));
        assert!(t.get("model").is_none());
    }

    #[test]
    fn injects_builtin_mcp_servers_for_codex() {
        let host_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        write_container_config(
            host_home.path(),
            container_home.path(),
            "http://host.docker.internal:7000/",
            &[],
            true,
            true,
        )
        .unwrap();
        let out = fs::read_to_string(container_home.path().join(".codex/config.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        let servers = parsed["mcp_servers"].as_table().unwrap();
        assert_eq!(
            servers[crate::task_runner::NAME]["url"].as_str(),
            Some("http://host.docker.internal:7000/mcp/task-runner")
        );
        assert_eq!(
            servers[crate::host_fs::NAME]["url"].as_str(),
            Some("http://host.docker.internal:7000/mcp/host-fs")
        );
    }

    #[test]
    fn writes_codex_agents_md_with_host_text_and_container_notice() {
        let host_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        fs::create_dir_all(host_home.path().join(".codex")).unwrap();
        fs::write(
            host_home.path().join(".codex/AGENTS.md"),
            "host codex instructions",
        )
        .unwrap();

        write_container_agents_md(host_home.path(), container_home.path()).unwrap();

        let out = fs::read_to_string(container_home.path().join(".codex/AGENTS.md")).unwrap();
        assert!(out.starts_with("host codex instructions\n\n"));
        assert!(out.contains(crate::container_notice::MARKER));
        assert!(out.contains("task_runner MCP server"));
        assert!(out.contains(crate::task_runner::CLI_GUIDANCE));
    }

    #[test]
    fn rewrites_host_codex_mcp_servers_to_broker_routes() {
        let host_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        fs::create_dir_all(host_home.path().join(".codex")).unwrap();
        fs::write(
            host_home.path().join(".codex/config.toml"),
            r#"
[mcp_servers.local]
url = "http://127.0.0.1:3333/mcp"

[mcp_servers.host-fs]
url = "http://example.invalid/custom-host-fs"
"#,
        )
        .unwrap();
        let mcp_servers =
            crate::mcp::load_codex_servers(&host_home.path().join(".codex/config.toml")).unwrap();
        write_container_config(
            host_home.path(),
            container_home.path(),
            "http://host.docker.internal:7000",
            &mcp_servers,
            true,
            true,
        )
        .unwrap();
        let out = fs::read_to_string(container_home.path().join(".codex/config.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        let servers = parsed["mcp_servers"].as_table().unwrap();
        assert!(servers.contains_key(crate::task_runner::NAME));
        assert_eq!(
            servers["local"]["url"].as_str(),
            Some("http://host.docker.internal:7000/mcp/local")
        );
        assert_eq!(
            servers[crate::host_fs::NAME]["url"].as_str(),
            Some("http://host.docker.internal:7000/mcp/host-fs")
        );
    }

    #[test]
    fn prepares_history_mount_paths_without_mounting_whole_codex_home() {
        let host_home = tempfile::tempdir().unwrap();
        let mounts = prepare_history_mounts(host_home.path()).unwrap();
        let codex_root = std::fs::canonicalize(host_home.path().join(".codex")).unwrap();

        assert_eq!(mounts.sessions_dir, codex_root.join("sessions"));
        assert_eq!(
            mounts.archived_sessions_dir,
            codex_root.join("archived_sessions")
        );
        assert_eq!(
            mounts.shell_snapshots_dir,
            codex_root.join("shell_snapshots")
        );
        assert_eq!(
            mounts.session_index_path,
            codex_root.join("session_index.jsonl")
        );
        assert_eq!(mounts.history_path, codex_root.join("history.jsonl"));
        assert!(mounts.sessions_dir.is_dir());
        assert!(mounts.archived_sessions_dir.is_dir());
        assert!(mounts.shell_snapshots_dir.is_dir());
        assert!(mounts.session_index_path.is_file());
        assert!(mounts.history_path.is_file());
    }

    #[test]
    fn prepares_auth_by_mounting_host_auth_directly_without_shared_copy() {
        let host_home = tempfile::tempdir().unwrap();
        let codex_root = host_home.path().join(".codex");
        fs::create_dir_all(&codex_root).unwrap();
        let auth_path = codex_root.join("auth.json");
        fs::write(&auth_path, "{}").unwrap();

        let auth = prepare_auth(host_home.path()).unwrap();

        assert_eq!(auth.path, std::fs::canonicalize(auth_path).unwrap());
    }
}
