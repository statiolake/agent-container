use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::{ProjectDirs, UserDirs};

pub struct HostPaths {
    pub home: PathBuf,
    pub claude_root: PathBuf,
    pub workspace: PathBuf,
    pub container_home: PathBuf,
}

impl HostPaths {
    pub fn detect() -> Result<Self> {
        let user_dirs = UserDirs::new().context("failed to detect user home directory")?;
        let home = user_dirs.home_dir().to_path_buf();
        let claude_root = home.join(".claude");
        let workspace =
            std::env::current_dir().context("failed to read current working directory")?;
        let container_home = detect_container_home()?;
        Ok(Self {
            home,
            claude_root,
            workspace,
            container_home,
        })
    }

    pub fn host_project_dir(&self) -> PathBuf {
        self.claude_root
            .join("projects")
            .join(encode_project_dir(&self.workspace))
    }

    pub fn container_workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn container_project_dir_name(&self) -> String {
        encode_project_dir(self.container_workspace())
    }

    pub fn host_claude_md(&self) -> PathBuf {
        self.claude_root.join("CLAUDE.md")
    }
}

/// Persistent `$HOME` directory used by the containerised Claude Code.
/// Kept separate from the host's `~` so host settings, hooks, and plugins
/// never leak in, while onboarding, login, and other transient state (which
/// Claude Code writes to both `~/.claude/` and `~/.claude.json`) survives
/// across runs.
fn detect_container_home() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "agent-container")
        .context("failed to resolve XDG project directories")?;
    Ok(dirs.data_local_dir().join("home"))
}

/// Convert an absolute path to the directory name Claude Code uses under
/// `~/.claude/projects/`. Claude Code replaces path separators and `.` with
/// `-`.
pub fn encode_project_dir<P: AsRef<Path>>(path: P) -> String {
    let path = path.as_ref();
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '/' | '\\' | '.' | ':' => out.push('-'),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_claude_code() {
        assert_eq!(
            encode_project_dir("/home/user/projects/agent-container"),
            "-home-user-projects-agent-container"
        );
    }

    #[test]
    fn container_workspace_uses_host_path_identity() {
        let workspace = PathBuf::from("/Users/example/repo");
        let paths = HostPaths {
            home: PathBuf::from("/Users/example"),
            claude_root: PathBuf::from("/Users/example/.claude"),
            workspace: workspace.clone(),
            container_home: PathBuf::from("/Users/example/.local/share/agent-container/home"),
        };

        assert_eq!(paths.container_workspace(), workspace.as_path());
        assert_eq!(paths.container_project_dir_name(), "-Users-example-repo");
    }
}
