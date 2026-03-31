//! Agent state inference from terminal output.
//!
//! Maps PTY output patterns and OSC 633 command tracker transitions to agent
//! session states, then writes state files in the format consumed by
//! [`thermal_core::claude_state::ClaudeStatePoller`].
//!
//! This replaces the need for external hook scripts that write to
//! `/tmp/claude-code-state/`, `/tmp/codex-state/`, and `/tmp/copilot-state/`.
//!
//! # Architecture
//!
//! ```text
//! PTY output bytes
//!   → byte_processor (existing)
//!     → OSC 633 → CommandTracker (existing)
//!     → AgentStateInference (this module)
//!       → pattern matching on recent output lines
//!       → state file writes to /tmp/{agent}-state/
//! ```

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Serialize;
use tracing::{debug, trace, warn};

use crate::osc633::CommandState;

// ── Agent types ─────────────────────────────────────────────────────────────

/// The type of agent running in this terminal session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentType {
    Claude,
    Codex,
    Copilot,
}

impl AgentType {
    /// The state directory path for this agent type.
    pub fn state_dir(&self) -> &'static str {
        match self {
            AgentType::Claude => "/tmp/claude-code-state",
            AgentType::Codex => "/tmp/codex-state",
            AgentType::Copilot => "/tmp/copilot-state",
        }
    }

    /// Try to infer agent type from a spawn command string.
    pub fn from_command(cmd: &str) -> Option<Self> {
        let lower = cmd.to_lowercase();
        if lower.contains("claude") {
            Some(AgentType::Claude)
        } else if lower.contains("codex") {
            Some(AgentType::Codex)
        } else if lower.contains("copilot") {
            Some(AgentType::Copilot)
        } else {
            None
        }
    }

    /// String representation matching the existing state file format.
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentType::Claude => "claude",
            AgentType::Codex => "codex",
            AgentType::Copilot => "copilot",
        }
    }
}

// ── Inferred status ─────────────────────────────────────────────────────────

/// Agent status, matching the format in `ClaudeStatus`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferredStatus {
    Idle,
    Processing,
    ToolUse { tool_name: String },
    AwaitingInput,
}

impl InferredStatus {
    fn status_str(&self) -> &str {
        match self {
            InferredStatus::Idle => "idle",
            InferredStatus::Processing => "processing",
            InferredStatus::ToolUse { .. } => "tool_use",
            InferredStatus::AwaitingInput => "awaiting_input",
        }
    }

    fn tool_name(&self) -> Option<&str> {
        match self {
            InferredStatus::ToolUse { tool_name } => Some(tool_name),
            _ => None,
        }
    }
}

// ── State file format ───────────────────────────────────────────────────────

