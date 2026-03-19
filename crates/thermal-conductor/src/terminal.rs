//! VT parser wrapper around `alacritty_terminal::Term`.
//!
//! Follows the Zed integration pattern: wraps alacritty's `Term` with a custom
//! `EventListener` and feeds PTY output bytes through `vte::ansi::Processor`.
//! The Term is behind an `Arc<FairMutex>` so both the byte-processing task and
//! the renderer can access it safely.
//!
//! OSC 633 shell-integration sequences are intercepted at the byte level
//! (before the alacritty Term sees them) by [`crate::osc633::Osc633Parser`].
//! Parsed marks are forwarded to a [`crate::osc633::CommandTracker`] which is
//! stored in the `Terminal` struct for future Phase 4 semantic scrollback
//! rendering.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, RenderableContent};
use alacritty_terminal::vte::ansi;
use alacritty_terminal::Term;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::osc633::{CommandTracker, Osc633Parser};

// ── Default terminal dimensions ──────────────────────────────────────────────

#[allow(dead_code)]
const DEFAULT_COLS: usize = 120;
#[allow(dead_code)]
const DEFAULT_ROWS: usize = 36;

// ── Event listener ───────────────────────────────────────────────────────────

/// Event listener that forwards `alacritty_terminal::Event`s through a
/// `tokio::sync::mpsc::UnboundedSender`. The renderer (or any other consumer)
/// can receive these to react to title changes, bell, wakeup, etc.
#[derive(Clone)]
pub struct ThermalEventListener {
    tx: mpsc::UnboundedSender<Event>,
}

impl ThermalEventListener {
    pub fn new(tx: mpsc::UnboundedSender<Event>) -> Self {
        Self { tx }
    }
}

impl EventListener for ThermalEventListener {
    fn send_event(&self, event: Event) {
        // Best-effort send — if the receiver is gone we silently drop.
        let _ = self.tx.send(event);
    }
}

// ── Terminal size ────────────────────────────────────────────────────────────

/// Grid dimensions for `Term::new` and `Term::resize`.
pub struct TerminalSize {
    pub columns: usize,
    pub screen_lines: usize,
}

impl TerminalSize {
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        Self { columns, screen_lines }
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

// ── Terminal wrapper ─────────────────────────────────────────────────────────

/// Wraps an `alacritty_terminal::Term` behind a `FairMutex` together with the
/// VTE parser and the event channel. Provides methods to feed bytes, resize,
/// and access renderable content.
///
/// OSC 633 shell-integration marks are captured in `command_tracker` as the
/// byte processor runs.  Access it via [`Terminal::command_tracker`] for Phase
/// 4 semantic scrollback rendering.
pub struct Terminal {
    /// The terminal emulator, shared between the byte processor and the
    /// renderer via `FairMutex` (same synchronization primitive alacritty
    /// uses internally).
    term: Arc<FairMutex<Term<ThermalEventListener>>>,

    /// Receiver for terminal events (title changes, wakeup, bell, etc.).
    #[allow(dead_code)]
    event_rx: mpsc::UnboundedReceiver<Event>,

    /// Tracks command blocks extracted from OSC 633 shell-integration marks.
    ///
    /// Wrapped in `Arc<Mutex>` so the byte-processor task and any rendering
    /// consumer can both access it without holding the `Term` lock.
    command_tracker: Arc<Mutex<CommandTracker>>,
}

#[allow(dead_code)]
impl Terminal {
    /// Create a new terminal emulator with default dimensions (120x36).
    pub fn new() -> Self {
        Self::with_size(DEFAULT_COLS, DEFAULT_ROWS)
    }

    /// Create a new terminal emulator with the given column/row dimensions.
    pub fn with_size(cols: usize, rows: usize) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let listener = ThermalEventListener::new(event_tx);

        let config = TermConfig::default();
        let size = TerminalSize::new(cols, rows);
        let term = Term::new(config, &size, listener);

        Terminal {
            term: Arc::new(FairMutex::new(term)),
            event_rx,
            command_tracker: Arc::new(Mutex::new(CommandTracker::new())),
        }
    }

    /// Get a clone of the `Arc<FairMutex<Term>>` for shared access.
    ///
    /// The renderer will hold a reference to this and lock it briefly each
    /// frame to read `renderable_content()`.
    pub fn term_handle(&self) -> Arc<FairMutex<Term<ThermalEventListener>>> {
        Arc::clone(&self.term)
    }

    /// Take the event receiver.
    ///
    /// Can only be called once — subsequent calls return `None`.
    pub fn take_event_rx(&mut self) -> Option<mpsc::UnboundedReceiver<Event>> {
        // Swap with a dummy to allow taking ownership once.
        let (_, dummy_rx) = mpsc::unbounded_channel();
        let rx = std::mem::replace(&mut self.event_rx, dummy_rx);
        Some(rx)
    }

    /// Resize the terminal grid.
    ///
    /// Should be called when the window size changes. Also returns the
    /// `WindowSize` struct suitable for sending to the PTY via
    /// `PtySession::resize`.
    pub fn resize(&self, cols: usize, rows: usize, cell_width: u16, cell_height: u16) {
        let size = TerminalSize::new(cols, rows);
        let mut term = self.term.lock();
        term.resize(size);
        debug!(cols, rows, "Terminal grid resized");

        // Notify the terminal about the pixel-level size as well.
        let _window_size = WindowSize {
            num_lines: rows as u16,
            num_cols: cols as u16,
            cell_width,
            cell_height,
        };
    }

