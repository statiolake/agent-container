use std::collections::{BTreeSet, HashMap};
use std::future;
use std::io::{ErrorKind, IsTerminal};
#[cfg(unix)]
use std::os::unix::process::CommandExt as StdCommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
#[cfg(unix)]
use tokio::signal::unix::Signal;

use crate::aws::BedrockSetup;
use crate::paths::HostPaths;

const AGENT_IMAGE_TAG: &str = "agent-container:dev";
const PROXY_IMAGE_TAG: &str = "agent-container-proxy:dev";
const DOCKER_ATTACH_DETACH_KEYS: &str = "ctrl-],ctrl-]";
const OWNER_LABEL: &str = "dev.statiolake.agent-container";
const PROJECT_LABEL: &str = "dev.statiolake.agent-container.project";
const OWNER_PID_LABEL: &str = "dev.statiolake.agent-container.owner-pid";

/// Build required images. The agent image can be force-built on demand;
/// the proxy image is still only built when missing.
pub async fn ensure_images(dockerfile_dir: &Path, rebuild_agent: bool) -> Result<()> {
    ensure_one(AGENT_IMAGE_TAG, dockerfile_dir, "Dockerfile", rebuild_agent).await?;
    ensure_one(
        PROXY_IMAGE_TAG,
        &dockerfile_dir.join("proxy"),
        "Dockerfile",
        false,
    )
    .await?;
    Ok(())
}

async fn ensure_one(
    tag: &str,
    context_dir: &Path,
    dockerfile_name: &str,
    force_build: bool,
) -> Result<()> {
    if !force_build {
        let status = Command::new("docker")
            .args(["image", "inspect", tag])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .context("failed to invoke docker")?;
        if status.success() {
            return Ok(());
        }
    }
    let reason = if force_build {
        "requested rebuild"
    } else {
        "image missing"
    };
    eprintln!("[agent-container] building image {tag} ({reason})...");
    let dockerfile = context_dir.join(dockerfile_name);
    let status = Command::new("docker")
        .args([
            "build",
            "-t",
            tag,
            "-f",
            dockerfile.to_str().context("non-utf8 dockerfile path")?,
            context_dir
                .to_str()
                .context("non-utf8 build context path")?,
        ])
        .stdin(Stdio::null())
        .status()
        .await
        .context("failed to spawn docker build")?;
    if !status.success() {
        bail!("docker build for {tag} failed with status {status}");
    }
    Ok(())
}

pub struct RunOptions {
    pub host: HostPaths,
    pub credentials_path: PathBuf,
    pub codex_auth_path: PathBuf,
    pub codex_history: crate::codex::CodexHistoryMounts,
    pub cursor_state_path: PathBuf,
    pub cursor_auth_path: PathBuf,
    pub bedrock_setup: Option<BedrockSetup>,
    /// Pre-built `http://<host>:<port>` URL the container should use to
    /// reach the broker. The hostname encodes the engine-flavour choice
    /// (Docker Desktop, Rancher Desktop, native Linux) made up-front by
    /// `host_kind::HostKind`; everything downstream just reads it.
    pub broker_url_from_container: String,
    /// The command to invoke inside the container, e.g.
    /// `["claude", "--permission-mode", "bypassPermissions"]` or `["codex"]`.
    pub agent_command: Vec<String>,
    pub extra_args: Vec<String>,
    /// User-defined `proxy.allow` patterns, already merged across global
    /// and workspace settings. Appended to the bundled base allowlist and
    /// mounted into tinyproxy.
    pub proxy_allow: Vec<String>,
    /// Merged filesystem policy. Additional mount roots are bind-mounted,
    /// and hidden / readonly filters are overlaid for paths that already
    /// exist at container creation time.
    pub filesystem: crate::settings::FilesystemPolicy,
}

struct RunArtifacts {
    host_claude_projects_dir: PathBuf,
    workspace_agent_container_mount_src: PathBuf,
    allowlist_path: PathBuf,
    compose_files: Vec<PathBuf>,
    cleanup_paths: Vec<PathBuf>,
}

