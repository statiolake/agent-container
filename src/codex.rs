//! Helpers for the Codex pathway: ship the host's ChatGPT-subscription
//! auth token into the container through a short-lived 0600 temp file and
//! prepare a workspace-scoped Codex history view for resume/history. The
//! rest of `~/.codex` (trust_level lists, unrelated sessions, plugins,
//! caches, …) stays outside the container. We also pin a minimal
//! `config.toml` inside the container so Codex does not try to nest its
//! own bubblewrap sandbox (which fails inside docker because user
//! namespaces cannot be recreated).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::paths::encode_project_dir;
use crate::shared_cred::{HostSync, SharedCredFile, shared_dir};

pub struct CodexAuthFile {
    pub path: PathBuf,
    /// Owns the shared lock; see [`crate::shared_cred`]. The last
    /// agent-container to drop this writes the (possibly refreshed)
    /// auth.json back to `~/.codex/auth.json` on the host.
    _shared: SharedCredFile,
}

pub struct CodexHistoryMounts {
    pub sessions_dir: PathBuf,
    pub archived_sessions_dir: PathBuf,
    pub shell_snapshots_dir: PathBuf,
    pub session_index_path: PathBuf,
    pub history_path: PathBuf,
}

/// Open `~/.codex/auth.json` through the shared-credential machinery.
///
/// All concurrent agent-container processes on this host see the same
/// `auth.json`, so a token refresh in one container is observable by
/// the others via the bind-mounted shared file. The host copy is
/// updated only when the last container exits.
pub fn prepare_auth(host_home: &Path) -> Result<CodexAuthFile> {
    let src = host_home.join(".codex/auth.json");
    let shared_path = shared_dir()?.join("codex-auth.json");
    let host_sync = HostSync::File(src.clone());
    let (shared, _raw) = SharedCredFile::open(shared_path, host_sync, move || {
        fs::read_to_string(&src).with_context(|| {
            format!(
                "failed to read Codex auth at {}; run `codex login` on the host first",
                src.display()
            )
        })
    })?;
    Ok(CodexAuthFile {
        path: shared.path.clone(),
        _shared: shared,
    })
}

/// Prepare a Codex history tree for this workspace only.
///
/// Codex stores sessions globally under `~/.codex/sessions` and identifies
/// their workspace inside each JSONL file. To avoid exposing every host
/// session to the container, import only files whose recorded cwd matches
/// the current workspace into the persistent container home, then mount
/// that workspace-specific tree at `~/.codex` history paths.
pub fn prepare_history_mounts(
    host_home: &Path,
    container_home: &Path,
    workspace: &Path,
) -> Result<CodexHistoryMounts> {
    let host_codex_root = host_home.join(".codex");
    let history_root = container_home
        .join(".codex")
        .join("workspace-history")
        .join(encode_project_dir(workspace));

    let sessions_dir = ensure_dir(&history_root.join("sessions"))?;
    let archived_sessions_dir = ensure_dir(&history_root.join("archived_sessions"))?;
    let shell_snapshots_dir = ensure_dir(&history_root.join("shell_snapshots"))?;
    let session_index_path = ensure_file(&history_root.join("session_index.jsonl"))?;
    let history_path = ensure_file(&history_root.join("history.jsonl"))?;

    let workspace_keys = workspace_keys(workspace)?;
    let imported = import_matching_sessions(
        &host_codex_root.join("sessions"),
        &sessions_dir,
        &workspace_keys,
    )?;
    let imported_archived = import_matching_sessions(
        &host_codex_root.join("archived_sessions"),
        &archived_sessions_dir,
        &workspace_keys,
    )?;
    let mut session_ids: HashSet<String> = imported.keys().cloned().collect();
    session_ids.extend(imported_archived.keys().cloned());

    copy_matching_shell_snapshots(
        &host_codex_root.join("shell_snapshots"),
        &shell_snapshots_dir,
        &session_ids,
    )?;
    write_filtered_session_index(
        &host_codex_root.join("session_index.jsonl"),
        &session_index_path,
        &session_ids,
    )?;

    Ok(CodexHistoryMounts {
        sessions_dir,
        archived_sessions_dir,
        shell_snapshots_dir,
        session_index_path,
        history_path,
    })
}

