//! # poc-rfq
//!
//! Multi-dealer request-for-quote (RFQ) system and block-trade tape for the
//! perpetuals + options CLOB engine — the Paradigm/Derive-style negotiated
//! liquidity channel (audit gaps G-11, G-13, G-14, plus the G-38 fee-grouping
//! helper the engine calls at settlement).
//!
//! ## Model
//!
//! An RFQ is a **package of legs** priced as one atomic all-or-nothing block:
//!
//! 1. [`RfqBook::create_rfq`] — the taker publishes an **unsigned intent**
//!    (no funds move; the intent is only recorded), optionally directed at a
//!    private counterparty list and bounded by min/max total cost.
//! 2. [`RfqBook::send_quote`] / [`RfqBook::replace_quote`] — makers answer
//!    with per-leg prices; the taker's total signed flow is computed and
//!    frozen at quote time.
//! 3. [`RfqBook::execute`] — the taker picks one quote; the RFQ and quote
//!    settle atomically and every other live quote on the RFQ is cancelled.
//!
//! Money is exact integer `u128` quote-minor units end to end. Legs carry
//! their instrument's numeric spec so packages value independently of the
//! engine, using the same `tick * lot / 10^base_decimals` convention as
//! `poc-core`. All collections are `BTree*`, so iteration order — and hence
//! every observable outcome — is deterministic and replayable.
//!
//! The companion [`block`] module journals privately negotiated trades and
//! prints them on the public tape after a configurable delay.

pub mod block;
pub mod dealer;

pub use block::{BlockLedger, BlockOrigin, BlockTrade};
pub use dealer::{
    DealerProgram, DealerRecord, DealerTier, ExecuteOutcome, ProgramConfig, ProgramGate,
    PullOutcome,
};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use poc_core::num::{mul_div, Rounding};
use poc_core::{Side, SubaccountId, Symbol, TimestampMs};

// ---------------------------------------------------------------------------
// Legs
// ---------------------------------------------------------------------------

/// Numeric spec of one leg's instrument so the RFQ system can value packages
/// without depending on the engine (mirrors `poc-core` tick/lot conventions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegSpec {
    /// Price tick: quote minor units per `1.0` base (from the instrument).
    pub tick_size_quote_minor: u128,
    /// Contract lot: base minor units per lot.
    pub lot_size_base_minor: u128,
    /// Base decimals (minor units per major, e.g. 5 for BTC).
    pub base_decimals: u32,
}

/// One leg of an RFQ package (taker's perspective).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RfqLeg {
    /// Instrument symbol, e.g. `BTC-PERP`.
    pub symbol: Symbol,
    /// Taker's side on this leg; the maker takes the opposite side.
    pub side: Side,
    /// Quantity in lots (positive).
    pub qty_lots: u64,
    /// Instrument numeric spec.
    pub spec: LegSpec,
}

impl RfqLeg {
    /// `10^base_decimals` base minor units per whole base unit.
    fn base_unit(&self) -> Option<u128> {
        10_u128.checked_pow(self.spec.base_decimals)
    }

    /// Notional of this leg at `price_ticks`, in quote minor units.
    ///
    /// `(ticks * tick_size) * (lots * lot_size) / 10^base_decimals`, rounded
    /// half-up — the same convention as
    /// `poc_core::Instrument::notional_quote_minor`. Returns `None` on
    /// overflow.
    #[must_use]
    pub fn notional(&self, price_ticks: u64) -> Option<u128> {
        let price = u128::from(price_ticks).checked_mul(self.spec.tick_size_quote_minor)?;
        let base_minor = u128::from(self.qty_lots).checked_mul(self.spec.lot_size_base_minor)?;
        mul_div(
            price,
            base_minor,
            self.base_unit()?,
            Rounding::NearestHalfUp,
        )
    }

    /// Signed cash flow of the taker on this leg for a SPECIFIC quantity
    /// (lots) at `price_ticks` (negative = taker pays, positive = taker
    /// receives) — the partial-execution pricer. Returns `None` on
    /// overflow.
    #[must_use]
    pub fn taker_flow_at_qty(&self, price_ticks: u64, qty_lots: u64) -> Option<i128> {
        let price = u128::from(price_ticks).checked_mul(self.spec.tick_size_quote_minor)?;
        let base_minor = u128::from(qty_lots).checked_mul(self.spec.lot_size_base_minor)?;
        let n = i128::try_from(mul_div(
            price,
            base_minor,
            self.base_unit()?,
            Rounding::NearestHalfUp,
        )?)
        .ok()?;
        match self.side {
            Side::Bid => n.checked_neg(),
            Side::Ask => Some(n),
        }
    }

    /// Signed cash flow of the taker on this leg at `price_ticks`
    /// (negative = taker pays, positive = taker receives). Returns `None` on
    /// overflow.
    #[must_use]
    pub fn taker_flow(&self, price_ticks: u64) -> Option<i128> {
        let n = i128::try_from(self.notional(price_ticks)?).ok()?;
        match self.side {
            Side::Bid => n.checked_neg(),
            Side::Ask => Some(n),
        }
    }
}

// ---------------------------------------------------------------------------
// RFQ + quote records
// ---------------------------------------------------------------------------

/// Lifecycle state of an RFQ request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RfqStatus {
    /// Published, inside its quoting window, accepting quotes (possibly
    /// with a partially-consumed remaining size).
    Open,
    /// Every leg fully executed; terminal.
    Filled,
    /// Cancelled by the taker; terminal.
    Cancelled,
    /// Quoting window passed; terminal.
    Expired,
}

/// Lifecycle state of a maker's quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuoteStatus {
    /// Resting and executable.
    Live,
    /// Picked by the taker; terminal.
    Executed,
    /// Withdrawn, replaced, or killed by RFQ settlement; terminal.
    Cancelled,
    /// Quote TTL passed; terminal.
    Expired,
}

/// A published RFQ intent (taker-owned, unsigned — an intent cannot move
/// funds by itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RfqRequest {
    /// Engine-assigned id.
    pub rfq_id: u64,
    /// Publishing taker subaccount.
    pub taker: SubaccountId,
    /// Legs of the package, in order.
    pub legs: Vec<RfqLeg>,
    /// Empty = open to all makers; otherwise only these makers see it.
    pub counterparties: Vec<SubaccountId>,
    /// Lower bound on the taker's total cost (`-total_flow`) at execution.
    pub min_total_cost_quote_minor: Option<u128>,
    /// Upper bound on the taker's total cost (`-total_flow`) at execution.
    pub max_total_cost_quote_minor: Option<u128>,
    /// Creation timestamp (ms epoch).
    pub created_ts: TimestampMs,
    /// End of the quoting window (ms epoch).
    pub valid_until: TimestampMs,
    /// Remaining size per leg (lots): starts at `legs[i].qty_lots` and is
    /// reduced by partial executions. An `Open` RFQ with a zero leg is
    /// still quotable on its other legs.
    pub remaining_lots: Vec<u64>,
    /// Current lifecycle state.
    pub status: RfqStatus,
}

/// A maker's priced answer to an RFQ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quote {
    /// Engine-assigned id.
    pub quote_id: u64,
    /// Parent RFQ.
    pub rfq_id: u64,
    /// Quoting maker subaccount.
    pub maker: SubaccountId,
    /// Maker's price per leg, in ticks, aligned with the RFQ's legs order.
    /// The maker's side on each leg is the opposite of the leg's taker side.
    pub leg_prices_ticks: Vec<u64>,
    /// Committed size per leg (lots) — what the dealer is firm on. A
    /// full-size quote carries the RFQ's remaining size at quote time;
    /// a partial-size quote commits less.
    pub qty_lots: Vec<u64>,
    /// Executed size per leg so far (lots) — partial executions consume
    /// the quote without killing it; the remainder stays firm.
    pub filled_lots: Vec<u64>,
    /// Taker's total signed flow at these prices for the FULL committed
    /// size, computed and frozen at quote time (negative = taker pays
    /// net). Partial executions re-derive flow for the taken part.
    pub total_taker_flow_quote_minor: i128,
    /// Creation timestamp (ms epoch).
    pub created_ts: TimestampMs,
    /// End of the quote's validity (ms epoch). Independent of the RFQ's own
    /// window — execution requires both to be live.
    pub valid_until: TimestampMs,
    /// Current lifecycle state.
    pub status: QuoteStatus,
}

impl Quote {
    /// Remaining committed size per leg (lots): `qty - filled`. Partial
    /// executions consume this; the quote is fully consumed when every
    /// leg reaches zero.
    #[must_use]
    pub fn remaining_lots(&self) -> Vec<u64> {
        self.qty_lots
            .iter()
            .zip(&self.filled_lots)
            .map(|(&q, &f)| q.saturating_sub(f))
            .collect()
    }

    /// Whether the quote is fully consumed (every leg's remaining is
    /// zero) — the `Executed` transition condition.
    #[must_use]
    pub fn fully_consumed(&self) -> bool {
        self.remaining_lots().iter().all(|&r| r == 0)
    }
}

/// A successfully executed RFQ — consumed by the engine to settle trades.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RfqExecution {
    /// RFQ that was filled.
    pub rfq_id: u64,
    /// Quote that was executed.
    pub quote_id: u64,
    /// Taker subaccount.
    pub taker: SubaccountId,
    /// Maker subaccount.
    pub maker: SubaccountId,
    /// Legs with the EXECUTION price (ticks) per leg, in RFQ leg order:
    /// `(symbol, taker side, qty lots, price ticks)`.
    pub legs: Vec<(Symbol, Side, u64, u64)>,
    /// Taker's total signed flow (negative = taker pays net).
    pub total_taker_flow_quote_minor: i128,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from the RFQ state machine and block ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RfqError {
    /// Referenced RFQ does not exist.
    UnknownRfq,
    /// Referenced quote does not exist (or does not belong to the RFQ).
    UnknownQuote,
    /// The RFQ is not `Open` (already filled, cancelled, or expired).
    NotOpen,
    /// The RFQ's or quote's validity window has passed.
    QuoteWindowClosed,
    /// The maker does not own the referenced quote.
    NotQuoteOwner,
    /// The account is not the RFQ's taker.
    NotRfqOwner,
    /// The maker is not an allowed counterparty on this directed RFQ.
    NotCounterparty,
    /// Quote prices do not line up with the RFQ's legs.
    LegCountMismatch,
    /// A leg price of zero was supplied.
    ZeroPrice,
    /// TTL must be positive (and must not overflow the clock).
    InvalidTtl,
    /// Quantity must be positive and legs non-empty.
    InvalidQty,
    /// The executed total cost violates the RFQ's min/max bounds.
    BoundsViolation,
    /// A partial execution's take vector is malformed (length, zero-everywhere,
    /// or larger than the quote's or RFQ's remaining size on some leg).
    InvalidPartial,
    /// The maker is not enrolled in the dealer program (G-15).
    NotEnrolled,
    /// The maker is suspended until the carried timestamp (G-15).
    Suspended(TimestampMs),
    /// The maker was dropped from the dealer program (G-15).
    Dropped,
    /// The taker (or a block-trade side) is also the counterparty.
    SelfQuote,
    /// The referenced quote is no longer live (executed/cancelled/expired).
    AlreadySettled,
    /// A money computation overflowed.
    MathOverflow,
}

