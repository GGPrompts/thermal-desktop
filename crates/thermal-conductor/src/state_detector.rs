use std::time::Instant;

/// Detects the state of an agent from its terminal output
pub struct StateDetector {
    last_output_time: Instant,
    last_line_count: usize,
    current_state: DetectedState,
    error_patterns: Vec<String>,
    prompt_patterns: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedState {
    Idle,       // Prompt visible, no output
    Running,    // Active output streaming
    Thinking,   // No output but process active (>2s since last output)
    Error,      // Error pattern detected
    Complete,   // Task finished (prompt returned after activity)
}

impl StateDetector {
    pub fn new() -> Self {
        Self {
            last_output_time: Instant::now(),
            last_line_count: 0,
            current_state: DetectedState::Idle,
            error_patterns: vec![
                "error".to_string(),
                "Error".to_string(),
                "ERROR".to_string(),
                "FAILED".to_string(),
                "panic".to_string(),
                "PANIC".to_string(),
                "fatal".to_string(),
                "Fatal".to_string(),
            ],
            prompt_patterns: vec![
                "$ ".to_string(),
                "❯ ".to_string(),
                "% ".to_string(),
                ">>> ".to_string(),
                "# ".to_string(),
            ],
        }
    }

    /// Analyze new terminal content and return the detected state
    pub fn analyze(&mut self, content: &str) -> DetectedState {
        let lines: Vec<&str> = content.lines().collect();
        let line_count = lines.len();
        let now = Instant::now();

        // Check if output has changed
        let output_changed = line_count != self.last_line_count;

        if output_changed {
            self.last_output_time = now;
            self.last_line_count = line_count;
        }

        let elapsed = now.duration_since(self.last_output_time);
        let last_line = lines.last().map(|l| l.trim()).unwrap_or("");

        // Strip ANSI escape sequences for pattern matching
        let clean_last_line = strip_ansi(last_line);

        // Detect state based on patterns
        let new_state = if self.has_error_pattern(&lines) {
            DetectedState::Error
        } else if self.is_prompt(&clean_last_line) {
            if self.current_state == DetectedState::Running
               || self.current_state == DetectedState::Thinking {
                DetectedState::Complete  // Was active, now showing prompt = just finished
            } else {
                DetectedState::Idle
            }
        } else if output_changed {
            DetectedState::Running
        } else if elapsed.as_secs() > 2 {
            DetectedState::Thinking
        } else {
            self.current_state  // No change
        };

        // Complete state auto-transitions to Idle after 3 seconds
        if self.current_state == DetectedState::Complete && elapsed.as_secs() > 3 {
            self.current_state = DetectedState::Idle;
            return DetectedState::Idle;
        }

        self.current_state = new_state;
        new_state
    }

    /// Check if the last few lines contain error patterns
    fn has_error_pattern(&self, lines: &[&str]) -> bool {
        // Only check last 5 lines to avoid false positives from scrollback
        let check_lines = if lines.len() > 5 { &lines[lines.len()-5..] } else { lines };
        for line in check_lines {
            let clean = strip_ansi(line);
            for pattern in &self.error_patterns {
                if clean.contains(pattern.as_str()) {
                    return true;
                }
            }
        }
        false
    }

    /// Check if a line looks like a shell prompt
    fn is_prompt(&self, line: &str) -> bool {
        for pattern in &self.prompt_patterns {
            if line.ends_with(pattern.trim()) || line.contains(pattern.as_str()) {
                return true;
            }
        }
        false
    }

    /// Get the current state
    pub fn state(&self) -> DetectedState {
        self.current_state
    }

    /// Add custom error patterns
    pub fn add_error_pattern(&mut self, pattern: String) {
        self.error_patterns.push(pattern);
    }

    /// Add custom prompt patterns
    pub fn add_prompt_pattern(&mut self, pattern: String) {
        self.prompt_patterns.push(pattern);
    }
}

/// Strip ANSI escape sequences from a string
pub fn strip_ansi(s: &str) -> String {
    // Simple ANSI stripper: remove ESC[...m and ESC[...other sequences
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip ESC and the following sequence
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                // Read until we hit a letter (the command)
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_alphabetic() || next == 'm' {
                        break;
                    }
                }
            }
        } else {
            result.push(c);
        }
    }

    result
}

impl Default for StateDetector {
    fn default() -> Self {
        Self::new()
    }
}
