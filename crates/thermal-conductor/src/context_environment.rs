//! Environment detection for context-aware shader effects.
//!
//! Detects the terminal's execution environment (Docker, git worktree, SSH,
//! main branch) and maps it to a `TerminalContext` variant. The GPU terminal
//! uses this to render environment-specific border effects so you can instantly
//! see *where* a terminal is running.

use std::path::Path;

/// The detected execution environment of the terminal session.
///
/// Ordered by visual priority — higher-priority contexts override lower ones
/// when multiple conditions are true simultaneously.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum TerminalContext {
    /// Main branch / normal local session — no special effect.
    MainBranch = 0,
    /// Inside a Docker container — blue/indigo shimmer.
    DockerContainer = 1,
    /// Inside a git worktree — amber/gold border glow.
    GitWorktree = 2,
    /// Connected via SSH — red vignette danger zone.
    SshRemote = 3,
}

impl TerminalContext {
    /// Returns the `context_type` uniform value for the shader (0-3).
    pub fn as_uniform(self) -> u32 {
        self as u32
    }
}

/// Check if we are running inside a Docker container.
///
/// Detection heuristics (in order):
/// 1. `/.dockerenv` file exists (Docker creates this in every container)
/// 2. `CONTAINER_ID` environment variable is set
/// 3. `/run/.containerenv` exists (Podman)
fn is_docker() -> bool {
    Path::new("/.dockerenv").exists()
        || std::env::var("CONTAINER_ID").is_ok()
        || Path::new("/run/.containerenv").exists()
}

/// Check if we are connected via SSH.
///
/// The `SSH_CONNECTION` environment variable is set by the SSH daemon when
/// a remote session is established.
fn is_ssh() -> bool {
    std::env::var("SSH_CONNECTION").is_ok() || std::env::var("SSH_TTY").is_ok()
}

/// Check if the current working directory is inside a git worktree.
///
/// Runs `git worktree list` and checks if there are multiple worktrees
/// (the first is always the main working tree). If the current directory
/// is in a non-primary worktree, this returns true.
///
/// Falls back to checking the `.git` file (worktrees have a `.git` *file*
/// pointing to the main repo's `.git/worktrees/<name>` directory, while
/// normal repos have a `.git` *directory*).
fn is_git_worktree() -> bool {
    // Fast path: check if `.git` is a file (worktree indicator) rather
    // than a directory.
    let git_path = Path::new(".git");
    if git_path.is_file() {
        return true;
    }

    // Slower path: run `git worktree list` and check for multiple entries.
    if let Ok(output) = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Each worktree block starts with "worktree " line.
            let worktree_count = stdout.lines().filter(|l| l.starts_with("worktree ")).count();
            return worktree_count > 1;
        }
    }

    false
}

/// Detect the current terminal context.
///
/// Priority order (highest first):
/// 1. SSH — danger zone, always takes priority
/// 2. Docker — contained environment
/// 3. Git worktree — sandboxed development
/// 4. Main branch — default, no visual effect
pub fn detect_context() -> TerminalContext {
    if is_ssh() {
        TerminalContext::SshRemote
    } else if is_docker() {
        TerminalContext::DockerContainer
    } else if is_git_worktree() {
        TerminalContext::GitWorktree
    } else {
        TerminalContext::MainBranch
    }
}

/// Detect context with an explicit worktree hint from the spawn profile.
#[allow(dead_code)]
///
/// When a session is spawned via the TUI Spawn tab with `git_worktree = true`
/// in its profile, we can skip the filesystem check and directly return
/// `GitWorktree` (unless a higher-priority context applies).
pub fn detect_context_with_hint(profile_is_worktree: bool) -> TerminalContext {
    if is_ssh() {
        TerminalContext::SshRemote
    } else if is_docker() {
        TerminalContext::DockerContainer
    } else if profile_is_worktree || is_git_worktree() {
        TerminalContext::GitWorktree
    } else {
        TerminalContext::MainBranch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_branch_uniform_is_zero() {
        assert_eq!(TerminalContext::MainBranch.as_uniform(), 0);
    }

    #[test]
    fn docker_uniform_is_one() {
        assert_eq!(TerminalContext::DockerContainer.as_uniform(), 1);
    }

    #[test]
    fn worktree_uniform_is_two() {
        assert_eq!(TerminalContext::GitWorktree.as_uniform(), 2);
    }

    #[test]
    fn ssh_uniform_is_three() {
        assert_eq!(TerminalContext::SshRemote.as_uniform(), 3);
    }

    #[test]
    fn detect_context_returns_some_value() {
        // Just verify it doesn't panic — actual result depends on environment.
        let _ctx = detect_context();
    }
}