impl fmt::Display for RfqError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RfqError::UnknownRfq => write!(f, "unknown rfq"),
            RfqError::UnknownQuote => write!(f, "unknown quote"),
            RfqError::NotOpen => write!(f, "rfq is not open"),
            RfqError::QuoteWindowClosed => write!(f, "validity window closed"),
            RfqError::NotQuoteOwner => write!(f, "not the quote owner"),
            RfqError::NotRfqOwner => write!(f, "not the rfq owner"),
            RfqError::NotCounterparty => write!(f, "maker is not an allowed counterparty"),
            RfqError::LegCountMismatch => write!(f, "quote legs do not match rfq legs"),
            RfqError::ZeroPrice => write!(f, "leg price must be positive"),
            RfqError::InvalidTtl => write!(f, "ttl must be positive"),
            RfqError::InvalidQty => write!(f, "quantity must be positive"),
            RfqError::BoundsViolation => write!(f, "total cost outside rfq bounds"),
            RfqError::InvalidPartial => {
                write!(f, "invalid partial take")
            }
            RfqError::NotEnrolled => write!(f, "maker not enrolled in dealer program"),
            RfqError::Suspended(until) => write!(f, "maker suspended until {until}"),
            RfqError::Dropped => write!(f, "maker dropped from dealer program"),
            RfqError::SelfQuote => write!(f, "cannot trade with self"),
            RfqError::AlreadySettled => write!(f, "quote is no longer live"),
            RfqError::MathOverflow => write!(f, "arithmetic overflow"),
        }
    }
}

impl std::error::Error for RfqError {}

// ---------------------------------------------------------------------------
// The state machine
// ---------------------------------------------------------------------------

/// The RFQ state machine: requests, quotes, and their lifecycle.
///
/// Pure state — no balances move here. The consuming engine layers margin,
/// fees, and journaling on top of the [`RfqExecution`] values this book
/// produces.
#[derive(Debug)]
pub struct RfqBook {
    /// All RFQs by id.
    rfqs: BTreeMap<u64, RfqRequest>,
    /// All quotes by id.
    quotes: BTreeMap<u64, Quote>,
    /// Quote ids per RFQ (ascending id — the taker's quote channel).
    rfq_quotes: BTreeMap<u64, BTreeSet<u64>>,
    /// Quote ids per maker.
    maker_quotes: BTreeMap<SubaccountId, BTreeSet<u64>>,
    next_rfq_id: u64,
    next_quote_id: u64,
}

impl Default for RfqBook {
    fn default() -> Self {
        Self::new()
    }
}

impl RfqBook {
    /// Default quoting window when a request omits one: 10 minutes.
    pub const DEFAULT_WINDOW_MS: TimestampMs = 600_000;

