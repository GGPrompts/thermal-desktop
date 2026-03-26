//! Agent communication graph — tracks inter-agent relationships and message arcs.
//!
//! Builds a node graph from Claude session state files, using `parent_session_id`
//! to establish subagent relationships. Provides force-directed layout for
//! GPU-rendered visualization.

use std::collections::HashMap;
use std::time::Instant;

use thermal_core::claude_state::{ClaudeSessionState, ClaudeStatus};

/// Maximum number of message arcs to keep in the graph history.
const MAX_ARCS: usize = 200;

/// Height of the graph overlay area in pixels.
pub const GRAPH_OVERLAY_HEIGHT: u32 = 300;

// ── Force-directed layout parameters ────────────────────────────────────────

/// Repulsion force constant between nodes.
const REPULSION_K: f32 = 8000.0;
/// Attraction force toward center.
const CENTER_GRAVITY: f32 = 0.02;
/// Attraction force along parent-child edges.
const EDGE_ATTRACTION: f32 = 0.005;
/// Velocity damping per tick.
const DAMPING: f32 = 0.85;
/// Minimum distance between nodes to avoid singularity.
const MIN_DIST: f32 = 30.0;

// ── Node and Arc types ──────────────────────────────────────────────────────

/// A node in the agent communication graph.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AgentNode {
    /// Session ID from the Claude state file.
    pub session_id: String,
    /// Current agent status.
    pub status: ClaudeStatus,
    /// Current tool being used (if any).
    pub current_tool: Option<String>,
    /// Context window usage percentage (0-100).
    pub context_percent: f32,
    /// Working directory (for label display).
    pub working_dir: Option<String>,
    /// Parent session ID (for subagent relationships).
    pub parent_session_id: Option<String>,
    /// Layout position in pixel coordinates.
    pub pos: [f32; 2],
    /// Layout velocity for force-directed simulation.
    pub vel: [f32; 2],
    /// When this node was first seen.
    pub first_seen: Instant,
    /// When this node was last updated.
    pub last_updated: Instant,
}

/// A message arc between two agents.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MessageArc {
    /// Session ID of the sending agent.
    pub from_session: String,
    /// Session ID of the receiving agent.
    pub to_session: String,
    /// When this message was detected.
    pub timestamp: Instant,
    /// The tool that triggered this arc (e.g. Agent, Bash, etc.).
    pub tool_name: String,
    /// Approximate data size (context percent delta, used for line thickness).
    pub data_size: f32,
    /// Alpha fade-out value (1.0 = fully visible, 0.0 = gone).
    pub alpha: f32,
}

/// The agent communication graph state.
pub struct AgentGraph {
    /// Active nodes keyed by session_id.
    pub nodes: HashMap<String, AgentNode>,
    /// Recent message arcs between agents.
    pub arcs: Vec<MessageArc>,
    /// Whether the graph overlay is visible.
    pub visible: bool,
    /// Previous session snapshot for detecting tool changes (session_id -> last tool).
    prev_tools: HashMap<String, Option<String>>,
    /// Layout area dimensions (set from render surface).
    layout_width: f32,
    layout_height: f32,
}

