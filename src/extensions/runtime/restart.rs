//! Restart policy with exponential backoff for process extensions.

use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct RestartPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// Multiplier per attempt. Defaults to 2.0.
    pub multiplier: f32,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(5),
            multiplier: 2.0,
        }
    }
}

impl RestartPolicy {
    /// Compute the delay before attempt `n` (1-indexed). Attempt 1 → base_delay.
    /// Capped at `max_delay`. Returns `None` if `n > max_attempts` or `n == 0`.
    pub fn delay_for_attempt(&self, attempt: u32) -> Option<Duration> {
        if attempt == 0 || attempt > self.max_attempts {
            return None;
        }
        let exp = (attempt - 1) as i32;
        let factor = self.multiplier.powi(exp);
        let nanos = (self.base_delay.as_nanos() as f64 * factor as f64)
            .min(self.max_delay.as_nanos() as f64) as u64;
        Some(Duration::from_nanos(nanos))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_attempt_1_returns_base_delay() {
        let p = RestartPolicy::default();
        assert_eq!(p.delay_for_attempt(1), Some(Duration::from_millis(250)));
    }

    #[test]
    fn attempt_2_doubles_base() {
        let p = RestartPolicy::default();
        assert_eq!(p.delay_for_attempt(2), Some(Duration::from_millis(500)));
    }

    #[test]
    fn attempt_capped_at_max_delay() {
        let p = RestartPolicy {
            max_attempts: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(2),
            multiplier: 10.0,
        };
        assert_eq!(p.delay_for_attempt(2), Some(Duration::from_secs(2)));
    }

    #[test]
    fn attempt_zero_returns_none() {
        let p = RestartPolicy::default();
        assert_eq!(p.delay_for_attempt(0), None);
    }

    #[test]
    fn attempt_beyond_max_returns_none() {
        let p = RestartPolicy::default();
        assert_eq!(p.delay_for_attempt(p.max_attempts + 1), None);
    }

    #[test]
    fn custom_multiplier() {
        let p = RestartPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(60),
            multiplier: 3.0,
        };
        // attempt 3 -> base * 3^2 = 900ms
        assert_eq!(p.delay_for_attempt(3), Some(Duration::from_millis(900)));
    }

    #[test]
    fn default_max_attempts_is_three() {
        assert_eq!(RestartPolicy::default().max_attempts, 3);
    }
}
