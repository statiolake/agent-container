//! Shared credential file with last-out write-back.
//!
//! Several `agent-container` invocations on the same host need to share
//! a single credential file so that an OAuth refresh inside one
//! container is visible to the others (and to the host on shutdown).
//! Each container takes a `flock(LOCK_SH)` on a sidecar lock file for
//! the lifetime of its `agent-container` process. On exit the process
//! releases the shared lock and tries to upgrade to `LOCK_EX |
//! LOCK_NB`; if the upgrade succeeds, no sibling is still alive, so
//! the process owns the cleanup pass — read the (possibly refreshed)
//! shared file, write it back to the host (Keychain on macOS, file
//! elsewhere), then unlink the shared copy.
//!
//! The OS releases the shared lock automatically when the FD is closed
//! (including on `SIGKILL`), so PID-based ref-counting isn't needed.
//!
//! This is best-effort: if write-back fails (Keychain ACL denial, the
//! host file vanished, …) the shared copy is removed anyway. Leaving a
//! stale copy behind would mask a future fresh login by feeding the
//! container a token the host considers invalid.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rustix::fs::{FlockOperation, flock};

/// Where to write the credential bytes back to the host when the last
/// container exits.
pub enum HostSync {
    /// macOS: update a `generic-password` Keychain item via `security
    /// add-generic-password -U`.
    Keychain {
        service: String,
        account: Option<String>,
    },
    /// Linux (and Codex everywhere): atomically replace the host file.
    File(PathBuf),
}

pub struct SharedCredFile {
    pub path: PathBuf,
    lock_path: PathBuf,
    /// Held for the lifetime of this agent-container process. Closing
    /// it releases the OS-level shared lock — see `Drop`.
    lock_file: Option<File>,
    host_sync: HostSync,
}

impl SharedCredFile {
    /// Open the shared credential file (creating it from `loader` when
    /// no other container has populated it yet) and take a shared lock
    /// on its sidecar lock file. Returns the handle plus the raw
    /// credential bytes, so the caller can parse fields like `expires_at`.
    pub fn open(
        shared_path: PathBuf,
        host_sync: HostSync,
        loader: impl FnOnce() -> Result<String>,
    ) -> Result<(Self, String)> {
        if let Some(parent) = shared_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let lock_path = lock_path_for(&shared_path);

        // The lock file's existence (not the credential file's) decides
        // ownership. Create-if-missing is fine: it has no secret content.
        let lock_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open lock at {}", lock_path.display()))?;
        flock(&lock_file, FlockOperation::LockShared).with_context(|| {
            format!("failed to take shared lock on {}", lock_path.display())
        })?;

        let raw = if shared_file_is_populated(&shared_path) {
            fs::read_to_string(&shared_path).with_context(|| {
                format!(
                    "failed to read shared credentials at {}",
                    shared_path.display()
                )
            })?
        } else {
            let raw = loader()?;
            write_secret_atomic(&shared_path, raw.trim())?;
            raw
        };

        Ok((
            Self {
                path: shared_path,
                lock_path,
                lock_file: Some(lock_file),
                host_sync,
            },
            raw,
        ))
    }
}

fn shared_file_is_populated(p: &Path) -> bool {
    fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false)
}

fn lock_path_for(p: &Path) -> PathBuf {
    let mut name = p
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(".lock");
    p.with_file_name(name)
}

