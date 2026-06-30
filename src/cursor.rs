use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct CursorStateDir {
    pub path: PathBuf,
}

pub fn prepare_state(host_home: &Path) -> Result<CursorStateDir> {
    let path = host_home.join(".cursor");
    std::fs::create_dir_all(&path)
        .with_context(|| format!("failed to prepare Cursor state dir {}", path.display()))?;
    Ok(CursorStateDir { path })
}

pub fn has_host_cli_config(host_home: &Path) -> bool {
    host_home.join(".cursor/cli-config.json").is_file()
}

pub fn has_portable_auth_env() -> bool {
    std::env::var_os("CURSOR_API_KEY").is_some() || std::env::var_os("CURSOR_AUTH_TOKEN").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_state_creates_cursor_dir() {
        let home = tempfile::tempdir().unwrap();
        let state = prepare_state(home.path()).unwrap();
        assert_eq!(state.path, home.path().join(".cursor"));
        assert!(state.path.is_dir());
    }

    #[test]
    fn host_cli_config_detects_cli_config() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".cursor")).unwrap();
        std::fs::write(home.path().join(".cursor/cli-config.json"), "{}").unwrap();
        assert!(has_host_cli_config(home.path()));
    }
}
