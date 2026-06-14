//! Multi-process write guard for `Store::open`.
//!
//! mem is single-writer by design — CLAUDE.md "Architecture" §1 states
//! "Two HTTP services pointed at the same DB will fight; DuckDB is
//! single-writer." Until this module landed, the documented contract
//! had no runtime enforcement: a second `mem serve` against the same
//! `MEM_DB_PATH` would open fine and silently race on writes, corrupting
//! the lance manifest chain.
//!
//! Closes ROADMAP incident TODO #3.
//!
//! ## Mechanism
//!
//! On `Store::open(path)` we:
//!
//! 1. Compute a sibling sentinel path `<path>.lock` (avoiding the lance
//!    dataset directory itself, so the file doesn't interfere with
//!    lance's manifest enumeration).
//! 2. Open the sentinel with `create=true`, write `<pid>\n<iso-ts>\n<host>`
//!    for diagnostics, then `try_lock_exclusive()` via `fs4`. The lock is
//!    OS-level advisory (Unix `flock`, Windows `LockFileEx`) — it
//!    releases automatically when the file handle drops, so a crash
//!    leaves no stale lock.
//! 3. On success, return an [`OpenLock`] guard. `Store` holds it for
//!    its full lifetime; drop releases the lock.
//! 4. On `WouldBlock`, return `StorageError::InvalidInput` with the
//!    held PID parsed from the file (best-effort; the message is
//!    diagnostic, not authoritative).
//!
//! ## Opt-out
//!
//! `MEM_OPEN_LOCK_DISABLED=1` (`true`/`yes` also accepted) skips the
//! acquire entirely — escape hatch for environments where the lock is
//! genuinely wrong (network filesystem that doesn't honor flock, lock
//! file corrupted on disk, etc). Use sparingly; the warning the env
//! var bypasses is there for a reason.
//!
//! ## What this does NOT guard against
//!
//! - Concurrent reads from a second process (those are safe; the lock
//!   is only acquired by `Store::open`, and DuckDB read-only connections
//!   from outside our process don't go through our open path).
//! - In-process double-open (one process calling `Store::open` twice
//!   on the same path) — the second call gets the same `WouldBlock`
//!   error, which is the right answer.
//! - Filesystem corruption from non-`mem` processes deleting the lock.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs4::FileExt;

use crate::storage::types::StorageError;

/// RAII guard for the open-time advisory lock. The held `File` keeps
/// the OS lock alive; dropping it releases. `path` is stashed for
/// diagnostics (logging on release / debug printing).
#[derive(Debug)]
pub struct OpenLock {
    /// Held file handle — the lock lives on this descriptor. Never
    /// read after acquisition; existence is the lock.
    _file: File,
    /// Sentinel path, kept for `Drop` diagnostics + tests.
    path: PathBuf,
}

impl OpenLock {
    /// Path to the sentinel file (mostly for tests + diagnostics).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for OpenLock {
    fn drop(&mut self) {
        // Best-effort: remove the sentinel file so a fresh process
        // sees an empty lock state. The OS-level flock would have
        // released regardless on file-handle drop (which happens
        // implicitly in `_file`'s drop right after this method),
        // so unlink failure here is non-fatal — just leaves a stale
        // sentinel that the next open will overwrite.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Read the env-var opt-out (`MEM_OPEN_LOCK_DISABLED=1`). Truthy
/// values bypass the lock acquisition entirely; default unset → lock
/// is acquired.
pub fn lock_disabled() -> bool {
    matches!(
        std::env::var("MEM_OPEN_LOCK_DISABLED")
            .ok()
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Acquire the open-time advisory lock for `db_path`. Returns the
/// guard on success, or `Err(StorageError::InvalidInput("..."))` if
/// the lock is held by another process — the message contains the
/// held PID parsed from the sentinel (best-effort).
///
/// When `MEM_OPEN_LOCK_DISABLED=1` is set, returns `Ok(None)` — the
/// caller still needs to thread an `Option<OpenLock>` field, but the
/// lock acquisition itself is skipped.
pub fn acquire(db_path: &Path) -> Result<Option<OpenLock>, StorageError> {
    if lock_disabled() {
        return Ok(None);
    }
    let sentinel = sentinel_path(db_path);

    // Ensure the parent dir exists — the sentinel is a sibling of the
    // DB, so the parent is the same one `Store::open` is about to
    // create the lance dir under. Safe to create proactively.
    if let Some(parent) = sentinel.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(StorageError::Io)?;
        }
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&sentinel)
        .map_err(StorageError::Io)?;

    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            // We own the lock — overwrite the sentinel body with our
            // PID + timestamp for diagnostics. Truncate first so a
            // stale body from a crashed process doesn't linger.
            file.set_len(0).map_err(StorageError::Io)?;
            let body = format!(
                "{pid}\n{ts}\n{host}\n",
                pid = std::process::id(),
                ts = crate::storage::current_timestamp(),
                host = std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into()),
            );
            file.write_all(body.as_bytes()).map_err(StorageError::Io)?;
            Ok(Some(OpenLock {
                _file: file,
                path: sentinel,
            }))
        }
        Err(_) => {
            // Lock held by another process. Try to parse the PID out
            // of the sentinel for a more useful error message; the
            // body is best-effort because the lock holder may be
            // mid-write to the sentinel when we read.
            let held_pid = read_held_pid(&sentinel);
            Err(StorageError::InvalidInput(format!(
                "mem DB at {} is already open by another process{}. \
                 Refusing to open — mem is single-writer (CLAUDE.md \
                 architecture §1). Stop the other process, or set \
                 MEM_OPEN_LOCK_DISABLED=1 to bypass (read the module \
                 doc on `storage::open_lock` first).",
                db_path.display(),
                match held_pid {
                    Some(pid) => format!(" (PID {pid})"),
                    None => String::new(),
                }
            )))
        }
    }
}

