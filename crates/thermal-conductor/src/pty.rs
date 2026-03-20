//! Direct PTY session management using `nix::pty::openpty()`.
//!
//! Spawns a shell process in a PTY, with an async tokio reader that sends
//! output bytes through an mpsc channel. Replaces the tmux/portable_pty
//! approach with direct PTY ownership for lower latency and simpler
//! architecture.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use nix::pty::openpty;
use nix::sys::signal::{self, Signal};
use nix::unistd::{self, ForkResult, Pid};
use nix::libc;
use tokio::sync::mpsc;
use tracing::{error, info};

/// A PTY session that owns a shell process and provides async I/O channels.
///
/// The session spawns a child shell process in a new PTY, then reads its
/// output asynchronously via a tokio task. Input can be written directly
/// to the master fd.
pub struct PtySession {
    /// The PTY master file descriptor (our end of the PTY pair).
    master_fd: OwnedFd,

    /// PID of the child shell process.
    child_pid: Pid,

    /// Receiver for output bytes from the async reader task.
    output_rx: mpsc::Receiver<Vec<u8>>,

    /// Set to true when the child process exits (reader thread detects EOF/EIO).
    exited: Arc<AtomicBool>,
}

#[allow(dead_code)]
impl PtySession {
    /// Spawn a new PTY session running the given shell.
    ///
    /// Opens a PTY pair, forks a child process that execs the shell with the
    /// slave end as its controlling terminal, and starts an async reader task
    /// that sends output bytes through an mpsc channel.
    ///
    /// # Arguments
    /// * `shell` - Path to the shell binary (e.g. "/bin/zsh")
    /// * `cwd` - Optional working directory for the child process. If `None`,
    ///   the child inherits the parent's working directory.
    ///
    /// # Returns
    /// A `PtySession` with the master fd, child pid, and output channel.
    pub fn spawn(shell: &str, cwd: Option<&str>) -> Result<Self> {
        // Open the PTY pair.
        let pty = openpty(None, None).context("openpty() failed")?;
        let master_fd = pty.master;
        let slave_fd = pty.slave;

        // Prepare the shell command for execvp.
        let shell_cstr =
            CString::new(shell).context("Invalid shell path (contains null byte)")?;

        // Fork the child process.
        //
        // SAFETY: We call only async-signal-safe functions between fork and
        // exec (setsid, dup2, close, execvp). No heap allocation, no locks.
        match unsafe { unistd::fork() }.context("fork() failed")? {
            ForkResult::Child => {
                // --- Child process ---

                // Drop the master fd in the child; we only need the slave.
                drop(master_fd);

                // Create a new session so this child is the session leader.
                unistd::setsid().expect("setsid failed");

                // Set up stdin/stdout/stderr to point to the slave PTY.
                let slave_raw = slave_fd.as_raw_fd();
                unistd::dup2(slave_raw, 0).expect("dup2 stdin failed");
                unistd::dup2(slave_raw, 1).expect("dup2 stdout failed");
                unistd::dup2(slave_raw, 2).expect("dup2 stderr failed");

                // Set the slave PTY as the controlling terminal for this session.
                // TIOCSCTTY with arg 0 makes the given fd the controlling terminal.
                unsafe {
                    libc::ioctl(0, libc::TIOCSCTTY, 0);
                }

                // Close the original slave fd if it isn't one of 0/1/2.
                if slave_raw > 2 {
                    drop(slave_fd);
                }

                // Change working directory if requested.
                if let Some(dir) = cwd {
                    let dir_cstr = CString::new(dir).expect("cwd contains null byte");
                    if unistd::chdir(dir_cstr.as_c_str()).is_err() {
                        eprintln!("chdir({dir}) failed, using parent cwd");
                    }
                }

                // Set TERM so the shell knows it has color support.
                // SAFETY: We are in a forked child before exec — single-threaded.
                unsafe {
                    std::env::set_var("TERM", "xterm-256color");
                    std::env::set_var("COLORTERM", "truecolor");
                }

                // Exec the shell (replaces this process image).
                // execvp only returns on error; .expect() panics on failure.
                // On success, this process image is replaced entirely.
                match unistd::execvp(&shell_cstr, &[&shell_cstr]) {
                    Ok(infallible) => match infallible {},
                    Err(e) => {
                        eprintln!("execvp failed: {e}");
                        std::process::abort();
                    }
                }
            }
            ForkResult::Parent { child } => {
                // --- Parent process ---

                // Drop the slave fd in the parent; only the child needs it.
                drop(slave_fd);

                info!(pid = child.as_raw(), shell = shell, "PTY child spawned");

                // Set up the reader channel.
                let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>(256);

                // Clone the raw fd for the reader thread. The OwnedFd
                // stays in PtySession; we give the reader a dup'd fd.
                let reader_fd = unistd::dup(master_fd.as_raw_fd())
                    .context("Failed to dup master fd for reader")?;

                // Shared flag: set to true when the child process exits.
                let exited = Arc::new(AtomicBool::new(false));
                let exited_clone = Arc::clone(&exited);

                // Spawn a dedicated OS thread for blocking PTY reads.
                // This avoids all tokio AsyncFd complexity and reliably reads
                // from the PTY master fd using standard blocking I/O.
                std::thread::Builder::new()
                    .name("pty-reader".to_string())
                    .spawn(move || {
                        Self::reader_thread(reader_fd, output_tx);
                        exited_clone.store(true, Ordering::Release);
                    })
                    .context("Failed to spawn PTY reader thread")?;

                Ok(PtySession {
                    master_fd,
                    child_pid: child,
                    output_rx,
                    exited,
                })
            }
        }
    }

