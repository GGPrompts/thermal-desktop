//! Conductor — top-level coordinator that ties sessions, captures, layout, and
//! rendering together.

use crate::capture::PaneCapture;
use crate::layout::LayoutEngine;
use crate::renderer::WgpuState;
use crate::session::SessionManager;
use crate::tmux::TmuxError;

/// Orchestrates the full render loop: polls panes, tracks dirty state, and
/// delegates rendering to `WgpuState`.
#[allow(dead_code)]
pub struct Conductor {
    pub session: SessionManager,
    pub captures: Vec<PaneCapture>,
    /// `dirty[i]` is true when pane `i` has new content that needs re-rendering.
    pub dirty: Vec<bool>,
    pub layout: LayoutEngine,
    /// Previous char counts per pane — used for dirty detection.
    prev_char_counts: Vec<usize>,
}

#[allow(dead_code)]
impl Conductor {
    /// Create a new Conductor with an empty capture list.
    pub fn new(session: SessionManager, layout: LayoutEngine) -> Self {
        let n = session.pane_ids().len();
        Self {
            session,
            captures: Vec::new(),
            dirty: vec![false; n],
            layout,
            prev_char_counts: vec![0; n],
        }
    }

    /// Poll all panes sequentially. Marks a pane dirty if its character count
    /// has changed since the last poll.
    ///
    /// Returns the number of panes that were marked dirty.
    pub fn poll(&mut self) -> usize {
        let pane_ids: Vec<String> = self.session.pane_ids().to_owned();
        let n = pane_ids.len();

        // Grow tracking vecs if new panes appeared.
        if self.dirty.len() < n {
            self.dirty.resize(n, false);
            self.prev_char_counts.resize(n, 0);
        }

        // Rebuild captures vec to match current pane count.
        let mut new_captures: Vec<Option<PaneCapture>> = (0..n).map(|_| None).collect();

        for (i, pane_id) in pane_ids.iter().enumerate() {
            match PaneCapture::capture(&self.session.session, pane_id, None) {
                Ok(capture) => {
                    let count = capture.char_count();
                    if count != self.prev_char_counts[i] {
                        self.prev_char_counts[i] = count;
                        self.dirty[i] = true;
                    }
                    new_captures[i] = Some(capture);
                }
                Err(e) => {
                    tracing::warn!("poll: capture error for pane {}: {}", pane_id, e);
                }
            }
        }

        // Replace captures, keeping old value for panes that errored.
        // Convert old captures to indexed Vec<Option<PaneCapture>> so we can
        // take by index rather than iterating sequentially.
        let mut old_indexed: Vec<Option<PaneCapture>> =
            std::mem::replace(&mut self.captures, Vec::with_capacity(n))
                .into_iter()
                .map(Some)
                .collect();
        // Pad to n in case panes were added.
        old_indexed.resize_with(n, || None);

        for (i, opt) in new_captures.into_iter().enumerate() {
            match opt {
                Some(c) => self.captures.push(c),
                None => {
                    // Re-use the old capture for this specific pane index.
                    if let Some(old) = old_indexed[i].take() {
                        self.captures.push(old);
                    }
                }
            }
        }

        self.dirty.iter().filter(|&&d| d).count()
    }

    /// Render all dirty panes into their viewport rects using `renderer`.
    /// Clears dirty flags after rendering.
    pub fn render_frame(&mut self, _renderer: &mut WgpuState) {
        // Rendering requires an active GPU surface (available only with a
        // Wayland compositor). This method computes the layout rects and
        // clears dirty flags. Actual draw calls are wired in the winit event
        // loop on bare-metal.
        let rects = self.layout.compute_rects();

        for (i, dirty) in self.dirty.iter_mut().enumerate() {
            if *dirty {
                if let (Some(_capture), Some(_rect)) =
                    (self.captures.get(i), rects.get(i))
                {
                    // renderer.render_capture(capture, *rect, ...) goes here
                    // once we have a text_renderer and an open render pass.
                }
                *dirty = false;
            }
        }
    }

    /// Kill a specific pane by index, removing it from the session and tracking
    /// structures.
    pub fn kill_pane(&mut self, idx: usize) -> Result<(), TmuxError> {
        if idx >= self.session.pane_ids().len() {
            return Ok(());
        }
        let pane_id = self.session.pane_ids()[idx].clone();
        self.session.session.kill_pane(&pane_id)?;
        if idx < self.captures.len() {
            self.captures.remove(idx);
        }
        if idx < self.dirty.len() {
            self.dirty.remove(idx);
        }
        if idx < self.prev_char_counts.len() {
            self.prev_char_counts.remove(idx);
        }
        self.layout.pane_count = self.session.pane_ids().len();
        Ok(())
    }
}