/// Orchestrate the compose project: start relay, run agent, always tear down.
pub async fn run(opts: RunOptions) -> Result<i32> {
    let pid = std::process::id();
    let project = format!("agent-container-{pid}");
    spawn_stale_stack_cleanup();
    let mut agent_argv = opts.agent_command.clone();
    agent_argv.extend(opts.extra_args.clone());
    let RunArtifacts {
        host_claude_projects_dir,
        workspace_agent_container_mount_src,
        allowlist_path,
        compose_files,
        cleanup_paths,
    } = prepare_run_artifacts(pid, &opts, &agent_argv)?;

    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();

    let mut env: HashMap<String, String> = [
        ("WORKSPACE_PATH", opts.host.workspace.display().to_string()),
        (
            "CONTAINER_WORKSPACE_PATH",
            opts.host.container_workspace().display().to_string(),
        ),
        (
            "HOST_CLAUDE_PROJECTS_DIR",
            host_claude_projects_dir.display().to_string(),
        ),
        (
            "WORKSPACE_AGENT_CONTAINER_MOUNT_SRC",
            workspace_agent_container_mount_src.display().to_string(),
        ),
        (
            "CREDENTIALS_PATH",
            opts.credentials_path.display().to_string(),
        ),
        (
            "CODEX_AUTH_PATH",
            opts.codex_auth_path.display().to_string(),
        ),
        (
            "CODEX_SESSIONS_DIR",
            opts.codex_history.sessions_dir.display().to_string(),
        ),
        (
            "CODEX_ARCHIVED_SESSIONS_DIR",
            opts.codex_history
                .archived_sessions_dir
                .display()
                .to_string(),
        ),
        (
            "CODEX_SHELL_SNAPSHOTS_DIR",
            opts.codex_history.shell_snapshots_dir.display().to_string(),
        ),
        (
            "CODEX_SESSION_INDEX_PATH",
            opts.codex_history.session_index_path.display().to_string(),
        ),
        (
            "CODEX_HISTORY_PATH",
            opts.codex_history.history_path.display().to_string(),
        ),
        (
            "HOST_CURSOR_DIR",
            opts.cursor_state_path.display().to_string(),
        ),
        (
            "CURSOR_AUTH_PATH",
            opts.cursor_auth_path.display().to_string(),
        ),
        ("ALLOWLIST_PATH", allowlist_path.display().to_string()),
        ("HOST_UID", uid.to_string()),
        ("HOST_GID", gid.to_string()),
        ("AGENT_CONTAINER_PROJECT", project.clone()),
        ("AGENT_CONTAINER_OWNER_PID", pid.to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();

    // Point the container at the host broker so the in-container refresh
    // script can fetch fresh AWS credentials on demand through the proxy.
    // The URL's hostname is picked per-engine (see `host_kind::HostKind`)
    // because `host.docker.internal` doesn't reach the host's loopback on
    // every flavour (Rancher Desktop in particular).
    env.insert(
        "AGENT_CONTAINER_HOST_ENDPOINT".to_string(),
        opts.broker_url_from_container.clone(),
    );

    // Forward the host terminal description so in-container TUIs choose
    // the correct colour palette.
    for key in ["TERM", "COLORTERM"] {
        if let Ok(v) = std::env::var(key) {
            env.insert(key.to_string(), v);
        }
    }

    // Bedrock env vars: declared as `${VAR:-}` in compose.yml, so an unset
    // shell var translates to an empty string in the container.
    //
    // AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_SESSION_TOKEN are
    // deliberately NOT forwarded: creds live only in Claude Code's memory
    // (via awsCredentialExport). AWS_PROFILE is set only from the selected
    // Bedrock setup, not inherited from the host shell.
    let mut put = |k: &str, v: String| {
        env.insert(k.to_string(), v);
    };
    if let Some(setup) = &opts.bedrock_setup {
        put("CLAUDE_CODE_USE_BEDROCK", "1".to_string());
        put("AWS_PROFILE", setup.profile.clone());
        if let Some(model) = &setup.model {
            put("ANTHROPIC_MODEL", model.clone());
        }
        if let Some(region) = &setup.region {
            put("AWS_REGION", region.clone());
            put("AWS_DEFAULT_REGION", region.clone());
        }
    }
    for key in [
        "CLAUDE_CODE_USE_BEDROCK",
        "ANTHROPIC_MODEL",
        "AWS_PROFILE",
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
    ] {
        env.entry(key.to_string()).or_default();
    }

    // Cursor Agent supports API-key and direct token envs. Unlike generic
    // AWS env, these are scoped to Cursor authentication and are only
    // forwarded when the host explicitly set them.
    for key in ["CURSOR_API_KEY", "CURSOR_AUTH_TOKEN"] {
        if let Ok(v) = std::env::var(key) {
            env.insert(key.to_string(), v);
        } else {
            env.entry(key.to_string()).or_default();
        }
    }

    let ctx = ComposeCtx {
        project: project.clone(),
        compose_files,
        env: env.clone(),
    };

    // Guarantees `compose down` and staging cleanup on any exit path
    // (panic/error/normal).
    struct Cleanup<'a> {
        compose: &'a ComposeCtx,
        paths: Vec<PathBuf>,
    }
    impl<'a> Drop for Cleanup<'a> {
        fn drop(&mut self) {
            if let Err(e) = compose_down_sync(self.compose) {
                eprintln!("[agent-container] warning: compose down failed: {e:#}");
            }
            for path in &self.paths {
                if let Err(e) = remove_path_any(path)
                    && e.kind() != ErrorKind::NotFound
                {
                    eprintln!(
                        "[agent-container] warning: failed to remove temporary path {}: {e}",
                        path.display()
                    );
                }
            }
        }
    }
    let _cleanup = Cleanup {
        compose: &ctx,
        paths: cleanup_paths,
    };

    // 1) Start the forward proxy sidecar in the background.
    let proxy_up = ctx
        .compose(&["up", "-d", "--no-color", "proxy"])
        .spawn()
        .context("failed to spawn docker compose up")?;
    let proxy_up =
        wait_compose_child_or_interrupt(proxy_up, ctx.clone(), "docker compose up").await?;
    let ChildExit::Exited(status) = proxy_up else {
        return Ok(130);
    };
    if !status.success() {
        bail!("`docker compose up -d proxy` failed with status {status}");
    }
    let proxy_reload = tokio::spawn(watch_proxy_settings(
        ctx.clone(),
        opts.host.workspace.clone(),
        allowlist_path.clone(),
        crate::proxy_allowlist::render(&opts.proxy_allow),
    ));

    // 2) Create the agent container without starting it, copy the generated
    // home files into its writable layer, then attach-start it.
    let create = ctx
        .compose(&["create", "--no-build", "agent"])
        .spawn()
        .context("failed to spawn docker compose create")?;
    let create =
        wait_compose_child_or_interrupt(create, ctx.clone(), "docker compose create").await?;
    let ChildExit::Exited(status) = create else {
        proxy_reload.abort();
        return Ok(130);
    };
    if !status.success() {
        bail!("`docker compose create agent` failed with status {status}");
    }

    let agent_container_id = compose_service_container_id(&ctx, "agent")
        .await
        .context("failed to resolve created agent container id")?;
    copy_staged_home_into_container(&opts.host.staged_home, &agent_container_id, uid, gid)
        .await
        .context("failed to copy staged home into agent container")?;

    let mut cmd = Command::new("docker");
    cmd.args(docker_start_attach_args(
        &agent_container_id,
        is_stdin_tty(),
    ));
    let child = cmd.spawn().context("failed to spawn docker start")?;
    let status = wait_compose_child_or_interrupt(child, ctx.clone(), "docker start").await;
    proxy_reload.abort();
    let exit = match status? {
        ChildExit::Exited(status) => status.code().unwrap_or(1),
        ChildExit::Interrupted => 130,
    };

    // `_cleanup` runs `compose down` on scope exit.
    Ok(exit)
}

fn prepare_run_artifacts(
    pid: u32,
    opts: &RunOptions,
    agent_argv: &[String],
) -> Result<RunArtifacts> {
    let staged_root = opts.host.staged_root();
    opts.host.prepare_staged_root()?;

    let host_claude_projects_dir = opts.host.host_claude_projects_dir();
    std::fs::create_dir_all(&host_claude_projects_dir).with_context(|| {
        format!(
            "failed to prepare Claude projects dir {}",
            host_claude_projects_dir.display()
        )
    })?;
    std::fs::create_dir_all(&opts.host.staged_home).with_context(|| {
        format!(
            "failed to prepare staged agent home at {}",
            opts.host.staged_home.display()
        )
    })?;

    let workspace_agent_container_dir = opts.host.workspace.join(".agent-container");
    let (workspace_agent_container_mount_src, empty_workspace_agent_container_mount) =
        if workspace_agent_container_dir.is_dir() {
            (workspace_agent_container_dir, None)
        } else {
            let path = empty_workspace_agent_container_dir(pid)?;
            (path.clone(), Some(path))
        };

    let secret_shadow_root_path = secret_shadow_root(pid);
    let secret_shadows = prepare_secret_shadow_mounts(
        &opts.host.workspace,
        opts.host.container_workspace(),
        pid,
        &opts.filesystem,
    )?;
    let shadow_compose_file = write_secret_shadow_compose_override(pid, &secret_shadows)?;

    let command_compose_file =
        write_agent_command_compose_override(pid, agent_argv, is_stdin_tty())?;
    let allowlist_path = crate::proxy_allowlist::cache_path_for(pid)?;
    crate::proxy_allowlist::generate(&opts.proxy_allow, &allowlist_path)
        .context("failed to materialise proxy allowlist for tinyproxy")?;

    let mut compose_files = vec![default_compose_file(), command_compose_file.clone()];
    if let Some(file) = shadow_compose_file.clone() {
        compose_files.push(file);
    }

    let mut cleanup_paths = vec![
        staged_root,
        allowlist_path.clone(),
        secret_shadow_root_path,
        command_compose_file,
    ];
    if let Some(path) = empty_workspace_agent_container_mount {
        cleanup_paths.push(path);
    }
    if let Some(path) = shadow_compose_file {
        cleanup_paths.push(path);
    }

    Ok(RunArtifacts {
        host_claude_projects_dir,
        workspace_agent_container_mount_src,
        allowlist_path,
        compose_files,
        cleanup_paths,
    })
}

enum ChildExit {
    Exited(std::process::ExitStatus),
    Interrupted,
}

async fn compose_service_container_id(ctx: &ComposeCtx, service: &str) -> Result<String> {
    let output = ctx
        .compose(&["ps", "-a", "-q", service])
        .output()
        .await
        .with_context(|| format!("failed to spawn docker compose ps for {service}"))?;
    if !output.status.success() {
        bail!(
            "`docker compose ps -a -q {service}` failed with {}",
            output.status
        );
    }
    let id = String::from_utf8(output.stdout).context("docker compose ps emitted non-utf8 id")?;
    let id = id.trim();
    if id.is_empty() {
        bail!("docker compose did not report a container id for service {service}");
    }
    Ok(id.to_string())
}

async fn copy_staged_home_into_container(
    staged_home: &Path,
    container_id: &str,
    uid: u32,
    gid: u32,
) -> Result<()> {
    let mut tar = Vec::new();
    crate::staging_archive::write_tar_as(staged_home, &mut tar, uid, gid)
        .context("failed to build staged home tar stream")?;

    let mut child = Command::new("docker")
        .args(["cp", "-", &format!("{container_id}:/home/agent")])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .context("failed to spawn docker cp")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .context("failed to open docker cp stdin")?;
        stdin
            .write_all(&tar)
            .await
            .context("failed to stream staged home tar")?;
        stdin
            .shutdown()
            .await
            .context("failed to close docker cp stdin")?;
    }
    drop(child.stdin.take());

    let status = child.wait().await.context("failed to wait for docker cp")?;
    if !status.success() {
        bail!("`docker cp - {container_id}:/home/agent` failed with {status}");
    }
    Ok(())
}