/// JSON structure written to state files, matching the `SessionStateV1` schema
/// defined in `thermal-core/schemas/thermal-protocol.ggl`.
///
/// Field names and types are aligned with the ggl-generated `SessionStateV1`
/// struct so that the JSON written here deserializes correctly via
/// `ClaudeStatePoller` (thermal-core). We keep a local struct rather than
/// importing `thermal-core` to avoid pulling GPU/Wayland dependencies into
/// this lightweight, Android-compatible crate.
#[derive(Debug, Serialize)]
struct StateFile {
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_type: Option<String>,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_updated: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

// ── Pattern matchers (compiled once) ────────────────────────────────────────

struct Patterns {
    /// Braille spinner characters used by Claude/Codex during processing.
    spinner: Regex,
    /// Tool call patterns: "Read(", "Edit(", "Bash(", "Write(", etc.
    tool_call: Regex,
    /// Awaiting input indicators.
    awaiting_input: Regex,
    /// Context percentage patterns (e.g., "Context: 42%", "42% context").
    context_percent: Regex,
    /// Agent identification from output (e.g., "Claude Code", "Codex").
    agent_ident_claude: Regex,
    agent_ident_codex: Regex,
    agent_ident_copilot: Regex,
    /// Model name extraction from output.
    model_pattern: Regex,
}

impl Patterns {
    fn new() -> Self {
        Self {
            spinner: Regex::new(r"[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]|Thinking\.{2,}").unwrap(),
            tool_call: Regex::new(
                r"(?:^|\s)(Read|Edit|Write|Bash|Glob|Grep|TodoRead|TodoWrite|WebSearch|WebFetch|mcp_|ListFiles|SearchFiles|ExecuteCommand|ReplaceInFile|ReadFile|WriteToFile)\s*[\(\{(]"
            ).unwrap(),
            awaiting_input: Regex::new(
                r"(?:^>\s|esc to interrupt|waiting for (?:input|response)|^\s*\$\s*$|Press Enter|Type (?:a|your) (?:message|response))"
            ).unwrap(),
            context_percent: Regex::new(r"(\d{1,3})(?:\.\d)?%\s*(?:context|ctx)").unwrap(),
            agent_ident_claude: Regex::new(r"(?i)claude\s*(?:code|3|4)?").unwrap(),
            agent_ident_codex: Regex::new(r"(?i)codex").unwrap(),
            agent_ident_copilot: Regex::new(r"(?i)copilot").unwrap(),
            model_pattern: Regex::new(
                r"(?:model|using)[\s:]+([a-zA-Z0-9._-]+(?:opus|sonnet|haiku|gpt[0-9.-]+|o[134]-?[a-z]*)[a-zA-Z0-9._-]*)"
            ).unwrap(),
        }
    }
}

// ── Main inference engine ───────────────────────────────────────────────────

/// Configuration for the inference engine.
pub struct InferenceConfig {
    /// Session ID used in state files.
    pub session_id: String,
    /// PID of the PTY child process.
    pub child_pid: u32,
    /// Agent type (if known from spawn command). Inferred from output if None.
    pub agent_type: Option<AgentType>,
    /// Working directory of the session.
    pub working_dir: Option<String>,
}

/// Agent state inference engine.
///
/// Tracks recent terminal output lines and command tracker state to infer
/// the agent's current status. Writes state files atomically when the
/// inferred state changes.
pub struct AgentStateInference {
    config: InferenceConfig,
    /// Recent output lines (ring buffer, newest at back).
    recent_lines: VecDeque<String>,
    /// Current accumulated line (not yet terminated by newline).
    current_line: String,
    /// Last inferred status.
    last_status: InferredStatus,
    /// Last OSC 633 command state we saw.
    last_command_state: Option<CommandState>,
    /// Detected agent type from output (may override config).
    detected_agent_type: Option<AgentType>,
    /// Detected model name from output.
    detected_model: Option<String>,
    /// Last detected context percentage.
    context_percent: Option<f32>,
    /// Timestamp of last state file write (throttle writes).
    last_write: Instant,
    /// Path to the state file we manage.
    state_file_path: Option<PathBuf>,
    /// Compiled regex patterns (shared across calls).
    patterns: Patterns,
}

/// Maximum number of recent lines to keep in the ring buffer.
const MAX_RECENT_LINES: usize = 50;

/// Minimum interval between state file writes.
const MIN_WRITE_INTERVAL: Duration = Duration::from_millis(500);

impl AgentStateInference {
    /// Create a new inference engine with the given configuration.
    pub fn new(config: InferenceConfig) -> Self {
        let state_file_path = config.agent_type.map(|at| {
            PathBuf::from(at.state_dir()).join(format!("{}.json", config.session_id))
        });

        Self {
            config,
            recent_lines: VecDeque::with_capacity(MAX_RECENT_LINES + 1),
            current_line: String::new(),
            last_status: InferredStatus::Idle,
            last_command_state: None,
            detected_agent_type: None,
            detected_model: None,
            context_percent: None,
            last_write: Instant::now() - MIN_WRITE_INTERVAL, // allow immediate first write
            state_file_path,
            patterns: Patterns::new(),
        }
    }

    /// Get the effective agent type (config override > detected > None).
    fn effective_agent_type(&self) -> Option<AgentType> {
        self.config.agent_type.or(self.detected_agent_type)
    }

    /// Feed a chunk of raw PTY output bytes.
    ///
    /// Extracts visible text lines (stripping ANSI escape sequences) and
    /// updates the recent lines buffer. Call [`Self::update_command_state`]
    /// separately with the latest `CommandState` from the tracker.
    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        // Simple ANSI stripping: skip escape sequences to extract visible text.
        let text = strip_ansi_visible(bytes);

