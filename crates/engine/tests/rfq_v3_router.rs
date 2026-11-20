//! Integration tests for the best-execution router (G-11 depth):
//! `RfqExecuteBest` prices the best live quote AND the composite book
//! route, then executes the cheaper one net of fees.

use poc_core::{Instrument, PerpMarket, Side, TimestampMs};
use poc_engine::{Command, Engine, EngineConfig, Event, OrderRequest, RfqLegCommand};

const T0: TimestampMs = 1_000_000;

fn engine_with_market() -> Engine {
    let mut e = Engine::new(EngineConfig::default());
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    e
}

fn seed(e: &mut Engine) {
    for provider in ["pyth", "chainlink"] {
        e.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: provider.into(),
            ts: T0,
            price_quote_minor: 8_000_000,
        });
    }
    e.process(Command::Tick { now: T0 });
    for sub in 1..=4u64 {
        e.process(Command::Deposit {
            subaccount: sub,
            amount_quote_minor: 100_000_000,
        });
    }
}

fn rfq_leg(symbol: &str, side: Side, qty: u64) -> RfqLegCommand {
    RfqLegCommand {
        symbol: symbol.into(),
        side,
        qty_lots: qty,
    }
}

fn create_rfq(e: &mut Engine, taker: u64, qty: u64, now: TimestampMs) -> u64 {
    e.process(Command::RfqCreate {
        taker,
        legs: vec![rfq_leg("BTC-PERP", Side::Bid, qty)],
        counterparties: vec![],
        min_total_cost_quote_minor: None,
        max_total_cost_quote_minor: None,
        ttl_ms: 600_000,
        now,
    });
    let mut id = 1;
    while e.rfq_view().rfq(id).is_some() {
        id += 1;
    }
    id - 1
}

fn resting_ask(e: &mut Engine, sub: u64, price: u64, qty: u64) {
    e.process(Command::Place {
        request: OrderRequest::limit(sub, "BTC-PERP", Side::Ask, price, qty),
        now: T0 + 2,
    });
}

// ---------------------------------------------------------------------------
// 1. The cheaper BOOK route wins; the RFQ is swept, not quoted.
// ---------------------------------------------------------------------------

#[test]
fn cheaper_book_route_wins() {
    let mut e = engine_with_market();
    seed(&mut e);
    let rfq_id = create_rfq(&mut e, 1, 10, T0);
    // Dealer 2 quotes 80_000 ticks for 10 lots.
    e.process(Command::RfqQuote {
        maker: 2,
        rfq_id,
        leg_prices_ticks: vec![80_000],
        ttl_ms: 600_000,
        now: T0,
    });
    // The book asks 79_000 for 10 lots — cheaper.
    resting_ask(&mut e, 3, 79_000, 10);
    let events = e.process(Command::RfqExecuteBest {
        taker: 1,
        rfq_id,
        now: T0 + 1,
    });
    // The report chose the book.
    let chosen = events.iter().find_map(|ev| match ev {
        Event::BestExecutionReport { chosen, .. } => Some(*chosen),
        _ => None,
    });
    assert_eq!(chosen, Some("book"));
    // Settled as ordinary trades vs the resting maker + a sweep.
    let trades: Vec<&Event> = events
        .iter()
        .filter(|ev| matches!(ev, Event::TradeExecuted(_)))
        .collect();
    assert_eq!(trades.len(), 1, "one book fill");
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Event::RfqBookSwept { .. })));
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Event::RfqSettled { .. }) == false));
    // The RFQ is consumed.
    assert_eq!(
        e.rfq_view().rfq(rfq_id).unwrap().status,
        poc_rfq::RfqStatus::Filled
    );
    // The quote survived only as Cancelled (drained by the sweep).
    assert_eq!(
        e.rfq_view().quote(1).unwrap().status,
        poc_rfq::QuoteStatus::Cancelled
    );
    // Position came from the book maker.
    assert_eq!(e.account(1).unwrap().lots_of("BTC-PERP"), 10);
    assert_eq!(e.account(3).unwrap().lots_of("BTC-PERP"), -10);
}

// ---------------------------------------------------------------------------
// 2. The cheaper QUOTE wins; the books are untouched.
// ---------------------------------------------------------------------------

