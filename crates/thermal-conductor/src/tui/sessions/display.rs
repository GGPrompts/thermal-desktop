//! Display ordering and history tracking types.

use std::time::Instant;

use thermal_core::ClaudeSessionState;

// ---------------------------------------------------------------------------
// History tracking
// ---------------------------------------------------------------------------

pub(super) const MAX_HISTORY: usize = 12;

pub(super) struct HistoryEntry {
    pub(super) text: String,
    pub(super) timestamp: Instant,
}

// ---------------------------------------------------------------------------
// Display ordering -- parents first, subagents nested underneath
// ---------------------------------------------------------------------------

pub(super) struct DisplayRow {
    pub(super) session: ClaudeSessionState,
    pub(super) is_subagent: bool,
    pub(super) is_last_child: bool,
}

pub(super) fn build_display_order(sessions: &[ClaudeSessionState]) -> Vec<DisplayRow> {
    let mut parents: Vec<&ClaudeSessionState> = sessions
        .iter()
        .filter(|s| s.parent_session_id.is_none())
        .collect();
    parents.sort_by(|a, b| a.session_id.cmp(&b.session_id));

    let mut rows = Vec::with_capacity(sessions.len());

    for parent in &parents {
        rows.push(DisplayRow {
            session: (*parent).clone(),
            is_subagent: false,
            is_last_child: false,
        });

        let mut children: Vec<&ClaudeSessionState> = sessions
            .iter()
            .filter(|s| s.parent_session_id.as_deref() == Some(&parent.session_id))
            .collect();
        children.sort_by(|a, b| a.session_id.cmp(&b.session_id));

        let child_count = children.len();
        for (i, child) in children.into_iter().enumerate() {
            rows.push(DisplayRow {
                session: child.clone(),
                is_subagent: true,
                is_last_child: i == child_count - 1,
            });
        }
    }

    // Orphan subagents
    for s in sessions {
        if s.parent_session_id.is_some()
            && !parents
                .iter()
                .any(|p| Some(p.session_id.as_str()) == s.parent_session_id.as_deref())
        {
            rows.push(DisplayRow {
                session: s.clone(),
                is_subagent: true,
                is_last_child: true,
            });
        }
    }

    rows
}