    /// Blocking reader thread that reads from the PTY master fd and sends
    /// bytes through the channel. Exits on EOF or error.
    fn reader_thread(raw_fd: i32, tx: mpsc::Sender<Vec<u8>>) {
        info!("PTY reader thread starting");

        // SAFETY: raw_fd is a valid dup'd fd from the parent.
        let mut file = unsafe { std::fs::File::from_raw_fd(raw_fd) };
        let mut buf = [0u8; 4096];

        loop {
            use std::io::Read;
            match file.read(&mut buf) {
                Ok(0) => {
                    info!("PTY reader EOF");
                    break;
                }
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        // Receiver dropped — session is shutting down.
                        break;
                    }
                }
                Err(e) => {
                    // EIO is expected when the child exits (slave side closes).
                    if e.kind() == std::io::ErrorKind::Other
                        || e.raw_os_error() == Some(libc::EIO)
                    {
                        info!("PTY reader: child exited (EIO)");
                    } else {
                        error!(error = %e, "PTY read error");
                    }
                    break;
                }
            }
        }
    }

    /// Write bytes to the PTY master (forwarded to the child's stdin).
    ///
    /// This performs a synchronous write on the master fd. For typical
    /// interactive input volumes this is fine and avoids the complexity
    /// of an async writer.
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        // Borrow the raw fd to write without consuming the OwnedFd.
        let raw = self.master_fd.as_raw_fd();
        // SAFETY: raw is a valid fd owned by self.master_fd.
        let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
        let result = file.write_all(bytes).context("PTY write failed");
        // Prevent the File from closing our fd on drop.
        std::mem::forget(file);
        result
    }

    /// Resize the PTY to the given dimensions.
    ///
    /// Sends a TIOCSWINSZ ioctl to the master fd, which delivers SIGWINCH
    /// to the child process so it can adapt to the new size.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let ws = nix::pty::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // SAFETY: TIOCSWINSZ is a well-defined ioctl for terminal resize.
        unsafe {
            let ret = libc::ioctl(self.master_fd.as_raw_fd(), libc::TIOCSWINSZ, &ws);
            if ret == -1 {
                return Err(std::io::Error::last_os_error())
                    .context("TIOCSWINSZ ioctl failed");
            }
        }

        info!(cols, rows, "PTY resized");
        Ok(())
    }

    /// Take the output receiver channel.
    ///
    /// This can only be called once — subsequent calls will get `None`.
    /// The receiver yields `Vec<u8>` chunks of terminal output. When the
    /// channel closes, the child process has exited.
    pub fn take_output(&mut self) -> mpsc::Receiver<Vec<u8>> {
        // Swap out the receiver with a dummy one.
        let (_, dummy_rx) = mpsc::channel(1);
        std::mem::replace(&mut self.output_rx, dummy_rx)
    }

    /// Get the child process PID.
    pub fn child_pid(&self) -> Pid {
        self.child_pid
    }

    /// Returns true if the child shell process has exited.
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Send SIGHUP to the child process (standard terminal hangup signal).
        // The child may already be dead, so ignore errors.
        let _ = signal::kill(self.child_pid, Signal::SIGHUP);

        info!(pid = self.child_pid.as_raw(), "PTY session dropped, sent SIGHUP to child");
    }
}