async fn wait_compose_child_or_interrupt(
    mut child: tokio::process::Child,
    ctx: ComposeCtx,
    label: &str,
) -> Result<ChildExit> {
    tokio::select! {
        status = child.wait() => {
            let status = status.with_context(|| format!("failed to wait for {label}"))?;
            Ok(ChildExit::Exited(status))
        }
        signal = shutdown_signal() => {
            eprintln!("[agent-container] {signal} received; cleaning up compose stack...");

            match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    eprintln!("[agent-container] warning: failed to wait for interrupted {label}: {e}");
                }
                Err(_) => {
                    match tokio::time::timeout(Duration::from_secs(2), child.kill()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            eprintln!("[agent-container] warning: failed to stop {label} after interrupt: {e}");
                        }
                        Err(_) => {
                            eprintln!("[agent-container] warning: timed out stopping {label} after interrupt");
                        }
                    }
                }
            }

            if let Err(e) = compose_down_sync(&ctx) {
                eprintln!("[agent-container] warning: compose down after interrupt failed: {e:#}");
            }
            Ok(ChildExit::Interrupted)
        }
    }
}

#[cfg(unix)]
async fn shutdown_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = signal(SignalKind::terminate()).ok();
    let mut hup = signal(SignalKind::hangup()).ok();
    tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            if let Err(e) = signal {
                eprintln!("[agent-container] warning: failed to install Ctrl+C handler: {e}");
            }
            "interrupt"
        }
        _ = recv_optional_signal(&mut term) => "SIGTERM",
        _ = recv_optional_signal(&mut hup) => "SIGHUP",
    }
}

