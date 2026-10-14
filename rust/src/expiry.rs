//! Expiry and auction scheduler: a minute-resolution timing wheel.
//!
//! 0DTE sessions have hundreds of series expiring at known minute
//! boundaries (hourly expiries plus the MOC close). The wheel holds
//! `(minute, Action)` entries in `minute % N` buckets; advancing one minute
//! is O(due entries) — no heap, no per-timer allocation.

use crate::types::Nanos;

/// Scheduled market event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    /// Open the MOC auction window for a symbol.
    OpenAuction(u32),
    /// Close a reopening auction (circuit-breaker resume).
    CloseAuction(u32),
    /// Settle and expire a symbol.
    Settle(u32),
}

/// Minute-resolution timing wheel with wrap-around guard.
#[derive(Clone, Debug)]
pub struct Wheel {
    buckets: Vec<Vec<(u64, Action)>>,
    /// Current minute (last advanced).
    cursor: u64,
    mask: u64,
}

impl Wheel {
    /// A wheel with `n_buckets` (power of two) one-minute buckets.
    pub fn new(n_buckets: usize) -> Wheel {
        let n = n_buckets.max(2).next_power_of_two();
        Wheel {
            buckets: (0..n).map(|_| Vec::new()).collect(),
            cursor: 0,
            mask: (n - 1) as u64,
        }
    }

    /// Nanoseconds per minute (re-exported for convenience).
    pub const NANOS_PER_MIN: u64 = 60_000_000_000;

    /// Schedule `action` at nanosecond `at`.
    pub fn schedule(&mut self, at: Nanos, action: Action) {
        let minute = at / Self::NANOS_PER_MIN;
        let b = (minute & self.mask) as usize;
        self.buckets[b].push((minute, action));
    }

    /// Drain actions due at exactly `minute` into `out`. Entries scheduled
    /// for earlier minutes (missed) are dropped defensively.
    pub fn pop_minute(&mut self, minute: u64, out: &mut Vec<Action>) {
        let b = (minute & self.mask) as usize;
        let bucket = &mut self.buckets[b];
        let mut i = 0;
        let mut w = 0;
        while i < bucket.len() {
            let (m, a) = bucket[i];
            if m == minute {
                out.push(a);
            } else if m > minute {
                bucket[w] = bucket[i];
                w += 1;
            }
            i += 1;
        }
        bucket.truncate(w);
    }

    /// Advance from the cursor to `to` (nanos), invoking `f` for each due
    /// action with the minute at which it fires. `f(minute, action)`.
    pub fn advance<F: FnMut(u64, Action)>(&mut self, to: Nanos, mut f: F) {
        let to_min = to / Self::NANOS_PER_MIN;
        let mut m = self.cursor + 1;
        while m <= to_min {
            let mut due = Vec::new();
            self.pop_minute(m, &mut due);
            for a in due {
                f(m, a);
            }
            m += 1;
        }
        if to_min > self.cursor {
            self.cursor = to_min;
        }
    }

    /// Current minute cursor.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: u64 = Wheel::NANOS_PER_MIN;

    #[test]
    fn schedules_and_fires_in_order() {
        let mut w = Wheel::new(64);
        w.schedule(10 * M, Action::Settle(1));
        w.schedule(5 * M, Action::OpenAuction(1));
        w.schedule(10 * M, Action::Settle(2));
        let mut fired = Vec::new();
        w.advance(12 * M, |m, a| fired.push((m, a)));
        assert_eq!(
            fired,
            vec![
                (5, Action::OpenAuction(1)),
                (10, Action::Settle(1)),
                (10, Action::Settle(2)),
            ]
        );
        // Idempotent re-advance.
        let mut again = Vec::new();
        w.advance(12 * M, |_, _| again.push(0));
        assert!(again.is_empty());
    }

    #[test]
    fn wraps_around_buckets() {
        let mut w = Wheel::new(4);
        w.schedule(2 * M, Action::Settle(9));
        w.schedule(6 * M, Action::Settle(10)); // same bucket, 4 min later
        let mut fired = Vec::new();
        w.advance(7 * M, |m, a| fired.push((m, a)));
        assert_eq!(fired, vec![(2, Action::Settle(9)), (6, Action::Settle(10))]);
    }
}