        for ch in text.chars() {
            if ch == '\n' || ch == '\r' {
                if !self.current_line.is_empty() {
                    let line = std::mem::take(&mut self.current_line);
                    self.push_line(line);
                }
            } else if ch.is_control() {
                // Skip other control chars
            } else {
                self.current_line.push(ch);
            }
        }

        // Also try to detect agent type and model from the output.
        self.detect_agent_from_output();

        // Run inference and write if state changed.
        self.infer_and_write();
    }

    /// Update with the latest OSC 633 command state.
    ///
    /// Should be called after the command tracker processes marks.
    pub fn update_command_state(&mut self, state: CommandState) {
        if self.last_command_state.as_ref() != Some(&state) {
            self.last_command_state = Some(state);
            self.infer_and_write();
        }
    }

    /// Clean up the state file on session exit.
    pub fn cleanup(&self) {
        if let Some(ref path) = self.state_file_path {
            if path.exists() {
                if let Err(e) = std::fs::remove_file(path) {
                    warn!(path = %path.display(), error = %e, "Failed to remove state file on cleanup");
                } else {
                    debug!(path = %path.display(), "Removed state file on session exit");
                }
            }
        }
    }

    fn push_line(&mut self, line: String) {
        self.recent_lines.push_back(line);
        if self.recent_lines.len() > MAX_RECENT_LINES {
            self.recent_lines.pop_front();
        }
    }

    /// Detect agent type from terminal output if not already configured.
    fn detect_agent_from_output(&mut self) {
        if self.config.agent_type.is_some() && self.detected_model.is_some() {
            return; // Already fully configured
        }

        // Scan recent lines for detection signals, collecting results
        // without mutating self (avoids borrow-checker conflict on the
        // `recent_lines` iterator).
        let mut new_agent_type: Option<AgentType> = None;
        let mut new_model: Option<String> = None;

        let need_agent = self.config.agent_type.is_none() && self.detected_agent_type.is_none();
        let need_model = self.detected_model.is_none();

        for line in self.recent_lines.iter().rev().take(10) {
            if need_agent && new_agent_type.is_none() {
                if self.patterns.agent_ident_claude.is_match(line) {
                    new_agent_type = Some(AgentType::Claude);
                } else if self.patterns.agent_ident_codex.is_match(line) {
                    new_agent_type = Some(AgentType::Codex);
                } else if self.patterns.agent_ident_copilot.is_match(line) {
                    new_agent_type = Some(AgentType::Copilot);
                }
            }

            if need_model && new_model.is_none() {
                if let Some(caps) = self.patterns.model_pattern.captures(line) {
                    if let Some(m) = caps.get(1) {
                        new_model = Some(m.as_str().to_string());
                    }
                }
            }

            // Stop early if we found everything we need.
            if (!need_agent || new_agent_type.is_some())
                && (!need_model || new_model.is_some())
            {
                break;
            }
        }

        // Apply detected values.
        if let Some(at) = new_agent_type {
            debug!(agent = at.as_str(), "Detected agent type from output");
            self.detected_agent_type = Some(at);
            self.update_state_file_path();
        }
        if let Some(model) = new_model {
            debug!(model = %model, "Detected model from output");
            self.detected_model = Some(model);
        }
    }

    /// Update the state file path after agent type is detected.
    fn update_state_file_path(&mut self) {
        if let Some(agent_type) = self.effective_agent_type() {
            self.state_file_path = Some(
                PathBuf::from(agent_type.state_dir())
                    .join(format!("{}.json", self.config.session_id)),
            );
        }
    }

    /// Run the inference pipeline and write state if it changed.
    fn infer_and_write(&mut self) {
        let new_status = self.infer_status();

        // Detect context percentage from recent output.
        self.detect_context_percent();

        let status_changed = new_status != self.last_status;
        let throttle_ok = self.last_write.elapsed() >= MIN_WRITE_INTERVAL;

        if status_changed && throttle_ok {
            self.last_status = new_status;
            self.write_state_file();
        } else if status_changed {
            // Status changed but we're throttled — remember for next call.
            self.last_status = new_status;
        }
    }