#[cfg(unix)]
async fn recv_optional_signal(signal: &mut Option<Signal>) {
    if let Some(signal) = signal {
        let _ = signal.recv().await;
    } else {
        future::pending::<()>().await;
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> &'static str {
    if let Err(e) = tokio::signal::ctrl_c().await {
        eprintln!("[agent-container] warning: failed to install Ctrl+C handler: {e}");
    }
    "interrupt"
}

fn compose_down_sync(ctx: &ComposeCtx) -> Result<()> {
    let mut cmd = std::process::Command::new("docker");
    cmd.args(["compose", "-p", &ctx.project])
        .args(ctx.compose_file_args())
        .args(["down", "--remove-orphans", "--timeout", "5"])
        .envs(&ctx.env)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_from_terminal_interrupts(&mut cmd);
    let status = cmd
        .status()
        .context("failed to spawn docker compose down")?;
    if !status.success() {
        bail!("`docker compose down` failed with status {status}");
    }
    Ok(())
}

fn spawn_stale_stack_cleanup() {
    tokio::spawn(async {
        if let Err(e) = cleanup_stale_agent_container_stacks().await {
            eprintln!("[agent-container] warning: failed to cleanup stale compose stacks: {e:#}");
        }
    });
}

async fn cleanup_stale_agent_container_stacks() -> Result<()> {
    let ids = docker_list_ids("container", &[("label", OWNER_LABEL)])
        .await
        .context("failed to list agent-container containers")?;
    if ids.is_empty() {
        return Ok(());
    }

    let mut stale_projects = BTreeSet::new();
    for object in docker_inspect(&ids)
        .await
        .context("failed to inspect agent-container containers")?
    {
        let Some(labels) = object.config.and_then(|config| config.labels) else {
            continue;
        };
        let Some(project) = labels.get(PROJECT_LABEL).filter(|v| !v.is_empty()) else {
            continue;
        };
        let Some(pid) = labels
            .get(OWNER_PID_LABEL)
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if !owner_process_alive(pid) {
            stale_projects.insert(project.clone());
        }
    }

    for project in stale_projects {
        eprintln!("[agent-container] cleaning up stale compose stack {project}");
        remove_labeled_objects("container", "rm", &["-f"], &project)
            .await
            .with_context(|| format!("failed to remove stale containers for {project}"))?;
        remove_labeled_objects("network", "rm", &[], &project)
            .await
            .with_context(|| format!("failed to remove stale networks for {project}"))?;
    }
    Ok(())
}

async fn remove_labeled_objects(
    kind: &str,
    remove_subcommand: &str,
    remove_args: &[&str],
    project: &str,
) -> Result<()> {
    let project_filter = project_filter(project);
    let ids = docker_list_ids(kind, &[("label", OWNER_LABEL), ("label", &project_filter)]).await?;
    if ids.is_empty() {
        return Ok(());
    }
    let mut cmd = Command::new("docker");
    cmd.arg(kind)
        .arg(remove_subcommand)
        .args(remove_args)
        .args(&ids)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = cmd
        .status()
        .await
        .with_context(|| format!("failed to spawn docker {kind} {remove_subcommand}"))?;
    if !status.success() {
        bail!("docker {kind} {remove_subcommand} failed with status {status}");
    }
    Ok(())
}

async fn docker_list_ids(kind: &str, filters: &[(&str, &str)]) -> Result<Vec<String>> {
    let mut cmd = Command::new("docker");
    cmd.arg(kind)
        .arg("ls")
        .arg("-q")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if kind == "container" {
        cmd.arg("-a");
    }
    for (name, value) in filters {
        cmd.arg("--filter").arg(format!("{name}={value}"));
    }
    let output = cmd
        .output()
        .await
        .with_context(|| format!("failed to spawn docker {kind} ls"))?;
    if !output.status.success() {
        bail!("docker {kind} ls failed with status {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

async fn docker_inspect(ids: &[String]) -> Result<Vec<DockerInspectObject>> {
    let output = Command::new("docker")
        .arg("inspect")
        .args(ids)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .context("failed to spawn docker inspect")?;
    if !output.status.success() {
        bail!("docker inspect failed with status {}", output.status);
    }
    serde_json::from_slice(&output.stdout).context("failed to parse docker inspect output")
}

fn project_filter(project: &str) -> String {
    format!("{PROJECT_LABEL}={project}")
}

#[derive(Deserialize)]
struct DockerInspectObject {
    #[serde(rename = "Config")]
    config: Option<DockerInspectConfig>,
}

#[derive(Deserialize)]
struct DockerInspectConfig {
    #[serde(rename = "Labels")]
    labels: Option<HashMap<String, String>>,
}

fn owner_process_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn detach_from_terminal_interrupts(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
}

#[derive(Debug, Clone)]
struct SecretShadowMount {
    source: PathBuf,
    target: PathBuf,
    read_only: bool,
}

fn prepare_secret_shadow_mounts(
    workspace: &Path,
    container_workspace: &Path,
    pid: u32,
    filesystem: &crate::settings::FilesystemPolicy,
) -> Result<Vec<SecretShadowMount>> {
    let shadow_root = secret_shadow_root(pid);
    std::fs::create_dir_all(&shadow_root)
        .with_context(|| format!("failed to prepare {}", shadow_root.display()))?;

    let mut mounts = Vec::new();
    let matcher = crate::host_fs::FilesystemMatcher::new(workspace, filesystem)?;
    let canonical_workspace = std::fs::canonicalize(workspace)?;
    for root in matcher.root_paths() {
        let is_workspace_root = root == canonical_workspace.as_path();
        let target_root = if is_workspace_root {
            container_workspace.to_path_buf()
        } else {
            root.to_path_buf()
        };
        if !is_workspace_root {
            mounts.push(SecretShadowMount {
                source: root.to_path_buf(),
                target: target_root.clone(),
                read_only: matcher.root_readonly(root),
            });
        }
        collect_secret_shadow_mounts(
            root,
            root,
            &target_root,
            &shadow_root,
            &matcher,
            &mut mounts,
        )?;
    }
    append_claude_project_config_shadow_mounts(
        workspace,
        container_workspace,
        &shadow_root,
        &mut mounts,
    )?;
    Ok(mounts)
}

fn append_claude_project_config_shadow_mounts(
    workspace: &Path,
    container_workspace: &Path,
    shadow_root: &Path,
    mounts: &mut Vec<SecretShadowMount>,
) -> Result<()> {
    for relative in [
        ".mcp.json",
        ".claude/settings.json",
        ".claude/settings.local.json",
    ] {
        let src = workspace.join(relative);
        if !src.is_file() {
            continue;
        }
        let raw = std::fs::read_to_string(&src)
            .with_context(|| format!("failed to read {}", src.display()))?;
        let sanitized = crate::sync::sanitize_claude_config_for_container(&raw)
            .with_context(|| format!("failed to sanitize {}", src.display()))?;
        let dest = shadow_root
            .join("sanitized-claude-project-config")
            .join(relative);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to prepare {}", parent.display()))?;
        }
        std::fs::write(&dest, sanitized)
            .with_context(|| format!("failed to write {}", dest.display()))?;
        mounts.push(SecretShadowMount {
            source: dest,
            target: container_workspace.join(relative),
            read_only: true,
        });
    }
    Ok(())
}

fn collect_secret_shadow_mounts(
    root: &Path,
    path: &Path,
    container_root: &Path,
    empty_dir: &Path,
    matcher: &crate::host_fs::FilesystemMatcher,
    mounts: &mut Vec<SecretShadowMount>,
) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            eprintln!(
                "[agent-container] warning: skipping unreadable filesystem path {}: {e}",
                path.display()
            );
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("failed to stat {}", path.display())),
    };
    match matcher.classify_resolved_for_shadow(path)? {
        crate::host_fs::FilesystemAccess::Hidden => {
            let relative = path
                .strip_prefix(root)
                .with_context(|| format!("{} is not under {}", path.display(), root.display()))?;
            mounts.push(SecretShadowMount {
                source: if meta.is_dir() {
                    empty_dir.to_path_buf()
                } else {
                    PathBuf::from("/dev/null")
                },
                target: container_root.join(relative),
                read_only: true,
            });
            return Ok(());
        }
        crate::host_fs::FilesystemAccess::Readonly => {
            let relative = path
                .strip_prefix(root)
                .with_context(|| format!("{} is not under {}", path.display(), root.display()))?;
            mounts.push(SecretShadowMount {
                source: path.to_path_buf(),
                target: container_root.join(relative),
                read_only: true,
            });
            return Ok(());
        }
        crate::host_fs::FilesystemAccess::Readwrite => {}
    }

    if meta.is_dir() {
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                eprintln!(
                    "[agent-container] warning: skipping unreadable filesystem directory {}: {e}",
                    path.display()
                );
                return Ok(());
            }
            Err(e) => return Err(e).with_context(|| format!("failed to list {}", path.display())),
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                    eprintln!(
                        "[agent-container] warning: skipping unreadable filesystem entry under {}: {e}",
                        path.display()
                    );
                    continue;
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("failed to list {}", path.display()));
                }
            };
            collect_secret_shadow_mounts(
                root,
                &entry.path(),
                container_root,
                empty_dir,
                matcher,
                mounts,
            )?;
        }
    }
    Ok(())
}