fn workspace_keys(workspace: &Path) -> Result<HashSet<String>> {
    let mut keys = HashSet::new();
    keys.insert(workspace.display().to_string());
    if let Ok(canonical) = std::fs::canonicalize(workspace) {
        keys.insert(canonical.display().to_string());
    }
    Ok(keys)
}

fn import_matching_sessions(
    src_root: &Path,
    dest_root: &Path,
    workspace_keys: &HashSet<String>,
) -> Result<HashMap<String, PathBuf>> {
    let mut imported = HashMap::new();
    if !src_root.is_dir() {
        return Ok(imported);
    }
    for src in jsonl_files_under(src_root)? {
        let Some(meta) = read_session_meta(&src)? else {
            continue;
        };
        if !workspace_keys.contains(&meta.cwd) {
            continue;
        }
        let relative = src
            .strip_prefix(src_root)
            .with_context(|| format!("{} is not under {}", src.display(), src_root.display()))?;
        let dest = dest_root.join(relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::copy(&src, &dest)
            .with_context(|| format!("failed to import Codex session {}", src.display()))?;
        imported.insert(meta.id, dest);
    }
    Ok(imported)
}

fn jsonl_files_under(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_jsonl_files(root, &mut files)?;
    Ok(files)
}

fn collect_jsonl_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to list {}", path.display()))? {
        let entry = entry?;
        let path = entry.path();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            collect_jsonl_files(&path, files)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
    Ok(())
}

#[derive(Debug)]
struct SessionMeta {
    id: String,
    cwd: String,
}

fn read_session_meta(path: &Path) -> Result<Option<SessionMeta>> {
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(64) {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let Some(id) = payload.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(cwd) = payload.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        return Ok(Some(SessionMeta {
            id: id.to_string(),
            cwd: cwd.to_string(),
        }));
    }
    Ok(None)
}

fn copy_matching_shell_snapshots(
    src_root: &Path,
    dest_root: &Path,
    session_ids: &HashSet<String>,
) -> Result<()> {
    if !src_root.is_dir() {
        return Ok(());
    }
    for entry in
        fs::read_dir(src_root).with_context(|| format!("failed to list {}", src_root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !entry.metadata()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !session_ids.iter().any(|id| name.starts_with(id)) {
            continue;
        }
        fs::copy(&path, dest_root.join(name.as_ref()))
            .with_context(|| format!("failed to import Codex shell snapshot {}", path.display()))?;
    }
    Ok(())
}

fn write_filtered_session_index(
    host_index_path: &Path,
    dest_index_path: &Path,
    session_ids: &HashSet<String>,
) -> Result<()> {
    let mut lines_by_id = HashMap::new();
    collect_index_lines(dest_index_path, None, &mut lines_by_id)?;
    collect_index_lines(host_index_path, Some(session_ids), &mut lines_by_id)?;

    let mut lines: Vec<String> = lines_by_id.into_values().collect();
    lines.sort_by(|a, b| {
        let a_ts = index_updated_at(a).unwrap_or_default();
        let b_ts = index_updated_at(b).unwrap_or_default();
        a_ts.cmp(&b_ts)
    });
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    fs::write(dest_index_path, out)
        .with_context(|| format!("failed to write {}", dest_index_path.display()))
}

fn collect_index_lines(
    path: &Path,
    session_ids: Option<&HashSet<String>>,
    out: &mut HashMap<String, String>,
) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    for line in BufReader::new(file).lines() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        if session_ids.map(|ids| ids.contains(id)).unwrap_or(true) {
            out.insert(id.to_string(), line);
        }
    }
    Ok(())
}

