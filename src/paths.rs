use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::UserDirs;

pub struct HostPaths {
    pub claude_root: PathBuf,
    pub workspace: PathBuf,
}

impl HostPaths {
    pub fn detect() -> Result<Self> {
        let user_dirs = UserDirs::new().context("failed to detect user home directory")?;
        let home = user_dirs.home_dir().to_path_buf();
        let claude_root = home.join(".claude");
        let workspace = std::env::current_dir().context("failed to read current working directory")?;
        Ok(Self {
            claude_root,
            workspace,
        })
    }

    pub fn host_project_dir(&self) -> PathBuf {
        self.claude_root
            .join("projects")
            .join(encode_project_dir(&self.workspace))
    }

    pub fn host_claude_md(&self) -> PathBuf {
        self.claude_root.join("CLAUDE.md")
    }
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
    fn container_workspace_encodes_to_dash_workspace() {
        // The compose file hard-codes `-workspace` for the in-container session
        // dir; keep this test so a mismatch fails loudly.
        assert_eq!(encode_project_dir("/workspace"), "-workspace");
    }
}
