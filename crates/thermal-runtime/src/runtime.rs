//! Centralized runtime path logic and stale-artifact cleanup for thermal daemons.
//!
//! All thermal daemons use Unix sockets and pidfiles under
//! `$XDG_RUNTIME_DIR/thermal/` (falling back to `/run/user/<uid>/thermal/`).
//! This module provides a single canonical set of helpers so every daemon
//! constructs paths the same way, validates pidfiles the same way, and cleans
//! up stale sockets the same way.

use std::fs;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

// ── Runtime directory ───────────────────────────────────────────────────────

/// Return the thermal runtime directory.
///
/// Prefers `$XDG_RUNTIME_DIR/thermal`, falling back to
/// `/run/user/<uid>/thermal` when the env var is unset.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join("thermal")
    } else {
        let uid = nix::unistd::getuid().as_raw();
        PathBuf::from(format!("/run/user/{uid}/thermal"))
    }
}

/// Ensure the runtime directory exists, creating it if needed.
pub fn ensure_runtime_dir() -> std::io::Result<()> {
    fs::create_dir_all(runtime_dir())
}

/// Return the full path for a named socket (e.g. `"conductor"` -> `.../conductor.sock`).
pub fn socket_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.sock"))
}

/// Return the full path for a named pidfile (e.g. `"dispatcher"` -> `.../dispatcher.pid`).
pub fn pidfile_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.pid"))
}

// ── Stale socket cleanup ────────────────────────────────────────────────────

/// Check whether a Unix socket has a live listener.
///
/// Attempts a non-blocking `connect()` on the socket path. Returns:
/// - `true`  — a daemon is listening (connection succeeded or was not refused)
/// - `false` — the socket is stale (connection refused / not a socket)
fn socket_has_listener(path: &Path) -> bool {
    use std::os::unix::net::UnixStream;
    match UnixStream::connect(path) {
        Ok(_) => true,
        Err(e) => {
            // ConnectionRefused means nobody is listening.
            // NotFound means the file vanished between our exists-check and connect.
            // Other errors (permission, etc.) we treat as "not stale" to be safe.
            !matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            )
        }
    }
}

/// If `path` points to a stale socket (file exists but no listener), remove it
/// and log clearly. Returns `true` if a stale socket was removed.
///
/// If the socket has a live listener, logs a warning and returns `false`.
/// If the socket file does not exist, returns `false` (nothing to do).
pub fn cleanup_stale_socket(daemon_name: &str, path: &Path) -> bool {
    if !path.exists() {
        return false;
    }

    if socket_has_listener(path) {
        warn!(
            daemon = daemon_name,
            path = %path.display(),
            "Socket exists and has a live listener — another instance may be running"
        );
        return false;
    }

    // Socket is stale — remove it.
    match fs::remove_file(path) {
        Ok(()) => {
            info!(
                daemon = daemon_name,
                path = %path.display(),
                "Removed stale socket (no listener)"
            );
            true
        }
        Err(e) => {
            warn!(
                daemon = daemon_name,
                path = %path.display(),
                error = %e,
                "Failed to remove stale socket"
            );
            false
        }
    }
}

// ── Pidfile helpers ─────────────────────────────────────────────────────────

/// Read a pidfile and return the PID if the process is still alive.
///
/// Returns `Some(pid)` if the pidfile exists and `/proc/<pid>` exists.
/// Returns `None` if the pidfile is missing, unparseable, or the PID is dead.
/// Stale pidfiles are removed with a log message.
pub fn validate_pidfile(daemon_name: &str, path: &Path) -> Option<u32> {
    if !path.exists() {
        return None;
    }

    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                daemon = daemon_name,
                path = %path.display(),
                error = %e,
                "Failed to read pidfile — removing"
            );
            let _ = fs::remove_file(path);
            return None;
        }
    };

    let pid: u32 = match contents.trim().parse() {
        Ok(p) => p,
        Err(_) => {
            warn!(
                daemon = daemon_name,
                path = %path.display(),
                contents = contents.trim(),
                "Pidfile contains invalid PID — removing"
            );
            let _ = fs::remove_file(path);
            return None;
        }
    };

    if Path::new(&format!("/proc/{pid}")).exists() {
        Some(pid)
    } else {
        info!(
            daemon = daemon_name,
            pid,
            path = %path.display(),
            "Stale pidfile (process not running) — removing"
        );
        let _ = fs::remove_file(path);
        None
    }
}

