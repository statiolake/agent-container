//! Shared credential file with last-out write-back.
//!
//! Several `agent-container` invocations on the same host need to share
//! a single credential file so that an OAuth refresh inside one
//! container is visible to the others and written back to the host while
//! the session is still alive.
//! A sidecar lock file, not the credential file's existence, is the
//! source of truth: if a new invocation can take `LOCK_EX | LOCK_NB`,
//! there are no live siblings and any leftover shared credential is
//! stale, so it is recreated from the host. The invocation then holds
//! `LOCK_SH` for its lifetime. On exit it tries to upgrade back to
//! `LOCK_EX | LOCK_NB`; if the upgrade succeeds, no sibling is still
//! alive, so the process owns the cleanup pass — read the possibly
//! refreshed shared file, write it back one last time, then unlink the
//! shared copy. During the session, each holder also watches the shared
//! file and writes stable changes back to the host, so host-side clients
//! do not have to wait for every container to exit before seeing a token
//! refresh.
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
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use rustix::fs::{FlockOperation, flock};

/// Where to write the credential bytes back to the host when the last
/// container exits.
#[derive(Clone)]
pub enum HostSync {
    /// macOS: update a `generic-password` Keychain item via `security
    /// add-generic-password -U`.
    #[cfg(target_os = "macos")]
    Keychain {
        service: String,
        account: Option<String>,
    },
    /// macOS Cursor Agent: update the individual Keychain items that
    /// Cursor's credential manager uses, from the Linux-side auth.json.
    #[cfg(target_os = "macos")]
    CursorKeychain {
        account: String,
        access_token_service: String,
        refresh_token_service: String,
        api_key_service: String,
    },
    /// Linux: atomically replace the host file.
    #[cfg(any(not(target_os = "macos"), test))]
    File(PathBuf),
}

pub struct SharedCredFile {
    pub path: PathBuf,
    lock_path: PathBuf,
    /// Held for the lifetime of this agent-container process. Closing
    /// it releases the OS-level shared lock — see `Drop`.
    lock_file: Option<File>,
    host_sync: HostSync,
    sync_stop: Arc<AtomicBool>,
    sync_thread: Option<JoinHandle<()>>,
}

impl SharedCredFile {
    /// Open the shared credential file and take a shared lock on its
    /// sidecar lock file. If no live sibling holds the lock, the shared
    /// file is recreated from `loader` even when an old copy exists.
    /// Returns the handle plus the raw credential bytes, so the caller
    /// can parse fields like `expires_at`.
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

        let lock_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open lock at {}", lock_path.display()))?;

        let owns_fresh_session =
            flock(&lock_file, FlockOperation::NonBlockingLockExclusive).is_ok();

        let raw = if owns_fresh_session {
            let raw = loader()?;
            write_secret_atomic(&shared_path, raw.trim())?;
            raw
        } else {
            flock(&lock_file, FlockOperation::LockShared).with_context(|| {
                format!("failed to take shared lock on {}", lock_path.display())
            })?;
            fs::read_to_string(&shared_path).with_context(|| {
                format!(
                    "failed to read shared credentials at {}",
                    shared_path.display()
                )
            })?
        };

        if owns_fresh_session {
            flock(&lock_file, FlockOperation::LockShared).with_context(|| {
                format!(
                    "failed to downgrade lock to shared at {}",
                    lock_path.display()
                )
            })?;
        }

        let sync_stop = Arc::new(AtomicBool::new(false));
        let sync_thread = Some(spawn_change_sync_thread(
            shared_path.clone(),
            host_sync.clone(),
            raw.trim().to_string(),
            Arc::clone(&sync_stop),
        ));

        Ok((
            Self {
                path: shared_path,
                lock_path,
                lock_file: Some(lock_file),
                host_sync,
                sync_stop,
                sync_thread,
            },
            raw,
        ))
    }
}

fn lock_path_for(p: &Path) -> PathBuf {
    let mut name = p.file_name().map(|s| s.to_os_string()).unwrap_or_default();
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

fn spawn_change_sync_thread(
    path: PathBuf,
    host_sync: HostSync,
    initial_raw: String,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut last_synced = initial_raw;
        let mut pending: Option<String> = None;
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(sync_poll_interval());
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let raw = raw.trim().to_string();
            if raw == last_synced {
                pending = None;
                continue;
            }
            if pending.as_deref() != Some(raw.as_str()) {
                pending = Some(raw);
                continue;
            }
            let Some(raw) = pending.take() else {
                continue;
            };
            match host_sync.apply(&raw) {
                Ok(()) => last_synced = raw,
                Err(e) => {
                    tracing::warn!(
                        %e,
                        "failed to write refreshed credentials back to host",
                    );
                }
            }
        }
    })
}

#[cfg(test)]
fn sync_poll_interval() -> Duration {
    Duration::from_millis(20)
}

#[cfg(not(test))]
fn sync_poll_interval() -> Duration {
    Duration::from_secs(1)
}

impl Drop for SharedCredFile {
    fn drop(&mut self) {
        self.sync_stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.sync_thread.take() {
            let _ = thread.join();
        }

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

        if let Ok(raw) = fs::read_to_string(&self.path)
            && let Err(e) = self.host_sync.apply(raw.trim())
        {
            tracing::warn!(
                %e,
                "failed to write credentials back to host; discarding shared copy",
            );
        }
        let _ = fs::remove_file(&self.path);
        drop(lock);
        let _ = fs::remove_file(&self.lock_path);
    }
}