    /// A fresh, empty book. Ids are assigned from 1.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rfqs: BTreeMap::new(),
            quotes: BTreeMap::new(),
            rfq_quotes: BTreeMap::new(),
            maker_quotes: BTreeMap::new(),
            next_rfq_id: 1,
            next_quote_id: 1,
        }
    }

    /// The id the next [`Self::create_rfq`] call will assign.
    ///
    /// Plan-stage peek: lets the engine stamp the *predicted* id onto the
    /// `RfqCreated` event so external consumers (FIX desks, the gateway)
    /// can map their requests to engine ids without scanning state.
    /// Deterministic because plan and apply run against identical state.
    pub fn next_rfq_id(&self) -> u64 {
        self.next_rfq_id
    }

    /// The id the next quote submission will assign (plan-stage peek,
    /// symmetric with [`Self::next_rfq_id`]).
    pub fn next_quote_id(&self) -> u64 {
        self.next_quote_id
    }

    /// Taker publishes a request — an **unsigned intent**: no state mutates
    /// beyond recording it.
    ///
    /// Validates: legs non-empty, per-leg quantity positive, TTL positive
    /// (and non-overflowing), the maker list may not contain the taker, and
    /// `min <= max` when both bounds are set. An empty counterparty list
    /// means the RFQ is open to all makers. Returns the new RFQ id.
    pub fn create_rfq(
        &mut self,
        taker: SubaccountId,
        legs: Vec<RfqLeg>,
        counterparties: Vec<SubaccountId>,
        min_total_cost: Option<u128>,
        max_total_cost: Option<u128>,
        ttl_ms: TimestampMs,
        now: TimestampMs,
    ) -> Result<u64, RfqError> {
        if legs.is_empty() {
            return Err(RfqError::InvalidQty);
        }
        if legs.iter().any(|l| l.qty_lots == 0) {
            return Err(RfqError::InvalidQty);
        }
        if ttl_ms == 0 {
            return Err(RfqError::InvalidTtl);
        }
        if counterparties.contains(&taker) {
            return Err(RfqError::SelfQuote);
        }
        if let (Some(min), Some(max)) = (min_total_cost, max_total_cost) {
            if min > max {
                return Err(RfqError::BoundsViolation);
            }
        }
        let valid_until = now.checked_add(ttl_ms).ok_or(RfqError::InvalidTtl)?;
        let rfq_id = self.next_rfq_id;
        self.next_rfq_id = rfq_id.checked_add(1).ok_or(RfqError::MathOverflow)?;
        let remaining_lots = legs.iter().map(|l| l.qty_lots).collect();
        self.rfqs.insert(
            rfq_id,
            RfqRequest {
                rfq_id,
                taker,
                legs,
                counterparties,
                min_total_cost_quote_minor: min_total_cost,
                max_total_cost_quote_minor: max_total_cost,
                created_ts: now,
                valid_until,
                remaining_lots,
                status: RfqStatus::Open,
            },
        );
        Ok(rfq_id)
    }

    /// Taker cancels their RFQ (only while `Open`).
    ///
    /// Cancelling also cancels every live quote on the RFQ so the taker's
    /// best-quote channel drains immediately.
    pub fn cancel_rfq(&mut self, taker: SubaccountId, rfq_id: u64) -> Result<(), RfqError> {
        match self.rfqs.get(&rfq_id) {
            None => return Err(RfqError::UnknownRfq),
            Some(rfq) if rfq.taker != taker => return Err(RfqError::NotRfqOwner),
            Some(rfq) if rfq.status != RfqStatus::Open => return Err(RfqError::NotOpen),
            Some(_) => {}
        }
        let quote_ids: Vec<u64> = self
            .rfq_quotes
            .get(&rfq_id)
            .map_or_else(Vec::new, |s| s.iter().copied().collect());
        for qid in quote_ids {
            self.cancel_if_live(qid);
        }
        if let Some(rfq) = self.rfqs.get_mut(&rfq_id) {
            rfq.status = RfqStatus::Cancelled;
        }
        Ok(())
    }

    /// Maker submits a **full-size** quote on an `Open`, in-window RFQ they
    /// may see: the committed size is the RFQ's remaining size per leg at
    /// quote time.
    ///
    /// Prices must be positive per leg and line up with the RFQ's legs; the
    /// taker's total signed flow for the full size is computed and frozen
    /// at quote time. A maker may hold several quotes on one RFQ and swap
    /// any of them atomically via [`replace_quote`](Self::replace_quote).
    /// Returns the new quote id.
    pub fn send_quote(
        &mut self,
        maker: SubaccountId,
        rfq_id: u64,
        leg_prices_ticks: Vec<u64>,
        ttl_ms: TimestampMs,
        now: TimestampMs,
    ) -> Result<u64, RfqError> {
        let rfq = self.rfqs.get(&rfq_id).ok_or(RfqError::UnknownRfq)?;
        Self::check_quotable(rfq, maker, &leg_prices_ticks, now)?;
        let qty = rfq.remaining_lots.clone();
        self.send_quote_sized(maker, rfq_id, leg_prices_ticks, qty, ttl_ms, now)
    }

    /// Maker submits a **partial-size** quote: the dealer commits only
    /// `qty_lots[i]` per leg (the Paradigm “working size” — a dealer may
    /// not want the whole block). Each committed quantity must be positive
    /// and may not exceed the RFQ's remaining size on that leg.
    pub fn send_quote_sized(
        &mut self,
        maker: SubaccountId,
        rfq_id: u64,
        leg_prices_ticks: Vec<u64>,
        qty_lots: Vec<u64>,
        ttl_ms: TimestampMs,
        now: TimestampMs,
    ) -> Result<u64, RfqError> {
        let rfq = self.rfqs.get(&rfq_id).ok_or(RfqError::UnknownRfq)?;
        Self::check_quotable(rfq, maker, &leg_prices_ticks, now)?;
        if qty_lots.len() != rfq.legs.len() {
            return Err(RfqError::LegCountMismatch);
        }
        if qty_lots.contains(&0) {
            return Err(RfqError::InvalidQty);
        }
        for (i, &q) in qty_lots.iter().enumerate() {
            if q > rfq.remaining_lots[i] {
                return Err(RfqError::InvalidQty);
            }
        }
        let total = Self::flow_for(rfq, &leg_prices_ticks, &qty_lots)?;
        let valid_until = Self::quote_valid_until(ttl_ms, now)?;
        let legs_len = rfq.legs.len();
        let quote_id = self.alloc_quote_id()?;
        self.insert_quote(Quote {
            quote_id,
            rfq_id,
            maker,
            leg_prices_ticks,
            qty_lots,
            filled_lots: vec![0; legs_len],
            total_taker_flow_quote_minor: total,
            created_ts: now,
            valid_until,
            status: QuoteStatus::Live,
        });
        Ok(quote_id)
    }

    /// Atomic cancel-and-replace (Derive `replace_quote`): cancels the old
    /// quote and submits the new one in a single step.
    ///
    /// The new quote is fully validated **first** — if validation fails the
    /// old quote is left untouched. The new quote gets a fresh id, rides on
    /// the same RFQ, and is sized at the RFQ's current remaining size.
    pub fn replace_quote(
        &mut self,
        maker: SubaccountId,
        quote_id: u64,
        leg_prices_ticks: Vec<u64>,
        ttl_ms: TimestampMs,
        now: TimestampMs,
    ) -> Result<u64, RfqError> {
        let old_rfq_id = match self.quotes.get(&quote_id) {
            None => return Err(RfqError::UnknownQuote),
            Some(q) if q.maker != maker => return Err(RfqError::NotQuoteOwner),
            Some(q) if q.status != QuoteStatus::Live => return Err(RfqError::AlreadySettled),
            Some(q) => q.rfq_id,
        };
        let rfq = self.rfqs.get(&old_rfq_id).ok_or(RfqError::UnknownRfq)?;
        Self::check_quotable(rfq, maker, &leg_prices_ticks, now)?;
        let qty = rfq.remaining_lots.clone();
        let total = Self::flow_for(rfq, &leg_prices_ticks, &qty)?;
        let valid_until = Self::quote_valid_until(ttl_ms, now)?;
        let legs_len = rfq.legs.len();

        // Validated — now mutate atomically.
        if let Some(old) = self.quotes.get_mut(&quote_id) {
            old.status = QuoteStatus::Cancelled;
        }
        let new_id = self.alloc_quote_id()?;
        self.insert_quote(Quote {
            quote_id: new_id,
            rfq_id: old_rfq_id,
            maker,
            leg_prices_ticks,
            qty_lots: qty,
            filled_lots: vec![0; legs_len],
            total_taker_flow_quote_minor: total,
            created_ts: now,
            valid_until,
            status: QuoteStatus::Live,
        });
        Ok(new_id)
    }

    /// Maker withdraws a live quote.
    pub fn cancel_quote(&mut self, maker: SubaccountId, quote_id: u64) -> Result<(), RfqError> {
        let quote = self.quotes.get(&quote_id).ok_or(RfqError::UnknownQuote)?;
        if quote.maker != maker {
            return Err(RfqError::NotQuoteOwner);
        }
        if quote.status != QuoteStatus::Live {
            return Err(RfqError::AlreadySettled);
        }
        self.cancel_if_live(quote_id);
        Ok(())
    }

    /// Best live quote for an RFQ: the one with the **lowest total cost to
    /// the taker** — i.e. the highest `total_taker_flow_quote_minor`, since
    /// cost is `-flow` — with ties broken by the lowest quote id. `None`
    /// when there are no live quotes.
    #[must_use]
    pub fn best_quote(&self, rfq_id: u64) -> Option<&Quote> {
        let mut best: Option<&Quote> = None;
        for &qid in self.rfq_quotes.get(&rfq_id).into_iter().flatten() {
            let Some(q) = self.quotes.get(&qid) else {
                continue;
            };
            if q.status != QuoteStatus::Live {
                continue;
            }
            // Ascending id iteration keeps the earliest (lowest) id on ties.
            let better = match best {
                None => true,
                Some(b) => q.total_taker_flow_quote_minor > b.total_taker_flow_quote_minor,
            };
            if better {
                best = Some(q);
            }
        }
        best
    }

    /// Taker executes a quote **in full**: the take per leg is the
    /// minimum of the quote's and the RFQ's remaining size — a
    /// full-size quote therefore settles the RFQ, while a
    /// partial-size quote consumes only what the dealer committed.
    ///
    /// See [`execute_partial`](Self::execute_partial) for sized takes.
    pub fn execute(
        &mut self,
        taker: SubaccountId,
        rfq_id: u64,
        quote_id: u64,
        now: TimestampMs,
    ) -> Result<RfqExecution, RfqError> {
        let take = self.take_vector(taker, rfq_id, quote_id, None, now)?.0; // validated full take
        self.settle(taker, rfq_id, quote_id, &take, now)
    }

    /// Taker accepts a **partial** of one quote (Paradigm's “accept a
    /// partial”): the take vector says how many lots per leg to lift now.
    ///
    /// The remainder of the quote **stays firm** until its own TTL — that
    /// is the whole point of firmness: a partial fill does not release the
    /// dealer from their remaining commitment. The RFQ's remaining size
    /// shrinks by the take; the RFQ closes (`Filled`) only when every
    /// leg's remaining reaches zero. Other live quotes are NOT cancelled by
    /// a partial — they keep competing for the remainder.
    pub fn execute_partial(
        &mut self,
        taker: SubaccountId,
        rfq_id: u64,
        quote_id: u64,
        take_per_leg: Vec<u64>,
        now: TimestampMs,
    ) -> Result<RfqExecution, RfqError> {
        let take = self
            .take_vector(taker, rfq_id, quote_id, Some(&take_per_leg), now)?
            .0;
        self.settle(taker, rfq_id, quote_id, &take, now)
    }

    /// Convenience: execute the current **best** live quote in full —
    /// "the taker executes against the best one".
    pub fn execute_best(
        &mut self,
        taker: SubaccountId,
        rfq_id: u64,
        now: TimestampMs,
    ) -> Result<RfqExecution, RfqError> {
        let best_id = self
            .best_quote(rfq_id)
            .filter(|q| q.remaining_lots().iter().any(|&r| r > 0))
            .map(|q| q.quote_id)
            .ok_or(RfqError::UnknownQuote)?;
        self.execute(taker, rfq_id, best_id, now)
    }

    /// **Pure** take resolution (plan stage): validates the request and
    /// returns the execution record that `execute_partial` would settle —
    /// without mutating anything. The engine's planner calls this to
    /// build `RfqSettled` events; its apply stage then replays the take
    /// through `execute_partial`, which deterministically re-validates
    /// (idempotently) and mutates. `None` take = full execution.
    pub fn plan_take(
        &self,
        taker: SubaccountId,
        rfq_id: u64,
        quote_id: u64,
        take_per_leg: Option<&[u64]>,
        now: TimestampMs,
    ) -> Result<RfqExecution, RfqError> {
        let (take, maker) = self.take_vector(taker, rfq_id, quote_id, take_per_leg, now)?;
        let (rfq, quote) = match (self.rfqs.get(&rfq_id), self.quotes.get(&quote_id)) {
            (Some(r), Some(q)) => (r, q),
            _ => return Err(RfqError::UnknownRfq),
        };
        let flow = Self::flow_for(rfq, &quote.leg_prices_ticks, &take)?;
        let cost = flow.checked_neg().ok_or(RfqError::MathOverflow)?;
        if !cost_within_bounds(
            cost,
            rfq.min_total_cost_quote_minor,
            rfq.max_total_cost_quote_minor,
        ) {
            return Err(RfqError::BoundsViolation);
        }
        let mut legs = Vec::with_capacity(rfq.legs.len());
        for ((leg, &px), &t) in rfq.legs.iter().zip(&quote.leg_prices_ticks).zip(&take) {
            legs.push((leg.symbol.clone(), leg.side, t, px));
        }
        Ok(RfqExecution {
            rfq_id,
            quote_id,
            taker,
            maker,
            total_taker_flow_quote_minor: flow,
            legs,
        })
    }

    /// Validation core shared by `execute` and `execute_partial`.
    /// Returns `(take vector, maker)` on success; mutates nothing.
    #[allow(clippy::type_complexity)]
    fn take_vector(
        &self,
        taker: SubaccountId,
        rfq_id: u64,
        quote_id: u64,
        take_per_leg: Option<&[u64]>,
        now: TimestampMs,
    ) -> Result<(Vec<u64>, SubaccountId), RfqError> {
        let rfq = self.rfqs.get(&rfq_id).ok_or(RfqError::UnknownRfq)?;
        if rfq.status != RfqStatus::Open {
            return Err(RfqError::NotOpen);
        }
        if now > rfq.valid_until {
            return Err(RfqError::QuoteWindowClosed);
        }
        if rfq.taker != taker {
            return Err(RfqError::NotRfqOwner);
        }
        let quote = self.quotes.get(&quote_id).ok_or(RfqError::UnknownQuote)?;
        if quote.rfq_id != rfq_id {
            return Err(RfqError::UnknownQuote);
        }
        if quote.status != QuoteStatus::Live {
            return Err(RfqError::AlreadySettled);
        }
        if now > quote.valid_until {
            return Err(RfqError::QuoteWindowClosed);
        }
        let quote_rem = quote.remaining_lots();
        if quote_rem.iter().all(|&r| r == 0) {
            return Err(RfqError::AlreadySettled);
        }
        let legs = rfq.legs.len();
        let take: Vec<u64> = match take_per_leg {
            None => quote_rem
                .iter()
                .zip(&rfq.remaining_lots)
                .map(|(&q, &r)| q.min(r))
                .collect(),
            Some(req) => {
                if req.len() != legs {
                    return Err(RfqError::InvalidPartial);
                }
                for i in 0..legs {
                    if req[i] > quote_rem[i] || req[i] > rfq.remaining_lots[i] {
                        return Err(RfqError::InvalidPartial);
                    }
                }
                if req.iter().all(|&t| t == 0) {
                    return Err(RfqError::InvalidPartial);
                }
                req.to_vec()
            }
        };
        Ok((take, quote.maker))
    }

    /// Mutation core shared by `execute` and `execute_partial` — consumes
    /// `take` from the quote and the RFQ, enforces cost bounds, and builds
    /// the execution record. Atomic: all checks precede all mutations.
    fn settle(
        &mut self,
        taker: SubaccountId,
        rfq_id: u64,
        quote_id: u64,
        take: &[u64],
        _now: TimestampMs,
    ) -> Result<RfqExecution, RfqError> {
        // Cost bounds are enforced on the FLOW OF THE TAKE.
        let (rfq, quote) = match (self.rfqs.get(&rfq_id), self.quotes.get(&quote_id)) {
            (Some(r), Some(q)) => (r, q),
            _ => return Err(RfqError::UnknownRfq),
        };
        let flow = Self::flow_for(rfq, &quote.leg_prices_ticks, take)?;
        let cost = flow.checked_neg().ok_or(RfqError::MathOverflow)?;
        if !cost_within_bounds(
            cost,
            rfq.min_total_cost_quote_minor,
            rfq.max_total_cost_quote_minor,
        ) {
            return Err(RfqError::BoundsViolation);
        }

        // Build the execution record.
        let mut legs = Vec::with_capacity(rfq.legs.len());
        for ((leg, &px), &t) in rfq.legs.iter().zip(&quote.leg_prices_ticks).zip(take) {
            legs.push((leg.symbol.clone(), leg.side, t, px));
        }
        let maker = quote.maker;
        let execution = RfqExecution {
            rfq_id,
            quote_id,
            taker,
            maker,
            total_taker_flow_quote_minor: flow,
            legs,
        };

        // Mutations (all checks passed).
        let rfq_filled = match self.rfqs.get_mut(&rfq_id) {
            None => false,
            Some(r) => {
                for (rem, &t) in r.remaining_lots.iter_mut().zip(take) {
                    *rem = rem.saturating_sub(t);
                }
                let filled = r.remaining_lots.iter().all(|&rem| rem == 0);
                if filled {
                    r.status = RfqStatus::Filled;
                }
                filled
            }
        };
        if let Some(q) = self.quotes.get_mut(&quote_id) {
            for (filled, &t) in q.filled_lots.iter_mut().zip(take) {
                *filled = filled.saturating_add(t);
            }
            if q.fully_consumed() {
                q.status = QuoteStatus::Executed;
            }
        }
        if rfq_filled {
            // The RFQ is done: drain every other live quote.
            let others: Vec<u64> = self
                .rfq_quotes
                .get(&rfq_id)
                .into_iter()
                .flatten()
                .copied()
                .filter(|qid| *qid != quote_id)
                .collect();
            for qid in others {
                self.cancel_if_live(qid);
            }
        }
        Ok(execution)
    }

    /// Expire quotes and RFQs whose windows have passed.
    ///
    /// Quotes transition `Live -> Expired` on their own TTL; an `Open` RFQ
    /// whose `valid_until` passed becomes `Expired`. Returns the ids of
    /// RFQs that transitioned `Open -> Expired` on *this* sweep (a second
    /// sweep therefore returns nothing for them).
    pub fn sweep(&mut self, now: TimestampMs) -> Vec<u64> {
        for quote in self.quotes.values_mut() {
            if quote.status == QuoteStatus::Live && now > quote.valid_until {
                quote.status = QuoteStatus::Expired;
            }
        }
        let mut expired = Vec::new();
        for (id, rfq) in self.rfqs.iter_mut() {
            if rfq.status == RfqStatus::Open && now > rfq.valid_until {
                rfq.status = RfqStatus::Expired;
                expired.push(*id);
            }
        }
        expired
    }

    /// RFQs a given maker may currently see and quote: `Open`, inside their
    /// window, and open-to-all or naming the maker. Deterministic id order.
    #[must_use]
    pub fn visible_rfqs(&self, maker: SubaccountId, now: TimestampMs) -> Vec<&RfqRequest> {
        self.rfqs
            .values()
            .filter(|rfq| {
                rfq.status == RfqStatus::Open
                    && now <= rfq.valid_until
                    && (rfq.counterparties.is_empty() || rfq.counterparties.contains(&maker))
            })
            .collect()
    }

    /// Live quotes on an RFQ (the taker's best-quote channel), ascending id.
    #[must_use]
    pub fn live_quotes(&self, rfq_id: u64) -> Vec<&Quote> {
        self.rfq_quotes
            .get(&rfq_id)
            .into_iter()
            .flatten()
            .filter_map(|&qid| self.quotes.get(&qid))
            .filter(|q| q.status == QuoteStatus::Live)
            .collect()
    }

    /// **Privacy view** (the G-11 discipline): quotes are private to the
    /// requesting account. The RFQ's taker sees every live quote on it;
    /// a maker sees only their own; anyone else sees nothing. The engine
    /// MUST serve quote lists exclusively through this method.
    #[must_use]
    pub fn quotes_for_viewer(&self, rfq_id: u64, viewer: SubaccountId) -> Vec<&Quote> {
        let Some(rfq) = self.rfqs.get(&rfq_id) else {
            return Vec::new();
        };
        if viewer != rfq.taker {
            // A maker on the RFQ sees only their own quotes.
            return self
                .rfq_quotes
                .get(&rfq_id)
                .into_iter()
                .flatten()
                .filter_map(|&qid| self.quotes.get(&qid))
                .filter(|q| q.status == QuoteStatus::Live && q.maker == viewer)
                .collect();
        }
        self.live_quotes(rfq_id)
    }

    /// Live quotes held by one maker, ascending id — the index the engine
    /// walks for MMP trips and cancel-on-disconnect sweeps (G-37).
    #[must_use]
    pub fn live_quotes_by_maker(&self, maker: SubaccountId) -> Vec<&Quote> {
        self.maker_quotes
            .get(&maker)
            .into_iter()
            .flatten()
            .filter_map(|&qid| self.quotes.get(&qid))
            .filter(|q| q.status == QuoteStatus::Live)
            .collect()
    }

    /// Ids of RFQs that are Open and past their window (read-only scan
    /// for plan stages; the engine journals the expiry, apply mutates).
    pub fn expired_rfq_ids(&self, now: TimestampMs) -> Vec<u64> {
        self.rfqs
            .values()
            .filter(|r| r.status == RfqStatus::Open && now > r.valid_until)
            .map(|r| r.rfq_id)
            .collect()
    }

    /// Ids of quotes that are Live and past their window (read-only scan).
    pub fn expired_quote_ids(&self, now: TimestampMs) -> Vec<u64> {
        self.quotes
            .values()
            .filter(|q| q.status == QuoteStatus::Live && now > q.valid_until)
            .map(|q| q.quote_id)
            .collect()
    }

    /// Owner-agnostic cancel (Open -> Cancelled, live quotes cancelled).
    pub fn cancel_rfq_by_id(&mut self, rfq_id: u64) -> bool {
        if let Some(r) = self.rfqs.get_mut(&rfq_id) {
            if r.status == RfqStatus::Open {
                r.status = RfqStatus::Cancelled;
                let qids: Vec<u64> = self
                    .rfq_quotes
                    .get(&rfq_id)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                for qid in qids {
                    if let Some(q) = self.quotes.get_mut(&qid) {
                        if q.status == QuoteStatus::Live {
                            q.status = QuoteStatus::Cancelled;
                        }
                    }
                }
                return true;
            }
        }
        false
    }

    /// Owner-agnostic expiry (Open -> Expired, live quotes cancelled).
    pub fn expire_rfq_by_id(&mut self, rfq_id: u64) -> bool {
        if let Some(r) = self.rfqs.get_mut(&rfq_id) {
            if r.status == RfqStatus::Open {
                r.status = RfqStatus::Expired;
                let qids: Vec<u64> = self
                    .rfq_quotes
                    .get(&rfq_id)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                for qid in qids {
                    if let Some(q) = self.quotes.get_mut(&qid) {
                        if q.status == QuoteStatus::Live {
                            q.status = QuoteStatus::Cancelled;
                        }
                    }
                }
                return true;
            }
        }
        false
    }

    /// Owner-agnostic quote cancel (Live -> Cancelled).
    pub fn cancel_quote_by_id(&mut self, quote_id: u64) -> bool {
        if let Some(q) = self.quotes.get_mut(&quote_id) {
            if q.status == QuoteStatus::Live {
                q.status = QuoteStatus::Cancelled;
                return true;
            }
        }
        false
    }

    /// Owner-agnostic quote expiry (Live -> Expired).
    pub fn expire_quote_by_id(&mut self, quote_id: u64) -> bool {
        if let Some(q) = self.quotes.get_mut(&quote_id) {
            if q.status == QuoteStatus::Live {
                q.status = QuoteStatus::Expired;
                return true;
            }
        }
        false
    }

    /// Look up an RFQ by id.
    #[must_use]
    pub fn rfq(&self, rfq_id: u64) -> Option<&RfqRequest> {
        self.rfqs.get(&rfq_id)
    }

    /// Look up a quote by id.
    #[must_use]
    pub fn quote(&self, quote_id: u64) -> Option<&Quote> {
        self.quotes.get(&quote_id)
    }

    // -- internals ----------------------------------------------------------

    /// Validation shared by `send_quote` and `replace_quote`: RFQ open, in
    /// window, maker allowed to quote it, prices aligned and positive.
    fn check_quotable(
        rfq: &RfqRequest,
        maker: SubaccountId,
        leg_prices_ticks: &[u64],
        now: TimestampMs,
    ) -> Result<(), RfqError> {
        if rfq.status != RfqStatus::Open {
            return Err(RfqError::NotOpen);
        }
        if now > rfq.valid_until {
            return Err(RfqError::QuoteWindowClosed);
        }
        if maker == rfq.taker {
            return Err(RfqError::SelfQuote);
        }
        if !rfq.counterparties.is_empty() && !rfq.counterparties.contains(&maker) {
            return Err(RfqError::NotCounterparty);
        }
        if leg_prices_ticks.len() != rfq.legs.len() {
            return Err(RfqError::LegCountMismatch);
        }
        if leg_prices_ticks.contains(&0) {
            return Err(RfqError::ZeroPrice);
        }
        Ok(())
    }

    /// Sum of taker flows across legs at the quoted prices for a given
    /// per-leg quantity vector (partial-execution pricer).
    fn flow_for(
        rfq: &RfqRequest,
        leg_prices_ticks: &[u64],
        qty_lots: &[u64],
    ) -> Result<i128, RfqError> {
        let mut total: i128 = 0;
        for ((leg, &price), &qty) in rfq.legs.iter().zip(leg_prices_ticks).zip(qty_lots) {
            total = total
                .checked_add(
                    leg.taker_flow_at_qty(price, qty)
                        .ok_or(RfqError::MathOverflow)?,
                )
                .ok_or(RfqError::MathOverflow)?;
        }
        Ok(total)
    }

    /// Quote validity end from a TTL; rejects zero and overflowing TTLs.
    fn quote_valid_until(ttl_ms: TimestampMs, now: TimestampMs) -> Result<TimestampMs, RfqError> {
        if ttl_ms == 0 {
            return Err(RfqError::InvalidTtl);
        }
        now.checked_add(ttl_ms).ok_or(RfqError::InvalidTtl)
    }

    /// Allocate the next quote id.
    fn alloc_quote_id(&mut self) -> Result<u64, RfqError> {
        let id = self.next_quote_id;
        self.next_quote_id = id.checked_add(1).ok_or(RfqError::MathOverflow)?;
        Ok(id)
    }

    /// Insert a freshly built `Live` quote and index it.
    fn insert_quote(&mut self, quote: Quote) {
        self.rfq_quotes
            .entry(quote.rfq_id)
            .or_default()
            .insert(quote.quote_id);
        self.maker_quotes
            .entry(quote.maker)
            .or_default()
            .insert(quote.quote_id);
        self.quotes.insert(quote.quote_id, quote);
    }

    /// Transition a quote to `Cancelled` if (and only if) it is `Live`.
    fn cancel_if_live(&mut self, quote_id: u64) {
        match self.quotes.get_mut(&quote_id) {
            Some(q) if q.status == QuoteStatus::Live => q.status = QuoteStatus::Cancelled,
            _ => {}
        }
    }
}

