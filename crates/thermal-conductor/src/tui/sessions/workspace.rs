//! Hyprland workspace lookup helpers.

use std::collections::HashMap;
use std::process::Command;

/// Query hyprctl for all client windows and return a PID -> workspace map.
pub(super) fn query_hyprland_workspaces() -> HashMap<u32, i64> {
    let output = match Command::new("hyprctl").args(["clients", "-j"]).output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return HashMap::new(),
    };

    #[derive(serde::Deserialize)]
    struct HyprClient {
        pid: u32,
        workspace: HyprWorkspace,
    }
    #[derive(serde::Deserialize)]
    struct HyprWorkspace {
        id: i64,
    }

    let clients: Vec<HyprClient> = match serde_json::from_slice(&output) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    clients
        .into_iter()
        .map(|c| (c.pid, c.workspace.id))
        .collect()
}

/// Walk up the process tree from `pid` until we find a PID in `window_pids`.
/// Returns the workspace ID if found.
pub(super) fn find_workspace_for_pid(pid: u32, window_pids: &HashMap<u32, i64>) -> Option<i64> {
    let mut current = pid;
    // Walk up to 10 levels to avoid infinite loops.
    for _ in 0..10 {
        if let Some(&ws) = window_pids.get(&current) {
            return Some(ws);
        }
        // Read parent PID from /proc.
        let stat = match std::fs::read_to_string(format!("/proc/{current}/stat")) {
            Ok(s) => s,
            Err(_) => return None,
        };
        // Format: "pid (comm) state ppid ..."
        // Find the closing ')' then split to get ppid.
        let after_comm = match stat.rfind(')') {
            Some(pos) => &stat[pos + 2..],
            None => return None,
        };
        let ppid: u32 = match after_comm.split_whitespace().nth(1) {
            Some(s) => match s.parse() {
                Ok(p) => p,
                Err(_) => return None,
            },
            None => return None,
        };
        if ppid <= 1 {
            return None;
        }
        current = ppid;
    }
    None
}
