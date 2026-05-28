use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
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
    /// Merged `[host_fs].allow` patterns. Explicit deny rules and the
    /// built-in sensitive-file denies are shadow-mounted for files that
    /// already exist in the workspace at container creation time.
    pub host_fs_allow: Vec<String>,
}

/// Orchestrate the compose project: start relay, run agent, always tear down.
pub async fn run(opts: RunOptions) -> Result<i32> {
    let host_project_dir = opts.host.host_project_dir();
    std::fs::create_dir_all(&host_project_dir).with_context(|| {
        format!(
            "failed to prepare session dir {}",
            host_project_dir.display()
        )
    })?;
    std::fs::create_dir_all(&opts.host.container_home).with_context(|| {
        format!(
            "failed to prepare persistent claude-home at {}",
            opts.host.container_home.display()
        )
    })?;

    // Use /dev/null as the CLAUDE.md mount source when the host lacks one, so
    // compose always has a concrete path to bind.
    let claude_md = opts.host.host_claude_md();
    let claude_md_src = if claude_md.is_file() {
        claude_md
    } else {
        PathBuf::from("/dev/null")
    };

    // The workspace is intentionally writable, but its agent-container
    // settings directory controls host-side behavior. Overlay it read-only
    // inside the container; if the workspace has no such directory, mount
    // an empty read-only directory so the agent cannot create one from
    // inside the container.
    let workspace_agent_container_dir = opts.host.workspace.join(".agent-container");
    let workspace_agent_container_mount_src = if workspace_agent_container_dir.is_dir() {
        workspace_agent_container_dir
    } else {
        empty_workspace_agent_container_dir(std::process::id())?
    };
    let secret_shadows = prepare_secret_shadow_mounts(
        &opts.host.workspace,
        &opts.host.container_workspace(),
        std::process::id(),
        &opts.host_fs_allow,
    )?;

    let project = format!("agent-container-{}", std::process::id());
    let compose_file = default_compose_file();
    let shadow_compose_file =
        write_secret_shadow_compose_override(std::process::id(), &secret_shadows)?;

    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();

    let allowlist_path = crate::proxy_allowlist::cache_path_for(std::process::id())?;
    crate::proxy_allowlist::generate(&opts.proxy_allow, &allowlist_path)
        .context("failed to materialise proxy allowlist for tinyproxy")?;

    let mut env: HashMap<String, String> = [
        ("WORKSPACE_PATH", opts.host.workspace.display().to_string()),
        (
            "CONTAINER_WORKSPACE_PATH",
            opts.host.container_workspace().display().to_string(),
        ),
        (
            "CONTAINER_HOME_PATH",
            opts.host.container_home.display().to_string(),
        ),
        ("HOST_PROJECT_DIR", host_project_dir.display().to_string()),
        (
            "CONTAINER_PROJECT_DIR_NAME",
            opts.host.container_project_dir_name(),
        ),
        (
            "WORKSPACE_AGENT_CONTAINER_MOUNT_SRC",
            workspace_agent_container_mount_src.display().to_string(),
        ),
        (
            "CREDENTIALS_PATH",
            opts.credentials_path.display().to_string(),
        ),
        ("CLAUDE_MD_MOUNT_SRC", claude_md_src.display().to_string()),
        (
            "CODEX_AUTH_PATH",
            opts.codex_auth_path.display().to_string(),
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
        compose_files: shadow_compose_file.into_iter().fold(
            vec![compose_file.clone()],
            |mut files, file| {
                files.push(file);
                files
            },
        ),
        env: env.clone(),
    };

    // Guarantees `compose down` on any exit path (panic/error/normal).
    struct Cleanup<'a>(&'a ComposeCtx);
    impl<'a> Drop for Cleanup<'a> {
        fn drop(&mut self) {
            let ctx = self.0;
            let status = std::process::Command::new("docker")
                .args(["compose", "-p", &ctx.project])
                .args(ctx.compose_file_args())
                .args(["down", "--remove-orphans", "--timeout", "5"])
                .envs(&ctx.env)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if let Err(e) = status {
                eprintln!("[agent-container] warning: compose down failed: {e}");
            }
        }
    }
    let _cleanup = Cleanup(&ctx);

    // 1) Start the forward proxy sidecar in the background.
    let status = ctx
        .compose(&["up", "-d", "--no-color", "proxy"])
        .status()
        .await
        .context("failed to spawn docker compose up")?;
    if !status.success() {
        bail!("`docker compose up -d proxy` failed");
    }
    let proxy_reload = tokio::spawn(watch_proxy_settings(
        ctx.clone(),
        opts.host.workspace.clone(),
        allowlist_path.clone(),
        crate::proxy_allowlist::render(&opts.proxy_allow),
    ));

    // 2) Run the agent in the foreground, inheriting stdio for the TUI.
    let mut cmd = ctx.compose(&["run", "--rm", "--name", &format!("{project}-agent")]);
    if !is_stdin_tty() {
        cmd.arg("-T");
    }
    cmd.arg("agent");
    cmd.args(&opts.agent_command);
    if !opts.extra_args.is_empty() {
        cmd.args(&opts.extra_args);
    }
    let status = cmd
        .status()
        .await
        .context("failed to spawn docker compose run");
    proxy_reload.abort();
    let status = status?;

    // `_cleanup` runs `compose down` on scope exit.
    Ok(status.code().unwrap_or(1))
}

