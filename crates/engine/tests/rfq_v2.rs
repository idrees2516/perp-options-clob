//! Integration tests for the G-11 deep-implementation: partial RFQ
//! executions through the full venue path (margin + fees + journal),
//! the G-15 dealer program (enrollment gate, pull strikes, suspension,
//! drop, tier promotion), quote privacy views, and the Transparency
//! pattern — RFQ executions printing on the delayed negotiated tape.

use poc_core::{Instrument, PerpMarket, Side, TimestampMs};
use poc_engine::{Command, Engine, EngineConfig, Event, RfqLegCommand};
use poc_rfq::{BlockOrigin, DealerTier, PullOutcome, QuoteStatus, RfqStatus};

const T0: TimestampMs = 1_000_000;

fn engine_with_market(enforce_dealer_program: bool) -> Engine {
    let config = EngineConfig {
        enforce_dealer_program,
        ..EngineConfig::default()
    };
    let mut e = Engine::new(config);
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
    // Ids are assigned sequentially from 1 at apply: the new id is the
    // highest existing one.
    let mut id = 1;
    while e.rfq_view().rfq(id).is_some() {
        id += 1;
    }
    id - 1
}

// ---------------------------------------------------------------------------
// 1. Partial execution through the venue path
// ---------------------------------------------------------------------------

#[test]
fn partial_rfq_execution_settles_through_margin_fees_and_journal() {
    let mut e = engine_with_market(false);
    seed(&mut e);
    let rfq_id = create_rfq(&mut e, 1, 100, T0);

    // Dealer 2 quotes full size.
    e.process(Command::RfqQuote {
        maker: 2,
        rfq_id,
        leg_prices_ticks: vec![80_000],
        ttl_ms: 600_000,
        now: T0,
    });

    // Partial acceptance: take 40 of 100 lots.
    let ev = e.process(Command::RfqExecutePartial {
        taker: 1,
        rfq_id,
        quote_id: 1,
        take_per_leg: vec![40],
        now: T0 + 1,
    });
    assert!(
        ev.iter().any(|x| matches!(x, Event::RfqSettled { .. })),
        "partial executes: {ev:?}"
    );

    // Positions reflect the PARTIAL fill only.
    assert_eq!(e.account(1).map(|a| a.lots_of("BTC-PERP")), Some(40));
    assert_eq!(e.account(2).map(|a| a.lots_of("BTC-PERP")), Some(-40));

    // The RFQ stays open with 60 remaining; the quote stays firm.
    let rfq = e.rfq_view().rfq(rfq_id).cloned().unwrap();
    assert_eq!(rfq.status, RfqStatus::Open);
    assert_eq!(rfq.remaining_lots, vec![60]);
    let quote = e.rfq_view().quote(1).cloned().unwrap();
    assert_eq!(quote.status, QuoteStatus::Live);
    assert_eq!(quote.remaining_lots(), vec![60]);
    assert_eq!(quote.filled_lots, vec![40]);

    // The taker paid a fee on the partial notional (venue path taken).
    assert!(e.account(1).map(|a| a.fees_paid_quote_minor).unwrap_or(0) > 0);

    // Second partial completes the RFQ.
    let ev = e.process(Command::RfqExecutePartial {
        taker: 1,
        rfq_id,
        quote_id: 1,
        take_per_leg: vec![60],
        now: T0 + 2,
    });
    assert!(ev.iter().any(|x| matches!(x, Event::RfqSettled { .. })));
    assert_eq!(e.account(1).map(|a| a.lots_of("BTC-PERP")), Some(100));
    let rfq = e.rfq_view().rfq(rfq_id).cloned().unwrap();
    assert_eq!(rfq.status, RfqStatus::Filled);
    let quote = e.rfq_view().quote(1).cloned().unwrap();
    assert_eq!(quote.status, QuoteStatus::Executed);

    // A third attempt is rejected: nothing remains.
    let ev = e.process(Command::RfqExecutePartial {
        taker: 1,
        rfq_id,
        quote_id: 1,
        take_per_leg: vec![1],
        now: T0 + 3,
    });
    assert!(matches!(ev[0], Event::RfqRejected { .. }));
}