impl HostSync {
    fn apply(&self, raw: &str) -> Result<()> {
        match self {
            #[cfg(target_os = "macos")]
            HostSync::Keychain { service, account } => {
                crate::keychain::write_generic_password(service, account.as_deref(), raw)
            }
            #[cfg(target_os = "macos")]
            HostSync::CursorKeychain {
                account,
                access_token_service,
                refresh_token_service,
                api_key_service,
            } => crate::cursor::write_keychain_auth(
                account,
                access_token_service,
                refresh_token_service,
                api_key_service,
                raw,
            ),
            #[cfg(any(not(target_os = "macos"), test))]
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
    use std::process::{Command, Stdio};
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use std::time::{Duration, Instant};

    static SHARED_CRED_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn shared_cred_test_guard() -> MutexGuard<'static, ()> {
        SHARED_CRED_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn loader_runs_on_first_open_and_skipped_when_already_populated() {
        let _guard = shared_cred_test_guard();
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
    fn stale_shared_file_is_recreated_when_lock_is_free() {
        let _guard = shared_cred_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("creds.json");
        let dest = dir.path().join("host.json");
        fs::write(&shared, "stale").unwrap();

        let (_handle, raw) = SharedCredFile::open(shared.clone(), HostSync::File(dest), || {
            Ok("fresh".to_string())
        })
        .unwrap();

        assert_eq!(raw, "fresh");
        assert_eq!(fs::read_to_string(&shared).unwrap(), "fresh");
    }

    #[test]
    fn last_drop_writes_back_to_host_and_unlinks_shared() {
        let _guard = shared_cred_test_guard();
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
        assert!(
            !shared.exists(),
            "shared file should be removed on last exit"
        );
        assert!(!lock.exists(), "lock file should be removed on last exit");
    }

    #[test]
    fn changed_shared_file_is_written_back_before_last_drop() {
        let _guard = shared_cred_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("creds.json");
        let dest = dir.path().join("host.json");

        let (_handle, _raw) =
            SharedCredFile::open(shared.clone(), HostSync::File(dest.clone()), || {
                Ok("first".to_string())
            })
            .unwrap();
        fs::write(&shared, "refreshed").unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if fs::read_to_string(&dest).ok().as_deref() == Some("refreshed") {
                assert!(
                    shared.exists(),
                    "shared file must remain while the container is alive"
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for refreshed credentials to be written back");
    }

    #[test]
    fn intermediate_drop_does_not_write_back_or_unlink() {
        let _guard = shared_cred_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("creds.json");
        let dest = dir.path().join("host.json");
        let ready = dir.path().join("child.ready");
        let release = dir.path().join("child.release");

        let (handle_a, _raw) =
            SharedCredFile::open(shared.clone(), HostSync::File(dest.clone()), || {
                Ok("v1".to_string())
            })
            .unwrap();
        let mut child = spawn_shared_cred_child(&shared, &dest, &ready, &release);
        wait_for_path(&ready);

        // Drop A while a separate sibling process is still alive: nothing should be written back,
        // and the shared file must stay so B keeps using it.
        drop(handle_a);
        assert!(!dest.exists(), "host file must not be touched mid-session");
        assert!(
            shared.exists(),
            "shared file must remain while the sibling holds the lock"
        );

        // Now let the sibling exit: this is the last container, it owns the cleanup.
        fs::write(&release, "release").unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "child helper failed with {status}");
        assert!(dest.exists(), "host file must be written on the last drop");
        assert!(
            !shared.exists(),
            "shared file must be removed on the last drop"
        );
    }

    #[test]
    #[ignore]
    fn shared_cred_child_helper() {
        if std::env::var_os("AGENT_CONTAINER_SHARED_CRED_HELPER").is_none() {
            return;
        }
        let shared = PathBuf::from(std::env::var_os("AGENT_CONTAINER_SHARED").unwrap());
        let dest = PathBuf::from(std::env::var_os("AGENT_CONTAINER_SHARED_DEST").unwrap());
        let ready = PathBuf::from(std::env::var_os("AGENT_CONTAINER_SHARED_READY").unwrap());
        let release = PathBuf::from(std::env::var_os("AGENT_CONTAINER_SHARED_RELEASE").unwrap());

        let (_handle, raw) = SharedCredFile::open(shared, HostSync::File(dest), || {
            Ok("child-loader-should-not-run".to_string())
        })
        .unwrap();
        assert_eq!(raw, "v1");
        fs::write(&ready, "ready").unwrap();
        wait_for_path(&release);
    }

    fn spawn_shared_cred_child(
        shared: &Path,
        dest: &Path,
        ready: &Path,
        release: &Path,
    ) -> std::process::Child {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "shared_cred::tests::shared_cred_child_helper",
                "--ignored",
            ])
            .env("AGENT_CONTAINER_SHARED_CRED_HELPER", "1")
            .env("AGENT_CONTAINER_SHARED", shared)
            .env("AGENT_CONTAINER_SHARED_DEST", dest)
            .env("AGENT_CONTAINER_SHARED_READY", ready)
            .env("AGENT_CONTAINER_SHARED_RELEASE", release)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn wait_for_path(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if path.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {}", path.display());
    }
}
