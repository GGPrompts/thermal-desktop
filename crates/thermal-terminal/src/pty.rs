//! Direct PTY session management using `nix::pty::openpty()`.
//!
//! Spawns a child process in a PTY, with a dedicated reader thread that sends
//! output bytes through a tokio mpsc channel.  This module provides the
//! platform-agnostic core; platform-specific spawn helpers (e.g. proot on
//! Android, cwd-only on desktop) live in the downstream crates.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use nix::libc;
use nix::pty::openpty;
use nix::sys::signal::{self, Signal};
use nix::unistd::{self, ForkResult, Pid};
use tokio::sync::mpsc;
use tracing::{error, info};

/// A PTY session that owns a child process and provides async I/O channels.
///
/// The session spawns a child process in a new PTY, then reads its output via
/// a dedicated OS thread.  Input can be written directly to the master fd.
pub struct PtySession {
    /// The PTY master file descriptor (our end of the PTY pair).
    master_fd: OwnedFd,

    /// PID of the child process.
    child_pid: Pid,

    /// Receiver for output bytes from the reader thread.
    output_rx: mpsc::Receiver<Vec<u8>>,

    /// Set to true when the child process exits (reader thread detects EOF/EIO).
    exited: Arc<AtomicBool>,
}

#[allow(dead_code)]
impl PtySession {
    /// Spawn a new PTY session running the given shell with an optional
    /// working directory.
    ///
    /// This is a convenience wrapper around [`spawn_command`](Self::spawn_command)
    /// for the simple case of running a bare shell.
    ///
    /// # Arguments
    /// * `shell` - Path to the shell binary (e.g. "/bin/zsh")
    /// * `cwd`   - Optional working directory for the child process.
    pub fn spawn(shell: &str, cwd: Option<&str>) -> Result<Self> {
        let mut env = HashMap::new();
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());

        Self::spawn_command(shell, &[shell], cwd, env)
    }

    /// Spawn a PTY session running an arbitrary command with arguments.
    ///
    /// Opens a PTY pair, forks a child process that execs the program with the
    /// slave end as its controlling terminal, and starts a reader thread that
    /// sends output bytes through an mpsc channel.
    ///
    /// # Arguments
    /// * `program`   - Path to the executable
    /// * `args`      - argv (first element is conventionally the program name)
    /// * `cwd`       - Optional working directory for the child
    /// * `extra_env` - Additional environment variables to set in the child
    pub fn spawn_command(
        program: &str,
        args: &[&str],
        cwd: Option<&str>,
        extra_env: HashMap<String, String>,
    ) -> Result<Self> {
        // Open the PTY pair.
        let pty = openpty(None, None).context("openpty() failed")?;
        let master_fd = pty.master;
        let slave_fd = pty.slave;

        // Prepare the command and arguments for execvp.
        let program_cstr =
            CString::new(program).context("Invalid program path (contains null byte)")?;
        let args_cstr: Vec<CString> = args
            .iter()
            .map(|a| CString::new(*a).context("Invalid arg (contains null byte)"))
            .collect::<Result<Vec<_>>>()?;
        let args_refs: Vec<&std::ffi::CStr> = args_cstr.iter().map(|c| c.as_c_str()).collect();

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

                // Set environment variables for the child process.
                // SAFETY: We are in a forked child before exec -- single-threaded.
                unsafe {
                    for (k, v) in &extra_env {
                        std::env::set_var(k, v);
                    }
                }

                // Exec the command (replaces this process image).
                match unistd::execvp(&program_cstr, &args_refs) {
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

                info!(pid = child.as_raw(), program = program, "PTY child spawned");

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
                        // Receiver dropped -- session is shutting down.
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
                return Err(std::io::Error::last_os_error()).context("TIOCSWINSZ ioctl failed");
            }
        }

        info!(cols, rows, "PTY resized");
        Ok(())
    }

    /// Take the output receiver channel.
    ///
    /// This can only be called once -- subsequent calls will get a dummy
    /// receiver that never produces values.
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

    /// Returns the raw file descriptor of the PTY master.
    ///
    /// Useful for callers that need to pass it to `poll()` or similar.
    pub fn master_raw_fd(&self) -> i32 {
        self.master_fd.as_raw_fd()
    }

    /// Returns true if the child process has exited.
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Send SIGHUP to the child process (standard terminal hangup signal).
        // The child may already be dead, so ignore errors.
        let _ = signal::kill(self.child_pid, Signal::SIGHUP);

        info!(
            pid = self.child_pid.as_raw(),
            "PTY session dropped, sent SIGHUP to child"
        );
    }
}
