//! Transcript filtering for thermal-voice.
//!
//! Filters out noise, Whisper hallucinations, and low-quality transcripts
//! before they are dispatched as voice commands. Acts as a fallback safety
//! layer alongside wake word detection.

use tracing::info;

/// Known Whisper hallucination phrases. These are commonly produced when
/// the model is fed silence or background noise.
const HALLUCINATION_PHRASES: &[&str] = &[
    "thank you for watching",
    "thanks for watching",
    "subscribe",
    "like and subscribe",
    "please subscribe",
    "thank you for listening",
    "thanks for listening",
    "see you next time",
    "see you in the next",
    "goodbye",
    "good bye",
    "the end",
    "music",
    "applause",
    "laughter",
    "silence",
    "you",
    // Common Whisper artifacts when processing noise
    "...",
    "um",
    "uh",
    "hmm",
    "oh",
    "ah",
];

/// Minimum word count for a transcript to be considered valid.
const MIN_WORD_COUNT: usize = 2;

/// Result of filtering a transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterResult {
    /// Transcript is valid and should be processed.
    Accept(String),
    /// Transcript was rejected (reason included for logging).
    Reject(String),
}

/// Filter a transcript, returning Accept or Reject.
///
/// Checks performed:
/// 1. Empty/whitespace-only transcripts are rejected.
/// 2. Transcripts with fewer than MIN_WORD_COUNT words are rejected.
/// 3. Known Whisper hallucination phrases are rejected.
/// 4. Transcripts that are all-punctuation or all-filler are rejected.
pub fn filter_transcript(transcript: &str) -> FilterResult {
    let trimmed = transcript.trim();

    // Empty check
    if trimmed.is_empty() {
        return FilterResult::Reject("empty transcript".to_string());
    }

    // Strip common Whisper artifacts: leading/trailing punctuation, brackets
    let cleaned = trimmed
        .trim_matches(|c: char| c == '[' || c == ']' || c == '(' || c == ')')
        .trim();

    if cleaned.is_empty() {
        return FilterResult::Reject("only brackets/punctuation".to_string());
    }

    // Check against hallucination phrases (case-insensitive)
    let lower = cleaned.to_lowercase();
    // Strip trailing punctuation for matching
    let matchable = lower.trim_end_matches(|c: char| c.is_ascii_punctuation());

    for &phrase in HALLUCINATION_PHRASES {
        if matchable == phrase {
            return FilterResult::Reject(format!("hallucination: '{phrase}'"));
        }
    }

    // Word count check
    let word_count = cleaned.split_whitespace().count();
    if word_count < MIN_WORD_COUNT {
        return FilterResult::Reject(format!(
            "too few words ({word_count} < {MIN_WORD_COUNT}): '{cleaned}'"
        ));
    }

    // Check if it's just repeated filler words
    let filler_words: &[&str] = &["um", "uh", "hmm", "oh", "ah", "like", "so", "yeah"];
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    let filler_count = words
        .iter()
        .filter(|w| {
            let w_lower = w.to_lowercase();
            let w_clean = w_lower.trim_matches(|c: char| c.is_ascii_punctuation());
            filler_words.contains(&w_clean)
        })
        .count();
    if filler_count == words.len() {
        return FilterResult::Reject(format!("all filler words: '{cleaned}'"));
    }

    info!("transcript accepted: '{cleaned}' ({word_count} words)");
    FilterResult::Accept(cleaned.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_transcript_rejected() {
        assert_eq!(
            filter_transcript(""),
            FilterResult::Reject("empty transcript".to_string())
        );
        assert_eq!(
            filter_transcript("   "),
            FilterResult::Reject("empty transcript".to_string())
        );
    }

    #[test]
    fn short_transcript_rejected() {
        match filter_transcript("hello") {
            FilterResult::Reject(reason) => assert!(reason.contains("too few words")),
            _ => panic!("expected rejection"),
        }
    }

    #[test]
    fn hallucination_rejected() {
        match filter_transcript("Thank you for watching") {
            FilterResult::Reject(reason) => assert!(reason.contains("hallucination")),
            _ => panic!("expected rejection"),
        }
        match filter_transcript("Subscribe.") {
            FilterResult::Reject(reason) => {
                // Could be hallucination or too few words
                assert!(
                    reason.contains("hallucination") || reason.contains("too few"),
                    "unexpected reason: {reason}"
                );
            }
            _ => panic!("expected rejection"),
        }
    }

    #[test]
    fn valid_transcript_accepted() {
        match filter_transcript("open the terminal window") {
            FilterResult::Accept(text) => {
                assert_eq!(text, "open the terminal window");
            }
            FilterResult::Reject(reason) => panic!("unexpected rejection: {reason}"),
        }
    }

    #[test]
    fn bracketed_noise_rejected() {
        match filter_transcript("[Music]") {
            FilterResult::Reject(_) => {} // Expected
            _ => panic!("expected rejection"),
        }
    }

    #[test]
    fn filler_words_rejected() {
        match filter_transcript("um uh like so") {
            FilterResult::Reject(reason) => assert!(reason.contains("filler")),
            _ => panic!("expected rejection"),
        }
    }

    #[test]
    fn three_words_accepted() {
        match filter_transcript("play some music") {
            FilterResult::Accept(text) => assert_eq!(text, "play some music"),
            FilterResult::Reject(reason) => panic!("unexpected rejection: {reason}"),
        }
    }

    #[test]
    fn hallucination_case_insensitive() {
        match filter_transcript("THANK YOU FOR WATCHING") {
            FilterResult::Reject(reason) => assert!(reason.contains("hallucination")),
            _ => panic!("expected rejection"),
        }
    }

    #[test]
    fn two_word_commands_accepted() {
        match filter_transcript("open Firefox") {
            FilterResult::Accept(text) => assert_eq!(text, "open Firefox"),
            FilterResult::Reject(reason) => panic!("unexpected rejection: {reason}"),
        }
        match filter_transcript("mute audio") {
            FilterResult::Accept(text) => assert_eq!(text, "mute audio"),
            FilterResult::Reject(reason) => panic!("unexpected rejection: {reason}"),
        }
        match filter_transcript("yes no") {
            FilterResult::Accept(text) => assert_eq!(text, "yes no"),
            FilterResult::Reject(reason) => panic!("unexpected rejection: {reason}"),
        }
    }

    #[test]
    fn hallucination_with_trailing_punctuation() {
        match filter_transcript("Thank you for watching.") {
            FilterResult::Reject(reason) => assert!(reason.contains("hallucination")),
            _ => panic!("expected rejection"),
        }
    }
}