    /// Infer the current agent status from command tracker state + output patterns.
    fn infer_status(&self) -> InferredStatus {
        // OSC 633 command state takes priority — it's the most reliable signal.
        if let Some(ref cmd_state) = self.last_command_state {
            match cmd_state {
                CommandState::PromptStart | CommandState::Input => {
                    // Shell is at a prompt — but the agent may be showing its
                    // own prompt, not the shell prompt. Check output patterns.
                    return self.infer_from_output_or(InferredStatus::AwaitingInput);
                }
                CommandState::Executing => {
                    // A command is running. Check if we can identify a tool.
                    return self.infer_executing_status();
                }
                CommandState::Finished => {
                    // Command just finished. Back to idle/awaiting.
                    return self.infer_from_output_or(InferredStatus::Idle);
                }
            }
        }

        // No OSC 633 data — rely purely on output heuristics.
        self.infer_from_output_or(InferredStatus::Idle)
    }

    /// During command execution, check output for tool use or processing patterns.
    fn infer_executing_status(&self) -> InferredStatus {
        // Check the live unterminated line first — it has the most recent content
        // (prompts and spinners are often rendered without a trailing newline).
        let lines_iter = std::iter::once(&self.current_line)
            .filter(|l| !l.is_empty())
            .chain(self.recent_lines.iter().rev());

        // Check for tool call patterns.
        for line in lines_iter.clone().take(16) {
            if let Some(caps) = self.patterns.tool_call.captures(line) {
                if let Some(m) = caps.get(1) {
                    return InferredStatus::ToolUse {
                        tool_name: m.as_str().to_string(),
                    };
                }
            }
        }

        // Check for spinner/thinking patterns.
        for line in lines_iter.take(6) {
            if self.patterns.spinner.is_match(line) {
                return InferredStatus::Processing;
            }
        }

        // Default during execution: processing.
        InferredStatus::Processing
    }

    /// Check output patterns, falling back to the given default.
    fn infer_from_output_or(&self, default: InferredStatus) -> InferredStatus {
        // Include the live unterminated line — prompts like `>` and spinners
        // are often rendered without a trailing newline.
        let lines_iter = std::iter::once(&self.current_line)
            .filter(|l| !l.is_empty())
            .chain(self.recent_lines.iter().rev());

        // Check for awaiting input patterns in very recent lines.
        for line in lines_iter.clone().take(6) {
            if self.patterns.awaiting_input.is_match(line) {
                return InferredStatus::AwaitingInput;
            }
        }

        // Check for tool use patterns (agent might be showing tool output).
        for line in lines_iter.clone().take(11) {
            if let Some(caps) = self.patterns.tool_call.captures(line) {
                if let Some(m) = caps.get(1) {
                    return InferredStatus::ToolUse {
                        tool_name: m.as_str().to_string(),
                    };
                }
            }
        }

        // Check for spinner/thinking.
        for line in lines_iter.take(4) {
            if self.patterns.spinner.is_match(line) {
                return InferredStatus::Processing;
            }
        }

        default
    }

