//! Platform-independent policy used by the Windows service session manager.

use std::time::{Duration, Instant};

const CANDIDATE_RETRY_BASE: Duration = Duration::from_secs(1);
const CANDIDATE_RETRY_MAX_EXPONENT: u32 = 2;

#[derive(Debug)]
pub struct CandidateRetryBackoff<T> {
    failed_target: Option<T>,
    consecutive_failures: u32,
    last_failure_at: Option<Instant>,
}

impl<T> Default for CandidateRetryBackoff<T> {
    fn default() -> Self {
        Self {
            failed_target: None,
            consecutive_failures: 0,
            last_failure_at: None,
        }
    }
}

impl<T: Copy + Eq> CandidateRetryBackoff<T> {
    pub fn can_attempt(&self, target: T, now: Instant) -> bool {
        if self.failed_target != Some(target) {
            return true;
        }

        self.last_failure_at
            .is_none_or(|failed_at| now.saturating_duration_since(failed_at) >= self.retry_delay())
    }

    pub fn record_failure(&mut self, target: T, now: Instant) -> Duration {
        if self.failed_target == Some(target) {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        } else {
            self.failed_target = Some(target);
            self.consecutive_failures = 1;
        }
        self.last_failure_at = Some(now);
        self.retry_delay()
    }

    pub fn reset_if_target_changed(&mut self, target: T) {
        if self
            .failed_target
            .is_some_and(|failed_target| failed_target != target)
        {
            self.reset();
        }
    }

    pub fn reset(&mut self) {
        self.failed_target = None;
        self.consecutive_failures = 0;
        self.last_failure_at = None;
    }

    fn retry_delay(&self) -> Duration {
        if self.consecutive_failures == 0 {
            return Duration::ZERO;
        }

        let exponent = self
            .consecutive_failures
            .saturating_sub(1)
            .min(CANDIDATE_RETRY_MAX_EXPONENT);
        CANDIDATE_RETRY_BASE * (1 << exponent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_failures_back_off_but_a_new_target_is_immediate() {
        let start = Instant::now();
        let mut backoff = CandidateRetryBackoff::default();

        assert!(backoff.can_attempt((5, "Default"), start));
        assert_eq!(
            backoff.record_failure((5, "Default"), start),
            Duration::from_secs(1)
        );
        assert!(!backoff.can_attempt((5, "Default"), start + Duration::from_millis(999)));
        assert!(backoff.can_attempt((5, "Default"), start + Duration::from_secs(1)));

        assert_eq!(
            backoff.record_failure((5, "Default"), start + Duration::from_secs(1)),
            Duration::from_secs(2)
        );
        assert_eq!(
            backoff.record_failure((5, "Default"), start + Duration::from_secs(3)),
            Duration::from_secs(4)
        );
        assert_eq!(
            backoff.record_failure((5, "Default"), start + Duration::from_secs(7)),
            Duration::from_secs(4)
        );

        assert!(backoff.can_attempt((6, "Winlogon"), start + Duration::from_secs(7)));
        backoff.reset_if_target_changed((6, "Winlogon"));
        assert!(backoff.can_attempt((6, "Winlogon"), start + Duration::from_secs(7)));
    }

    #[test]
    fn successful_candidate_resets_failure_history() {
        let start = Instant::now();
        let mut backoff = CandidateRetryBackoff::default();
        backoff.record_failure((5, "Default"), start);
        backoff.reset();

        assert!(backoff.can_attempt((5, "Default"), start));
    }
}
