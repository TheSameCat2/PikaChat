use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    base_delay_ms: u64,
    max_delay_ms: u64,
}

impl RetryPolicy {
    pub fn new(base_delay_ms: u64, max_delay_ms: u64) -> Self {
        Self {
            base_delay_ms,
            max_delay_ms,
        }
    }

    pub fn base_delay_ms(&self) -> u64 {
        self.base_delay_ms
    }

    pub fn max_delay_ms(&self) -> u64 {
        self.max_delay_ms
    }

    pub fn delay_for_attempt(&self, attempt: u32, retry_after_hint_ms: Option<u64>) -> Duration {
        let shift = attempt.min(20);
        let multiplier = 1_u64 << shift;
        let calculated = self.base_delay_ms.saturating_mul(multiplier);
        let hinted = retry_after_hint_ms.unwrap_or(0);
        let bounded = calculated.max(hinted).min(self.max_delay_ms);
        Duration::from_millis(bounded)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(500, 30_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_base_delay() {
        let policy = RetryPolicy::new(250, 8_000);
        assert_eq!(
            policy.delay_for_attempt(0, None),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn scales_exponentially_for_attempts() {
        let policy = RetryPolicy::new(100, 10_000);
        assert_eq!(
            policy.delay_for_attempt(3, None),
            Duration::from_millis(800)
        );
    }

    #[test]
    fn caps_delay_at_max() {
        let policy = RetryPolicy::new(1_000, 4_000);
        assert_eq!(
            policy.delay_for_attempt(5, None),
            Duration::from_millis(4_000)
        );
    }

    #[test]
    fn honors_retry_after_hint_when_larger() {
        let policy = RetryPolicy::new(500, 20_000);
        assert_eq!(
            policy.delay_for_attempt(1, Some(10_000)),
            Duration::from_millis(10_000)
        );
    }
}