// ---------------------------------------------------------------------------
// 2. Working-size quotes (partial-size commitments)
// ---------------------------------------------------------------------------

#[test]
fn sized_quote_commits_less_than_request() {
    let mut e = engine_with_market(false);
    seed(&mut e);
    let rfq_id = create_rfq(&mut e, 1, 100, T0);

    // Dealer 2 commits only 30 lots at their price.
    let ev = e.process(Command::RfqQuoteSized {
        maker: 2,
        rfq_id,
        leg_prices_ticks: vec![80_000],
        qty_lots: vec![30],
        ttl_ms: 600_000,
        now: T0,
    });
    assert!(matches!(ev[0], Event::RfqQuoted { .. }));
    let quote = e.rfq_view().quote(1).cloned().unwrap();
    assert_eq!(quote.qty_lots, vec![30]);

    // Full execute consumes only the commitment.
    let ev = e.process(Command::RfqExecute {
        taker: 1,
        rfq_id,
        quote_id: 1,
        now: T0 + 1,
    });
    assert!(ev.iter().any(|x| matches!(x, Event::RfqSettled { .. })));
    assert_eq!(e.account(1).map(|a| a.lots_of("BTC-PERP")), Some(30));
    let rfq = e.rfq_view().rfq(rfq_id).cloned().unwrap();
    assert_eq!(rfq.status, RfqStatus::Open);
    assert_eq!(rfq.remaining_lots, vec![70]);

    // Oversized working size is rejected at the plan gate.
    let rfq2 = create_rfq(&mut e, 3, 50, T0);
    let ev = e.process(Command::RfqQuoteSized {
        maker: 2,
        rfq_id: rfq2,
        leg_prices_ticks: vec![80_000],
        qty_lots: vec![51],
        ttl_ms: 600_000,
        now: T0,
    });
    assert!(matches!(ev[0], Event::RfqRejected { .. }));
}

// ---------------------------------------------------------------------------
// 3. The G-15 dealer program gate
// ---------------------------------------------------------------------------

#[test]
fn dealer_program_gates_quoting_when_enforced() {
    let mut e = engine_with_market(true);
    seed(&mut e);
    let rfq_id = create_rfq(&mut e, 1, 100, T0);

    // Not enrolled: rejected.
    let ev = e.process(Command::RfqQuote {
        maker: 2,
        rfq_id,
        leg_prices_ticks: vec![80_000],
        ttl_ms: 600_000,
        now: T0,
    });
    assert!(matches!(ev[0], Event::RfqRejected { .. }));

    // Enroll: accepted.
    e.process(Command::DealerEnroll {
        dealer: 2,
        tier: DealerTier::Basic,
        now: T0,
    });
    let ev = e.process(Command::RfqQuote {
        maker: 2,
        rfq_id,
        leg_prices_ticks: vec![80_000],
        ttl_ms: 600_000,
        now: T0,
    });
    assert!(matches!(ev[0], Event::RfqQuoted { .. }));

    // The registry is visible.
    assert!(e.dealer_program_view().record(2).is_some());
    assert_eq!(
        e.dealer_program_view().record(2).map(|r| r.tier),
        Some(poc_rfq::DealerTier::Basic)
    );
    // Enforcement off (default): open quoting for everyone.
    let mut e2 = engine_with_market(false);
    seed(&mut e2);
    let rfq2 = create_rfq(&mut e2, 1, 100, T0);
    let ev = e2.process(Command::RfqQuote {
        maker: 9,
        rfq_id: rfq2,
        leg_prices_ticks: vec![80_000],
        ttl_ms: 600_000,
        now: T0,
    });
    assert!(matches!(ev[0], Event::RfqQuoted { .. }));
}

// ---------------------------------------------------------------------------
// 4. Pull strikes -> suspension -> re-admission -> drop
// ---------------------------------------------------------------------------

