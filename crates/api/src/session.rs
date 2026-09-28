//! Sequenced market-data sessions (G-27): snapshot + incremental delta
//! with gap detection and deterministic resync.
//!
//! The pattern every production venue uses (Derive's channel model,
//! Paradex WS): a subscriber receives one snapshot at sequence `n`,
//! then deltas `n+1, n+2, …`. A missed delta (network blip, slow
//! consumer) is detected client-side by the sequence gap and repaired
//! by [`MarketDataSession::resync`] — the session never silently
//! continues from a torn state.

use std::collections::BTreeMap;

/// A book level: `(price_ticks, total_lots)`.
pub type Level = (u64, u64);

/// One incremental book update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    /// Sequence number of this update (must be previous + 1).
    pub seq: u64,
    /// Bid levels *replacing* the given price (0 lots removes).
    pub bids: Vec<Level>,
    /// Ask levels replacing the given price (0 lots removes).
    pub asks: Vec<Level>,
}

/// A full book snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Sequence number of the snapshot.
    pub seq: u64,
    /// Bid levels (best first, descending).
    pub bids: Vec<Level>,
    /// Ask levels (best first, ascending).
    pub asks: Vec<Level>,
}

/// Publisher-side book state for one symbol: applies source updates and
/// fans out deltas with sequence numbers.
#[derive(Debug, Default)]
pub struct BookFeed {
    seq: u64,
    bids: BTreeMap<u64, u64>,
    asks: BTreeMap<u64, u64>,
}

impl BookFeed {
    /// New empty feed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a source update and emit the delta for subscribers.
    pub fn update(&mut self, bids: Vec<Level>, asks: Vec<Level>) -> Delta {
        self.seq += 1;
        for (price, lots) in &bids {
            set_level(&mut self.bids, *price, *lots);
        }
        for (price, lots) in &asks {
            set_level(&mut self.asks, *price, *lots);
        }
        Delta {
            seq: self.seq,
            bids,
            asks,
        }
    }

    /// Current snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            seq: self.seq,
            bids: self.bids.iter().rev().map(|(&p, &q)| (p, q)).collect(),
            asks: self.asks.iter().map(|(&p, &q)| (p, q)).collect(),
        }
    }
}

fn set_level(map: &mut BTreeMap<u64, u64>, price: u64, lots: u64) {
    if lots == 0 {
        map.remove(&price);
    } else {
        map.insert(price, lots);
    }
}

/// Subscriber-side session state for one symbol.
#[derive(Debug, Default)]
pub struct MarketDataSession {
    /// Last applied sequence (0 = not synced yet).
    pub seq: u64,
    bids: BTreeMap<u64, u64>,
    asks: BTreeMap<u64, u64>,
    /// Set when a gap was detected; the book is stale until `resync`.
    pub desynced: bool,
    /// Deltas dropped while desynced.
    pub dropped: u64,
}

impl MarketDataSession {
    /// New empty session.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a snapshot (initial or resync). Clears any desync state.
    pub fn apply_snapshot(&mut self, snap: &Snapshot) {
        self.seq = snap.seq;
        self.bids = snap.bids.iter().copied().collect();
        self.asks = snap.asks.iter().copied().collect();
        self.desynced = false;
        self.dropped = 0;
    }

    /// Apply one delta. A sequence gap sets `desynced` (the caller must
    /// resync); deltas are ignored until then.
    pub fn apply_delta(&mut self, delta: &Delta) {
        if self.desynced {
            self.dropped += 1;
            return;
        }
        if delta.seq != self.seq + 1 {
            self.desynced = true;
            self.dropped += 1;
            return;
        }
        self.seq = delta.seq;
        for &(price, lots) in &delta.bids {
            set_level(&mut self.bids, price, lots);
        }
        for &(price, lots) in &delta.asks {
            set_level(&mut self.asks, price, lots);
        }
    }

    /// Whether a resync is required.
    #[must_use]
    pub fn needs_resync(&self) -> bool {
        self.desynced || self.seq == 0
    }

    /// Best bid/ask (ticks, lots).
    #[must_use]
    pub fn best(&self) -> (Option<Level>, Option<Level>) {
        (
            self.bids.iter().next_back().map(|(&p, &q)| (p, q)),
            self.asks.iter().next().map(|(&p, &q)| (p, q)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_then_deltas_track_the_source() {
        let mut feed = BookFeed::new();
        feed.update(vec![(100, 5)], vec![(101, 4)]);
        feed.update(vec![(99, 8)], vec![(102, 2)]);

        let mut session = MarketDataSession::new();
        assert!(session.needs_resync());
        session.apply_snapshot(&feed.snapshot());
        assert!(!session.needs_resync());

        let d1 = feed.update(vec![(100, 6)], vec![(101, 0)]);
        session.apply_delta(&d1);
        assert_eq!(session.seq, 3);
        assert_eq!(session.best(), (Some((100, 6)), Some((102, 2))));
        // The 101 ask was removed by a zero-size delta.
        assert!(session.best().1.is_some());
    }

    #[test]
    fn gap_marks_desync_and_ignores_further_deltas() {
        let mut feed = BookFeed::new();
        feed.update(vec![(10, 1)], vec![(11, 1)]);
        let mut session = MarketDataSession::new();
        session.apply_snapshot(&feed.snapshot());

        let d2 = feed.update(vec![(10, 2)], vec![]);
        let d3 = feed.update(vec![(9, 3)], vec![]);
        // The subscriber misses d2 and receives d3: gap (3 != 1+1).
        session.apply_delta(&d3);
        assert!(session.needs_resync());
        assert_eq!(session.dropped, 1);
        // Later deltas are ignored while desynced.
        let d4 = feed.update(vec![(8, 9)], vec![]);
        session.apply_delta(&d4);
        assert_eq!(session.dropped, 2);
        // The book still shows the snapshot state (d2 was never
        // delivered; d3/d4 were dropped in the gap).
        assert_eq!(session.best(), (Some((10, 1)), Some((11, 1))));
        let _ = d2;
    }

    #[test]
    fn resync_recovers_exactly() {
        let mut feed = BookFeed::new();
        feed.update(vec![(50, 5)], vec![(51, 5)]);
        let mut session = MarketDataSession::new();
        session.apply_snapshot(&feed.snapshot());
        let d = feed.update(vec![(50, 0)], vec![]);
        session.apply_delta(&d);

        feed.update(vec![(49, 3)], vec![(52, 1)]);
        feed.update(vec![(48, 2)], vec![(53, 6)]);
        let snap = feed.snapshot();
        session.apply_snapshot(&snap);
        assert!(!session.needs_resync());
        assert_eq!(session.seq, 4);
        assert_eq!(session.best(), (Some((49, 3)), Some((51, 5))));
    }
}
