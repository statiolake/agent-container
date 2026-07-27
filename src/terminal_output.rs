use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

static CHILD_TERMINAL_DEPTH: AtomicUsize = AtomicUsize::new(0);
static CHILD_LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

pub struct ChildTerminalGuard;

impl ChildTerminalGuard {
    pub fn acquire() -> Self {
        CHILD_TERMINAL_DEPTH.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for ChildTerminalGuard {
    fn drop(&mut self) {
        CHILD_TERMINAL_DEPTH.fetch_sub(1, Ordering::SeqCst);
    }
}

pub fn line(args: fmt::Arguments<'_>) {
    if CHILD_TERMINAL_DEPTH.load(Ordering::SeqCst) == 0 {
        eprintln!("{args}");
        return;
    }

    if let Some(path) = CHILD_LOG_PATH.get()
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{args}");
    }
}

pub fn set_child_log_path(path: Option<PathBuf>) {
    if let Some(path) = path {
        let _ = CHILD_LOG_PATH.set(path);
    }
}