fn write_secret_shadow_compose_override(
    pid: u32,
    mounts: &[SecretShadowMount],
) -> Result<Option<PathBuf>> {
    if mounts.is_empty() {
        return Ok(None);
    }

    let path = std::env::temp_dir().join(format!("agent-container-secret-shadows-{pid}.yml"));
    let mut out = String::from("services:\n  agent:\n    volumes:\n");
    for mount in mounts {
        out.push_str("      - type: bind\n");
        out.push_str(&format!(
            "        source: '{}'\n",
            yaml_single_quote(&mount.source.display().to_string())
        ));
        out.push_str(&format!(
            "        target: '{}'\n",
            yaml_single_quote(&mount.target.display().to_string())
        ));
        if mount.read_only {
            out.push_str("        read_only: true\n");
        }
    }
    std::fs::write(&path, out)
        .with_context(|| format!("failed to write compose override {}", path.display()))?;
    eprintln!(
        "[agent-container] filesystem guard: hiding {} denied workspace path(s)",
        mounts.len()
    );
    Ok(Some(path))
}

fn write_agent_command_compose_override(pid: u32, argv: &[String], tty: bool) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("agent-container-command-{pid}.yml"));
    let mut out = String::from("services:\n  agent:\n    command:\n");
    for arg in argv {
        out.push_str(&format!("      - '{}'\n", yaml_single_quote(arg)));
    }
    out.push_str(&format!(
        "    stdin_open: {}\n",
        if tty { "true" } else { "false" }
    ));
    out.push_str(&format!(
        "    tty: {}\n",
        if tty { "true" } else { "false" }
    ));
    std::fs::write(&path, out)
        .with_context(|| format!("failed to write compose override {}", path.display()))?;
    Ok(path)
}