#[test]
fn cheaper_quote_wins() {
    let mut e = engine_with_market();
    seed(&mut e);
    let rfq_id = create_rfq(&mut e, 1, 10, T0);
    e.process(Command::RfqQuote {
        maker: 2,
        rfq_id,
        leg_prices_ticks: vec![79_000],
        ttl_ms: 600_000,
        now: T0,
    });
    // The book asks more.
    resting_ask(&mut e, 3, 80_000, 10);
    let events = e.process(Command::RfqExecuteBest {
        taker: 1,
        rfq_id,
        now: T0 + 1,
    });
    let chosen = events.iter().find_map(|ev| match ev {
        Event::BestExecutionReport { chosen, .. } => Some(*chosen),
        _ => None,
    });
    assert_eq!(chosen, Some("quote"));
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Event::RfqSettled { .. })));
    assert!(!events
        .iter()
        .any(|ev| matches!(ev, Event::RfqBookSwept { .. })));
    // The book is untouched.
    assert_eq!(e.account(3).unwrap().lots_of("BTC-PERP"), 0);
    assert_eq!(e.account(2).unwrap().lots_of("BTC-PERP"), -10);
}

// ---------------------------------------------------------------------------
// 3. No route at all: rejected.
// ---------------------------------------------------------------------------

#[test]
fn no_route_rejects() {
    let mut e = engine_with_market();
    seed(&mut e);
    let rfq_id = create_rfq(&mut e, 1, 10, T0);
    let events = e.process(Command::RfqExecuteBest {
        taker: 1,
        rfq_id,
        now: T0 + 1,
    });
    assert!(matches!(
        events.first(),
        Some(Event::RfqRejected {
            reason: "no execution route",
            ..
        })
    ));
}

// ---------------------------------------------------------------------------
// 4. The cost collar blocks an out-of-bounds book route.
// ---------------------------------------------------------------------------

#[test]
fn cost_collar_blocks_an_expensive_book_route() {
    let mut e = engine_with_market();
    seed(&mut e);
    // The RFQ caps the taker's cost at 10 lots @ 78_000 ticks.
    let cap = 78_000_u128 * 100 * 10 * 100 / 100_000;
    e.process(Command::RfqCreate {
        taker: 1,
        legs: vec![rfq_leg("BTC-PERP", Side::Bid, 10)],
        counterparties: vec![],
        min_total_cost_quote_minor: None,
        max_total_cost_quote_minor: Some(cap),
        ttl_ms: 600_000,
        now: T0,
    });
    // No quotes; the only route is a book asking 79_000 — above the
    // collar, so the router refuses it.
    resting_ask(&mut e, 3, 79_000, 10);
    let events = e.process(Command::RfqExecuteBest {
        taker: 1,
        rfq_id: 1,
        now: T0 + 1,
    });
    assert!(
        matches!(events.first(), Some(Event::RfqRejected { .. })),
        "events: {events:?}"
    );
    assert_eq!(e.account(1).unwrap().lots_of("BTC-PERP"), 0);
    // A quote inside the collar still executes through the router.
    e.process(Command::RfqQuote {
        maker: 2,
        rfq_id: 1,
        leg_prices_ticks: vec![77_000],
        ttl_ms: 600_000,
        now: T0 + 2,
    });
    let events = e.process(Command::RfqExecuteBest {
        taker: 1,
        rfq_id: 1,
        now: T0 + 3,
    });
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Event::RfqSettled { .. })));
    assert_eq!(e.account(1).unwrap().lots_of("BTC-PERP"), 10);
}

// ---------------------------------------------------------------------------
// 5. Partial book coverage falls back to the (partial) quote.
// ---------------------------------------------------------------------------

#[test]
fn partial_book_coverage_falls_back_to_the_quote() {
    let mut e = engine_with_market();
    seed(&mut e);
    let rfq_id = create_rfq(&mut e, 1, 10, T0);
    // The book can only cover 4 of 10 lots.
    resting_ask(&mut e, 3, 79_000, 4);
    // A partial-sized quote for 6 lots.
    e.process(Command::RfqQuoteSized {
        maker: 2,
        rfq_id,
        leg_prices_ticks: vec![80_000],
        qty_lots: vec![6],
        ttl_ms: 600_000,
        now: T0,
    });
    let events = e.process(Command::RfqExecuteBest {
        taker: 1,
        rfq_id,
        now: T0 + 1,
    });
    // The book route is all-or-nothing: infeasible. The partial quote
    // executes (progress beats rejection).
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, Event::RfqSettled { .. })),
        "events: {events:?}"
    );
    assert!(!events
        .iter()
        .any(|ev| matches!(ev, Event::RfqBookSwept { .. })));
    assert_eq!(e.account(1).unwrap().lots_of("BTC-PERP"), 6);
    assert_eq!(e.account(3).unwrap().lots_of("BTC-PERP"), 0);
}

// ---------------------------------------------------------------------------
// 6. Bit-for-bit replay through the router.
// ---------------------------------------------------------------------------

