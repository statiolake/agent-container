use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::aws::{BedrockCredentials, BedrockSetup};
use crate::paths::HostPaths;

const AGENT_IMAGE_TAG: &str = "agent-container:dev";
const PROXY_IMAGE_TAG: &str = "agent-container-proxy:dev";

/// Build both images if they are missing.
pub async fn ensure_images(dockerfile_dir: &Path) -> Result<()> {
    ensure_one(AGENT_IMAGE_TAG, dockerfile_dir, "Dockerfile").await?;
    ensure_one(
        PROXY_IMAGE_TAG,
        &dockerfile_dir.join("proxy"),
        "Dockerfile",
    )
    .await?;
    Ok(())
}

async fn ensure_one(tag: &str, context_dir: &Path, dockerfile_name: &str) -> Result<()> {
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
    eprintln!("[agent-container] building image {tag} (first run only)...");
    let dockerfile = context_dir.join(dockerfile_name);
    let status = Command::new("docker")
        .args([
            "build",
            "-t",
            tag,
            "-f",
            dockerfile.to_str().context("non-utf8 dockerfile path")?,
            context_dir.to_str().context("non-utf8 build context path")?,
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
    pub bedrock: Option<(BedrockSetup, BedrockCredentials)>,
    pub extra_args: Vec<String>,
}

/// Orchestrate the compose project: start relay, run agent, always tear down.
pub async fn run(opts: RunOptions) -> Result<i32> {
    let host_project_dir = opts.host.host_project_dir();
    std::fs::create_dir_all(&host_project_dir)
        .with_context(|| format!("failed to prepare session dir {}", host_project_dir.display()))?;
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

    let project = format!("agent-container-{}", std::process::id());
    let compose_file = default_compose_file();

    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();

    let allowlist_path = default_dockerfile_dir().join("proxy").join("allowlist.txt");

    let mut env: HashMap<String, String> = [
        (
            "WORKSPACE_PATH",
            opts.host.workspace.display().to_string(),
        ),
        (
            "CONTAINER_HOME_PATH",
            opts.host.container_home.display().to_string(),
        ),
        (
            "HOST_PROJECT_DIR",
            host_project_dir.display().to_string(),
        ),
        (
            "CREDENTIALS_PATH",
            opts.credentials_path.display().to_string(),
        ),
        (
            "CLAUDE_MD_MOUNT_SRC",
            claude_md_src.display().to_string(),
        ),
        ("ALLOWLIST_PATH", allowlist_path.display().to_string()),
        ("HOST_UID", uid.to_string()),
        ("HOST_GID", gid.to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();

    // Bedrock env vars: declared as `${VAR:-}` in compose.yml, so an unset
    // shell var translates to an empty string in the container — which all
    // of these AWS / Claude Code toggles treat as "not set".
    let mut put = |k: &str, v: String| {
        env.insert(k.to_string(), v);
    };
    if let Some((setup, creds)) = &opts.bedrock {
        put("CLAUDE_CODE_USE_BEDROCK", "1".to_string());
        // Echo back the user's original toggle too so downstream code that
        // reads `USE_BEDROCK` directly also sees it.
        if setup.enable_var != "CLAUDE_CODE_USE_BEDROCK" {
            put(&setup.enable_var, "1".to_string());
        }
        if let Some(model) = &setup.model {
            put("ANTHROPIC_MODEL", model.clone());
        }
        if let Some(region) = creds.region.clone().or_else(|| setup.region.clone()) {
            put("AWS_REGION", region.clone());
            put("AWS_DEFAULT_REGION", region);
        }
        put("AWS_ACCESS_KEY_ID", creds.access_key_id.clone());
        put("AWS_SECRET_ACCESS_KEY", creds.secret_access_key.clone());
        if let Some(token) = &creds.session_token {
            put("AWS_SESSION_TOKEN", token.clone());
        }
    }
    // Always ensure compose gets values for these substitutions so an unset
    // `${VAR:-}` resolves to an empty string rather than inheriting from the
    // parent shell unexpectedly.
    for key in [
        "CLAUDE_CODE_USE_BEDROCK",
        "USE_BEDROCK",
        "ANTHROPIC_MODEL",
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
    ] {
        env.entry(key.to_string()).or_insert_with(String::new);
    }

    let ctx = ComposeCtx {
        project: project.clone(),
        compose_file: compose_file.clone(),
        env: env.clone(),
    };

    // Guarantees `compose down` on any exit path (panic/error/normal).
    struct Cleanup<'a>(&'a ComposeCtx);
    impl<'a> Drop for Cleanup<'a> {
        fn drop(&mut self) {
            let ctx = self.0;
            let status = std::process::Command::new("docker")
                .args(["compose", "-p", &ctx.project, "-f"])
                .arg(&ctx.compose_file)
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

    // 2) Run the agent in the foreground, inheriting stdio for the TUI.
    let mut cmd = ctx.compose(&["run", "--rm", "--name", &format!("{project}-agent")]);
    if !is_stdin_tty() {
        cmd.arg("-T");
    }
    cmd.arg("agent");
    cmd.args(["claude", "--dangerously-skip-permissions"]);
    if !opts.extra_args.is_empty() {
        cmd.args(&opts.extra_args);
    }
    let status = cmd
        .status()
        .await
        .context("failed to spawn docker compose run")?;

    // `_cleanup` runs `compose down` on scope exit.
    Ok(status.code().unwrap_or(1))
}

struct ComposeCtx {
    project: String,
    compose_file: PathBuf,
    env: HashMap<String, String>,
}

impl ComposeCtx {
    fn compose(&self, tail: &[&str]) -> Command {
        let mut cmd = Command::new("docker");
        cmd.args(["compose", "-p", &self.project, "-f"])
            .arg(&self.compose_file)
            .args(tail)
            .envs(&self.env);
        cmd
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
