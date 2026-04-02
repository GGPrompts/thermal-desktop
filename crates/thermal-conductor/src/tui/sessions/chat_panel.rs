//! Chat panel: bus connection, @-mention parsing, message routing, autocomplete.

use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::Instant;

use thermal_core::ClaudeSessionState;
use thermal_core::message::{AgentId, Message, MessageType};

use super::SessionsPage;

// ---------------------------------------------------------------------------
// Messages socket path
// ---------------------------------------------------------------------------

fn messages_socket_path() -> std::path::PathBuf {
    let uid = nix::unistd::getuid().as_raw();
    std::path::PathBuf::from(format!("/run/user/{uid}/thermal/messages.sock"))
}

// ---------------------------------------------------------------------------
// Bus connection (subscriber)
// ---------------------------------------------------------------------------

/// Non-blocking connection to the messages daemon.
pub(in crate::tui) struct BusConnection {
    reader: BufReader<UnixStream>,
    line_buf: String,
}

impl BusConnection {
    /// Attempt to connect and send a Subscribe message.
    pub(super) fn connect(since_seq: u64) -> Option<Self> {
        let path = messages_socket_path();
        let stream = UnixStream::connect(&path).ok()?;
        stream.set_nonblocking(true).ok()?;

        let subscribe = Message {
            seq: 0,
            ts: 0,
            from: AgentId::new("user", "tui"),
            to: AgentId::new("daemon", "bus"),
            context_id: None,
            project: None,
            content: String::new(),
            msg_type: MessageType::Subscribe {
                since_seq: if since_seq > 0 { Some(since_seq) } else { None },
            },
            metadata: Default::default(),
        };

        let mut json = serde_json::to_string(&subscribe).ok()?;
        json.push('\n');

        stream.set_nonblocking(false).ok()?;
        let mut write_stream = stream.try_clone().ok()?;
        write_stream.write_all(json.as_bytes()).ok()?;
        stream.set_nonblocking(true).ok()?;

        Some(Self {
            reader: BufReader::new(stream),
            line_buf: String::new(),
        })
    }