#[test]
fn router_journal_replays_bit_for_bit() {
    let config = EngineConfig::default();
    let mut e = engine_with_market();
    seed(&mut e);
    let rfq_id = create_rfq(&mut e, 1, 10, T0);
    e.process(Command::RfqQuote {
        maker: 2,
        rfq_id,
        leg_prices_ticks: vec![80_000],
        ttl_ms: 600_000,
        now: T0,
    });
    resting_ask(&mut e, 3, 79_500, 10);
    e.process(Command::RfqExecuteBest {
        taker: 1,
        rfq_id,
        now: T0 + 1,
    });
    let rfq_id2 = create_rfq(&mut e, 4, 5, T0 + 2);
    e.process(Command::RfqQuote {
        maker: 2,
        rfq_id: rfq_id2,
        leg_prices_ticks: vec![78_000],
        ttl_ms: 600_000,
        now: T0 + 2,
    });
    e.process(Command::RfqExecuteBest {
        taker: 4,
        rfq_id: rfq_id2,
        now: T0 + 3,
    });
    let journal: Vec<Event> = e.journal().to_vec();
    let replayed = Engine::replay(config, &journal);
    let fp = |eng: &Engine| {
        (
            eng.accounts_iter()
                .map(|(&s, a)| (s, a.cash_quote_minor, a.positions.clone()))
                .collect::<Vec<_>>(),
            eng.stats().trades,
        )
    };
    assert_eq!(fp(&e), fp(&replayed));
}

// ---------------------------------------------------------------------------
// 7. The end-to-end hybrid flow: sweep, shortfall auto-RFQ, dealer
//    inside the original limit fills via the router.
// ---------------------------------------------------------------------------

#[test]
fn end_to_end_hybrid_flow() {
    let mut e = engine_with_market();
    seed(&mut e);
    // The book offers only 3 of 8 lots below the taker's 80_000 limit.
    resting_ask(&mut e, 3, 79_000, 3);
    let events = e.process(Command::Place {
        request: OrderRequest {
            tif: poc_core::TimeInForce::Ioc,
            ..OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_000, 8)
        },
        now: T0 + 5,
    });
    // 3 lots filled; 5 shortfall auto-published an RFQ collared by the
    // limit (5 lots × 80_000 ticks = 5 × 80_000 × 100 × 100 / 100_000).
    assert_eq!(e.account(1).unwrap().lots_of("BTC-PERP"), 3);
    let rfq = events.iter().find_map(|ev| match ev {
        Event::RfqCreated {
            rfq_id,
            max_total_cost_quote_minor,
            ..
        } => Some((*rfq_id, *max_total_cost_quote_minor)),
        _ => None,
    });
    let (rfq_id, max_total) = rfq.expect("shortfall RFQ");
    let expected_cap = 80_000_u128 * 100 * 5 * 100 / 100_000;
    assert_eq!(max_total, Some(expected_cap));
    // A dealer quotes inside the original limit.
    e.process(Command::RfqQuote {
        maker: 2,
        rfq_id,
        leg_prices_ticks: vec![79_800],
        ttl_ms: 600_000,
        now: T0 + 6,
    });
    // The router executes the quote (the books have nothing left).
    let events = e.process(Command::RfqExecuteBest {
        taker: 1,
        rfq_id,
        now: T0 + 7,
    });
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Event::RfqSettled { .. })));
    assert_eq!(e.account(1).unwrap().lots_of("BTC-PERP"), 8);
    assert_eq!(e.account(2).unwrap().lots_of("BTC-PERP"), -5);
    // The auto-published RFQ's collar admitted the quote (79_800 < 80_000).
    assert_eq!(
        e.rfq_view().rfq(rfq_id).unwrap().status,
        poc_rfq::RfqStatus::Filled
    );
}

// ---------------------------------------------------------------------------
// 8. Router rejects foreign takers and dead RFQs.
// ---------------------------------------------------------------------------

#[test]
fn router_validates_ownership_and_windows() {
    let mut e = engine_with_market();
    seed(&mut e);
    let rfq_id = create_rfq(&mut e, 1, 10, T0);
    // Foreign taker.
    let events = e.process(Command::RfqExecuteBest {
        taker: 2,
        rfq_id,
        now: T0 + 1,
    });
    assert!(matches!(
        events.first(),
        Some(Event::RfqRejected {
            reason: "not rfq owner",
            ..
        })
    ));
    // Expired window.
    let events = e.process(Command::RfqExecuteBest {
        taker: 1,
        rfq_id,
        now: T0 + 600_001,
    });
    assert!(matches!(
        events.first(),
        Some(Event::RfqRejected {
            reason: "quote window closed",
            ..
        })
    ));
}
