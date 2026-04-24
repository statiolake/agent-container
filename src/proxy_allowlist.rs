//! Runtime generation of the tinyproxy allowlist.
//!
//! Assembled from two sources:
//!
//! 1. The bundled base file at `docker/proxy/allowlist.txt` — the set of
//!    hosts every session needs regardless of project (crates.io, npm,
//!    Anthropic API, etc.).
//! 2. `settings.proxy.allow` (global ∪ workspace, merged by
//!    [`crate::settings::Settings::load_merged`]).
//!
//! The merged file is written into XDG cache (`~/.cache/agent-container/`)
//! with the invoker's PID embedded so concurrent invocations don't race
//! on the same filename. tinyproxy bind-mounts the resulting file read-only.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;

/// Assemble `base + user_allow` (preserving base order, appending unique
/// user entries) and write the result to `dest`.
pub fn generate(base: &Path, user_allow: &[String], dest: &Path) -> Result<()> {
    let base_raw = fs::read_to_string(base)
        .with_context(|| format!("failed to read base allowlist at {}", base.display()))?;

    let already_present: std::collections::HashSet<&str> = base_raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    let mut out = base_raw.clone();
    if !out.ends_with('\n') {
        out.push('\n');
    }

    let extras: Vec<&String> = user_allow
        .iter()
        .filter(|pat| {
            let trimmed = pat.trim();
            !trimmed.is_empty() && !already_present.contains(trimmed)
        })
        .collect();
    if !extras.is_empty() {
        out.push_str("\n# user-defined (agent-container settings.toml proxy.allow)\n");
        for pat in extras {
            out.push_str(pat.trim());
            out.push('\n');
        }
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(dest, out)
        .with_context(|| format!("failed to write merged allowlist to {}", dest.display()))?;
    Ok(())
}

/// Per-process path under XDG cache so simultaneous invocations (e.g. a
/// dev running `agent-container run` alongside `agent-container shell`)
/// get their own file and don't clobber each other.
pub fn cache_path_for(pid: u32) -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "agent-container")
        .context("failed to resolve XDG project directories")?;
    Ok(dirs
        .cache_dir()
        .join(format!("proxy-allowlist.{pid}.txt")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        fs::write(path, content).unwrap();
    }

    #[test]
    fn appends_user_entries_after_base() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.txt");
        let dest = dir.path().join("merged.txt");
        write(&base, "# bundled\n^anthropic\\.com$\n");
        generate(
            &base,
            &[
                "^my-internal\\.example$".to_string(),
                "^another\\.host$".to_string(),
            ],
            &dest,
        )
        .unwrap();

        let got = fs::read_to_string(&dest).unwrap();
        assert!(got.contains("^anthropic\\.com$"));
        assert!(got.contains("^my-internal\\.example$"));
        assert!(got.contains("^another\\.host$"));
        assert!(got.contains("# user-defined"));
        let anthropic_idx = got.find("^anthropic\\.com$").unwrap();
        let user_idx = got.find("^my-internal\\.example$").unwrap();
        assert!(anthropic_idx < user_idx, "user entries must come after base");
    }

    #[test]
    fn skips_duplicates_already_in_base() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.txt");
        let dest = dir.path().join("merged.txt");
        write(&base, "^already-there\\.com$\n");
        generate(
            &base,
            &["^already-there\\.com$".to_string(), "^new\\.com$".to_string()],
            &dest,
        )
        .unwrap();

        let got = fs::read_to_string(&dest).unwrap();
        assert_eq!(
            got.matches("^already-there\\.com$").count(),
            1,
            "duplicate should not be re-appended"
        );
        assert!(got.contains("^new\\.com$"));
    }

    #[test]
    fn skips_blank_and_whitespace_entries() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.txt");
        let dest = dir.path().join("merged.txt");
        write(&base, "^x$\n");
        generate(
            &base,
            &["".to_string(), "   ".to_string(), "^y$".to_string()],
            &dest,
        )
        .unwrap();
        let got = fs::read_to_string(&dest).unwrap();
        assert!(got.contains("^y$"));
        // No stray user-defined section when everything was blank:
        assert_eq!(got.matches("user-defined").count(), 1);
    }

    #[test]
    fn produces_file_even_with_no_user_entries() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.txt");
        let dest = dir.path().join("merged.txt");
        write(&base, "^only-base$\n");
        generate(&base, &[], &dest).unwrap();
        let got = fs::read_to_string(&dest).unwrap();
        assert!(got.contains("^only-base$"));
        assert!(!got.contains("user-defined"));
    }
}