    /// Detect context percentage from recent output.
    fn detect_context_percent(&mut self) {
        for line in self.recent_lines.iter().rev().take(10) {
            if let Some(caps) = self.patterns.context_percent.captures(line) {
                if let Some(m) = caps.get(1) {
                    if let Ok(pct) = m.as_str().parse::<f32>() {
                        if (0.0..=100.0).contains(&pct) {
                            self.context_percent = Some(pct);
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Write the current state to a JSON file atomically.
    fn write_state_file(&mut self) {
        let Some(agent_type) = self.effective_agent_type() else {
            trace!("No agent type detected, skipping state file write");
            return;
        };

        let state_dir = Path::new(agent_type.state_dir());

        // Ensure the state directory exists.
        if !state_dir.exists() {
            if let Err(e) = std::fs::create_dir_all(state_dir) {
                warn!(dir = %state_dir.display(), error = %e, "Failed to create state directory");
                return;
            }
        }

        let state_file = StateFile {
            session_id: self.config.session_id.clone(),
            agent_type: Some(agent_type.as_str().to_string()),
            status: self.last_status.status_str().to_string(),
            current_tool: self.last_status.tool_name().map(|s| s.to_string()),
            working_dir: self.config.working_dir.clone(),
            last_updated: Some(now_rfc3339()),
            pid: Some(self.config.child_pid as i64),
            model: self.detected_model.clone(),
            context_percent: self.context_percent.map(|v| v as f64),
            source: Some("terminal_inference".to_string()),
        };

        let file_path = state_dir.join(format!("{}.json", self.config.session_id));

        // Atomic write: write to .tmp then rename.
        let tmp_path = state_dir.join(format!("{}.json.tmp", self.config.session_id));
        match serde_json::to_string_pretty(&state_file) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&tmp_path, json.as_bytes()) {
                    warn!(path = %tmp_path.display(), error = %e, "Failed to write temp state file");
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp_path, &file_path) {
                    warn!(path = %file_path.display(), error = %e, "Failed to rename state file");
                    // Clean up tmp file.
                    let _ = std::fs::remove_file(&tmp_path);
                    return;
                }
                self.last_write = Instant::now();
                self.state_file_path = Some(file_path.clone());
                trace!(
                    status = self.last_status.status_str(),
                    path = %file_path.display(),
                    "Wrote agent state file"
                );
            }
            Err(e) => {
                warn!(error = %e, "Failed to serialize state file");
            }
        }
    }
}

// ── ANSI stripping ──────────────────────────────────────────────────────────

/// Strip ANSI escape sequences from bytes, returning visible text.
///
/// This is a simplified stripper that handles the common CSI (ESC [ ...) and
/// OSC (ESC ] ...) sequences. It doesn't need to be perfect — we're doing
/// regex matching on the extracted text, so occasional artifacts are fine.
fn strip_ansi_visible(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        if b == 0x1B {
            // ESC
            i += 1;
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                b'[' => {
                    // CSI sequence: ESC [ ... (ends at 0x40-0x7E)
                    i += 1;
                    while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1; // skip final byte
                    }
                }
                b']' => {
                    // OSC sequence: ESC ] ... (ends at BEL or ST)
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                b'(' | b')' | b'*' | b'+' => {
                    // Character set designation: ESC ( X
                    i += 1;
                    if i < bytes.len() {
                        i += 1;
                    }
                }
                b'_' => {
                    // APC sequence: ESC _ ... ST
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                _ => {
                    // Other ESC sequences (2-byte): skip one more byte.
                    i += 1;
                }
            }
        } else if b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t' {
            // Skip non-printable control characters (except newline/cr/tab).
            i += 1;
        } else {
            // Visible character or whitespace — handle UTF-8.
            let remaining = &bytes[i..];
            if let Some(ch) = decode_utf8_char(remaining) {
                out.push(ch);
                i += ch.len_utf8();
            } else {
                // Invalid UTF-8, skip byte.
                i += 1;
            }
        }
    }

    out
}

/// Decode a single UTF-8 character from the start of a byte slice.
fn decode_utf8_char(bytes: &[u8]) -> Option<char> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.chars().next())
        .or_else(|| {
            // Try progressively shorter slices (1-4 bytes) to handle partial
            // multi-byte sequences at chunk boundaries.
            for len in (1..=4.min(bytes.len())).rev() {
                if let Ok(s) = std::str::from_utf8(&bytes[..len]) {
                    return s.chars().next();
                }
            }
            None
        })
}