    /// Try to read any available messages (non-blocking).
    pub(super) fn poll(&mut self) -> Vec<Message> {
        let mut msgs = Vec::new();
        loop {
            self.line_buf.clear();
            match self.reader.read_line(&mut self.line_buf) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = self.line_buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(msg) = serde_json::from_str::<Message>(trimmed) {
                        msgs.push(msg);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        msgs
    }
}

// ---------------------------------------------------------------------------
// @-mention parsing
// ---------------------------------------------------------------------------

/// Valid target agent types for @-mentions.
pub(super) const VALID_MENTION_TARGETS: &[&str] =
    &["system", "claude", "codex", "planner", "user", "dispatcher"];

/// Parse @-mention with live session display names for resolution.
pub(super) fn parse_at_mention_with_sessions(
    text: &str,
    sessions: &[ClaudeSessionState],
) -> (Option<AgentId>, String) {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix('@') {
        let (mention, content) = match rest.find(char::is_whitespace) {
            Some(pos) => (&rest[..pos], rest[pos..].trim_start()),
            None => (rest, ""),
        };
        let mention_lower = mention.to_lowercase();

        if VALID_MENTION_TARGETS.contains(&mention_lower.as_str()) {
            return (
                Some(AgentId::new(mention_lower, "default")),
                content.to_string(),
            );
        }

        for s in sessions {
            let display = s.model_display_name().to_lowercase();
            if display == mention_lower {
                let agent_type = s.agent_type.as_deref().unwrap_or("claude");
                return (
                    Some(AgentId::new(agent_type, &s.session_id)),
                    content.to_string(),
                );
            }
        }
    }
    (None, trimmed.to_string())
}

/// Send a message to the message bus via messages.sock.
pub(super) fn send_to_message_bus(text: &str, target: Option<&AgentId>) -> bool {
    let path = messages_socket_path();
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_nonblocking(false);

    let to = match target {
        Some(id) => id.clone(),
        None => AgentId::new("*", "broadcast"),
    };

    let msg = Message {
        seq: 0,
        ts: 0,
        from: AgentId::new("user", "tui"),
        to,
        context_id: None,
        project: None,
        content: text.to_string(),
        msg_type: MessageType::AgentMsg,
        metadata: Default::default(),
    };
    let mut json = match serde_json::to_string(&msg) {
        Ok(j) => j,
        Err(_) => return false,
    };
    json.push('\n');
    stream.write_all(json.as_bytes()).is_ok()
}

/// Maximum recent messages to display in the chat area.
pub(super) const MAX_RECENT_MESSAGES: usize = 50;

/// A recent message displayed in the inline chat area.
pub(in crate::tui) struct ChatEntry {
    pub(super) from_label: String,
    pub(super) content: String,
    pub(super) timestamp: Instant,
}

// ---------------------------------------------------------------------------
// SessionsPage chat methods
// ---------------------------------------------------------------------------

impl SessionsPage {
    /// Attempt to connect to the message bus (throttled to every 3 seconds).
    pub(super) fn try_bus_connect(&mut self) {
        if let Some(last) = self.last_bus_connect_attempt {
            if last.elapsed().as_secs() < 3 {
                return;
            }
        }
        self.last_bus_connect_attempt = Some(Instant::now());

        if let Some(conn) = BusConnection::connect(self.last_bus_seq) {
            self.bus_connection = Some(conn);
        }
    }

    /// Poll the bus connection for incoming messages and add them to chat_messages.
    pub(super) fn poll_bus_messages(&mut self) {
        let msgs = if let Some(ref mut conn) = self.bus_connection {
            let msgs = conn.poll();
            if msgs.is_empty() {
                return;
            }
            msgs
        } else {
            return;
        };

        for msg in msgs {
            if msg.seq > self.last_bus_seq {
                self.last_bus_seq = msg.seq;
            }
            match &msg.msg_type {
                MessageType::Subscribe { .. } | MessageType::Ack { .. } => continue,
                _ => {}
            }
            if msg.from.agent_type == "user" && msg.from.key == "tui" {
                continue;
            }
            let entry = ChatEntry {
                from_label: format!("{}/{}", msg.from.agent_type, msg.from.key),
                content: msg.content.clone(),
                timestamp: Instant::now(),
            };
            self.chat_messages.push_back(entry);
        }

        while self.chat_messages.len() > MAX_RECENT_MESSAGES {
            self.chat_messages.pop_front();
        }
    }

    pub(super) fn send_chat_input(&mut self) {
        let text = self.chat_input.trim().to_string();
        if text.is_empty() {
            return;
        }

        let (mention_target, cleaned_content) =
            parse_at_mention_with_sessions(&text, &self.sessions);

        if let Some(ref target) = mention_target {
            let label = format!("you \u{2192} @{}", target);
            let ok = send_to_message_bus(&cleaned_content, Some(target));
            let entry = ChatEntry {
                from_label: label,
                content: cleaned_content.clone(),
                timestamp: Instant::now(),
            };
            self.chat_messages.push_back(entry);
            if !ok {
                self.chat_status =
                    Some(("Failed to send to message bus".into(), true, Instant::now()));
            }
        } else {
            let selected_targets: Vec<(String, String)> = if !self.selected_set.is_empty() {
                self.selected_set
                    .iter()
                    .filter_map(|&i| {
                        self.display_rows.get(i).and_then(|row| {
                            row.session
                                .working_dir
                                .clone()
                                .map(|cwd| (cwd, row.session.model_display_name()))
                        })
                    })
                    .collect()
            } else if let Some(i) = self.table_state.selected() {
                self.display_rows
                    .get(i)
                    .and_then(|row| {
                        row.session
                            .working_dir
                            .clone()
                            .map(|cwd| vec![(cwd, row.session.model_display_name())])
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            if selected_targets.is_empty() {
                let ok = send_to_message_bus(&text, None);
                let entry = ChatEntry {
                    from_label: "you \u{2192} bus".to_string(),
                    content: text.clone(),
                    timestamp: Instant::now(),
                };
                self.chat_messages.push_back(entry);
                if !ok {
                    self.chat_status =
                        Some(("Failed to send to message bus".into(), true, Instant::now()));
                }
            } else {
                let text_with_enter = format!("{}\r", text);
                let mut success_count = 0;
                let mut target_label = String::new();
                for (cwd, label) in &selected_targets {
                    if let Some((socket, wid)) = self.resolve_kitty_window(cwd) {
                        let match_arg = format!("id:{wid}");
                        let ok = Command::new("kitty")
                            .args([
                                "@",
                                "--to",
                                &socket,
                                "send-text",
                                "--match",
                                &match_arg,
                                "--",
                            ])
                            .arg(&text_with_enter)
                            .output()
                            .map(|o| o.status.success())
                            .unwrap_or(false);
                        if ok {
                            success_count += 1;
                        }
                    }
                    if target_label.is_empty() {
                        target_label = label.clone();
                    }
                }
                if selected_targets.len() > 1 {
                    target_label = format!("{} sessions", selected_targets.len());
                }
                let entry = ChatEntry {
                    from_label: format!("you \u{2192} {}", target_label),
                    content: text.clone(),
                    timestamp: Instant::now(),
                };
                self.chat_messages.push_back(entry);
                if success_count < selected_targets.len() {
                    self.chat_status = Some((
                        format!(
                            "Sent to {}/{} sessions",
                            success_count,
                            selected_targets.len()
                        ),
                        true,
                        Instant::now(),
                    ));
                }
            }
        }

        while self.chat_messages.len() > MAX_RECENT_MESSAGES {
            self.chat_messages.pop_front();
        }

        self.chat_history.push_front(text);
        if self.chat_history.len() > 100 {
            self.chat_history.pop_back();
        }
        self.chat_history_index = None;

        self.chat_input.clear();
        self.chat_cursor = 0;
    }

    pub(super) fn chat_handle_char(&mut self, ch: char) {
        self.chat_input.insert(self.chat_cursor, ch);
        self.chat_cursor += ch.len_utf8();
    }

    pub(super) fn chat_handle_backspace(&mut self) {
        if self.chat_cursor > 0 {
            let prev = self.chat_input[..self.chat_cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.chat_input.replace_range(prev..self.chat_cursor, "");
            self.chat_cursor = prev;
        }
    }

    /// Build the full list of @-mention completion candidates.
    pub(super) fn build_autocomplete(&self) -> Vec<String> {
        let mut items: Vec<String> = VALID_MENTION_TARGETS
            .iter()
            .map(|s| s.to_string())
            .collect();

        let mut seen = std::collections::HashSet::new();
        for row in &self.display_rows {
            let name = row.session.model_display_name();
            if seen.insert(name.clone()) {
                items.push(name);
            }
        }

        items.sort();
        items.dedup();
        items
    }

    /// Update autocomplete state based on current chat_input and cursor position.
    pub(super) fn update_autocomplete(&mut self) {
        let before_cursor = &self.chat_input[..self.chat_cursor];
        if let Some(at_pos) = before_cursor.rfind('@') {
            let after_at = &before_cursor[at_pos + 1..];
            if after_at.contains(' ') {
                self.autocomplete_active = false;
                return;
            }
            let prefix = after_at.to_lowercase();
            let candidates = self.build_autocomplete();
            let filtered: Vec<String> = candidates
                .into_iter()
                .filter(|item| item.to_lowercase().starts_with(&prefix))
                .collect();

            if filtered.is_empty() {
                self.autocomplete_active = false;
            } else {
                self.autocomplete_items = filtered;
                self.autocomplete_index = self
                    .autocomplete_index
                    .min(self.autocomplete_items.len().saturating_sub(1));
                self.autocomplete_active = true;
            }
        } else {
            self.autocomplete_active = false;
        }
    }

    /// Accept the currently selected autocomplete item.
    pub(super) fn accept_autocomplete(&mut self) {
        if !self.autocomplete_active || self.autocomplete_items.is_empty() {
            return;
        }
        let completion = self.autocomplete_items[self.autocomplete_index].clone();

        let before_cursor = &self.chat_input[..self.chat_cursor];
        if let Some(at_pos) = before_cursor.rfind('@') {
            let after_cursor = &self.chat_input[self.chat_cursor..];
            let new_input = format!(
                "{}@{} {}",
                &self.chat_input[..at_pos],
                completion,
                after_cursor,
            );
            self.chat_cursor = at_pos + 1 + completion.len() + 1;
            self.chat_input = new_input;
        }

        self.autocomplete_active = false;
    }
}