#[test]
fn pull_strikes_suspend_and_drop_dealer() {
    let mut e = engine_with_market(true);
    seed(&mut e);
    e.process(Command::DealerEnroll {
        dealer: 2,
        tier: DealerTier::Basic,
        now: T0,
    });

    // Three pull cycles: create an RFQ, quote it, cancel the quote.
    for round in 0..3u64 {
        let rfq_id = create_rfq(&mut e, 1, 10, T0 + round * 1_000);
        e.process(Command::RfqQuote {
            maker: 2,
            rfq_id,
            leg_prices_ticks: vec![80_000],
            ttl_ms: 600_000,
            now: T0 + round * 1_000,
        });
        let ev = e.process(Command::RfqCancel {
            subaccount: 2,
            rfq_id: None,
            quote_id: Some(1 + round),
            now: T0 + round * 1_000 + 1,
        });
        // Each cancel journals a strike.
        assert!(
            ev.iter().any(|x| matches!(x, Event::DealerStruck { .. })),
            "strike journaled: {ev:?}"
        );
    }

    // The third pull (all within the strike window) trips the suspension.
    let record = e.dealer_program_view().record(2).cloned().unwrap();
    assert_eq!(record.suspension_count, 1);
    assert!(record.suspended_until > T0);

    // While suspended, the dealer cannot quote (enforcement on).
    let rfq_id = create_rfq(&mut e, 1, 10, T0 + 10_000);
    let ev = e.process(Command::RfqQuote {
        maker: 2,
        rfq_id,
        leg_prices_ticks: vec![80_000],
        ttl_ms: 600_000,
        now: T0 + 10_000,
    });
    assert!(matches!(ev[0], Event::RfqRejected { .. }));

    // After the suspension lapses (default 1h), quoting works again.
    let later = T0 + 3_600_000 + 20_000;
    let rfq_id = create_rfq(&mut e, 1, 10, later);
    let ev = e.process(Command::RfqQuote {
        maker: 2,
        rfq_id,
        leg_prices_ticks: vec![80_000],
        ttl_ms: 600_000,
        now: later,
    });
    assert!(matches!(ev[0], Event::RfqQuoted { .. }));
}

#[test]
fn dealer_executions_credit_the_program() {
    let mut e = engine_with_market(false);
    seed(&mut e);
    e.process(Command::DealerEnroll {
        dealer: 2,
        tier: DealerTier::Basic,
        now: T0,
    });

    // One clean RFQ round-trip.
    let rfq_id = create_rfq(&mut e, 1, 100, T0);
    e.process(Command::RfqQuote {
        maker: 2,
        rfq_id,
        leg_prices_ticks: vec![80_000],
        ttl_ms: 600_000,
        now: T0,
    });
    let ev = e.process(Command::RfqExecute {
        taker: 1,
        rfq_id,
        quote_id: 1,
        now: T0 + 1,
    });
    assert!(ev.iter().any(|x| matches!(x, Event::RfqSettled { .. })));

    // The execution was credited.
    let record = e.dealer_program_view().record(2).cloned().unwrap();
    assert_eq!(record.executions, 1);
    assert_eq!(record.executed_lots, 100);
}

// ---------------------------------------------------------------------------
// 5. Transparency pattern: RFQ executions print on the delayed tape
// ---------------------------------------------------------------------------

