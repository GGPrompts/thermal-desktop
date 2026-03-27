//! Multi-turn conversational context for the dispatcher.
//!
//! Maintains a rolling window of recent user/assistant turn pairs so that
//! Haiku can resolve follow-up commands ("create issue for X" → "now assign
//! it to me") without a fresh context every time.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Maximum number of historical turn pairs to keep.
const MAX_TURN_PAIRS: usize = 8;

/// Session timeout — if no interaction for this long, history is cleared.
const SESSION_TIMEOUT: Duration = Duration::from_secs(120);

/// A single conversational turn: what the user said + what the assistant replied.
#[derive(Clone, Debug)]
struct Turn {
    user: String,
    assistant: String,
}

/// Rolling conversation context that persists across `dispatch_command()` calls.
#[derive(Debug)]
pub struct ConversationContext {
    turns: Vec<Turn>,
    last_interaction: Instant,
}

impl ConversationContext {
    /// Create a new, empty context.
    pub fn new() -> Self {
        Self {
            turns: Vec::new(),
            last_interaction: Instant::now(),
        }
    }

    /// Returns `true` if the session has been idle longer than `SESSION_TIMEOUT`.
    pub fn is_expired(&self) -> bool {
        self.last_interaction.elapsed() > SESSION_TIMEOUT
    }

    /// Clear all history and reset the timer.
    pub fn reset(&mut self) {
        self.turns.clear();
        self.last_interaction = Instant::now();
    }

    /// Record a completed turn (user transcript + assistant response text).
    pub fn add_turn(&mut self, user_text: &str, assistant_text: &str) {
        self.turns.push(Turn {
            user: user_text.to_string(),
            assistant: assistant_text.to_string(),
        });
        // Drop oldest turns if we exceed the cap
        if self.turns.len() > MAX_TURN_PAIRS {
            let excess = self.turns.len() - MAX_TURN_PAIRS;
            self.turns.drain(..excess);
        }
    }

    /// Update `last_interaction` to now.
    pub fn touch(&mut self) {
        self.last_interaction = Instant::now();
    }

    /// Build the full Anthropic `messages` array including historical context
    /// and the current user transcript.
    ///
    /// Layout:
    ///   [ {user: turn1} , {assistant: turn1} , ... , {user: current_transcript} ]
    ///
    /// The system prompt is handled separately by `api::call_haiku` (via the
    /// `"system"` field), so it is **not** included here.
    pub fn build_messages(&self, current_transcript: &str) -> Vec<Value> {
        let mut messages = Vec::with_capacity(self.turns.len() * 2 + 1);

        for turn in &self.turns {
            messages.push(json!({
                "role": "user",
                "content": turn.user,
            }));
            messages.push(json!({
                "role": "assistant",
                "content": turn.assistant,
            }));
        }

        messages.push(json!({
            "role": "user",
            "content": current_transcript,
        }));

        messages
    }

    /// Number of stored turn pairs.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.turns.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn new_context_is_empty_and_not_expired() {
        let ctx = ConversationContext::new();
        assert_eq!(ctx.len(), 0);
        assert!(!ctx.is_expired());
    }

    #[test]
    fn add_turn_stores_pair() {
        let mut ctx = ConversationContext::new();
        ctx.add_turn("open firefox", "Opening Firefox now.");
        assert_eq!(ctx.len(), 1);
    }

    #[test]
    fn reset_clears_history() {
        let mut ctx = ConversationContext::new();
        ctx.add_turn("a", "b");
        ctx.add_turn("c", "d");
        ctx.reset();
        assert_eq!(ctx.len(), 0);
    }

    #[test]
    fn build_messages_empty_history_has_single_user_message() {
        let ctx = ConversationContext::new();
        let msgs = ctx.build_messages("hello");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "hello");
    }

    #[test]
    fn build_messages_includes_history_then_current() {
        let mut ctx = ConversationContext::new();
        ctx.add_turn("first", "reply one");
        ctx.add_turn("second", "reply two");

        let msgs = ctx.build_messages("third");
        // 2 turns * 2 messages + 1 current = 5
        assert_eq!(msgs.len(), 5);

        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "first");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"], "reply one");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"], "second");
        assert_eq!(msgs[3]["role"], "assistant");
        assert_eq!(msgs[3]["content"], "reply two");
        assert_eq!(msgs[4]["role"], "user");
        assert_eq!(msgs[4]["content"], "third");
    }

    #[test]
    fn oldest_turns_dropped_when_exceeding_cap() {
        let mut ctx = ConversationContext::new();
        for i in 0..12 {
            ctx.add_turn(&format!("q{i}"), &format!("a{i}"));
        }
        assert_eq!(ctx.len(), MAX_TURN_PAIRS);
        // Oldest should be turn 4 (indices 0..3 were dropped)
        let msgs = ctx.build_messages("current");
        assert_eq!(msgs[0]["content"], "q4");
    }

    #[test]
    fn touch_updates_last_interaction() {
        let mut ctx = ConversationContext::new();
        // Burn a tiny bit of time
        thread::sleep(Duration::from_millis(5));
        let before = ctx.last_interaction;
        ctx.touch();
        assert!(ctx.last_interaction > before);
    }

    #[test]
    fn is_expired_after_timeout() {
        let mut ctx = ConversationContext::new();
        // Manually backdate the last interaction
        ctx.last_interaction = Instant::now() - SESSION_TIMEOUT - Duration::from_secs(1);
        assert!(ctx.is_expired());
    }

    #[test]
    fn is_not_expired_within_timeout() {
        let ctx = ConversationContext::new();
        assert!(!ctx.is_expired());
    }

    #[test]
    fn reset_after_expiry_clears_and_refreshes() {
        let mut ctx = ConversationContext::new();
        ctx.add_turn("old", "stale");
        ctx.last_interaction = Instant::now() - SESSION_TIMEOUT - Duration::from_secs(1);
        assert!(ctx.is_expired());
        ctx.reset();
        assert!(!ctx.is_expired());
        assert_eq!(ctx.len(), 0);
    }
}
