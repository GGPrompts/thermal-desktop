//! Intelligence escalation — classifies voice transcripts by complexity
//! to route simple commands to Haiku and complex requests to Sonnet.

/// Model used for simple, fast-path commands (status queries, single tool calls).
pub const BASE_MODEL: &str = "claude-haiku-4-20250414";

/// Model used for complex, multi-step requests (planning, analysis, code generation).
pub const ESCALATION_MODEL: &str = "claude-sonnet-4-6-20250514";

/// Minimum available memory (in GB) required to spawn a new agent.
pub const MIN_SPAWN_MEMORY_GB: f64 = 4.0;

/// Complexity classification for a voice transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComplexityLevel {
    /// Single tool call, status query, window management, short command.
    /// Routes to Haiku for speed.
    Simple,
    /// Multi-step request, planning, analysis, code generation.
    /// Routes to Sonnet for reasoning quality.
    Complex,
}

/// Keywords/phrases that signal a complex, multi-step request.
const COMPLEX_KEYWORDS: &[&str] = &[
    "and then",
    "after that",
    "first do",
    "then do",
    "step by step",
    "plan",
    "analyze",
    "analyse",
    "review",
    "refactor",
    "rewrite",
    "redesign",
    "architect",
    "implement",
    "build out",
    "set up",
    "debug",
    "diagnose",
    "investigate",
    "compare",
    "evaluate",
    "summarize the codebase",
    "explain the architecture",
    "code generation",
    "generate code",
    "write a function",
    "write a module",
    "write a test",
    "create a plan",
    "think about",
    "figure out",
];

/// Phrases that, when combined with "agent" or "spawn", indicate complex orchestration.
const SPAWN_QUALIFIERS: &[&str] = &[
    "multiple",
    "several",
    "three",
    "four",
    "five",
    "many",
    "each",
    "parallel",
    "coordinate",
    "orchestrate",
    "tournament",
    "consensus",
    "fan out",
    "fan-out",
];

/// Word count threshold — transcripts above this are likely complex.
const COMPLEX_WORD_THRESHOLD: usize = 20;

/// Classify a voice transcript by complexity to select the appropriate model.
///
/// Defaults to `Simple` (Haiku fast path) when in doubt.
pub fn classify_complexity(transcript: &str) -> ComplexityLevel {
    let lower = transcript.to_lowercase();
    let word_count = transcript.split_whitespace().count();

    // Check for explicit complex keywords/phrases
    for keyword in COMPLEX_KEYWORDS {
        if lower.contains(keyword) {
            return ComplexityLevel::Complex;
        }
    }

    // Check for qualified agent/spawn requests (e.g., "spawn multiple agents")
    let has_agent_word = lower.contains("agent") || lower.contains("spawn");
    if has_agent_word {
        for qualifier in SPAWN_QUALIFIERS {
            if lower.contains(qualifier) {
                return ComplexityLevel::Complex;
            }
        }
    }

    // Long transcripts are likely complex
    if word_count > COMPLEX_WORD_THRESHOLD {
        return ComplexityLevel::Complex;
    }

    // Default: simple (fast path)
    ComplexityLevel::Simple
}

/// Select the model string for a given complexity level.
pub fn model_for(level: ComplexityLevel) -> &'static str {
    match level {
        ComplexityLevel::Simple => BASE_MODEL,
        ComplexityLevel::Complex => ESCALATION_MODEL,
    }
}

