//! Bounded repair traffic, retaining fractional refill credit under dense NACKs.
use std::time::Instant;

const REPAIRS_PER_SECOND: f64 = 300.0;

pub(super) struct RepairBudget {
    at: Instant,
    tokens: f64,
}

impl RepairBudget {
    pub(super) fn new(at: Instant) -> Self {
        Self { at, tokens: 0.0 }
    }

    pub(super) fn take(&mut self, now: Instant) -> bool {
        self.tokens = (self.tokens
            + now.saturating_duration_since(self.at).as_secs_f64() * REPAIRS_PER_SECOND)
            .min(REPAIRS_PER_SECOND);
        self.at = now;
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn dense_requests_keep_their_fractional_refill_credit() {
        let start = Instant::now();
        let mut budget = RepairBudget::new(start);
        let admitted = (1..=500)
            .filter(|i| budget.take(start + Duration::from_millis(i * 2)))
            .count();
        assert!(
            (299..=300).contains(&admitted),
            "500/s demand should get the 300/s allowance, got {admitted}"
        );
    }

    #[test]
    fn idle_time_cannot_build_an_unbounded_repair_burst() {
        let start = Instant::now();
        let mut budget = RepairBudget::new(start);
        let now = start + Duration::from_secs(30);
        assert_eq!((0..1000).filter(|_| budget.take(now)).count(), 300);
    }

    #[test]
    fn a_whole_frame_uses_the_same_budget_as_fragment_repairs() {
        let start = Instant::now();
        let mut budget = RepairBudget::new(start);
        let now = start + Duration::from_millis(100);
        assert_eq!((0..50).filter(|_| budget.take(now)).count(), 30);
        assert!(!budget.take(now));
        assert!(budget.take(now + Duration::from_millis(4)));
    }
}