/// Check the taker's `cost` (`-total_flow`) against optional unsigned bounds.
///
/// A negative cost (taker receives net) always satisfies `max` and never
/// satisfies `min` — bounds are unsigned, i.e. constrain non-negative costs.
fn cost_within_bounds(cost: i128, min: Option<u128>, max: Option<u128>) -> bool {
    if let Some(min) = min {
        match u128::try_from(cost) {
            Ok(c) if c >= min => {}
            _ => return false, // negative cost, or below the minimum
        }
    }
    if let Some(max) = max {
        if let Ok(c) = u128::try_from(cost) {
            if c > max {
                return false;
            }
        }
        // negative cost is always within a non-negative maximum
    }
    true
}

// ---------------------------------------------------------------------------
// Multi-leg fee grouping (G-38 helper — called by the engine at settlement)
// ---------------------------------------------------------------------------

/// Fee classification of one leg for the grouped-discount rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LegFeeClass {
    /// Taker bought a call.
    LongCall,
    /// Taker bought a put.
    LongPut,
    /// Taker sold a call.
    ShortCall,
    /// Taker sold a put.
    ShortPut,
    /// Perpetual leg.
    Perp,
}

/// Classify a leg given its instrument family and the taker's side.
///
/// * `is_option == false` is always [`LegFeeClass::Perp`];
/// * option legs split long/short by side and call/put by kind.
#[must_use]
pub fn classify_leg(is_option: bool, is_call: bool, taker_side: Side) -> LegFeeClass {
    if !is_option {
        return LegFeeClass::Perp;
    }
    match (is_call, taker_side) {
        (true, Side::Bid) => LegFeeClass::LongCall,
        (true, Side::Ask) => LegFeeClass::ShortCall,
        (false, Side::Bid) => LegFeeClass::LongPut,
        (false, Side::Ask) => LegFeeClass::ShortPut,
    }
}

/// Derive-V3 grouped discount ladder for multi-leg RFQ fees.
///
/// Legs are grouped by [`LegFeeClass`]. The group with the largest total
/// fee pays full. Among the remaining groups, ranked cheapest first, the
/// discounts are: **100%** (cheapest), **50%** (2nd cheapest), **50%** (3rd
/// cheapest), and 0% for every other group. Group totals tie-break by class
/// declaration order, so results are deterministic; a single group is
/// trivially the largest and therefore pays full fee.
///
/// Discounts round the fee **down** (user favor) — with the 100%/50%/0%
/// ladder this is exact integer halving, so no precision is lost.
///
/// Returns per-leg discounted fees in input order.
#[must_use]
pub fn apply_group_discounts(leg_fees: &[(LegFeeClass, u128)]) -> Vec<u128> {
    // Aggregate fees per class (saturating: realistic fees never approach
    // u128::MAX and a saturating sum keeps this total function).
    let mut groups: BTreeMap<LegFeeClass, u128> = BTreeMap::new();
    for &(class, fee) in leg_fees {
        let total = groups.entry(class).or_insert(0);
        *total = (*total).saturating_add(fee);
    }

    // The largest group pays full. Ties on the total go to the earliest
    // declared class (max_by_key over a map with unique keys is exact).
    let largest = groups
        .iter()
        .max_by_key(|(class, total)| (*total, std::cmp::Reverse(*class)))
        .map(|(&class, _)| class);

    // Rank the remaining groups cheapest first (ties -> class order).
    let mut rest: Vec<(LegFeeClass, u128)> = groups
        .into_iter()
        .filter(|(class, _)| Some(*class) != largest)
        .collect();
    rest.sort_unstable_by_key(|&(class, total)| (total, class));

    let mut discount_bps: BTreeMap<LegFeeClass, u64> = BTreeMap::new();
    for (rank, &(class, _)) in rest.iter().enumerate() {
        let bps = match rank {
            0 => 10_000,    // cheapest group: 100% off
            1 | 2 => 5_000, // 2nd + 3rd cheapest: 50% off
            _ => 0,
        };
        discount_bps.insert(class, bps);
    }

    leg_fees
        .iter()
        .map(|&(class, fee)| {
            match discount_bps.get(&class).copied().unwrap_or(0) {
                10_000 => 0,
                5_000 => fee / 2, // floor -> user favor
                _ => fee,         // largest group (or rank >= 3): full fee
            }
        })
        .collect()
}