/// Write the current process's PID to the given pidfile path.
///
/// Creates parent directories if needed.
pub fn write_pidfile(daemon_name: &str, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let pid = std::process::id();
    fs::write(path, pid.to_string())?;
    info!(
        daemon = daemon_name,
        pid,
        path = %path.display(),
        "Wrote pidfile"
    );
    Ok(())
}

/// Remove a pidfile if it exists.
pub fn remove_pidfile(daemon_name: &str, path: &Path) {
    if path.exists() {
        let _ = fs::remove_file(path);
        info!(
            daemon = daemon_name,
            path = %path.display(),
            "Removed pidfile"
        );
    }
}

/// Single-instance guard: check pidfile, exit if another instance is alive.
///
/// If the pidfile points to a running process, prints a message to stderr
/// and calls `std::process::exit(0)`. Otherwise removes the stale pidfile.
pub fn enforce_single_instance(daemon_name: &str) {
    let path = pidfile_path(daemon_name);
    enforce_single_instance_at(daemon_name, &path);
}

/// Single-instance guard using an explicit pidfile path.
///
/// This is useful for binaries that want user-facing names like
/// `"thermal-bar"` while storing pidfiles under short runtime names like
/// `"bar.pid"`.
pub fn enforce_single_instance_at(display_name: &str, path: &Path) {
    if let Some(pid) = validate_pidfile(display_name, path) {
        eprintln!("{display_name} already running (pid {pid}). Exiting.");
        std::process::exit(0);
    }
}

// ── flock-based single-instance guard ─────────────────────────────────────

/// Return the path for a daemon's lockfile (e.g. `"conductor"` -> `.../conductor.lock`).
pub fn lockfile_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.lock"))
}

/// Atomic single-instance guard using `flock()`.
///
/// Acquires an exclusive non-blocking lock on a lockfile in the runtime
/// directory. Unlike pidfile-based guards, `flock()` is atomic (no TOCTOU
/// race) and the kernel automatically releases the lock when the process
/// exits (even on SIGKILL / crash).
///
/// Returns the held `File` handle — the lock is released when this handle
/// is dropped. Callers **must** keep the returned value alive for the
/// lifetime of the daemon.
///
/// If another instance already holds the lock, prints a message to stderr
/// and calls `std::process::exit(0)`.
pub fn acquire_instance_lock(daemon_name: &str) -> fs::File {
    let path = lockfile_path(daemon_name);
    acquire_instance_lock_at(daemon_name, &path)
}

/// Atomic single-instance guard using an explicit lockfile path.
///
/// Uses `Flock::lock` from nix. The returned `File` keeps the flock held
/// (via `into_raw_fd` -> `from_raw_fd` to avoid `Flock<T>` Drop unlocking).
pub fn acquire_instance_lock_at(display_name: &str, path: &Path) -> fs::File {
    use nix::fcntl::{Flock, FlockArg};

    // Ensure the runtime directory exists.
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .truncate(false)
        .open(path)
        .unwrap_or_else(|e| {
            eprintln!("{display_name}: failed to open lockfile {}: {e}", path.display());
            std::process::exit(1);
        });

    // Try non-blocking exclusive lock.
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(mut locked) => {
            // Write our PID into the lockfile for diagnostics (not used for guard logic).
            use std::io::{Seek, Write};
            let _ = locked.set_len(0);
            let _ = locked.seek(std::io::SeekFrom::Start(0));
            let _ = write!(locked, "{}", std::process::id());
            info!(
                daemon = display_name,
                path = %path.display(),
                "Acquired instance lock"
            );
            // Extract the raw fd, then forget the Flock wrapper to prevent
            // its Drop from calling LOCK_UN. Reconstruct a plain File that
            // keeps the fd (and therefore the flock) open. The lock is
            // released when the File is dropped or the process exits.
            use std::os::fd::{AsRawFd, FromRawFd};
            let fd = locked.as_raw_fd();
            std::mem::forget(locked);
            // SAFETY: fd is a valid open file descriptor we own. We skipped
            // the Flock destructor, so the flock is still held.
            unsafe { fs::File::from_raw_fd(fd) }
        }
        Err((_file, _errno)) => {
            // Read the PID from the lockfile for a better error message.
            let holder_pid = fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok());
            if let Some(pid) = holder_pid {
                eprintln!("{display_name} already running (pid {pid}, locked). Exiting.");
            } else {
                eprintln!("{display_name} already running (lockfile held). Exiting.");
            }
            std::process::exit(0);
        }
    }
}

// ── Client-side stale socket detection ──────────────────────────────────────