/// Get the current time as an RFC 3339 string.
fn now_rfc3339() -> String {
    // Use a simple manual approach to avoid pulling in the `time` crate
    // (which thermal-terminal doesn't depend on).
    use std::time::SystemTime;
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();

    // Convert to UTC components.
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Convert days since epoch to year-month-day.
    // Simplified: use a basic algorithm for dates from 1970 onward.
    let (year, month, day) = days_to_ymd(days);

    format!(
        "{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z"
    )
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Civil calendar algorithm from Howard Hinnant.
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_engine(agent_type: Option<AgentType>) -> AgentStateInference {
        AgentStateInference::new(InferenceConfig {
            session_id: "test-session-1".to_string(),
            child_pid: 12345,
            agent_type,
            working_dir: Some("/home/user/project".to_string()),
        })
    }

    // ── ANSI stripping ──────────────────────────────────────────────────

    #[test]
    fn strip_plain_text() {
        let text = b"Hello, world!";
        assert_eq!(strip_ansi_visible(text), "Hello, world!");
    }

    #[test]
    fn strip_csi_color_codes() {
        // ESC[31m = red, ESC[0m = reset
        let text = b"\x1b[31mRed text\x1b[0m normal";
        assert_eq!(strip_ansi_visible(text), "Red text normal");
    }

    #[test]
    fn strip_osc_title() {
        // OSC 2 = set title, terminated by BEL
        let text = b"\x1b]2;My Title\x07Visible text";
        assert_eq!(strip_ansi_visible(text), "Visible text");
    }

    #[test]
    fn strip_preserves_newlines() {
        let text = b"line1\nline2\r\nline3";
        assert_eq!(strip_ansi_visible(text), "line1\nline2\r\nline3");
    }

    #[test]
    fn strip_mixed_escapes() {
        let text = b"\x1b[1;32m> \x1b[0mType a message\x1b]2;title\x07";
        assert_eq!(strip_ansi_visible(text), "> Type a message");
    }

    // ── Agent type detection ────────────────────────────────────────────

    #[test]
    fn agent_type_from_command() {
        assert_eq!(
            AgentType::from_command("claude --model opus"),
            Some(AgentType::Claude)
        );
        assert_eq!(
            AgentType::from_command("codex --model o4-mini"),
            Some(AgentType::Codex)
        );
        assert_eq!(
            AgentType::from_command("gh copilot suggest"),
            Some(AgentType::Copilot)
        );
        assert_eq!(AgentType::from_command("vim file.rs"), None);
    }

    #[test]
    fn agent_type_state_dir() {
        assert_eq!(AgentType::Claude.state_dir(), "/tmp/claude-code-state");
        assert_eq!(AgentType::Codex.state_dir(), "/tmp/codex-state");
        assert_eq!(AgentType::Copilot.state_dir(), "/tmp/copilot-state");
    }

    // ── Pattern matching ────────────────────────────────────────────────

    #[test]
    fn detect_spinner_processing() {
        let mut engine = make_engine(Some(AgentType::Claude));
        // Simulate spinner output.
        engine.push_line("⠋ Processing your request...".to_string());
        let status = engine.infer_status();
        assert_eq!(status, InferredStatus::Processing);
    }

    #[test]
    fn detect_thinking_processing() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.push_line("Thinking...".to_string());
        let status = engine.infer_status();
        assert_eq!(status, InferredStatus::Processing);
    }

    #[test]
    fn detect_tool_use_read() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        engine.push_line("Read(/home/user/file.rs)".to_string());
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::ToolUse {
                tool_name: "Read".to_string()
            }
        );
    }

    #[test]
    fn detect_tool_use_bash() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        engine.push_line("Bash(cargo build --release)".to_string());
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::ToolUse {
                tool_name: "Bash".to_string()
            }
        );
    }

    #[test]
    fn detect_tool_use_edit() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        engine.push_line("  Edit(src/main.rs)".to_string());
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::ToolUse {
                tool_name: "Edit".to_string()
            }
        );
    }

    #[test]
    fn detect_tool_use_write() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        engine.push_line("Write(new_file.rs)".to_string());
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::ToolUse {
                tool_name: "Write".to_string()
            }
        );
    }

    #[test]
    fn detect_tool_use_glob() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        engine.push_line("Glob(**/*.rs)".to_string());
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::ToolUse {
                tool_name: "Glob".to_string()
            }
        );
    }

    #[test]
    fn detect_tool_use_grep() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        engine.push_line("Grep(pattern)".to_string());
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::ToolUse {
                tool_name: "Grep".to_string()
            }
        );
    }

    #[test]
    fn detect_awaiting_input_prompt() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.push_line("> ".to_string());
        let status = engine.infer_status();
        assert_eq!(status, InferredStatus::AwaitingInput);
    }

    #[test]
    fn detect_awaiting_input_esc() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.push_line("esc to interrupt".to_string());
        let status = engine.infer_status();
        assert_eq!(status, InferredStatus::AwaitingInput);
    }

    #[test]
    fn detect_context_percent() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.push_line("42% context used".to_string());
        engine.detect_context_percent();
        assert_eq!(engine.context_percent, Some(42.0));
    }

    #[test]
    fn detect_context_percent_with_decimal() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.push_line("87.5% context remaining".to_string());
        // Our regex captures the integer part before the dot.
        engine.detect_context_percent();
        assert_eq!(engine.context_percent, Some(87.0));
    }

    // ── OSC 633 state integration ───────────────────────────────────────

    #[test]
    fn command_state_executing_default_processing() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        // No output patterns — default to processing.
        let status = engine.infer_status();
        assert_eq!(status, InferredStatus::Processing);
    }

    #[test]
    fn command_state_input_awaiting() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Input);
        // No specific output patterns — default to awaiting.
        let status = engine.infer_status();
        assert_eq!(status, InferredStatus::AwaitingInput);
    }

    #[test]
    fn command_state_finished_idle() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Finished);
        // No specific output patterns — default to idle.
        let status = engine.infer_status();
        assert_eq!(status, InferredStatus::Idle);
    }

    // ── State file format ───────────────────────────────────────────────

    #[test]
    fn state_file_serialization() {
        let state = StateFile {
            session_id: "abc-123".to_string(),
            agent_type: Some("claude".to_string()),
            status: "tool_use".to_string(),
            current_tool: Some("Read".to_string()),
            working_dir: Some("/home/user/project".to_string()),
            last_updated: Some("2026-03-30T12:00:00Z".to_string()),
            pid: Some(12345),
            model: Some("claude-sonnet-4-20250514".to_string()),
            context_percent: Some(42.0),
            source: Some("terminal_inference".to_string()),
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        assert!(json.contains("\"session_id\": \"abc-123\""));
        assert!(json.contains("\"status\": \"tool_use\""));
        assert!(json.contains("\"current_tool\": \"Read\""));
        assert!(json.contains("\"source\": \"terminal_inference\""));
        assert!(json.contains("\"pid\": 12345"));
    }

    #[test]
    fn state_file_omits_none_fields() {
        let state = StateFile {
            session_id: "abc-123".to_string(),
            agent_type: Some("claude".to_string()),
            status: "idle".to_string(),
            current_tool: None,
            working_dir: None,
            last_updated: Some("2026-03-30T12:00:00Z".to_string()),
            pid: Some(12345),
            model: None,
            context_percent: None,
            source: Some("terminal_inference".to_string()),
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        assert!(!json.contains("current_tool"));
        assert!(!json.contains("working_dir"));
        assert!(!json.contains("model"));
        assert!(!json.contains("context_percent"));
    }

    // ── RFC 3339 timestamp ──────────────────────────────────────────────

    #[test]
    fn now_rfc3339_format() {
        let ts = now_rfc3339();
        // Should match YYYY-MM-DDTHH:MM:SSZ format.
        assert!(ts.len() == 20, "Unexpected timestamp length: {ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
    }

    // ── Feed bytes integration ──────────────────────────────────────────

    #[test]
    fn feed_bytes_extracts_lines() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.feed_bytes(b"Hello world\nSecond line\n");
        assert_eq!(engine.recent_lines.len(), 2);
        assert_eq!(engine.recent_lines[0], "Hello world");
        assert_eq!(engine.recent_lines[1], "Second line");
    }

    #[test]
    fn feed_bytes_with_ansi() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.feed_bytes(b"\x1b[32mGreen text\x1b[0m\n");
        assert_eq!(engine.recent_lines.len(), 1);
        assert_eq!(engine.recent_lines[0], "Green text");
    }

    #[test]
    fn feed_bytes_accumulates_partial_lines() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.feed_bytes(b"partial");
        assert_eq!(engine.recent_lines.len(), 0);
        assert_eq!(engine.current_line, "partial");
        engine.feed_bytes(b" line\n");
        assert_eq!(engine.recent_lines.len(), 1);
        assert_eq!(engine.recent_lines[0], "partial line");
    }

    #[test]
    fn ring_buffer_eviction() {
        let mut engine = make_engine(Some(AgentType::Claude));
        for i in 0..60 {
            engine.push_line(format!("line {i}"));
        }
        assert_eq!(engine.recent_lines.len(), MAX_RECENT_LINES);
        // Oldest lines should be evicted.
        assert_eq!(engine.recent_lines[0], "line 10");
        assert_eq!(
            engine.recent_lines[MAX_RECENT_LINES - 1],
            "line 59"
        );
    }

    // ── Days to YMD ─────────────────────────────────────────────────────

    #[test]
    fn epoch_day_zero() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn known_date() {
        // 2026-03-30 = day 20542 from epoch
        // Let's verify a simpler one: 2000-01-01 = day 10957
        assert_eq!(days_to_ymd(10957), (2000, 1, 1));
    }
}
