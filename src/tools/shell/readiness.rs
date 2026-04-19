//! Output readiness detection — determines when a shell is waiting for input.
//!
//! Three strategies for detecting that a shell process has finished producing
//! output and is waiting for the next command:
//!
//! - **Timeout**: pure silence-based — if N ms pass with no output, we assume ready.
//! - **Prompt**: regex-based — scan the tail of output for known prompt patterns.
//! - **Hybrid**: try prompt detection first, fall back to silence timeout.
//!
//! The `check()` method is stateless and designed for polling loops — the caller
//! owns the timers, we just evaluate the current snapshot.

use std::time::Duration;

use regex::Regex;

use crate::tools::strip_ansi;

use super::config::ShellConfig;

// ---------------------------------------------------------------------------
// Strategy & result enums
// ---------------------------------------------------------------------------

/// The strategy for detecting when a shell is ready for input.
#[derive(Debug, Clone)]
pub enum ReadinessStrategy {
    /// Wait for N ms of output silence
    Timeout,
    /// Wait until output matches a prompt regex
    Prompt,
    /// Prompt detection with timeout fallback (default)
    Hybrid,
}

impl ReadinessStrategy {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "timeout" => ReadinessStrategy::Timeout,
            "prompt" => ReadinessStrategy::Prompt,
            _ => ReadinessStrategy::Hybrid,
        }
    }
}

/// Result of a readiness check
#[derive(Debug, PartialEq)]
pub enum ReadinessResult {
    /// Output matched a prompt pattern — shell is waiting for input
    Ready,
    /// Still receiving output, keep waiting
    Waiting,
    /// Silence timeout expired — return what we have
    SilenceTimeout,
    /// Maximum total wait time exceeded
    MaxTimeout,
}

// ---------------------------------------------------------------------------
// Detector
// ---------------------------------------------------------------------------

/// Stateless readiness evaluator — called repeatedly in a polling loop.
pub struct ReadinessDetector {
    strategy: ReadinessStrategy,
    patterns: Vec<Regex>,
    silence_timeout: Duration,
    max_timeout: Duration,
}

/// How many chars from the tail of output to inspect for prompt matching.
const PROMPT_TAIL_LEN: usize = 200;

impl ReadinessDetector {
    /// Build a detector from raw parts.
    ///
    /// Invalid regex patterns are silently skipped (with a `tracing::warn`).
    /// If *no* patterns compile, the strategy is downgraded to `Timeout`.
    pub fn new(
        strategy: ReadinessStrategy,
        patterns_str: &[String],
        silence_timeout_ms: u64,
        max_timeout_ms: u64,
    ) -> Self {
        let patterns: Vec<Regex> = patterns_str
            .iter()
            .filter_map(|p| match Regex::new(p) {
                Ok(re) => Some(re),
                Err(e) => {
                    tracing::warn!(pattern = %p, error = %e, "skipping invalid prompt regex");
                    None
                }
            })
            .collect();

        // If we wanted prompt detection but have no usable patterns, fall back.
        let strategy = if patterns.is_empty() {
            match strategy {
                ReadinessStrategy::Prompt | ReadinessStrategy::Hybrid => {
                    tracing::warn!(
                        "no valid prompt patterns — falling back to Timeout strategy"
                    );
                    ReadinessStrategy::Timeout
                }
                other => other,
            }
        } else {
            strategy
        };

        Self {
            strategy,
            patterns,
            silence_timeout: Duration::from_millis(silence_timeout_ms),
            max_timeout: Duration::from_millis(max_timeout_ms),
        }
    }

    /// Build from the default `ShellConfig`.
    pub fn from_config(config: &ShellConfig) -> Self {
        let strategy = crate::tools::shell::readiness::ReadinessStrategy::from_str(&config.readiness_strategy);
        Self::new(
            strategy,
            &config.prompt_patterns,
            config.readiness_timeout_ms,
            config.max_readiness_timeout_ms,
        )
    }