fn secret_shadow_root(pid: u32) -> PathBuf {
    std::env::temp_dir().join(format!("agent-container-secret-shadows-{pid}"))
}

fn yaml_single_quote(value: &str) -> String {
    value.replace('\'', "''")
}

async fn watch_proxy_settings(
    ctx: ComposeCtx,
    workspace: PathBuf,
    allowlist_path: PathBuf,
    mut last_allowlist: String,
) {
    let mut last_settings = crate::settings::watched_file_fingerprint(&workspace);
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        let current = crate::settings::watched_file_fingerprint(&workspace);
        if current == last_settings {
            continue;
        }
        last_settings = current;

        match reload_proxy_settings(&ctx, &workspace, &allowlist_path, &last_allowlist).await {
            Ok(Some(next_allowlist)) => {
                last_allowlist = next_allowlist;
                eprintln!("[agent-container] proxy allowlist reloaded");
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("[agent-container] warning: failed to reload proxy allowlist: {e:#}")
            }
        }
    }
}

async fn reload_proxy_settings(
    ctx: &ComposeCtx,
    workspace: &Path,
    allowlist_path: &PathBuf,
    last_allowlist: &str,
) -> Result<Option<String>> {
    let merged = crate::settings::Settings::load_merged(workspace)
        .context("failed to reload merged settings")?;
    let next_allowlist = crate::proxy_allowlist::render(&merged.proxy.allow);
    if next_allowlist == last_allowlist {
        return Ok(None);
    }

    crate::proxy_allowlist::generate(&merged.proxy.allow, allowlist_path)
        .context("failed to materialise updated proxy allowlist for tinyproxy")?;

    let mut cmd = ctx.compose(&["restart", "proxy"]);
    let status = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("failed to spawn docker compose restart proxy")?;
    if !status.success() {
        bail!("`docker compose restart proxy` failed with status {status}");
    }
    Ok(Some(next_allowlist))
}