impl AgentGraph {
    /// Create a new empty agent graph.
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            arcs: Vec::new(),
            visible: false,
            prev_tools: HashMap::new(),
            layout_width: 800.0,
            layout_height: 300.0,
        }
    }

    /// Toggle visibility.
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
        tracing::info!(visible = self.visible, "Agent graph toggled");
    }

    /// Update the graph from the latest session states.
    ///
    /// - Adds new nodes for sessions not yet tracked.
    /// - Updates existing nodes with fresh status/tool/context data.
    /// - Removes nodes for sessions no longer in the state list.
    /// - Detects tool changes and creates message arcs for subagent relationships.
    pub fn update_from_sessions(&mut self, sessions: &[ClaudeSessionState]) {
        let now = Instant::now();

        // Build set of active session IDs.
        let active_ids: HashMap<&str, &ClaudeSessionState> = sessions
            .iter()
            .filter(|s| !s.session_id.is_empty())
            .map(|s| (s.session_id.as_str(), s))
            .collect();

        // Remove nodes for sessions that are no longer active.
        self.nodes
            .retain(|id, _| active_ids.contains_key(id.as_str()));

        // Update or create nodes.
        for session in sessions {
            if session.session_id.is_empty() {
                continue;
            }

            let id = &session.session_id;

            if let Some(node) = self.nodes.get_mut(id) {
                // Update existing node.
                node.status = session.status.clone();
                node.current_tool = session.current_tool.clone();
                node.context_percent = session.context_percent.unwrap_or(0.0);
                node.working_dir = session.working_dir.clone();
                node.parent_session_id = session.parent_session_id.clone();
                node.last_updated = now;
            } else {
                // New node — place randomly near center.
                let cx = self.layout_width / 2.0;
                let cy = self.layout_height / 2.0;
                let jitter_x = ((id.len() as f32 * 37.0) % 100.0) - 50.0;
                let jitter_y = ((id.len() as f32 * 53.0) % 100.0) - 50.0;

                let node = AgentNode {
                    session_id: id.clone(),
                    status: session.status.clone(),
                    current_tool: session.current_tool.clone(),
                    context_percent: session.context_percent.unwrap_or(0.0),
                    working_dir: session.working_dir.clone(),
                    parent_session_id: session.parent_session_id.clone(),
                    pos: [cx + jitter_x, cy + jitter_y],
                    vel: [0.0, 0.0],
                    first_seen: now,
                    last_updated: now,
                };
                self.nodes.insert(id.clone(), node);
            }

            // Detect tool changes and create arcs for parent-child communication.
            let prev_tool = self.prev_tools.get(id).cloned().flatten();
            let curr_tool = session.current_tool.clone();

            if prev_tool != curr_tool {
                if let Some(ref tool) = curr_tool {
                    // If this session has a parent, create an arc from parent to child.
                    if let Some(ref parent_id) = session.parent_session_id {
                        if self.nodes.contains_key(parent_id) {
                            self.arcs.push(MessageArc {
                                from_session: parent_id.clone(),
                                to_session: id.clone(),
                                timestamp: now,
                                tool_name: tool.clone(),
                                data_size: session.context_percent.unwrap_or(5.0).max(5.0),
                                alpha: 1.0,
                            });
                        }
                    }
                }
            }

            self.prev_tools.insert(id.clone(), curr_tool);
        }

        // Clean up prev_tools for removed sessions.
        self.prev_tools
            .retain(|id, _| active_ids.contains_key(id.as_str()));

        // Fade and trim arcs.
        for arc in &mut self.arcs {
            let age = now.duration_since(arc.timestamp).as_secs_f32();
            // Fade over 10 seconds.
            arc.alpha = (1.0 - age / 10.0).clamp(0.0, 1.0);
        }
        self.arcs.retain(|arc| arc.alpha > 0.01);

        // Trim to max arcs.
        while self.arcs.len() > MAX_ARCS {
            self.arcs.remove(0);
        }
    }

    /// Set layout dimensions (called from the renderer with the actual overlay area).
    pub fn set_layout_size(&mut self, width: f32, height: f32) {
        self.layout_width = width;
        self.layout_height = height;
    }

    /// Run one tick of force-directed layout.
    pub fn tick_layout(&mut self) {
        if self.nodes.len() < 2 {
            // With 0 or 1 nodes, just center the single node.
            if let Some(node) = self.nodes.values_mut().next() {
                node.pos[0] = self.layout_width / 2.0;
                node.pos[1] = self.layout_height / 2.0;
                node.vel = [0.0, 0.0];
            }
            return;
        }

        let cx = self.layout_width / 2.0;
        let cy = self.layout_height / 2.0;

        // Collect node IDs and positions for force calculations.
        let positions: Vec<(String, [f32; 2])> = self
            .nodes
            .iter()
            .map(|(id, n)| (id.clone(), n.pos))
            .collect();

        // Build edge list from parent-child relationships.
        let edges: Vec<(String, String)> = self
            .nodes
            .values()
            .filter_map(|n| {
                n.parent_session_id.as_ref().map(|parent_id| {
                    (parent_id.clone(), n.session_id.clone())
                })
            })
            .filter(|(parent_id, _)| self.nodes.contains_key(parent_id))
            .collect();

        // Calculate forces.
        let mut forces: HashMap<String, [f32; 2]> = HashMap::new();
        for (id, _) in &positions {
            forces.insert(id.clone(), [0.0, 0.0]);
        }

        // Node-node repulsion (Coulomb's law).
        for i in 0..positions.len() {
            for j in (i + 1)..positions.len() {
                let (ref id_a, pos_a) = positions[i];
                let (ref id_b, pos_b) = positions[j];

                let dx = pos_a[0] - pos_b[0];
                let dy = pos_a[1] - pos_b[1];
                let dist = (dx * dx + dy * dy).sqrt().max(MIN_DIST);

                let force = REPULSION_K / (dist * dist);
                let fx = force * dx / dist;
                let fy = force * dy / dist;

                if let Some(f) = forces.get_mut(id_a) {
                    f[0] += fx;
                    f[1] += fy;
                }
                if let Some(f) = forces.get_mut(id_b) {
                    f[0] -= fx;
                    f[1] -= fy;
                }
            }
        }

        // Center gravity.
        for (id, pos) in &positions {
            let dx = cx - pos[0];
            let dy = cy - pos[1];
            if let Some(f) = forces.get_mut(id) {
                f[0] += dx * CENTER_GRAVITY;
                f[1] += dy * CENTER_GRAVITY;
            }
        }

        // Edge attraction (Hooke's law).
        for (parent_id, child_id) in &edges {
            if let (Some(p_pos), Some(c_pos)) = (
                positions.iter().find(|(id, _)| id == parent_id).map(|(_, p)| *p),
                positions.iter().find(|(id, _)| id == child_id).map(|(_, p)| *p),
            ) {
                let dx = c_pos[0] - p_pos[0];
                let dy = c_pos[1] - p_pos[1];
                let dist = (dx * dx + dy * dy).sqrt().max(1.0);

                // Desired distance is ~120px.
                let desired = 120.0;
                let displacement = dist - desired;
                let fx = EDGE_ATTRACTION * displacement * dx / dist;
                let fy = EDGE_ATTRACTION * displacement * dy / dist;

                if let Some(f) = forces.get_mut(parent_id) {
                    f[0] += fx;
                    f[1] += fy;
                }
                if let Some(f) = forces.get_mut(child_id) {
                    f[0] -= fx;
                    f[1] -= fy;
                }
            }
        }

        // Apply forces to velocities and positions.
        let margin = 40.0;
        for (id, node) in self.nodes.iter_mut() {
            if let Some(f) = forces.get(id) {
                node.vel[0] = (node.vel[0] + f[0]) * DAMPING;
                node.vel[1] = (node.vel[1] + f[1]) * DAMPING;
                node.pos[0] += node.vel[0];
                node.pos[1] += node.vel[1];

                // Clamp to layout area with margin.
                node.pos[0] = node.pos[0].clamp(margin, self.layout_width - margin);
                node.pos[1] = node.pos[1].clamp(margin, self.layout_height - margin);
            }
        }
    }

    /// Get an ordered list of nodes for rendering.
    pub fn node_list(&self) -> Vec<&AgentNode> {
        let mut nodes: Vec<&AgentNode> = self.nodes.values().collect();
        nodes.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        nodes
    }

    /// Derive a short label for a node (basename of working_dir, or truncated session_id).
    pub fn node_label(node: &AgentNode) -> String {
        if let Some(ref wd) = node.working_dir {
            std::path::Path::new(wd)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| Self::truncate_id(&node.session_id))
        } else {
            Self::truncate_id(&node.session_id)
        }
    }

    /// Truncate a session ID for display (first 8 chars).
    fn truncate_id(id: &str) -> String {
        if id.len() > 8 {
            format!("{}...", &id[..8])
        } else {
            id.to_string()
        }
    }
}