#[derive(Debug, Clone)]
struct SecretShadowMount {
    source: PathBuf,
    target: PathBuf,
    kind: SecretShadowKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretShadowKind {
    File,
    Directory,
}

fn prepare_secret_shadow_mounts(
    workspace: &Path,
    container_workspace: &Path,
    pid: u32,
    host_fs_allow: &[String],
) -> Result<Vec<SecretShadowMount>> {
    let workspace = std::fs::canonicalize(workspace)
        .with_context(|| format!("failed to resolve workspace {}", workspace.display()))?;
    let shadow_root = std::env::temp_dir().join(format!("agent-container-secret-shadows-{pid}"));
    std::fs::create_dir_all(&shadow_root)
        .with_context(|| format!("failed to prepare {}", shadow_root.display()))?;

    let mut mounts = Vec::new();
    collect_secret_shadow_mounts(
        &workspace,
        &workspace,
        container_workspace,
        &shadow_root,
        host_fs_allow,
        &mut mounts,
    )?;
    Ok(mounts)
}

fn collect_secret_shadow_mounts(
    root: &Path,
    path: &Path,
    container_root: &Path,
    empty_dir: &Path,
    host_fs_allow: &[String],
    mounts: &mut Vec<SecretShadowMount>,
) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?;
    if crate::host_fs::path_denied_by_rules(path, host_fs_allow) {
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
            kind: if meta.is_dir() {
                SecretShadowKind::Directory
            } else {
                SecretShadowKind::File
            },
        });
        return Ok(());
    }

    if meta.is_dir() {
        for entry in
            std::fs::read_dir(path).with_context(|| format!("failed to list {}", path.display()))?
        {
            let entry = entry?;
            collect_secret_shadow_mounts(
                root,
                &entry.path(),
                container_root,
                empty_dir,
                host_fs_allow,
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
        out.push_str("        read_only: true\n");
    }
    std::fs::write(&path, out)
        .with_context(|| format!("failed to write compose override {}", path.display()))?;
    eprintln!(
        "[agent-container] shadowing {} existing denied workspace path(s)",
        mounts.len()
    );
    Ok(Some(path))
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
    fn secret_shadow_mounts_include_sensitive_and_explicit_denies() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("work");
        std::fs::create_dir_all(workspace.join("private")).unwrap();
        std::fs::create_dir_all(workspace.join("blocked-dir")).unwrap();
        std::fs::write(workspace.join(".env"), "secret").unwrap();
        std::fs::write(workspace.join("private/token.txt"), "secret").unwrap();
        std::fs::write(workspace.join("README.md"), "ok").unwrap();

        let canonical_workspace = std::fs::canonicalize(&workspace).unwrap();
        let rules = vec![
            format!("{}/**", canonical_workspace.display()),
            format!("!{}/private/**", canonical_workspace.display()),
            format!("!{}/blocked-dir", canonical_workspace.display()),
        ];
        let mounts =
            prepare_secret_shadow_mounts(&workspace, Path::new("/workspace"), 42, &rules).unwrap();
        let targets: Vec<_> = mounts
            .iter()
            .map(|mount| mount.target.display().to_string())
            .collect();
        assert!(targets.contains(&"/workspace/.env".to_string()));
        assert!(targets.contains(&"/workspace/private".to_string()));
        assert!(targets.contains(&"/workspace/blocked-dir".to_string()));
        assert!(!targets.contains(&"/workspace/README.md".to_string()));
        let file_mount = mounts
            .iter()
            .find(|mount| mount.target == Path::new("/workspace/.env"))
            .unwrap();
        assert_eq!(file_mount.source, PathBuf::from("/dev/null"));
        assert_eq!(file_mount.kind, SecretShadowKind::File);
        let dir_mount = mounts
            .iter()
            .find(|mount| mount.target == Path::new("/workspace/blocked-dir"))
            .unwrap();
        assert_ne!(dir_mount.source, PathBuf::from("/dev/null"));
        assert_eq!(dir_mount.kind, SecretShadowKind::Directory);
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