/// One [`is_box_spread`] input leg: strike, is_call, taker side, underlying,
/// expiry.
type BoxLeg<'a> = (u128, bool, Side, &'a str, u64);

/// Detect a box spread: 4 option legs, same underlying + expiry, strikes
/// `K1 != K2`, with long call + short put at `K1` and short call + long put
/// at `K2` — in either orientation (which strike carries the long synthetic
/// is interchangeable).
///
/// Inputs per leg: `(strike, is_call, taker_side, underlying, expiry)`.
#[must_use]
pub fn is_box_spread(legs: &[(u128, bool, Side, &str, u64)]) -> bool {
    if legs.len() != 4 {
        return false;
    }
    let underlying = legs[0].3;
    let expiry = legs[0].4;
    if !legs.iter().all(|l| l.3 == underlying && l.4 == expiry) {
        return false;
    }

    // Group by strike: a box has exactly two strikes, each carrying exactly
    // one call and one put.
    let mut by_strike: BTreeMap<u128, Vec<&BoxLeg<'_>>> = BTreeMap::new();
    for leg in legs {
        by_strike.entry(leg.0).or_default().push(leg);
    }
    if by_strike.len() != 2 {
        return false;
    }

    // Per strike: Some(true) = long synthetic (long call + short put),
    //             Some(false) = short synthetic (short call + long put),
    //             None = anything else.
    let synths: Vec<Option<bool>> = by_strike.values().map(|g| synth_pair(g)).collect();
    matches!(
        synths.as_slice(),
        [Some(true), Some(false)] | [Some(false), Some(true)]
    )
}

