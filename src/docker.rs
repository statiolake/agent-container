use std::collections::HashMap;
use std::io::{ErrorKind, IsTerminal};
#[cfg(unix)]
use std::os::unix::process::CommandExt as StdCommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::aws::BedrockSetup;
use crate::paths::HostPaths;

const AGENT_IMAGE_TAG: &str = "agent-container:dev";
const PROXY_IMAGE_TAG: &str = "agent-container-proxy:dev";

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
    pub bedrock_setup: Option<BedrockSetup>,
    /// Pre-built `http://<host>:<port>` URL the container should use to
    /// reach the broker. The hostname encodes the engine-flavour choice
    /// (Docker Desktop, Rancher Desktop, native Linux) made up-front by
    /// `host_kind::HostKind`; everything downstream just reads it.
    pub broker_url_from_container: String,
    /// The command to invoke inside the container, e.g.
    /// `["claude", "--dangerously-skip-permissions"]` or `["codex"]`.
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
        ("ALLOWLIST_PATH", allowlist_path.display().to_string()),
        ("HOST_UID", uid.to_string()),
        ("HOST_GID", gid.to_string()),
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
    // AWS_PROFILE / AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY /
    // AWS_SESSION_TOKEN are deliberately NOT forwarded: creds live only
    // in Claude Code's memory (via awsCredentialExport), and letting the
    // host's own AWS env vars leak in would make the container transact
    // against whatever account the host shell happens to be pointing at
    // — not necessarily the one the operator chose in settings.json.
    let mut put = |k: &str, v: String| {
        env.insert(k.to_string(), v);
    };
    if let Some(setup) = &opts.bedrock_setup {
        put("CLAUDE_CODE_USE_BEDROCK", "1".to_string());
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
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
    ] {
        env.entry(key.to_string()).or_insert_with(String::new);
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
    copy_staged_home_into_container(&opts.host.staged_home, &agent_container_id)
        .await
        .context("failed to copy staged home into agent container")?;

    let mut cmd = Command::new("docker");
    cmd.args(["start", "-a"]);
    if is_stdin_tty() {
        cmd.arg("-i");
    }
    cmd.arg(&agent_container_id);
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
        &opts.host.container_workspace(),
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

async fn copy_staged_home_into_container(staged_home: &Path, container_id: &str) -> Result<()> {
    let mut tar = Vec::new();
    crate::staging_archive::write_tar(staged_home, &mut tar)
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
        signal = tokio::signal::ctrl_c() => {
            if let Err(e) = signal {
                eprintln!("[agent-container] warning: failed to install Ctrl+C handler: {e}");
            }
            eprintln!("[agent-container] interrupt received; cleaning up compose stack...");

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
    for root in matcher.roots() {
        let is_workspace_root = root == &canonical_workspace;
        let target_root = if is_workspace_root {
            container_workspace.to_path_buf()
        } else {
            root.clone()
        };
        if !is_workspace_root {
            mounts.push(SecretShadowMount {
                source: root.clone(),
                target: target_root.clone(),
                read_only: false,
            });
        }
        collect_secret_shadow_mounts(
            &root,
            &root,
            &target_root,
            &shadow_root,
            &matcher,
            &mut mounts,
        )?;
    }
    Ok(mounts)
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
    match matcher.classify_resolved(path)? {
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
        "[agent-container] shadowing {} existing denied workspace path(s)",
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
            .envs(&self.env);
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
        std::fs::create_dir_all(workspace.join(".claude")).unwrap();
        std::fs::write(workspace.join(".env"), "secret").unwrap();
        std::fs::write(workspace.join("private/token.txt"), "secret").unwrap();
        std::fs::write(workspace.join(".claude/settings.json"), "{}").unwrap();
        std::fs::write(workspace.join("README.md"), "ok").unwrap();

        let policy = crate::settings::FilesystemPolicy {
            mounts: Vec::new(),
            hide: vec![
                r"(^|/)\.env(\..*)?$".to_string(),
                r"^private(/|$)".to_string(),
                r"^blocked-dir$".to_string(),
            ],
            readonly: vec![r"(^|/)\.claude(/|$)".to_string()],
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
        assert!(targets.contains(&"/workspace/.claude".to_string()));
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
}
