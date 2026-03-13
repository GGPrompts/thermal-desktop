//! GitWatcher — monitors a git repository root for file changes using `notify`.

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// Watches a git repository root directory for file changes.
///
/// Uses the `notify` crate's recommended (OS-native) watcher. Call
/// [`GitWatcher::poll`] regularly to drain events and update
/// [`GitWatcher::changed_files`].
#[allow(dead_code)]
pub struct GitWatcher {
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<NotifyResult<Event>>,
    /// Accumulated list of changed paths since the last [`GitWatcher::clear`].
    pub changed_files: Vec<PathBuf>,
    repo_root: PathBuf,
}

#[allow(dead_code)]
impl GitWatcher {
    /// Start watching `repo_root` recursively for file system changes.
    pub fn new(repo_root: &Path) -> NotifyResult<Self> {
        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(tx)?;
        watcher.watch(repo_root, RecursiveMode::Recursive)?;

        Ok(Self {
            _watcher: watcher,
            rx,
            changed_files: Vec::new(),
            repo_root: repo_root.to_owned(),
        })
    }

    /// Drain pending events and update `changed_files`. Returns the new paths
    /// added in this poll cycle.
    pub fn poll(&mut self) -> Vec<PathBuf> {
        let mut new_paths = Vec::new();

        while let Ok(result) = self.rx.try_recv() {
            match result {
                Ok(event) => {
                    // Only care about Create and Modify events.
                    let is_interesting = matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_)
                    );
                    if !is_interesting {
                        continue;
                    }

                    for path in event.paths {
                        // Skip events inside the .git/ directory itself.
                        if path.components().any(|c| c.as_os_str() == ".git") {
                            continue;
                        }
                        if !self.changed_files.contains(&path) {
                            self.changed_files.push(path.clone());
                            new_paths.push(path);
                        }
                    }
                }
                Err(_) => {
                    // Watcher error — silently skip.
                }
            }
        }

        new_paths
    }

    /// Clear the `changed_files` list (call after rendering the HUD overlay).
    pub fn clear(&mut self) {
        self.changed_files.clear();
    }

    /// Returns true if `path` is inside the watched repository root.
    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.repo_root)
    }
}
