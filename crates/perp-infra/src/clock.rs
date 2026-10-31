//! System wall clock adapter for the `Clock` port.

use perp_app::ports::Clock;
use perp_domain::types::Millis;
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch from the operating system clock.
///
/// The service never lets event timestamps go backwards even if this clock
/// does (see `ExchangeService::submit`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> Millis {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_plausible_time_that_does_not_run_backwards_here() {
        let c = SystemClock;
        let a = c.now_ms();
        let b = c.now_ms();
        // after 2020-01-01 and non-decreasing across two adjacent reads
        assert!(a > 1_577_836_800_000);
        assert!(b >= a);
    }
}