fn empty_workspace_agent_container_dir(pid: u32) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("agent-container-empty-config-{pid}"));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to prepare empty config mount {}", dir.display()))?;
    Ok(dir)
}

fn remove_path_any(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    }
}

#[derive(Clone)]
struct ComposeCtx {
    project: String,
    compose_files: Vec<PathBuf>,
    env: HashMap<String, String>,
}

impl ComposeCtx {
    fn compose(&self, tail: &[&str]) -> Command {
        let mut cmd = Command::new("docker");
        cmd.args(["compose", "-p", &self.project])
            .args(self.compose_file_args())
            .args(tail)
            .envs(&self.env)
            .stdin(Stdio::null());
        cmd
    }

    fn compose_file_args(&self) -> Vec<String> {
        self.compose_files
            .iter()
            .flat_map(|path| ["-f".to_string(), path.display().to_string()])
            .collect()
    }
}

fn is_stdin_tty() -> bool {
    std::io::stdin().is_terminal()
}

fn docker_start_attach_args(container_id: &str, stdin_tty: bool) -> Vec<String> {
    let mut args = vec![
        "start".to_string(),
        "-a".to_string(),
        "--detach-keys".to_string(),
        DOCKER_ATTACH_DETACH_KEYS.to_string(),
    ];
    if stdin_tty {
        args.push("-i".to_string());
    }
    args.push(container_id.to_string());
    args
}

pub fn default_dockerfile_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("AGENT_CONTAINER_DOCKERFILE_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docker")
}