/// Try to connect to a daemon socket. If the socket file exists but the daemon
/// is not responding, clean up the stale socket and return a clear error.
///
/// Returns:
/// - `Ok(stream)` on successful connection
/// - `Err` with a descriptive message (stale socket cleaned up, or not found)
pub fn try_connect_or_cleanup(
    daemon_name: &str,
    path: &Path,
) -> Result<std::os::unix::net::UnixStream, String> {
    use std::os::unix::net::UnixStream;

    if !path.exists() {
        return Err(format!(
            "{daemon_name} socket not found at {} — is the daemon running?",
            path.display()
        ));
    }

    match UnixStream::connect(path) {
        Ok(stream) => Ok(stream),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            // Stale socket — clean up.
            let _ = fs::remove_file(path);
            Err(format!(
                "{daemon_name} socket exists at {} but daemon is not responding — removed stale socket",
                path.display()
            ))
        }
        Err(e) => Err(format!(
            "Failed to connect to {daemon_name} at {}: {e}",
            path.display()
        )),
    }
}

/// Try to connect to a daemon socket without mutating the filesystem.
///
/// Unlike [`try_connect_or_cleanup`], this helper never removes stale sockets.
/// It is suitable for read-only diagnostics such as `thc doctor`.
pub fn try_connect_read_only(
    daemon_name: &str,
    path: &Path,
) -> Result<std::os::unix::net::UnixStream, String> {
    use std::os::unix::net::UnixStream;

    if !path.exists() {
        return Err(format!(
            "{daemon_name} socket not found at {} — is the daemon running?",
            path.display()
        ));
    }

    UnixStream::connect(path).map_err(|e| {
        format!(
            "Failed to connect to {daemon_name} at {}: {e}",
            path.display()
        )
    })
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dir_ends_with_thermal() {
        let dir = runtime_dir();
        assert!(
            dir.to_str().unwrap().ends_with("/thermal"),
            "runtime dir should end with /thermal, got {:?}",
            dir
        );
    }

    #[test]
    fn socket_path_format() {
        let p = socket_path("conductor");
        assert!(p.to_str().unwrap().ends_with("/thermal/conductor.sock"));
    }

    #[test]
    fn pidfile_path_format() {
        let p = pidfile_path("dispatcher");
        assert!(p.to_str().unwrap().ends_with("/thermal/dispatcher.pid"));
    }

    #[test]
    fn validate_pidfile_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.pid");
        assert!(validate_pidfile("test", &path).is_none());
    }

    #[test]
    fn validate_pidfile_invalid_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.pid");
        fs::write(&path, "not-a-number").unwrap();
        assert!(validate_pidfile("test", &path).is_none());
        assert!(!path.exists(), "stale pidfile should be removed");
    }

    #[test]
    fn validate_pidfile_dead_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dead.pid");
        // PID 999999999 should not exist.
        fs::write(&path, "999999999").unwrap();
        assert!(validate_pidfile("test", &path).is_none());
        assert!(!path.exists(), "stale pidfile should be removed");
    }

    #[test]
    fn validate_pidfile_live_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.pid");
        // PID 1 (init) should always exist.
        fs::write(&path, "1").unwrap();
        assert_eq!(validate_pidfile("test", &path), Some(1));
        assert!(path.exists(), "live pidfile should not be removed");
    }

    #[test]
    fn write_and_remove_pidfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        write_pidfile("test", &path).unwrap();
        assert!(path.exists());
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, std::process::id().to_string());
        remove_pidfile("test", &path);
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_stale_socket_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.sock");
        assert!(!cleanup_stale_socket("test", &path));
    }

    #[test]
    fn cleanup_stale_socket_dead_listener() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.sock");
        // Bind and immediately drop to leave a stale socket file.
        {
            let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        }
        assert!(path.exists());
        assert!(cleanup_stale_socket("test", &path));
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_stale_socket_live_listener() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(!cleanup_stale_socket("test", &path));
        assert!(path.exists(), "live socket should not be removed");
    }

    #[test]
    fn try_connect_or_cleanup_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.sock");
        let result = try_connect_or_cleanup("test", &path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("is the daemon running?"));
    }

    #[test]
    fn try_connect_or_cleanup_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.sock");
        {
            let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        }
        let result = try_connect_or_cleanup("test", &path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("removed stale socket"));
        assert!(!path.exists());
    }

    #[test]
    fn try_connect_or_cleanup_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let result = try_connect_or_cleanup("test", &path);
        assert!(result.is_ok());
    }

    #[test]
    fn try_connect_read_only_stale_preserves_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.sock");
        {
            let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        }
        let result = try_connect_read_only("test", &path);
        assert!(result.is_err());
        assert!(
            path.exists(),
            "read-only probe should not remove stale socket"
        );
    }
}
