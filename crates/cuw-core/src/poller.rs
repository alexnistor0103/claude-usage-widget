use rand::Rng;
use std::time::Duration;

/// One request per account per minute, never faster (plan §3).
const BASE: Duration = Duration::from_secs(60);
const JITTER_MAX_SECS: u64 = 60;

/// Next delay with per-account jitter, to avoid a synchronized stampede.
pub fn next_interval() -> Duration {
    let extra = rand::thread_rng().gen_range(0..=JITTER_MAX_SECS);
    BASE + Duration::from_secs(extra)
}

/// Exponential backoff for 429/5xx, capped at 15 minutes.
pub fn backoff(attempt: u32) -> Duration {
    let secs = 30u64.saturating_mul(1 << attempt.min(5));
    Duration::from_secs(secs.min(15 * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_within_one_to_two_minutes() {
        for _ in 0..1000 {
            let d = next_interval();
            assert!(d >= Duration::from_secs(60));
            assert!(d <= Duration::from_secs(120));
        }
    }

    #[test]
    fn backoff_is_monotonic_and_capped() {
        let mut prev = Duration::ZERO;
        for attempt in 0..=20 {
            let d = backoff(attempt);
            assert!(d >= prev, "backoff must not decrease");
            assert!(d <= Duration::from_secs(15 * 60));
            prev = d;
        }
        assert_eq!(backoff(10), Duration::from_secs(15 * 60));
    }
}