fn sentinel_path(db_path: &Path) -> PathBuf {
    // `<db_path>.lock` as a sibling. If db_path's parent is "" (bare
    // filename), the sentinel lands in the cwd, which is fine.
    let mut p = db_path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

fn read_held_pid(sentinel: &Path) -> Option<u32> {
    let body = std::fs::read_to_string(sentinel).ok()?;
    body.lines().next()?.trim().parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// `MEM_OPEN_LOCK_DISABLED` is process-global, but `cargo test` runs
    /// every unit test in ONE multi-threaded process. The tests below
    /// either SET that var or call `acquire()` (which READS it via
    /// `lock_disabled()`), so without serialization a setter's window
    /// races a reader's check — e.g. a concurrent `set_var("…","1")`
    /// makes a reader's `acquire()` return `None` ("disabled") when it
    /// expected a real guard, and a concurrent `remove_var` makes a
    /// setter's `assert!(lock_disabled())` see an unset var. This was a
    /// ~60% CI flake (lower locally only because more cores finish the
    /// tiny tests before they overlap). The earlier "can't collide with
    /// other modules' env-var tests" comment mis-scoped the hazard: the
    /// collision is INTRA-module, between this module's own setters and
    /// readers. Serialize them all behind one mutex.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Hold the env mutex for a test's lifetime AND guarantee
    /// `MEM_OPEN_LOCK_DISABLED` is cleared on entry and on drop — so a
    /// setter that panics mid-assertion can't leak state into the next
    /// serialized test. Tolerates a poisoned mutex (a prior panic) by
    /// recovering the guard rather than cascading the failure.
    struct EnvGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl EnvGuard {
        fn acquire() -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::remove_var("MEM_OPEN_LOCK_DISABLED");
            EnvGuard(guard)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var("MEM_OPEN_LOCK_DISABLED");
        }
    }

    #[test]
    fn lock_disabled_recognizes_truthy_values() {
        let _env = EnvGuard::acquire();
        for v in ["1", "true", "TRUE", "yes", "YES"] {
            std::env::set_var("MEM_OPEN_LOCK_DISABLED", v);
            assert!(lock_disabled(), "value {v} should disable lock");
        }
        std::env::remove_var("MEM_OPEN_LOCK_DISABLED");
        assert!(!lock_disabled());
    }

    #[test]
    fn acquire_then_release_then_reacquire() {
        let _env = EnvGuard::acquire();
        let dir = tempdir().unwrap();
        let db = dir.path().join("mem.lance");
        let g1 = acquire(&db).expect("first acquire").expect("not disabled");
        assert!(g1.path().exists(), "sentinel file should be created");
        drop(g1);
        // After drop, the lock is released — next acquire succeeds.
        let g2 = acquire(&db).expect("second acquire").expect("not disabled");
        drop(g2);
    }

    #[test]
    fn second_acquire_while_held_returns_error() {
        let _env = EnvGuard::acquire();
        let dir = tempdir().unwrap();
        let db = dir.path().join("mem.lance");
        let _g = acquire(&db).expect("first acquire").expect("not disabled");
        let err = acquire(&db).expect_err("second acquire must fail while first held");
        match err {
            StorageError::InvalidInput(msg) => {
                assert!(
                    msg.contains("single-writer"),
                    "error message should mention single-writer policy: {msg}",
                );
                assert!(
                    msg.contains("MEM_OPEN_LOCK_DISABLED"),
                    "error should mention escape-hatch env var: {msg}",
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn sentinel_body_carries_pid_for_diagnostics() {
        let _env = EnvGuard::acquire();
        let dir = tempdir().unwrap();
        let db = dir.path().join("mem.lance");
        let g = acquire(&db).expect("acquire").expect("not disabled");
        let body = std::fs::read_to_string(g.path()).unwrap();
        let first_line = body.lines().next().unwrap();
        let parsed: u32 = first_line.parse().expect("first line must be the PID");
        assert_eq!(parsed, std::process::id());
    }

    #[test]
    fn disabled_env_returns_none() {
        let _env = EnvGuard::acquire();
        let dir = tempdir().unwrap();
        let db = dir.path().join("mem.lance");
        std::env::set_var("MEM_OPEN_LOCK_DISABLED", "1");
        let result = acquire(&db).expect("disabled acquire should succeed");
        assert!(result.is_none(), "disabled lock returns None guard");
        std::env::remove_var("MEM_OPEN_LOCK_DISABLED");
        // Sentinel was never created.
        assert!(!sentinel_path(&db).exists());
    }
}