fn index_updated_at(line: &str) -> Option<String> {
    serde_json::from_str::<Value>(line)
        .ok()?
        .get("updated_at")?
        .as_str()
        .map(str::to_string)
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

const MCP_SERVERS_KEY: &str = "mcp_servers";

/// Write `~/.codex/config.toml` into the container's persistent home.
///
/// The file is composed from two sources:
/// - Carry over the user's model / reasoning-effort / personality choices
///   from the host's `~/.codex/config.toml` so the container follows the
///   same behaviour. Host-absolute `[projects.*]` trust entries and any
///   sandbox-related toggles are dropped.
/// - Pin `approval_policy = "never"` and `sandbox_mode = "danger-full-access"`
///   because the docker container itself is the sandbox; Codex's bubblewrap
///   cannot recreate user namespaces inside docker and would otherwise
///   fail every nested shell exec.
pub fn write_container_config(
    host_home: &Path,
    container_home: &Path,
    broker_url_from_container: &str,
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
            if let Some(v) = host_table.get(MCP_SERVERS_KEY).cloned() {
                table.insert(MCP_SERVERS_KEY.to_string(), v);
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
    Ok(())
}

fn inject_builtin_mcp_servers(
    table: &mut toml::value::Table,
    broker_url: &str,
    task_runner_enabled: bool,
    host_fs_enabled: bool,
) {
    if !task_runner_enabled && !host_fs_enabled {
        return;
    }
    let mut servers = table
        .remove(MCP_SERVERS_KEY)
        .and_then(|value| match value {
            toml::Value::Table(table) => Some(table),
            _ => None,
        })
        .unwrap_or_default();
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
        toml::Value::String(format!(
            "{}/mcp/{}",
            broker_url.trim_end_matches('/'),
            name
        )),
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
    }

    #[test]
    fn works_without_host_config() {
        let host_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        write_container_config(
            host_home.path(),
            container_home.path(),
            "http://broker",
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
    fn preserves_host_codex_mcp_servers_and_does_not_override_builtin_names() {
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
        write_container_config(
            host_home.path(),
            container_home.path(),
            "http://host.docker.internal:7000",
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
            Some("http://127.0.0.1:3333/mcp")
        );
        assert_eq!(
            servers[crate::host_fs::NAME]["url"].as_str(),
            Some("http://example.invalid/custom-host-fs")
        );
    }

    #[test]
    fn prepares_workspace_scoped_history_mount_paths() {
        let host_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let workspace = host_home.path().join("repo");
        fs::create_dir_all(&workspace).unwrap();
        let mounts =
            prepare_history_mounts(host_home.path(), container_home.path(), &workspace).unwrap();
        let history_root = std::fs::canonicalize(
            container_home
                .path()
                .join(".codex/workspace-history")
                .join(encode_project_dir(&workspace)),
        )
        .unwrap();

        assert_eq!(mounts.sessions_dir, history_root.join("sessions"));
        assert_eq!(
            mounts.archived_sessions_dir,
            history_root.join("archived_sessions")
        );
        assert_eq!(
            mounts.shell_snapshots_dir,
            history_root.join("shell_snapshots")
        );
        assert_eq!(
            mounts.session_index_path,
            history_root.join("session_index.jsonl")
        );
        assert_eq!(mounts.history_path, history_root.join("history.jsonl"));
        assert!(mounts.sessions_dir.is_dir());
        assert!(mounts.session_index_path.is_file());
        assert!(mounts.history_path.is_file());
    }

    #[test]
    fn imports_only_sessions_for_current_workspace() {
        let host_home = tempfile::tempdir().unwrap();
        let container_home = tempfile::tempdir().unwrap();
        let workspace = host_home.path().join("repo");
        let other_workspace = host_home.path().join("other");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&other_workspace).unwrap();
        let host_codex = host_home.path().join(".codex");
        let host_sessions = host_codex.join("sessions/2026/06/01");
        fs::create_dir_all(&host_sessions).unwrap();
        write_session(
            &host_sessions.join("rollout-good.jsonl"),
            "good",
            &workspace.display().to_string(),
        );
        write_session(
            &host_sessions.join("rollout-other.jsonl"),
            "other",
            &other_workspace.display().to_string(),
        );
        fs::write(
            host_codex.join("session_index.jsonl"),
            "{\"id\":\"good\",\"thread_name\":\"good\",\"updated_at\":\"2026-06-01T00:00:00Z\"}\n\
             {\"id\":\"other\",\"thread_name\":\"other\",\"updated_at\":\"2026-06-01T00:00:01Z\"}\n",
        )
        .unwrap();

        let mounts =
            prepare_history_mounts(host_home.path(), container_home.path(), &workspace).unwrap();

        assert!(
            mounts
                .sessions_dir
                .join("2026/06/01/rollout-good.jsonl")
                .is_file()
        );
        assert!(
            !mounts
                .sessions_dir
                .join("2026/06/01/rollout-other.jsonl")
                .exists()
        );
        let index = fs::read_to_string(mounts.session_index_path).unwrap();
        assert!(index.contains("\"id\":\"good\""));
        assert!(!index.contains("\"id\":\"other\""));
    }

    fn write_session(path: &Path, id: &str, cwd: &str) {
        fs::write(
            path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}}}\n"
            ),
        )
        .unwrap();
    }
}