/// Read available memory from /proc/meminfo and return the value in GB.
///
/// Returns `None` if /proc/meminfo cannot be read or parsed (e.g., on non-Linux).
pub fn available_memory_gb() -> Option<f64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in content.lines() {
        if line.starts_with("MemAvailable:") {
            // Format: "MemAvailable:    1234567 kB"
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let kb: f64 = parts[1].parse().ok()?;
                return Some(kb / 1_048_576.0); // kB to GB
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Simple commands
    // -----------------------------------------------------------------------

    #[test]
    fn short_status_query_is_simple() {
        assert_eq!(classify_complexity("check status"), ComplexityLevel::Simple);
    }

    #[test]
    fn window_management_is_simple() {
        assert_eq!(
            classify_complexity("move window left"),
            ComplexityLevel::Simple
        );
    }

    #[test]
    fn single_tool_command_is_simple() {
        assert_eq!(
            classify_complexity("open firefox"),
            ComplexityLevel::Simple
        );
    }

    #[test]
    fn create_issue_is_simple() {
        assert_eq!(
            classify_complexity("create issue fix the bug"),
            ComplexityLevel::Simple
        );
    }

    #[test]
    fn take_screenshot_is_simple() {
        assert_eq!(
            classify_complexity("take a screenshot"),
            ComplexityLevel::Simple
        );
    }

    #[test]
    fn list_windows_is_simple() {
        assert_eq!(
            classify_complexity("list all windows"),
            ComplexityLevel::Simple
        );
    }

    #[test]
    fn spawn_one_agent_is_simple() {
        assert_eq!(
            classify_complexity("spawn a claude session"),
            ComplexityLevel::Simple
        );
    }

    // -----------------------------------------------------------------------
    // Complex commands — keyword triggers
    // -----------------------------------------------------------------------

    #[test]
    fn plan_keyword_is_complex() {
        assert_eq!(
            classify_complexity("plan the new feature"),
            ComplexityLevel::Complex
        );
    }

    #[test]
    fn analyze_keyword_is_complex() {
        assert_eq!(
            classify_complexity("analyze this codebase"),
            ComplexityLevel::Complex
        );
    }

    #[test]
    fn review_keyword_is_complex() {
        assert_eq!(
            classify_complexity("review the pull request"),
            ComplexityLevel::Complex
        );
    }

    #[test]
    fn refactor_keyword_is_complex() {
        assert_eq!(
            classify_complexity("refactor the dispatcher module"),
            ComplexityLevel::Complex
        );
    }

    #[test]
    fn and_then_phrase_is_complex() {
        assert_eq!(
            classify_complexity("open firefox and then take a screenshot"),
            ComplexityLevel::Complex
        );
    }

    #[test]
    fn after_that_phrase_is_complex() {
        assert_eq!(
            classify_complexity("create an issue after that assign it to me"),
            ComplexityLevel::Complex
        );
    }

    #[test]
    fn implement_keyword_is_complex() {
        assert_eq!(
            classify_complexity("implement the new voice pipeline"),
            ComplexityLevel::Complex
        );
    }

    #[test]
    fn debug_keyword_is_complex() {
        assert_eq!(
            classify_complexity("debug the failing test"),
            ComplexityLevel::Complex
        );
    }

    #[test]
    fn write_a_test_is_complex() {
        assert_eq!(
            classify_complexity("write a test for the dispatcher"),
            ComplexityLevel::Complex
        );
    }

    // -----------------------------------------------------------------------
    // Complex commands — qualified agent/spawn
    // -----------------------------------------------------------------------

    #[test]
    fn spawn_multiple_agents_is_complex() {
        assert_eq!(
            classify_complexity("spawn multiple agents on this project"),
            ComplexityLevel::Complex
        );
    }

    #[test]
    fn orchestrate_agents_is_complex() {
        assert_eq!(
            classify_complexity("orchestrate agents for the refactor"),
            ComplexityLevel::Complex
        );
    }

    #[test]
    fn fan_out_spawn_is_complex() {
        assert_eq!(
            classify_complexity("fan out and spawn agents for each module"),
            ComplexityLevel::Complex
        );
    }

    // -----------------------------------------------------------------------
    // Complex commands — word count threshold
    // -----------------------------------------------------------------------

    #[test]
    fn long_transcript_is_complex() {
        let long = "do this thing and also that other thing and make sure you handle all the edge cases and test everything thoroughly and report back";
        assert!(long.split_whitespace().count() > COMPLEX_WORD_THRESHOLD);
        assert_eq!(classify_complexity(long), ComplexityLevel::Complex);
    }

    // -----------------------------------------------------------------------
    // Case insensitivity
    // -----------------------------------------------------------------------

    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(
            classify_complexity("PLAN the deployment"),
            ComplexityLevel::Complex
        );
        assert_eq!(
            classify_complexity("Analyze The System"),
            ComplexityLevel::Complex
        );
    }

    // -----------------------------------------------------------------------
    // model_for
    // -----------------------------------------------------------------------

    #[test]
    fn simple_routes_to_haiku() {
        assert_eq!(model_for(ComplexityLevel::Simple), BASE_MODEL);
        assert!(model_for(ComplexityLevel::Simple).contains("haiku"));
    }

    #[test]
    fn complex_routes_to_sonnet() {
        assert_eq!(model_for(ComplexityLevel::Complex), ESCALATION_MODEL);
        assert!(model_for(ComplexityLevel::Complex).contains("sonnet"));
    }

    // -----------------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------------

    #[test]
    fn min_spawn_memory_is_four_gb() {
        assert!((MIN_SPAWN_MEMORY_GB - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn base_model_is_haiku() {
        assert!(BASE_MODEL.contains("haiku"));
    }

    #[test]
    fn escalation_model_is_sonnet() {
        assert!(ESCALATION_MODEL.contains("sonnet"));
    }

    // -----------------------------------------------------------------------
    // available_memory_gb — best-effort (works on Linux CI, no-op elsewhere)
    // -----------------------------------------------------------------------

    #[test]
    fn available_memory_returns_some_on_linux() {
        if cfg!(target_os = "linux") {
            let mem = available_memory_gb();
            assert!(mem.is_some(), "/proc/meminfo should be readable on Linux");
            let gb = mem.unwrap();
            assert!(gb > 0.0, "available memory should be positive");
        }
    }
}