    /// Evaluate the current output snapshot against the active strategy.
    ///
    /// Called in a tight poll loop — the caller tracks `silence_elapsed`
    /// (time since last new output byte) and `total_elapsed` (wall-clock since
    /// the command was sent).
    pub fn check(
        &self,
        output: &str,
        silence_elapsed: Duration,
        total_elapsed: Duration,
    ) -> ReadinessResult {
        // Hard ceiling — always wins.
        if total_elapsed >= self.max_timeout {
            return ReadinessResult::MaxTimeout;
        }

        match &self.strategy {
            ReadinessStrategy::Timeout => {
                if silence_elapsed >= self.silence_timeout {
                    ReadinessResult::SilenceTimeout
                } else {
                    ReadinessResult::Waiting
                }
            }
            ReadinessStrategy::Prompt => {
                if self.matches_prompt(output) {
                    ReadinessResult::Ready
                } else {
                    ReadinessResult::Waiting
                }
            }
            ReadinessStrategy::Hybrid => {
                if self.matches_prompt(output) {
                    ReadinessResult::Ready
                } else if silence_elapsed >= self.silence_timeout {
                    ReadinessResult::SilenceTimeout
                } else {
                    ReadinessResult::Waiting
                }
            }
        }
    }

    /// Check the tail of `output` for a known prompt pattern.
    ///
    /// Only inspects the last ~200 chars (prompts live at the end), and strips
    /// ANSI escapes before matching.  The match runs against the last non-empty
    /// line.
    pub fn matches_prompt(&self, output: &str) -> bool {
        if output.is_empty() || self.patterns.is_empty() {
            return false;
        }

        // Grab the tail to avoid scanning megabytes of scrollback.
        let tail = if output.len() > PROMPT_TAIL_LEN {
            &output[output.len() - PROMPT_TAIL_LEN..]
        } else {
            output
        };

        let clean = strip_ansi(tail);

        // Last non-empty line is where the prompt sits.
        let last_line = clean
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");

        if last_line.is_empty() {
            return false;
        }

        self.patterns.iter().any(|re| re.is_match(last_line))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a Timeout detector with sensible defaults.
    fn timeout_detector(silence_ms: u64) -> ReadinessDetector {
        ReadinessDetector::new(ReadinessStrategy::Timeout, &[], silence_ms, 10_000)
    }

    /// Helper: build a Prompt detector with the default pattern set.
    fn prompt_detector() -> ReadinessDetector {
        let config = ShellConfig::default();
        ReadinessDetector::new(
            ReadinessStrategy::Prompt,
            &config.prompt_patterns,
            config.readiness_timeout_ms,
            config.max_readiness_timeout_ms,
        )
    }

    /// Helper: build a Hybrid detector with the default pattern set.
    fn hybrid_detector() -> ReadinessDetector {
        ReadinessDetector::from_config(&ShellConfig::default())
    }

    // 1. Timeout: silence exceeds threshold → SilenceTimeout
    #[test]
    fn timeout_strategy_silence_triggers() {
        let det = timeout_detector(300);
        let result = det.check(
            "some output\n",
            Duration::from_millis(301),
            Duration::from_millis(500),
        );
        assert_eq!(result, ReadinessResult::SilenceTimeout);
    }

    // 1b. Timeout: silence below threshold → Waiting
    #[test]
    fn timeout_strategy_still_waiting() {
        let det = timeout_detector(300);
        let result = det.check(
            "some output\n",
            Duration::from_millis(100),
            Duration::from_millis(200),
        );
        assert_eq!(result, ReadinessResult::Waiting);
    }

    // 2. Prompt: output ending with `$ ` → Ready
    #[test]
    fn prompt_strategy_dollar_ready() {
        let det = prompt_detector();
        let result = det.check(
            "user@host:~$ ",
            Duration::from_millis(0),
            Duration::from_millis(50),
        );
        assert_eq!(result, ReadinessResult::Ready);
    }

    // 3. Prompt: output ending with `>>> ` (Python) → Ready
    #[test]
    fn prompt_strategy_python_ready() {
        let det = prompt_detector();
        let result = det.check(
            "Python 3.11.0\n>>> ",
            Duration::from_millis(0),
            Duration::from_millis(50),
        );
        assert_eq!(result, ReadinessResult::Ready);
    }

    // 4. Prompt: output NOT ending with prompt → still waiting (no silence fallback)
    #[test]
    fn prompt_strategy_no_match_silence_fallback() {
        let det = prompt_detector();
        // No prompt at the end, silence exceeded - but prompt strategy doesn't use silence
        let result = det.check(
            "compiling crate...\n",
            Duration::from_millis(500),
            Duration::from_millis(1000),
        );
        assert_eq!(result, ReadinessResult::Waiting);
    }

    // 4b. Prompt: no match, silence not exceeded → Waiting
    #[test]
    fn prompt_strategy_no_match_waiting() {
        let det = prompt_detector();
        let result = det.check(
            "compiling crate...\n",
            Duration::from_millis(100),
            Duration::from_millis(200),
        );
        assert_eq!(result, ReadinessResult::Waiting);
    }

    // 5. Hybrid: prompt match → Ready (before silence would trigger)
    #[test]
    fn hybrid_prompt_match_before_silence() {
        let det = hybrid_detector();
        let result = det.check(
            "welcome\nuser@host:~$ ",
            Duration::from_millis(10), // well below silence threshold
            Duration::from_millis(50),
        );
        assert_eq!(result, ReadinessResult::Ready);
    }

    // 6. Hybrid: no prompt match, silence elapsed → SilenceTimeout
    #[test]
    fn hybrid_silence_fallback() {
        let det = hybrid_detector();
        let result = det.check(
            "running long task...\n",
            Duration::from_millis(500),
            Duration::from_millis(1000),
        );
        assert_eq!(result, ReadinessResult::SilenceTimeout);
    }

    // 7. MaxTimeout always wins regardless of strategy
    #[test]
    fn max_timeout_always_wins() {
        for det in [timeout_detector(300), prompt_detector(), hybrid_detector()] {
            let result = det.check(
                "user@host:~$ ",
                Duration::from_millis(0),
                Duration::from_millis(10_001),
            );
            assert_eq!(result, ReadinessResult::MaxTimeout);
        }
    }

    // 8. matches_prompt against common prompts
    #[test]
    fn matches_prompt_common_patterns() {
        let det = hybrid_detector();
        let prompts = [
            "user@host:~$ ",
            "root@server:/var# ",
            ">>> ",
            "(gdb) ",
            "Password: ",
        ];
        for prompt in &prompts {
            assert!(
                det.matches_prompt(prompt),
                "expected match for prompt: {:?}",
                prompt,
            );
        }
    }

    // 9. Invalid regex patterns are skipped — no panic
    #[test]
    fn invalid_regex_skipped_no_panic() {
        let patterns = vec![
            "[invalid(".into(), // broken regex
            r"[$#] $".into(),   // valid
        ];
        let det = ReadinessDetector::new(
            ReadinessStrategy::Hybrid,
            &patterns,
            300,
            10_000,
        );
        // Should still work — the valid pattern compiled.
        assert!(det.matches_prompt("user@host:~$ "));
    }

    // 9b. ALL patterns invalid → falls back to Timeout
    #[test]
    fn all_invalid_patterns_fallback_to_timeout() {
        let patterns = vec!["[broken(".into(), "(also[bad".into()];
        let det = ReadinessDetector::new(
            ReadinessStrategy::Hybrid,
            &patterns,
            300,
            10_000,
        );
        // Strategy downgraded — prompt won't match, silence drives the result.
        assert!(!det.matches_prompt("user@host:~$ "));
        let result = det.check(
            "user@host:~$ ",
            Duration::from_millis(301),
            Duration::from_millis(500),
        );
        assert_eq!(result, ReadinessResult::SilenceTimeout);
    }

    // 10. Empty output never matches
    #[test]
    fn empty_output_no_match() {
        let det = hybrid_detector();
        assert!(!det.matches_prompt(""));
    }

    // Bonus: ANSI-laden prompt still matches after stripping
    #[test]
    fn ansi_stripped_before_matching() {
        let det = hybrid_detector();
        // Prompt with color codes wrapping it
        let ansi_prompt = "\x1b[32muser@host\x1b[0m:\x1b[34m~\x1b[0m$ ";
        assert!(det.matches_prompt(ansi_prompt));
    }

    // Bonus: only last line matters
    #[test]
    fn only_last_line_checked() {
        let det = hybrid_detector();
        // Prompt-like text in the middle, non-prompt at the end
        assert!(!det.matches_prompt("user@host:~$ \nstill running..."));
        // Prompt at the end
        assert!(det.matches_prompt("still running...\nuser@host:~$ "));
    }
}
