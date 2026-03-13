//! PaneCapture — polls tmux for pane content and parses ANSI-styled output.

use crate::ansi::{parse_ansi_styled, StyledChar};
use crate::tmux::{TmuxError, TmuxSession};

/// A snapshot of a single tmux pane's content, parsed into styled characters.
#[allow(dead_code)]
pub struct PaneCapture {
    /// The tmux pane ID, e.g. `"%0"`.
    pub pane_id: String,
    /// Parsed lines of styled characters (one inner Vec per line).
    pub lines: Vec<Vec<StyledChar>>,
    /// The raw ANSI string as returned by tmux.
    pub raw: String,
    /// When this capture was taken.
    pub captured_at: std::time::Instant,
}

#[allow(dead_code)]
impl PaneCapture {
    /// Capture the content of `pane_id` from `session`.
    ///
    /// `scrollback` controls how many lines of scrollback history to include.
    /// Passing `None` captures only the visible area.
    pub fn capture(
        session: &TmuxSession,
        pane_id: &str,
        scrollback: Option<i32>,
    ) -> Result<Self, TmuxError> {
        let raw = session.capture_pane(pane_id, scrollback)?;
        let lines = parse_ansi_styled(&raw);
        Ok(Self {
            pane_id: pane_id.to_owned(),
            lines,
            raw,
            captured_at: std::time::Instant::now(),
        })
    }

    /// Returns the number of parsed lines in this capture.
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Returns the total number of styled characters across all lines.
    pub fn char_count(&self) -> usize {
        self.lines.iter().map(|l| l.len()).sum()
    }
}