/// Write `raw` to `path` atomically (write to a sibling temp file with
/// 0600, then rename). Avoids a half-written file becoming visible to a
/// sibling reader between create + write.
fn write_secret_atomic(path: &Path, raw: &str) -> Result<()> {
    let mut tmp_name = path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        f.write_all(raw.as_bytes())?;
        f.flush().ok();
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to move {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

impl Drop for SharedCredFile {
    fn drop(&mut self) {
        // Try to upgrade our own shared lock to exclusive on the same
        // FD. EWOULDBLOCK means a sibling agent-container still holds a
        // shared lock, so we leave the cleanup pass to whoever exits
        // last. Doing the upgrade in place (rather than closing and
        // re-opening) avoids subtle differences in flock semantics when
        // multiple file descriptions reference the same inode — visible
        // on macOS in particular.
        let Some(lock) = self.lock_file.take() else {
            return;
        };
        if flock(&lock, FlockOperation::NonBlockingLockExclusive).is_err() {
            return;
        }

        if let Ok(raw) = fs::read_to_string(&self.path) {
            if let Err(e) = self.host_sync.apply(raw.trim()) {
                tracing::warn!(
                    %e,
                    "failed to write credentials back to host; discarding shared copy",
                );
            }
        }
        let _ = fs::remove_file(&self.path);
        drop(lock);
        let _ = fs::remove_file(&self.lock_path);
    }
}

impl HostSync {
    fn apply(&self, raw: &str) -> Result<()> {
        match self {
            HostSync::Keychain { service, account } => {
                let mut cmd = std::process::Command::new("security");
                cmd.args(["add-generic-password", "-U", "-s", service, "-w", raw]);
                if let Some(a) = account {
                    cmd.args(["-a", a]);
                }
                let status = cmd.status().context("failed to invoke `security`")?;
                if !status.success() {
                    bail!("security add-generic-password exited with {status}");
                }
                Ok(())
            }
            HostSync::File(path) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                write_secret_atomic(path, raw)
            }
        }
    }
}

/// Convenience: where shared credentials live for `agent-container`.
pub fn shared_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "agent-container")
        .context("failed to resolve XDG project directories")?;
    Ok(dirs.data_dir().join("shared"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_runs_on_first_open_and_skipped_when_already_populated() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("creds.json");
        let dest = dir.path().join("host.json");
        let host_sync = HostSync::File(dest.clone());

        let calls = std::cell::Cell::new(0);
        {
            let (_handle, raw) = SharedCredFile::open(shared.clone(), host_sync, || {
                calls.set(calls.get() + 1);
                Ok("payload-v1".to_string())
            })
            .unwrap();
            assert_eq!(raw, "payload-v1");
            assert_eq!(calls.get(), 1);
            // Second open while the first is alive: file already populated,
            // loader is not invoked.
            let host_sync2 = HostSync::File(dest.clone());
            let (_handle2, raw2) = SharedCredFile::open(shared.clone(), host_sync2, || {
                calls.set(calls.get() + 1);
                Ok("should-not-be-called".to_string())
            })
            .unwrap();
            assert_eq!(raw2, "payload-v1");
            assert_eq!(calls.get(), 1);
        }
    }

    #[test]
    fn last_drop_writes_back_to_host_and_unlinks_shared() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("creds.json");
        let lock = dir.path().join("creds.json.lock");
        let dest = dir.path().join("host.json");

        {
            let (_handle, _raw) =
                SharedCredFile::open(shared.clone(), HostSync::File(dest.clone()), || {
                    Ok("first".to_string())
                })
                .unwrap();
            // Simulate an in-container refresh writing a new value.
            fs::write(&shared, "refreshed").unwrap();
        }

        assert_eq!(fs::read_to_string(&dest).unwrap(), "refreshed");
        assert!(!shared.exists(), "shared file should be removed on last exit");
        assert!(!lock.exists(), "lock file should be removed on last exit");
    }

    #[test]
    fn intermediate_drop_does_not_write_back_or_unlink() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("creds.json");
        let dest = dir.path().join("host.json");

        let (handle_a, _raw) =
            SharedCredFile::open(shared.clone(), HostSync::File(dest.clone()), || {
                Ok("v1".to_string())
            })
            .unwrap();
        let (handle_b, _raw) =
            SharedCredFile::open(shared.clone(), HostSync::File(dest.clone()), || {
                panic!("loader should not be invoked when file is populated");
            })
            .unwrap();

        // Drop A while B is still alive: nothing should be written back,
        // and the shared file must stay so B keeps using it.
        drop(handle_a);
        assert!(!dest.exists(), "host file must not be touched mid-session");
        assert!(shared.exists(), "shared file must remain while B holds the lock");

        // Now drop B: this is the last container, it owns the cleanup.
        drop(handle_b);
        assert!(dest.exists(), "host file must be written on the last drop");
        assert!(!shared.exists(), "shared file must be removed on the last drop");
    }
}