/// One strike's pair orientation, if it is a clean call+put synthetic.
fn synth_pair(group: &[&BoxLeg<'_>]) -> Option<bool> {
    if group.len() != 2 {
        return None;
    }
    let mut call: Option<Side> = None;
    let mut put: Option<Side> = None;
    for leg in group {
        if leg.1 {
            call = Some(leg.2);
        } else {
            put = Some(leg.2);
        }
    }
    match (call, put) {
        (Some(Side::Bid), Some(Side::Ask)) => Some(true),
        (Some(Side::Ask), Some(Side::Bid)) => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const TAKER: SubaccountId = 1;
    const MAKER_A: SubaccountId = 2;
    const MAKER_B: SubaccountId = 3;
    const OUTSIDER: SubaccountId = 9;

    /// $1.00 ticks, 0.01-base lots, 5 base decimals — the audit conventions.
    fn spec() -> LegSpec {
        LegSpec {
            tick_size_quote_minor: 100,
            lot_size_base_minor: 1_000,
            base_decimals: 5,
        }
    }

    fn leg(symbol: &str, side: Side, qty: u64) -> RfqLeg {
        RfqLeg {
            symbol: symbol.into(),
            side,
            qty_lots: qty,
            spec: spec(),
        }
    }

    fn simple_rfq(book: &mut RfqBook, now: TimestampMs) -> u64 {
        book.create_rfq(
            TAKER,
            vec![leg("BTC-PERP", Side::Bid, 1)],
            vec![],
            None,
            None,
            60_000,
            now,
        )
        .unwrap()
    }

    // 1. create + quote + execute happy path --------------------------------

    #[test]
    fn create_quote_execute_happy_path() {
        assert_eq!(RfqBook::DEFAULT_WINDOW_MS, 600_000);
        let mut book = RfqBook::new();
        let now = 1_000_000;
        let rfq_id = book
            .create_rfq(
                TAKER,
                vec![leg("BTC-PERP", Side::Bid, 2)],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        assert_eq!(rfq_id, 1);
        assert_eq!(book.rfq(rfq_id).unwrap().status, RfqStatus::Open);

        let q1 = book
            .send_quote(MAKER_A, rfq_id, vec![65_000], 30_000, now + 1_000)
            .unwrap();
        let q2 = book
            .send_quote(MAKER_B, rfq_id, vec![64_000], 30_000, now + 2_000)
            .unwrap();
        assert_eq!((q1, q2), (1, 2));
        assert_eq!(book.live_quotes(rfq_id).len(), 2);

        // Taker executes maker B's cheaper quote.
        let exec = book.execute(TAKER, rfq_id, q2, now + 3_000).unwrap();
        assert_eq!(exec.rfq_id, rfq_id);
        assert_eq!(exec.quote_id, q2);
        assert_eq!(exec.taker, TAKER);
        assert_eq!(exec.maker, MAKER_B);
        assert_eq!(
            exec.legs,
            vec![("BTC-PERP".to_string(), Side::Bid, 2, 64_000)]
        );
        // 2 lots * 0.01 base * $64,000 = $1,280.00 -> taker pays 128_000 minor.
        assert_eq!(exec.total_taker_flow_quote_minor, -128_000);

        assert_eq!(book.rfq(rfq_id).unwrap().status, RfqStatus::Filled);
        assert_eq!(book.quote(q2).unwrap().status, QuoteStatus::Executed);
        // The loser's quote was cancelled atomically.
        assert_eq!(book.quote(q1).unwrap().status, QuoteStatus::Cancelled);
        assert!(book.live_quotes(rfq_id).is_empty());
        assert_eq!(book.best_quote(rfq_id), None);
    }

    // 2. taker cannot quote own RFQ -------------------------------------------

    #[test]
    fn taker_cannot_quote_own_rfq() {
        let mut book = RfqBook::new();
        let now = 2_000_000;
        let rfq = simple_rfq(&mut book, now);
        let err = book
            .send_quote(TAKER, rfq, vec![100], 10_000, now)
            .unwrap_err();
        assert_eq!(err, RfqError::SelfQuote);
        assert!(book.live_quotes(rfq).is_empty());
    }

    // 3. directed RFQ: only named makers see and quote it ---------------------

    #[test]
    fn directed_rfq_restricts_quoting_and_visibility() {
        let mut book = RfqBook::new();
        let now = 5_000_000;
        let open = book
            .create_rfq(
                TAKER,
                vec![leg("ETH-PERP", Side::Ask, 5)],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        let directed = book
            .create_rfq(
                TAKER,
                vec![leg("BTC-PERP", Side::Bid, 1)],
                vec![MAKER_A],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();

        // Non-named maker cannot quote.
        let err = book
            .send_quote(MAKER_B, directed, vec![100], 10_000, now)
            .unwrap_err();
        assert_eq!(err, RfqError::NotCounterparty);
        // Named maker can.
        book.send_quote(MAKER_A, directed, vec![101], 10_000, now)
            .unwrap();

        // Visibility filters: maker B sees only the open-to-all RFQ.
        let vis_b: Vec<u64> = book
            .visible_rfqs(MAKER_B, now)
            .iter()
            .map(|r| r.rfq_id)
            .collect();
        assert_eq!(vis_b, vec![open]);
        // Maker A sees both.
        let vis_a: Vec<u64> = book
            .visible_rfqs(MAKER_A, now)
            .iter()
            .map(|r| r.rfq_id)
            .collect();
        assert_eq!(vis_a, vec![open, directed]);
    }

    // 4. open-to-all RFQ is visible to everyone ------------------------------

    #[test]
    fn open_rfq_visible_to_all_makers() {
        let mut book = RfqBook::new();
        let now = 7_000_000;
        let rfq = simple_rfq(&mut book, now);
        for maker in [MAKER_A, MAKER_B, OUTSIDER] {
            let vis: Vec<u64> = book
                .visible_rfqs(maker, now)
                .iter()
                .map(|r| r.rfq_id)
                .collect();
            assert_eq!(vis, vec![rfq], "maker {maker} should see the open rfq");
        }
        // An out-of-window RFQ is not visible to anyone.
        assert!(book.visible_rfqs(MAKER_A, now + 60_001).is_empty());
    }

    // 5. TTL: quotes + RFQs expire in sweep; execute after expiry fails -------

    #[test]
    fn ttl_expiry_sweep_and_execute_fails() {
        let mut book = RfqBook::new();
        let now = 10_000_000;
        let rfq = simple_rfq(&mut book, now);
        let q = book
            .send_quote(MAKER_A, rfq, vec![50_000], 10_000, now)
            .unwrap();

        // Quote expires at now + 10_000 while the RFQ lives to now + 60_000.
        assert_eq!(book.quote(q).unwrap().status, QuoteStatus::Live);
        assert!(book.sweep(now + 10_001).is_empty());
        assert_eq!(book.quote(q).unwrap().status, QuoteStatus::Expired);
        assert_eq!(book.best_quote(rfq), None);

        // Executing the expired quote fails.
        assert_eq!(
            book.execute(TAKER, rfq, q, now + 10_001).unwrap_err(),
            RfqError::AlreadySettled
        );

        // RFQ window passes (no sweep yet): execute -> window closed.
        let rfq2 = book
            .create_rfq(
                TAKER,
                vec![leg("BTC-PERP", Side::Bid, 1)],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        let q2 = book
            .send_quote(MAKER_A, rfq2, vec![50_000], 90_000, now)
            .unwrap();
        assert_eq!(
            book.execute(TAKER, rfq2, q2, now + 60_001).unwrap_err(),
            RfqError::QuoteWindowClosed
        );

        // Sweep transitions the RFQ to Expired; execute then fails NotOpen.
        assert_eq!(book.sweep(now + 60_002), vec![rfq, rfq2]);
        assert_eq!(book.rfq(rfq).unwrap().status, RfqStatus::Expired);
        assert_eq!(book.rfq(rfq2).unwrap().status, RfqStatus::Expired);
        assert_eq!(
            book.execute(TAKER, rfq2, q2, now + 60_003).unwrap_err(),
            RfqError::NotOpen
        );
        // Quoting an expired RFQ fails too.
        assert_eq!(
            book.send_quote(MAKER_B, rfq2, vec![1], 10_000, now + 60_003)
                .unwrap_err(),
            RfqError::NotOpen
        );
    }

    // 6. replace_quote: atomicity, old-id death, new-id life -----------------

    #[test]
    fn replace_quote_atomicity() {
        let mut book = RfqBook::new();
        let now = 20_000_000;
        let rfq = simple_rfq(&mut book, now);
        let q1 = book
            .send_quote(MAKER_A, rfq, vec![50_000], 60_000, now)
            .unwrap();

        // Failed replace (zero price) leaves the old quote untouched.
        let err = book
            .replace_quote(MAKER_A, q1, vec![0], 60_000, now + 1)
            .unwrap_err();
        assert_eq!(err, RfqError::ZeroPrice);
        assert_eq!(book.quote(q1).unwrap().status, QuoteStatus::Live);
        assert_eq!(book.quote(q1).unwrap().leg_prices_ticks, vec![50_000]);

        // Successful replace: old id dead, new id live with new prices.
        let q2 = book
            .replace_quote(MAKER_A, q1, vec![49_000], 60_000, now + 2)
            .unwrap();
        assert_ne!(q1, q2);
        assert_eq!(book.quote(q1).unwrap().status, QuoteStatus::Cancelled);
        let new_q = book.quote(q2).unwrap();
        assert_eq!(new_q.status, QuoteStatus::Live);
        assert_eq!(new_q.leg_prices_ticks, vec![49_000]);
        // 1 lot * 0.01 base * $49,000 = $490.00 -> taker pays 49_000 minor.
        assert_eq!(new_q.total_taker_flow_quote_minor, -49_000);

        // Replacing a dead quote fails.
        assert_eq!(
            book.replace_quote(MAKER_A, q1, vec![48_000], 60_000, now + 3)
                .unwrap_err(),
            RfqError::AlreadySettled
        );
        // Another maker cannot replace someone else's quote.
        assert_eq!(
            book.replace_quote(MAKER_B, q2, vec![48_000], 60_000, now + 4)
                .unwrap_err(),
            RfqError::NotQuoteOwner
        );
    }

    // 7. best_quote picks lowest taker cost; tie by quote id ------------------

    #[test]
    fn best_quote_lowest_cost_tie_by_id() {
        let mut book = RfqBook::new();
        let now = 30_000_000;
        let rfq = book
            .create_rfq(
                TAKER,
                vec![leg("BTC-PERP", Side::Bid, 3)],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        let qa = book
            .send_quote(MAKER_A, rfq, vec![70_000], 60_000, now)
            .unwrap(); // cost 210_000
        let qb = book
            .send_quote(MAKER_B, rfq, vec![69_000], 60_000, now)
            .unwrap(); // cost 207_000
        let qc = book
            .send_quote(MAKER_A, rfq, vec![71_000], 60_000, now)
            .unwrap(); // cost 213_000
        assert!(qa < qb && qb < qc);
        assert_eq!(book.best_quote(rfq).unwrap().quote_id, qb);

        // Tie: identical prices -> identical flow -> lowest quote id wins.
        let rfq2 = book
            .create_rfq(
                TAKER,
                vec![leg("ETH-PERP", Side::Bid, 1)],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        let t1 = book
            .send_quote(MAKER_A, rfq2, vec![3_000], 60_000, now)
            .unwrap();
        let t2 = book
            .send_quote(MAKER_B, rfq2, vec![3_000], 60_000, now)
            .unwrap();
        assert_eq!(book.best_quote(rfq2).unwrap().quote_id, t1);
        assert_ne!(t1, t2);

        // Sell-side package: taker receives; lowest cost = highest receipt.
        let rfq3 = book
            .create_rfq(
                TAKER,
                vec![leg("ETH-PERP", Side::Ask, 2)],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        book.send_quote(MAKER_A, rfq3, vec![3_000], 60_000, now)
            .unwrap(); // +6_000
        let qs = book
            .send_quote(MAKER_B, rfq3, vec![3_100], 60_000, now)
            .unwrap(); // +6_200
        assert_eq!(book.best_quote(rfq3).unwrap().quote_id, qs);
    }

    // 8. execute enforces min/max total cost ----------------------------------

    #[test]
    fn execute_enforces_cost_bounds() {
        let mut book = RfqBook::new();
        let now = 40_000_000;
        // Buy 1 lot: cost at P ticks = P minor with the default spec.
        let rfq = book
            .create_rfq(
                TAKER,
                vec![leg("BTC-PERP", Side::Bid, 1)],
                vec![],
                Some(40_000),
                Some(60_000),
                60_000,
                now,
            )
            .unwrap();

        let too_cheap = book
            .send_quote(MAKER_A, rfq, vec![30_000], 60_000, now)
            .unwrap();
        let ok = book
            .send_quote(MAKER_A, rfq, vec![50_000], 60_000, now + 1)
            .unwrap();
        let too_dear = book
            .send_quote(MAKER_B, rfq, vec![70_000], 60_000, now + 2)
            .unwrap();

        assert_eq!(
            book.execute(TAKER, rfq, too_cheap, now + 3).unwrap_err(),
            RfqError::BoundsViolation
        );
        assert_eq!(
            book.execute(TAKER, rfq, too_dear, now + 4).unwrap_err(),
            RfqError::BoundsViolation
        );
        // Violations must not mutate state.
        assert_eq!(book.rfq(rfq).unwrap().status, RfqStatus::Open);
        assert_eq!(book.quote(too_cheap).unwrap().status, QuoteStatus::Live);

        let exec = book.execute(TAKER, rfq, ok, now + 5).unwrap();
        assert_eq!(exec.total_taker_flow_quote_minor, -50_000);
        assert_eq!(book.rfq(rfq).unwrap().status, RfqStatus::Filled);

        // min > max at creation is rejected outright.
        assert_eq!(
            book.create_rfq(
                TAKER,
                vec![leg("BTC-PERP", Side::Bid, 1)],
                vec![],
                Some(100),
                Some(50),
                60_000,
                now
            )
            .unwrap_err(),
            RfqError::BoundsViolation
        );
    }

    // 9. cancel_rfq: owner only, only while open -------------------------------

    #[test]
    fn cancel_rfq_owner_only_while_open() {
        let mut book = RfqBook::new();
        let now = 50_000_000;
        let rfq = simple_rfq(&mut book, now);
        let q = book
            .send_quote(MAKER_A, rfq, vec![50_000], 60_000, now)
            .unwrap();

        // Non-owner cannot cancel; unknown rfq errors.
        assert_eq!(
            book.cancel_rfq(MAKER_A, rfq).unwrap_err(),
            RfqError::NotRfqOwner
        );
        assert_eq!(
            book.cancel_rfq(TAKER, 999).unwrap_err(),
            RfqError::UnknownRfq
        );

        book.cancel_rfq(TAKER, rfq).unwrap();
        assert_eq!(book.rfq(rfq).unwrap().status, RfqStatus::Cancelled);
        // Cancelling drained the live quotes.
        assert_eq!(book.quote(q).unwrap().status, QuoteStatus::Cancelled);

        // Not cancellable twice; not quotable or executable anymore.
        assert_eq!(book.cancel_rfq(TAKER, rfq).unwrap_err(), RfqError::NotOpen);
        assert_eq!(
            book.send_quote(MAKER_B, rfq, vec![1], 60_000, now)
                .unwrap_err(),
            RfqError::NotOpen
        );
        assert_eq!(
            book.execute(TAKER, rfq, q, now).unwrap_err(),
            RfqError::NotOpen
        );
    }

    // 10. zero qty / empty legs / zero ttl rejected ---------------------------

    #[test]
    fn create_and_quote_validations() {
        let mut book = RfqBook::new();
        let now = 60_000_000;
        assert_eq!(
            book.create_rfq(TAKER, vec![], vec![], None, None, 60_000, now)
                .unwrap_err(),
            RfqError::InvalidQty
        );
        assert_eq!(
            book.create_rfq(
                TAKER,
                vec![leg("X", Side::Bid, 0)],
                vec![],
                None,
                None,
                60_000,
                now
            )
            .unwrap_err(),
            RfqError::InvalidQty
        );
        assert_eq!(
            book.create_rfq(
                TAKER,
                vec![leg("X", Side::Bid, 1)],
                vec![],
                None,
                None,
                0,
                now
            )
            .unwrap_err(),
            RfqError::InvalidTtl
        );
        assert_eq!(
            book.create_rfq(
                TAKER,
                vec![leg("X", Side::Bid, 1)],
                vec![TAKER],
                None,
                None,
                60_000,
                now
            )
            .unwrap_err(),
            RfqError::SelfQuote
        );
        // TTL that overflows the clock is rejected.
        assert_eq!(
            book.create_rfq(
                TAKER,
                vec![leg("X", Side::Bid, 1)],
                vec![],
                None,
                None,
                u64::MAX,
                u64::MAX
            )
            .unwrap_err(),
            RfqError::InvalidTtl
        );
        // Quote TTL must also be positive.
        let rfq = simple_rfq(&mut book, now);
        assert_eq!(
            book.send_quote(MAKER_A, rfq, vec![100], 0, now)
                .unwrap_err(),
            RfqError::InvalidTtl
        );
    }

    // 11. leg notional math matches poc-core conventions ------------------------

    #[test]
    fn leg_notional_matches_core_conventions() {
        // Spec per the task: tick = 100 ($1.00), lot = 1000 (0.01 base),
        // base_decimals = 5.
        let l = leg("BTC-PERP", Side::Bid, 3);
        // 3 lots (0.03 base) at 65_000 ticks ($65,000) = $1,950.00.
        assert_eq!(l.notional(65_000), Some(195_000));
        assert_eq!(l.taker_flow(65_000), Some(-195_000));
        let s = leg("BTC-PERP", Side::Ask, 3);
        assert_eq!(s.taker_flow(65_000), Some(195_000));
        // With this spec notional == ticks * lots exactly (100 * 1000 = 10^5).
        assert_eq!(l.notional(1), Some(3));
        assert_eq!(l.notional(123), Some(369));

        // Cross-check against poc-core's own instrument math.
        let inst = poc_core::Instrument::Perp(poc_core::PerpMarket {
            tick_size_quote_minor: 100,
            lot_size_base_minor: 1_000,
            base_decimals: 5,
            ..poc_core::PerpMarket::default()
        });
        for price in [0_u64, 1, 999, 65_000, u64::MAX / 2] {
            assert_eq!(inst.notional_quote_minor(price, 3), l.notional(price));
        }

        // Half-up rounding: tick=1, lot=1, base_decimals=1 -> ticks/10.
        let odd = RfqLeg {
            symbol: "X".into(),
            side: Side::Bid,
            qty_lots: 1,
            spec: LegSpec {
                tick_size_quote_minor: 1,
                lot_size_base_minor: 1,
                base_decimals: 1,
            },
        };
        assert_eq!(odd.notional(5), Some(1)); // 0.5 -> half-up -> 1
        assert_eq!(odd.notional(4), Some(0)); // 0.4 -> 0
        assert_eq!(odd.notional(15), Some(2)); // 1.5 -> 2

        // Overflow is None, and quoting such a package is MathOverflow.
        let huge = RfqLeg {
            symbol: "X".into(),
            side: Side::Bid,
            qty_lots: u64::MAX,
            spec: LegSpec {
                tick_size_quote_minor: 2,
                lot_size_base_minor: 1,
                base_decimals: 0,
            },
        };
        assert_eq!(huge.notional(u64::MAX), None);
        assert_eq!(huge.taker_flow(u64::MAX), None);
        let mut book = RfqBook::new();
        let rfq = book
            .create_rfq(TAKER, vec![huge.clone()], vec![], None, None, 60_000, 0)
            .unwrap();
        assert_eq!(
            book.send_quote(MAKER_A, rfq, vec![u64::MAX], 60_000, 0)
                .unwrap_err(),
            RfqError::MathOverflow
        );
    }

    // 12. quote leg-count mismatch rejected ------------------------------------

    #[test]
    fn quote_leg_count_mismatch_rejected() {
        let mut book = RfqBook::new();
        let now = 70_000_000;
        let rfq = book
            .create_rfq(
                TAKER,
                vec![leg("A", Side::Bid, 1), leg("B", Side::Ask, 2)],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        assert_eq!(
            book.send_quote(MAKER_A, rfq, vec![100], 60_000, now)
                .unwrap_err(),
            RfqError::LegCountMismatch
        );
        assert_eq!(
            book.send_quote(MAKER_A, rfq, vec![100, 200, 300], 60_000, now)
                .unwrap_err(),
            RfqError::LegCountMismatch
        );
        assert_eq!(
            book.send_quote(MAKER_A, rfq, vec![100, 0], 60_000, now)
                .unwrap_err(),
            RfqError::ZeroPrice
        );
        // Two-leg quote is fine; mixed sides net correctly:
        // buy 1 lot @ 100 ticks = 100 minor out, sell 2 lots @ 50 ticks = 100 in.
        let q = book
            .send_quote(MAKER_A, rfq, vec![100, 50], 60_000, now)
            .unwrap();
        assert_eq!(book.quote(q).unwrap().total_taker_flow_quote_minor, 0);
    }

    // 13. sweep returns expired rfq ids; double sweep idempotent ---------------

    #[test]
    fn sweep_returns_expired_rfqs_idempotent() {
        let mut book = RfqBook::new();
        let now = 80_000_000;
        let r1 = book
            .create_rfq(
                TAKER,
                vec![leg("A", Side::Bid, 1)],
                vec![],
                None,
                None,
                10_000,
                now,
            )
            .unwrap();
        let r2 = book
            .create_rfq(
                TAKER,
                vec![leg("B", Side::Bid, 1)],
                vec![],
                None,
                None,
                20_000,
                now,
            )
            .unwrap();
        let _r3 = book
            .create_rfq(
                TAKER,
                vec![leg("C", Side::Bid, 1)],
                vec![],
                None,
                None,
                90_000,
                now,
            )
            .unwrap();

        assert_eq!(book.sweep(now + 15_000), vec![r1]);
        assert_eq!(book.rfq(r1).unwrap().status, RfqStatus::Expired);
        assert_eq!(book.rfq(r2).unwrap().status, RfqStatus::Open);

        // A re-sweep at an earlier timestamp changes nothing.
        assert!(book.sweep(now + 16_000).is_empty());
        assert_eq!(book.sweep(now + 25_000), vec![r2]);
        // Double sweep is idempotent.
        assert!(book.sweep(now + 26_000).is_empty());
        assert!(book.sweep(now + 30_000).is_empty());
    }

    // 15. multi-leg fee grouping ladder ---------------------------------------

    #[test]
    fn group_discount_ladder() {
        use LegFeeClass::{LongCall, LongPut, Perp, ShortCall, ShortPut};

        // Call spread (2 groups): cheapest group free, largest pays full.
        let spread = vec![(LongCall, 100_u128), (ShortCall, 60)];
        assert_eq!(apply_group_discounts(&spread), vec![100, 0]);

        // Risk reversal (3 groups): 100% / 50% / 0%.
        let rr = vec![(LongCall, 100), (ShortPut, 50), (ShortCall, 30)];
        assert_eq!(apply_group_discounts(&rr), vec![100, 25, 0]);

        // Two legs in the same group: single group is trivially the
        // largest -> no discount on either leg.
        let same = vec![(LongCall, 100), (LongCall, 80)];
        assert_eq!(apply_group_discounts(&same), vec![100, 80]);

        // Four groups: cheapest free, 2nd + 3rd cheapest half, largest full.
        // Totals: LongCall 200 (largest), LongPut 40 (cheapest),
        // ShortPut 60 (2nd), ShortCall 90 (3rd).
        let four = vec![
            (LongCall, 200),
            (LongPut, 40),
            (ShortCall, 90),
            (ShortPut, 60),
        ];
        assert_eq!(apply_group_discounts(&four), vec![200, 0, 45, 30]);

        // Five groups: the 4th-cheapest (rank 3) pays full like the largest.
        let five = vec![
            (LongCall, 200),
            (LongPut, 40),
            (ShortCall, 90),
            (ShortPut, 60),
            (Perp, 150),
        ];
        assert_eq!(apply_group_discounts(&five), vec![200, 0, 45, 30, 150]);

        // 50% of an odd fee floors in the user's favor.
        let odd = vec![(LongCall, 100), (ShortPut, 51), (ShortCall, 50)];
        assert_eq!(apply_group_discounts(&odd), vec![100, 25, 0]);

        // Zero-fee groups are stable.
        let zeros = vec![(LongCall, 0), (ShortCall, 10)];
        assert_eq!(apply_group_discounts(&zeros), vec![0, 10]);

        // Empty package -> empty fees.
        assert_eq!(apply_group_discounts(&[]), Vec::<u128>::new());
    }

    #[test]
    fn classify_leg_classes() {
        assert_eq!(classify_leg(false, true, Side::Bid), LegFeeClass::Perp);
        assert_eq!(classify_leg(false, false, Side::Ask), LegFeeClass::Perp);
        assert_eq!(classify_leg(true, true, Side::Bid), LegFeeClass::LongCall);
        assert_eq!(classify_leg(true, true, Side::Ask), LegFeeClass::ShortCall);
        assert_eq!(classify_leg(true, false, Side::Bid), LegFeeClass::LongPut);
        assert_eq!(classify_leg(true, false, Side::Ask), LegFeeClass::ShortPut);
    }

    // 16. box spread detection --------------------------------------------------

    #[test]
    fn box_spread_detection() {
        let btc = "BTC";
        let exp = 1_800_000_000_u64;
        // Long synthetic @ 80k: long call + short put.
        let a = (80_000_u128, true, Side::Bid, btc, exp);
        let b = (80_000, false, Side::Ask, btc, exp);
        // Short synthetic @ 90k: short call + long put.
        let c = (90_000, true, Side::Ask, btc, exp);
        let d = (90_000, false, Side::Bid, btc, exp);

        // Positive: either orientation, any leg order.
        assert!(is_box_spread(&[a, b, c, d]));
        assert!(is_box_spread(&[d, c, b, a]));
        assert!(is_box_spread(&[c, a, d, b]));

        // Negative: different expiry.
        assert!(!is_box_spread(&[
            a,
            b,
            c,
            (90_000, false, Side::Bid, btc, exp + 1)
        ]));
        // Negative: different underlying.
        assert!(!is_box_spread(&[
            a,
            b,
            (90_000, true, Side::Ask, "ETH", exp),
            d
        ]));
        // Negative: long synthetic at BOTH strikes (two same-way synthetics).
        assert!(!is_box_spread(&[
            a,
            b,
            (90_000, true, Side::Bid, btc, exp),
            (90_000, false, Side::Ask, btc, exp)
        ]));
        // Negative: same-side pair at one strike (straddle shape, not a synth).
        assert!(!is_box_spread(&[
            a,
            (80_000, false, Side::Bid, btc, exp),
            c,
            d
        ]));
        // Negative: missing leg.
        assert!(!is_box_spread(&[a, b, c]));
        // Negative: extra leg.
        assert!(!is_box_spread(&[
            a,
            b,
            c,
            d,
            (100_000, true, Side::Bid, btc, exp)
        ]));
        // Negative: all four legs at one strike (K1 == K2).
        assert!(!is_box_spread(&[
            a,
            b,
            (80_000, true, Side::Ask, btc, exp),
            (80_000, false, Side::Bid, btc, exp)
        ]));
        // Negative: two calls at one strike, two puts at the other.
        assert!(!is_box_spread(&[
            (80_000, true, Side::Bid, btc, exp),
            (80_000, true, Side::Ask, btc, exp),
            (90_000, false, Side::Bid, btc, exp),
            (90_000, false, Side::Ask, btc, exp)
        ]));
    }

    // 17 (early): determinism — same scenario twice, identical outcome ---------

    #[test]
    fn determinism_same_scenario_same_state() {
        type Summary = (
            Vec<(u64, RfqStatus)>,
            Vec<(u64, QuoteStatus)>,
            RfqExecution,
            Vec<u64>,
        );
        fn scenario() -> Summary {
            let mut book = RfqBook::new();
            let now = 100_000_000;
            let r1 = book
                .create_rfq(
                    TAKER,
                    vec![leg("A", Side::Bid, 2), leg("B", Side::Ask, 1)],
                    vec![MAKER_A],
                    Some(100),
                    Some(10_000),
                    60_000,
                    now,
                )
                .unwrap();
            let r2 = book
                .create_rfq(
                    TAKER,
                    vec![leg("C", Side::Bid, 1)],
                    vec![],
                    None,
                    None,
                    30_000,
                    now,
                )
                .unwrap();
            let q1 = book
                .send_quote(MAKER_A, r1, vec![500, 250], 20_000, now + 1)
                .unwrap();
            let q2 = book
                .send_quote(MAKER_B, r2, vec![900], 20_000, now + 2)
                .unwrap();
            let q3 = book
                .replace_quote(MAKER_B, q2, vec![890], 20_000, now + 3)
                .unwrap();
            let exec = book.execute(TAKER, r1, q1, now + 4).unwrap();
            let expired = book.sweep(now + 35_000);
            let rfqs: Vec<_> = book.rfqs.values().map(|r| (r.rfq_id, r.status)).collect();
            let quotes: Vec<_> = book
                .quotes
                .values()
                .map(|q| (q.quote_id, q.status))
                .collect();
            assert_eq!(expired, vec![r2]);
            assert_ne!(q3, q2);
            (rfqs, quotes, exec, expired)
        }
        assert_eq!(scenario(), scenario());
    }

    // Extra A. cancel_quote lifecycle ------------------------------------------

    #[test]
    fn cancel_quote_owner_and_lifecycle() {
        let mut book = RfqBook::new();
        let now = 110_000_000;
        let rfq = simple_rfq(&mut book, now);
        let q = book
            .send_quote(MAKER_A, rfq, vec![50_000], 60_000, now)
            .unwrap();

        assert_eq!(
            book.cancel_quote(MAKER_A, 999).unwrap_err(),
            RfqError::UnknownQuote
        );
        assert_eq!(
            book.cancel_quote(MAKER_B, q).unwrap_err(),
            RfqError::NotQuoteOwner
        );
        book.cancel_quote(MAKER_A, q).unwrap();
        assert_eq!(book.quote(q).unwrap().status, QuoteStatus::Cancelled);
        // Cancelling twice fails; executing a cancelled quote fails.
        assert_eq!(
            book.cancel_quote(MAKER_A, q).unwrap_err(),
            RfqError::AlreadySettled
        );
        assert_eq!(
            book.execute(TAKER, rfq, q, now).unwrap_err(),
            RfqError::AlreadySettled
        );
        // The RFQ itself is still open for a fresh quote.
        let q2 = book
            .send_quote(MAKER_B, rfq, vec![49_000], 60_000, now + 1)
            .unwrap();
        assert_eq!(book.live_quotes(rfq).len(), 1);
        assert_eq!(book.best_quote(rfq).unwrap().quote_id, q2);
    }

    // Extra B. execute rejects unknowns, foreign quotes, non-owners -------------

    #[test]
    fn execute_rejects_unknowns_and_foreign_quotes() {
        let mut book = RfqBook::new();
        let now = 120_000_000;
        let r1 = simple_rfq(&mut book, now);
        let r2 = book
            .create_rfq(
                TAKER,
                vec![leg("ETH-PERP", Side::Bid, 1)],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        let q1 = book
            .send_quote(MAKER_A, r1, vec![50_000], 60_000, now)
            .unwrap();
        let q2 = book
            .send_quote(MAKER_B, r2, vec![3_000], 60_000, now)
            .unwrap();

        assert_eq!(
            book.execute(TAKER, 999, q1, now).unwrap_err(),
            RfqError::UnknownRfq
        );
        assert_eq!(
            book.execute(TAKER, r1, 999, now).unwrap_err(),
            RfqError::UnknownQuote
        );
        // q2 belongs to r2, not r1.
        assert_eq!(
            book.execute(TAKER, r1, q2, now).unwrap_err(),
            RfqError::UnknownQuote
        );
        // Only the taker may execute.
        assert_eq!(
            book.execute(MAKER_A, r1, q1, now).unwrap_err(),
            RfqError::NotRfqOwner
        );
        // A non-taker, non-maker outsider cannot execute either.
        assert_eq!(
            book.execute(OUTSIDER, r2, q2, now).unwrap_err(),
            RfqError::NotRfqOwner
        );
        assert_eq!(book.rfq(r1).unwrap().status, RfqStatus::Open);
    }

    // Extra C. per-maker live-quote index ---------------------------------------

    #[test]
    fn live_quotes_by_maker_tracks_lifecycle() {
        let mut book = RfqBook::new();
        let now = 130_000_000;
        let r1 = simple_rfq(&mut book, now);
        let r2 = book
            .create_rfq(
                TAKER,
                vec![leg("ETH-PERP", Side::Bid, 1)],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        let qa = book
            .send_quote(MAKER_A, r1, vec![50_000], 60_000, now)
            .unwrap();
        let qb = book
            .send_quote(MAKER_A, r2, vec![3_000], 60_000, now)
            .unwrap();
        let qc = book
            .send_quote(MAKER_B, r2, vec![3_100], 60_000, now)
            .unwrap();

        let a_ids: Vec<u64> = book
            .live_quotes_by_maker(MAKER_A)
            .iter()
            .map(|q| q.quote_id)
            .collect();
        assert_eq!(a_ids, vec![qa, qb]); // ascending id across rfqs
        let b_ids: Vec<u64> = book
            .live_quotes_by_maker(MAKER_B)
            .iter()
            .map(|q| q.quote_id)
            .collect();
        assert_eq!(b_ids, vec![qc]);

        // Executing r2 cancels every other live quote on it, including
        // maker A's qb — the maker index reflects it.
        book.execute(TAKER, r2, qc, now + 1).unwrap();
        let a_ids: Vec<u64> = book
            .live_quotes_by_maker(MAKER_A)
            .iter()
            .map(|q| q.quote_id)
            .collect();
        assert_eq!(a_ids, vec![qa]);
        // Replacing moves the live slot to a fresh id.
        let qd = book
            .replace_quote(MAKER_A, qa, vec![49_000], 60_000, now + 2)
            .unwrap();
        let a_ids: Vec<u64> = book
            .live_quotes_by_maker(MAKER_A)
            .iter()
            .map(|q| q.quote_id)
            .collect();
        assert_eq!(a_ids, vec![qd]);
        // A maker with no quotes yields an empty index.
        assert!(book.live_quotes_by_maker(OUTSIDER).is_empty());
    }

    // 25. partial acceptance: quote stays firm for the remainder ---------------

    #[test]
    fn partial_acceptance_keeps_quote_firm() {
        let mut book = RfqBook::new();
        let now = 1_000_000;
        let r = book
            .create_rfq(
                TAKER,
                vec![leg("BTC-PERP", Side::Bid, 10)],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        let q = book
            .send_quote_sized(MAKER_A, r, vec![50_000], vec![10], 60_000, now + 1)
            .unwrap();

        // Take 4 of the 10 lots.
        let ex = book.execute_partial(TAKER, r, q, vec![4], now + 2).unwrap();
        assert_eq!(ex.legs[0].2, 4);
        assert_eq!(
            ex.total_taker_flow_quote_minor,
            -4 * 50_000 * 100_000 / 100_000
        );

        // The quote is still Live with 6 lots remaining.
        assert_eq!(book.quote(q).unwrap().status, QuoteStatus::Live);
        assert_eq!(book.quote(q).unwrap().remaining_lots(), vec![6]);
        // The RFQ is still Open with 6 lots remaining.
        assert_eq!(book.rfq(r).unwrap().status, RfqStatus::Open);
        assert_eq!(book.rfq(r).unwrap().remaining_lots, vec![6]);

        // Take the remaining 6: quote consumed, RFQ filled, others drained.
        let other = book
            .send_quote(MAKER_B, r, vec![51_000], 60_000, now + 3)
            .unwrap();
        let ex2 = book.execute_partial(TAKER, r, q, vec![6], now + 4).unwrap();
        assert_eq!(ex2.legs[0].2, 6);
        assert_eq!(book.quote(q).unwrap().status, QuoteStatus::Executed);
        assert_eq!(book.rfq(r).unwrap().status, RfqStatus::Filled);
        assert_eq!(book.quote(other).unwrap().status, QuoteStatus::Cancelled);
    }

    // 25b. full execute on a partial-size quote consumes only the commitment -

    #[test]
    fn full_execute_honors_quote_size() {
        let mut book = RfqBook::new();
        let now = 1_100_000;
        let r = book
            .create_rfq(
                TAKER,
                vec![leg("BTC-PERP", Side::Bid, 10)],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        // Dealer A commits only 3 of 10.
        let qa = book
            .send_quote_sized(MAKER_A, r, vec![50_000], vec![3], 60_000, now + 1)
            .unwrap();
        let ex = book.execute(TAKER, r, qa, now + 2).unwrap();
        assert_eq!(ex.legs[0].2, 3);
        // Quote consumed, but the RFQ stays open with 7 remaining.
        assert_eq!(book.quote(qa).unwrap().status, QuoteStatus::Executed);
        assert_eq!(book.rfq(r).unwrap().status, RfqStatus::Open);
        assert_eq!(book.rfq(r).unwrap().remaining_lots, vec![7]);
    }

    // 25c. partial validation ---------------------------------------------------

    #[test]
    fn partial_validation() {
        let mut book = RfqBook::new();
        let now = 1_200_000;
        let r = book
            .create_rfq(
                TAKER,
                vec![
                    leg("BTC-PERP", Side::Bid, 10),
                    leg("ETH-PERP", Side::Ask, 4),
                ],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        let q = book
            .send_quote(MAKER_A, r, vec![50_000, 3_000], 60_000, now + 1)
            .unwrap();

        // Wrong length.
        assert_eq!(
            book.execute_partial(TAKER, r, q, vec![1], now + 2)
                .unwrap_err(),
            RfqError::InvalidPartial
        );
        // All-zero take.
        assert_eq!(
            book.execute_partial(TAKER, r, q, vec![0, 0], now + 2)
                .unwrap_err(),
            RfqError::InvalidPartial
        );
        // Take larger than the quote on a leg.
        assert_eq!(
            book.execute_partial(TAKER, r, q, vec![11, 0], now + 2)
                .unwrap_err(),
            RfqError::InvalidPartial
        );
        assert_eq!(
            book.execute_partial(TAKER, r, q, vec![0, 5], now + 2)
                .unwrap_err(),
            RfqError::InvalidPartial
        );
        // Book untouched by all failed attempts.
        assert_eq!(book.rfq(r).unwrap().remaining_lots, vec![10, 4]);
        assert_eq!(book.quote(q).unwrap().filled_lots, vec![0, 0]);

        // A legal partial across both legs.
        let ex = book
            .execute_partial(TAKER, r, q, vec![2, 1], now + 3)
            .unwrap();
        assert_eq!(ex.legs[0].2, 2);
        assert_eq!(ex.legs[1].2, 1);
        assert_eq!(book.rfq(r).unwrap().remaining_lots, vec![8, 3]);
        assert_eq!(book.quote(q).unwrap().filled_lots, vec![2, 1]);
        assert_eq!(book.quote(q).unwrap().remaining_lots(), vec![8, 3]);
    }

    // 25d. quote privacy: quotes are private to the requesting account --------

    #[test]
    fn quote_privacy_views() {
        let mut book = RfqBook::new();
        let now = 1_300_000;
        let r = book
            .create_rfq(
                TAKER,
                vec![leg("BTC-PERP", Side::Bid, 5)],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        let qa = book
            .send_quote(MAKER_A, r, vec![50_000], 60_000, now + 1)
            .unwrap();
        let qb = book
            .send_quote(MAKER_B, r, vec![51_000], 60_000, now + 2)
            .unwrap();

        // The taker sees BOTH live quotes (their competitive channel).
        let taker_ids: Vec<u64> = book
            .quotes_for_viewer(r, TAKER)
            .iter()
            .map(|q| q.quote_id)
            .collect();
        assert_eq!(taker_ids, vec![qa, qb]);

        // A maker sees only their own quote — never the rival's.
        let a_ids: Vec<u64> = book
            .quotes_for_viewer(r, MAKER_A)
            .iter()
            .map(|q| q.quote_id)
            .collect();
        assert_eq!(a_ids, vec![qa]);
        let b_ids: Vec<u64> = book
            .quotes_for_viewer(r, MAKER_B)
            .iter()
            .map(|q| q.quote_id)
            .collect();
        assert_eq!(b_ids, vec![qb]);

        // Anyone else sees nothing; unknown RFQs also return nothing.
        assert!(book.quotes_for_viewer(r, OUTSIDER).is_empty());
        assert!(book.quotes_for_viewer(999, TAKER).is_empty());
    }

    // 25e. execute_best: the taker executes against the best one ---------------

    #[test]
    fn execute_best_picks_best_quote() {
        let mut book = RfqBook::new();
        let now = 1_400_000;
        let r = book
            .create_rfq(
                TAKER,
                vec![leg("BTC-PERP", Side::Bid, 5)],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        let _qa = book
            .send_quote(MAKER_A, r, vec![50_000], 60_000, now + 1)
            .unwrap();
        // Maker B's price is better for the taker (they receive more flow:
        // buying cheaper). At 49_000 the taker pays less than at 50_000.
        let qb = book
            .send_quote(MAKER_B, r, vec![49_000], 60_000, now + 2)
            .unwrap();

        let ex = book.execute_best(TAKER, r, now + 3).unwrap();
        assert_eq!(ex.quote_id, qb);
        assert_eq!(ex.maker, MAKER_B);
        assert_eq!(book.rfq(r).unwrap().status, RfqStatus::Filled);

        // No live quotes: execute_best fails cleanly.
        let r2 = book
            .create_rfq(
                TAKER,
                vec![leg("ETH-PERP", Side::Ask, 2)],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        assert_eq!(
            book.execute_best(TAKER, r2, now + 4).unwrap_err(),
            RfqError::UnknownQuote
        );
    }

    // 25f. sized quotes: working size below the request -------------------------

    #[test]
    fn sized_quote_validation() {
        let mut book = RfqBook::new();
        let now = 1_500_000;
        let r = book
            .create_rfq(
                TAKER,
                vec![leg("BTC-PERP", Side::Bid, 10)],
                vec![],
                None,
                None,
                60_000,
                now,
            )
            .unwrap();
        // Zero size rejected.
        assert_eq!(
            book.send_quote_sized(MAKER_A, r, vec![50_000], vec![0], 60_000, now + 1)
                .unwrap_err(),
            RfqError::InvalidQty
        );
        // Size above the RFQ remaining rejected.
        assert_eq!(
            book.send_quote_sized(MAKER_A, r, vec![50_000], vec![11], 60_000, now + 1)
                .unwrap_err(),
            RfqError::InvalidQty
        );
        // Wrong length rejected.
        assert_eq!(
            book.send_quote_sized(MAKER_A, r, vec![50_000], vec![1, 2], 60_000, now + 1)
                .unwrap_err(),
            RfqError::LegCountMismatch
        );
        // Legal working size accepted.
        let q = book
            .send_quote_sized(MAKER_A, r, vec![50_000], vec![4], 60_000, now + 1)
            .unwrap();
        assert_eq!(book.quote(q).unwrap().qty_lots, vec![4]);
        assert_eq!(book.quote(q).unwrap().remaining_lots(), vec![4]);
    }
}