fn default_compose_file() -> PathBuf {
    if let Ok(path) = std::env::var("AGENT_CONTAINER_COMPOSE_FILE") {
        return PathBuf::from(path);
    }
    default_dockerfile_dir().join("compose.yml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_shadow_mounts_include_sensitive_and_explicit_filters() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("work");
        std::fs::create_dir_all(workspace.join("private")).unwrap();
        std::fs::create_dir_all(workspace.join("blocked-dir")).unwrap();
        std::fs::create_dir_all(workspace.join(".agent-container")).unwrap();
        std::fs::create_dir_all(workspace.join(".claude")).unwrap();
        std::fs::write(workspace.join(".env"), "secret").unwrap();
        std::fs::write(
            workspace.join(".mcp.json"),
            r#"{"mcpServers":{"local":{"command":"/opt/host/bin/server"}},"allowed":"keep"}"#,
        )
        .unwrap();
        std::fs::write(workspace.join("private/token.txt"), "secret").unwrap();
        std::fs::write(
            workspace.join(".claude/settings.json"),
            r#"{"mcpServers":{"local":{"command":"/opt/host/bin/server"}},"theme":"dark"}"#,
        )
        .unwrap();
        std::fs::write(workspace.join("README.md"), "ok").unwrap();

        let policy = crate::settings::FilesystemPolicy {
            mounts: Vec::new(),
            hide: vec![
                r"(^|/)\.env(\..*)?$".to_string(),
                r"^private(/|$)".to_string(),
                r"^blocked-dir$".to_string(),
            ],
            readonly: vec![
                r"(^|/)\.agent-container(/|$)".to_string(),
                r"(^|/)\.claude(/|$)".to_string(),
            ],
        };
        let mounts =
            prepare_secret_shadow_mounts(&workspace, Path::new("/workspace"), 42, &policy).unwrap();
        let targets: Vec<_> = mounts
            .iter()
            .map(|mount| mount.target.display().to_string())
            .collect();
        assert!(targets.contains(&"/workspace/.env".to_string()));
        assert!(targets.contains(&"/workspace/private".to_string()));
        assert!(targets.contains(&"/workspace/blocked-dir".to_string()));
        assert!(targets.contains(&"/workspace/.agent-container".to_string()));
        assert!(targets.contains(&"/workspace/.claude".to_string()));
        assert!(targets.contains(&"/workspace/.mcp.json".to_string()));
        assert!(targets.contains(&"/workspace/.claude/settings.json".to_string()));
        assert!(!targets.contains(&"/workspace/README.md".to_string()));
        let file_mount = mounts
            .iter()
            .find(|mount| mount.target == Path::new("/workspace/.env"))
            .unwrap();
        assert_eq!(file_mount.source, PathBuf::from("/dev/null"));
        let dir_mount = mounts
            .iter()
            .find(|mount| mount.target == Path::new("/workspace/blocked-dir"))
            .unwrap();
        assert_ne!(dir_mount.source, PathBuf::from("/dev/null"));
        let readonly_mount = mounts
            .iter()
            .find(|mount| mount.target == Path::new("/workspace/.claude"))
            .unwrap();
        assert_eq!(
            readonly_mount.source,
            std::fs::canonicalize(workspace.join(".claude")).unwrap()
        );
        let agent_container_mount = mounts
            .iter()
            .find(|mount| mount.target == Path::new("/workspace/.agent-container"))
            .unwrap();
        assert_eq!(
            agent_container_mount.source,
            std::fs::canonicalize(workspace.join(".agent-container")).unwrap()
        );
        let mcp_json_mount = mounts
            .iter()
            .find(|mount| mount.target == Path::new("/workspace/.mcp.json"))
            .unwrap();
        let mcp_json = std::fs::read_to_string(&mcp_json_mount.source).unwrap();
        assert!(!mcp_json.contains("mcpServers"));
        assert!(mcp_json.contains("allowed"));
        let settings_mount = mounts
            .iter()
            .find(|mount| mount.target == Path::new("/workspace/.claude/settings.json"))
            .unwrap();
        let settings = std::fs::read_to_string(&settings_mount.source).unwrap();
        assert!(!settings.contains("mcpServers"));
        assert!(settings.contains("theme"));
    }

    #[test]
    fn compose_file_args_include_every_compose_file() {
        let ctx = ComposeCtx {
            project: "p".into(),
            compose_files: vec![PathBuf::from("base.yml"), PathBuf::from("shadow.yml")],
            env: HashMap::new(),
        };
        assert_eq!(
            ctx.compose_file_args(),
            vec!["-f", "base.yml", "-f", "shadow.yml"]
        );
    }

    #[test]
    fn agent_image_includes_openpyxl() {
        let dockerfile = include_str!("../docker/Dockerfile");
        assert!(
            dockerfile.contains("python3-openpyxl"),
            "agent image should include openpyxl for XLSX work"
        );
    }

    #[test]
    fn agent_image_includes_extended_terminfo() {
        let dockerfile = include_str!("../docker/Dockerfile");
        assert!(
            dockerfile.contains("ncurses-term"),
            "agent image should include terminfo for tmux and modern terminals"
        );
    }

    #[test]
    fn docker_attach_uses_non_default_detach_keys() {
        let args = docker_start_attach_args("container-id", true);
        assert_eq!(args[0], "start");
        assert!(args.contains(&"-a".to_string()));
        assert!(args.contains(&"-i".to_string()));
        assert_eq!(
            args.windows(2)
                .find(|pair| pair[0] == "--detach-keys")
                .map(|pair| pair[1].as_str()),
            Some(DOCKER_ATTACH_DETACH_KEYS)
        );
        assert_ne!(DOCKER_ATTACH_DETACH_KEYS, "ctrl-p,ctrl-q");
    }

    #[test]
    fn compose_exposes_selected_bedrock_profile_env() {
        let compose = include_str!("../docker/compose.yml");
        assert!(
            compose.contains("AWS_PROFILE=${AWS_PROFILE:-}"),
            "agent service should receive the selected Bedrock profile"
        );
    }

    #[test]
    fn compose_mounts_cursor_state_and_auth_env() {
        let compose = include_str!("../docker/compose.yml");
        assert!(compose.contains("${HOST_CURSOR_DIR}:/home/agent/.cursor"));
        assert!(compose.contains("${CURSOR_AUTH_PATH}:/home/agent/.config/cursor/auth.json"));
        assert!(compose.contains("CURSOR_CONFIG_DIR=/home/agent/.cursor"));
        assert!(compose.contains("CURSOR_API_KEY=${CURSOR_API_KEY:-}"));
        assert!(compose.contains("CURSOR_AUTH_TOKEN=${CURSOR_AUTH_TOKEN:-}"));
        assert!(!compose.contains("CURSOR_OAUTH2_AUTH_TOKEN"));
    }

    #[test]
    fn compose_labels_agent_container_objects_for_stale_cleanup() {
        let compose = include_str!("../docker/compose.yml");
        assert!(compose.contains("dev.statiolake.agent-container=true"));
        assert!(
            compose.contains("dev.statiolake.agent-container.project=${AGENT_CONTAINER_PROJECT}")
        );
        assert!(
            compose
                .contains("dev.statiolake.agent-container.owner-pid=${AGENT_CONTAINER_OWNER_PID}")
        );
        assert!(compose.contains("jail:\n    driver: bridge\n    internal: true\n    labels:"));
        assert!(compose.contains("egress:\n    driver: bridge\n    labels:"));
    }

    #[test]
    fn agent_image_installs_cursor_agent() {
        let dockerfile = include_str!("../docker/Dockerfile");
        assert!(dockerfile.contains("https://cursor.com/install"));
        assert!(dockerfile.contains("/usr/local/bin/cursor-agent"));
        assert!(dockerfile.contains("exec cursor-agent"));
    }
}