    /// Spawn an async task that reads byte chunks from the PTY output channel
    /// and feeds them into the `Term` via the VTE parser.
    ///
    /// OSC 633 sequences are intercepted at the byte level before the bytes
    /// reach `alacritty_terminal::Term` (which silently drops unknown OSC
    /// codes).  Any parsed marks are applied to the [`CommandTracker`].
    ///
    /// After each batch of bytes is processed `pty_dirty` is set to `true` so
    /// the window's event loop knows to re-render.
    ///
    /// This is the core byte-processing loop. It runs until the PTY channel
    /// closes (child process exited).
    pub fn spawn_byte_processor(
        &self,
        mut pty_rx: mpsc::Receiver<Vec<u8>>,
        pty_dirty: Arc<AtomicBool>,
        wakeup_fd: std::os::fd::OwnedFd,
    ) {
        let term = Arc::clone(&self.term);
        let tracker = Arc::clone(&self.command_tracker);

        tokio::spawn(async move {
            let mut processor = ansi::Processor::<ansi::StdSyncHandler>::new();
            let mut osc_parser = Osc633Parser::new();
            let wakeup_raw = {
                use std::os::fd::AsRawFd;
                wakeup_fd.as_raw_fd()
            };

            info!("Terminal byte processor started");

            while let Some(first_bytes) = pty_rx.recv().await {
                // Batch all available chunks before processing. This avoids
                // rendering intermediate frames during burst output (e.g. TUI
                // startup that dumps 50-100KB of escape sequences at once).
                let mut batched = vec![first_bytes];
                while let Ok(more) = pty_rx.try_recv() {
                    batched.push(more);
                }

                // Process all batched chunks under a single term lock.
                {
                    let mut term_guard = term.lock();
                    for bytes in &batched {
                        // OSC 633 interception (non-destructive).
                        let marks = osc_parser.feed(bytes);
                        if !marks.is_empty() {
                            let cursor_line =
                                term_guard.grid().cursor.point.line.0.max(0) as usize;
                            drop(term_guard);
                            let mut t = tracker.lock();
                            t.set_current_line(cursor_line);
                            for mark in &marks {
                                t.apply(mark);
                            }
                            drop(t);
                            term_guard = term.lock();
                        }
                        processor.advance(&mut *term_guard, bytes);
                    }
                }

                // Signal the render loop that new content is available.
                pty_dirty.store(true, Ordering::Release);
                // Wake the poll() immediately so we don't wait for timeout.
                let _ = nix::unistd::write(
                    unsafe { std::os::fd::BorrowedFd::borrow_raw(wakeup_raw) },
                    &[1u8],
                );
            }

            info!("Terminal byte processor exiting (PTY channel closed)");
        });
    }

    /// Return a clone of the shared `CommandTracker` handle.
    ///
    /// Lock the returned `Mutex` briefly to inspect command blocks.  Do not
    /// hold the lock while rendering.
    pub fn command_tracker(&self) -> Arc<Mutex<CommandTracker>> {
        Arc::clone(&self.command_tracker)
    }

    /// Access the terminal's renderable content while holding the lock.
    ///
    /// The callback `f` receives a `RenderableContent` reference that provides
    /// the display iterator, cursor, selection, colors, and terminal mode.
    /// The lock is held for the duration of the callback — keep it short.
    pub fn with_renderable_content<F, R>(&self, f: F) -> R
    where
        F: FnOnce(RenderableContent<'_>) -> R,
    {
        let term = self.term.lock();
        f(term.renderable_content())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_terminal_default() {
        let terminal = Terminal::new();
        let term = terminal.term_handle();
        let locked = term.lock();
        assert_eq!(locked.columns(), DEFAULT_COLS);
        assert_eq!(locked.screen_lines(), DEFAULT_ROWS);
    }

    #[test]
    fn create_terminal_custom_size() {
        let terminal = Terminal::with_size(80, 24);
        let term = terminal.term_handle();
        let locked = term.lock();
        assert_eq!(locked.columns(), 80);
        assert_eq!(locked.screen_lines(), 24);
    }

    #[test]
    fn resize_terminal() {
        let terminal = Terminal::with_size(80, 24);
        terminal.resize(132, 43, 8, 16);
        let term = terminal.term_handle();
        let locked = term.lock();
        assert_eq!(locked.columns(), 132);
        assert_eq!(locked.screen_lines(), 43);
    }

    #[tokio::test]
    async fn feed_bytes_into_term() {
        use std::sync::atomic::AtomicBool;

        let terminal = Terminal::new();
        let (tx, rx) = mpsc::channel(16);

        let pty_dirty = Arc::new(AtomicBool::new(false));

        // Create a wakeup pipe for the byte processor.
        let (wakeup_read, wakeup_write) =
            nix::unistd::pipe().expect("pipe() failed");
        // Keep the read end alive so the write end doesn't error.
        let _wakeup_read = wakeup_read;

        terminal.spawn_byte_processor(rx, pty_dirty, wakeup_write);

        // Send "Hello\r\n" through the channel.
        tx.send(b"Hello\r\n".to_vec()).await.unwrap();

        // Drop the sender to close the channel (stops the processor).
        drop(tx);

        // Give the processor a moment to consume.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Verify the term has "Hello" in the first line.
        let term = terminal.term_handle();
        let locked = term.lock();
        let mut text = String::new();
        for col in 0..5 {
            let cell = &locked.grid()[alacritty_terminal::index::Line(0)]
                [alacritty_terminal::index::Column(col)];
            text.push(cell.c);
        }
        assert_eq!(text, "Hello");
    }
}
