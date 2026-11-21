//! Exponential backoff for reconnect loops.

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    cur: Duration,
}

impl Backoff {
    pub fn new(min: Duration, max: Duration) -> Self {
        Backoff { min, max, cur: min }
    }

    /// The delay to wait now; doubles the next one up to `max`.
    pub fn next_delay(&mut self) -> Duration {
        let d = self.cur;
        self.cur = (self.cur * 2).min(self.max);
        d
    }

    pub fn reset(&mut self) {
        self.cur = self.min;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_and_caps() {
        let s = Duration::from_secs;
        let mut b = Backoff::new(s(1), s(5));
        let got: Vec<_> = (0..5).map(|_| b.next_delay()).collect();
        assert_eq!(got, vec![s(1), s(2), s(4), s(5), s(5)]);
        b.reset();
        assert_eq!(b.next_delay(), s(1));
    }
}
