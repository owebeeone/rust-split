#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TimestampMs(pub i64);

pub trait Clock {
    fn now_ms(&self) -> TimestampMs;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FixedClock {
    now: TimestampMs,
}

impl FixedClock {
    pub fn new(now: TimestampMs) -> Self {
        Self { now }
    }
}

impl Clock for FixedClock {
    fn now_ms(&self) -> TimestampMs {
        self.now
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_clock_returns_configured_time() {
        let clock = FixedClock::new(TimestampMs(42));
        assert_eq!(clock.now_ms(), TimestampMs(42));
    }
}