#[test]
fn rfq_executions_print_on_the_delayed_tape() {
    let mut e = engine_with_market(false);
    seed(&mut e);
    let rfq_id = create_rfq(&mut e, 1, 50, T0);
    e.process(Command::RfqQuote {
        maker: 2,
        rfq_id,
        leg_prices_ticks: vec![80_000],
        ttl_ms: 600_000,
        now: T0,
    });
    e.process(Command::RfqExecute {
        taker: 1,
        rfq_id,
        quote_id: 1,
        now: T0 + 1,
    });

    // The tape holds an unprinted RFQ-origin print (block id 1).
    let block = e.blocks_view().block(1).cloned().unwrap();
    assert_eq!(block.origin, BlockOrigin::Rfq);
    assert!(!block.broadcast);
    assert_eq!(block.legs[0].2, 50);

    // Before the delay elapses, nothing prints on a sweep.
    let before = e.process(Command::Tick {
        now: T0 + poc_rfq::BlockLedger::DEFAULT_DELAY_MS,
    });
    assert!(!before
        .iter()
        .any(|x| matches!(x, Event::BlockPrinted { .. })));

    // At the delay boundary (execute ts + delay), the print fires.
    let at = e.process(Command::Tick {
        now: T0 + 1 + poc_rfq::BlockLedger::DEFAULT_DELAY_MS,
    });
    assert!(
        at.iter()
            .any(|x| matches!(x, Event::BlockPrinted { block_id: 1 })),
        "rfq print fires: {at:?}"
    );
    assert!(e.blocks_view().block(1).cloned().unwrap().broadcast);

    // A partial execution produces its own delayed print.
    let rfq2 = create_rfq(&mut e, 3, 40, T0 + 10);
    e.process(Command::RfqQuote {
        maker: 4,
        rfq_id: rfq2,
        leg_prices_ticks: vec![80_000],
        ttl_ms: 600_000,
        now: T0 + 10,
    });
    e.process(Command::RfqExecutePartial {
        taker: 3,
        rfq_id: rfq2,
        quote_id: 2,
        take_per_leg: vec![15],
        now: T0 + 11,
    });
    let block2 = e.blocks_view().block(2).cloned().unwrap();
    assert_eq!(block2.origin, BlockOrigin::Rfq);
    assert_eq!(block2.legs[0].2, 15);
}

// ---------------------------------------------------------------------------
// 6. Quote privacy through the engine view
// ---------------------------------------------------------------------------

#[test]
fn engine_quote_privacy_views() {
    let mut e = engine_with_market(false);
    seed(&mut e);
    let rfq_id = create_rfq(&mut e, 1, 10, T0);
    e.process(Command::RfqQuote {
        maker: 2,
        rfq_id,
        leg_prices_ticks: vec![80_000],
        ttl_ms: 600_000,
        now: T0,
    });
    e.process(Command::RfqQuote {
        maker: 3,
        rfq_id,
        leg_prices_ticks: vec![79_000],
        ttl_ms: 600_000,
        now: T0,
    });

    // The taker sees both quotes; each maker sees only their own.
    let view = e.rfq_view();
    assert_eq!(view.quotes_for_viewer(rfq_id, 1).len(), 2);
    assert_eq!(view.quotes_for_viewer(rfq_id, 2).len(), 1);
    assert_eq!(view.quotes_for_viewer(rfq_id, 3).len(), 1);
    assert_eq!(
        view.quotes_for_viewer(rfq_id, 2)[0].maker,
        2,
        "maker 2 must not see maker 3's quote"
    );
    assert!(view.quotes_for_viewer(rfq_id, 4).is_empty());
}

// ---------------------------------------------------------------------------
// 7. Determinism: the whole new machinery replays bit for bit
// ---------------------------------------------------------------------------

