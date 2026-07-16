use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::UserDirs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Clone)]
pub struct HostPaths {
    pub home: PathBuf,
    pub claude_root: PathBuf,
    pub workspace: PathBuf,
    pub staged_home: PathBuf,
}

impl HostPaths {
    pub fn detect() -> Result<Self> {
        let user_dirs = UserDirs::new().context("failed to detect user home directory")?;
        let home = user_dirs.home_dir().to_path_buf();
        let claude_root = home.join(".claude");
        let workspace =
            std::env::current_dir().context("failed to read current working directory")?;
        let staged_home = detect_staged_home();
        Ok(Self {
            home,
            claude_root,
            workspace,
            staged_home,
        })
    }

    pub fn container_workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn host_claude_projects_dir(&self) -> PathBuf {
        self.claude_root.join("projects")
    }

    pub fn host_claude_md(&self) -> PathBuf {
        self.claude_root.join("CLAUDE.md")
    }

    pub fn staged_root(&self) -> PathBuf {
        self.staged_home
            .parent()
            .unwrap_or(&self.staged_home)
            .to_path_buf()
    }

    pub fn prepare_staged_root(&self) -> Result<()> {
        let root = self.staged_root();
        std::fs::create_dir_all(&root)
            .with_context(|| format!("failed to create {}", root.display()))?;
        #[cfg(unix)]
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to chmod {}", root.display()))?;
        Ok(())
    }
}

/// Per-run host-side staging tree for generated agent config files.
///
/// This is deliberately not a persistent container `$HOME`: the docker image
/// supplies an ephemeral `/home/agent`, and only selected files/directories
/// from this tree are bind-mounted into it.
fn detect_staged_home() -> PathBuf {
    std::env::temp_dir()
        .join("agent-container")
        .join(std::process::id().to_string())
        .join("home")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_workspace_uses_host_path_identity() {
        let workspace = PathBuf::from("/Users/example/repo");
        let paths = HostPaths {
            home: PathBuf::from("/Users/example"),
            claude_root: PathBuf::from("/Users/example/.claude"),
            workspace: workspace.clone(),
            staged_home: PathBuf::from("/tmp/agent-container/123/home"),
        };

        assert_eq!(paths.container_workspace(), workspace.as_path());
        assert_eq!(
            paths.host_claude_projects_dir(),
            PathBuf::from("/Users/example/.claude/projects")
        );
        assert_eq!(
            paths.staged_root(),
            PathBuf::from("/tmp/agent-container/123")
        );
    }
}
