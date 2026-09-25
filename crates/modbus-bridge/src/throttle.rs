use std::time::{Duration, Instant};

/// How often a persisting failure is logged again after the first report.
pub const REPEAT_INTERVAL: Duration = Duration::from_secs(300);

/// Tracks one kind of recurring failure so it is logged when it starts,
/// then at most every [`REPEAT_INTERVAL`], and once more when it clears.
#[derive(Debug)]
pub struct ErrorLog {
    failures: u64,
    last_logged: Option<Instant>,
}

impl Default for ErrorLog {
    fn default() -> Self {
        Self::new()
    }
}

impl ErrorLog {
    pub const fn new() -> Self {
        Self {
            failures: 0,
            last_logged: None,
        }
    }

    /// Records a failure; returns the number of failures in this streak when
    /// it should be logged now.
    pub fn failure(&mut self) -> Option<u64> {
        self.failure_at(Instant::now())
    }

    /// Records a success; returns the length of the failure streak it ended.
    pub fn success(&mut self) -> Option<u64> {
        if self.failures == 0 {
            return None;
        }
        let ended = self.failures;
        self.failures = 0;
        self.last_logged = None;
        Some(ended)
    }

    fn failure_at(&mut self, now: Instant) -> Option<u64> {
        self.failures += 1;
        let due = self
            .last_logged
            .is_none_or(|t| now.duration_since(t) >= REPEAT_INTERVAL);
        if due {
            self.last_logged = Some(now);
            Some(self.failures)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_first_then_periodically_then_recovery() {
        let mut log = ErrorLog::new();
        let t0 = Instant::now();
        assert_eq!(log.failure_at(t0), Some(1));
        assert_eq!(log.failure_at(t0 + Duration::from_secs(2)), None);
        assert_eq!(log.failure_at(t0 + REPEAT_INTERVAL), Some(3));
        assert_eq!(log.success(), Some(3));
        assert_eq!(log.success(), None);
        assert_eq!(
            log.failure_at(t0 + REPEAT_INTERVAL),
            Some(1),
            "new streak logs at once"
        );
    }
}