#[test]
fn rfq_v2_replays_bit_for_bit() {
    fn run() -> Vec<Event> {
        let mut e = engine_with_market(true);
        seed(&mut e);
        let mut all = Vec::new();
        all.extend(e.process(Command::DealerEnroll {
            dealer: 2,
            tier: DealerTier::Basic,
            now: T0,
        }));
        let rfq_id = create_rfq(&mut e, 1, 100, T0);
        all.extend(e.process(Command::RfqQuoteSized {
            maker: 2,
            rfq_id,
            leg_prices_ticks: vec![80_000],
            qty_lots: vec![60],
            ttl_ms: 600_000,
            now: T0,
        }));
        all.extend(e.process(Command::RfqExecutePartial {
            taker: 1,
            rfq_id,
            quote_id: 1,
            take_per_leg: vec![25],
            now: T0 + 1,
        }));
        all.extend(e.process(Command::RfqExecute {
            taker: 1,
            rfq_id,
            quote_id: 1,
            now: T0 + 2,
        }));
        all.extend(e.process(Command::Tick {
            now: T0 + poc_rfq::BlockLedger::DEFAULT_DELAY_MS + 5,
        }));
        // Dealer pulls on an open RFQ (strike).
        let rfq2 = create_rfq(&mut e, 3, 10, T0 + 10);
        all.extend(e.process(Command::RfqQuote {
            maker: 2,
            rfq_id: rfq2,
            leg_prices_ticks: vec![80_000],
            ttl_ms: 600_000,
            now: T0 + 10,
        }));
        all.extend(e.process(Command::RfqCancel {
            subaccount: 2,
            rfq_id: None,
            quote_id: Some(2),
            now: T0 + 11,
        }));
        all
    }
    assert_eq!(run(), run());
}

// ---------------------------------------------------------------------------
// 8. Suspension echo events are journaled
// ---------------------------------------------------------------------------

#[test]
fn suspension_echo_events_journaled() {
    let mut e = engine_with_market(true);
    seed(&mut e);
    e.process(Command::DealerEnroll {
        dealer: 2,
        tier: DealerTier::Basic,
        now: T0,
    });

    let mut saw_suspended = false;
    for round in 0..3u64 {
        let rfq_id = create_rfq(&mut e, 1, 10, T0 + round * 10);
        e.process(Command::RfqQuote {
            maker: 2,
            rfq_id,
            leg_prices_ticks: vec![80_000],
            ttl_ms: 600_000,
            now: T0 + round * 10,
        });
        let ev = e.process(Command::RfqCancel {
            subaccount: 2,
            rfq_id: None,
            quote_id: Some(1 + round),
            now: T0 + round * 10 + 1,
        });
        if ev
            .iter()
            .any(|x| matches!(x, Event::DealerSuspended { .. }))
        {
            saw_suspended = true;
        }
    }
    assert!(saw_suspended, "the tripping pull journals DealerSuspended");

    // Suspension 1 lapses ~T0+3.6M. Second trip: three fresh pulls close
    // together (inside the strike window), after the first suspension
    // lapses -> suspension 2 at 2x length (7.2M).
    let mut second_suspension = false;
    for i in 0..3u64 {
        let at = T0 + 4_000_000 + i * 10;
        let rfq_id = create_rfq(&mut e, 1, 10, at);
        e.process(Command::RfqQuote {
            maker: 2,
            rfq_id,
            leg_prices_ticks: vec![80_000],
            ttl_ms: 600_000,
            now: at,
        });
        let ev = e.process(Command::RfqCancel {
            subaccount: 2,
            rfq_id: None,
            quote_id: Some(4 + i),
            now: at + 1,
        });
        if ev
            .iter()
            .any(|x| matches!(x, Event::DealerSuspended { .. }))
        {
            second_suspension = true;
        }
    }
    assert!(
        second_suspension,
        "the second suspension journals DealerSuspended"
    );

    // Third trip after the 7.2M suspension lapses -> permanent drop.
    let mut saw_dropped = false;
    for i in 0..3u64 {
        let at = T0 + 12_000_000 + i * 10;
        let rfq_id = create_rfq(&mut e, 1, 10, at);
        e.process(Command::RfqQuote {
            maker: 2,
            rfq_id,
            leg_prices_ticks: vec![80_000],
            ttl_ms: 600_000,
            now: at,
        });
        let ev = e.process(Command::RfqCancel {
            subaccount: 2,
            rfq_id: None,
            quote_id: Some(7 + i),
            now: at + 1,
        });
        if ev.iter().any(|x| matches!(x, Event::DealerDropped { .. })) {
            saw_dropped = true;
        }
    }
    assert!(saw_dropped, "the third suspension journals DealerDropped");
    assert_eq!(
        e.dealer_program_view().preview_pull(2, T0 + 100_000_000),
        PullOutcome::Dropped
    );
}
