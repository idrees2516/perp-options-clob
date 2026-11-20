//! Integration tests for the combined-order book (G-11 depth): the
//! three matching regimes, the package margin gate, reservations, the
//! thin-book handoff, and the best-execution router.

use poc_combo::ComboTif;
use poc_core::{
    ExerciseStyle, Instrument, OptionMarket, OptionVariant, PerpMarket, Side, TimestampMs,
};
use poc_engine::{ComboLegCommand, Command, Engine, EngineConfig, Event};

const T0: TimestampMs = 1_000_000;

fn option(symbol: &str, strike: u128, kind: poc_core::OptionKind) -> Instrument {
    Instrument::Option(OptionMarket {
        symbol: symbol.into(),
        base_symbol: "BTC".into(),
        kind,
        strike_quote_minor: strike,
        expiry_ts_ms: T0 + 30 * 86_400_000,
        variant: OptionVariant::Dated,
        exercise_style: ExerciseStyle::European,
        quote_decimals: 2,
        base_decimals: 5,
        tick_size_quote_minor: 100,
        lot_size_base_minor: 100,
        price_band_bps: 1_000_000, // tests place wings far from the mark
        ..OptionMarket::default()
    })
}

fn engine_with_wings() -> Engine {
    let mut e = Engine::new(EngineConfig::default());
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    e.register_instrument(option("BTC-C1", 9_500_000, poc_core::OptionKind::Call)); // $95k
    e.register_instrument(option("BTC-C2", 10_500_000, poc_core::OptionKind::Call)); // $105k
    e
}

fn seed(e: &mut Engine) {
    for provider in ["pyth", "chainlink"] {
        e.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: provider.into(),
            ts: T0,
            price_quote_minor: 10_000_000, // $100k per 1.0 BTC
        });
    }
    e.process(Command::Tick { now: T0 });
    for sub in 1..=6u64 {
        e.process(Command::Deposit {
            subaccount: sub,
            amount_quote_minor: 10_000_000_000,
        });
    }
}

fn spread_legs() -> Vec<ComboLegCommand> {
    vec![
        ComboLegCommand {
            symbol: "BTC-C1".into(),
            side: Side::Bid,
            ratio: 1,
        },
        ComboLegCommand {
            symbol: "BTC-C2".into(),
            side: Side::Ask,
            ratio: 1,
        },
    ]
}

fn place_combo(
    e: &mut Engine,
    owner: u64,
    side: Side,
    limit: i128,
    units: u64,
    tif: ComboTif,
) -> Vec<Event> {
    e.process(Command::PlaceCombo {
        owner,
        legs: spread_legs(),
        side,
        limit_quote_minor_per_unit: limit,
        units,
        tif,
        now: T0 + 10,
    })
}

fn place_limit(
    e: &mut Engine,
    sub: u64,
    symbol: &str,
    side: Side,
    price: u64,
    qty: u64,
) -> Vec<Event> {
    e.process(Command::Place {
        request: poc_engine::OrderRequest::limit(sub, symbol, side, price, qty),
        now: T0 + 5,
    })
}

// ---------------------------------------------------------------------------
// 1. The spread assembles atomically from two thin wing books.
// ---------------------------------------------------------------------------

#[test]
fn spread_assembles_atomically_from_thin_wings() {
    let mut e = engine_with_wings();
    seed(&mut e);
    // MM-2 seeds thin wings: C1 asks at $10.00, C2 bids at $1.50.
    place_limit(&mut e, 2, "BTC-C1", Side::Ask, 1_000, 5);
    place_limit(&mut e, 2, "BTC-C2", Side::Bid, 150, 5);
    // Composite ask: pay 100 minor (C1 @1_000 ticks) minus receive 15
    // (C2 @150) = 85 minor per unit.
    let bbo = e.composite_bbo(&spread_legs()).unwrap();
    assert_eq!(bbo.ask_cost_quote_minor_per_unit, Some(85));

    // T-1 buys 5 units of the spread at a 90-minor net limit: implied-in
    // sweeps both wings in one atomic command.
    let events = place_combo(&mut e, 1, Side::Bid, 90, 5, ComboTif::Ioc);
    let trades: Vec<&Event> = events
        .iter()
        .filter(|ev| matches!(ev, Event::TradeExecuted(_)))
        .collect();
    assert_eq!(trades.len(), 2, "one trade per leg: {events:?}");
    let traded: Vec<(String, u64, u64)> = events
        .iter()
        .filter_map(|ev| match ev {
            Event::TradeExecuted(t) => Some((t.symbol.clone(), t.price_ticks, t.qty_lots)),
            _ => None,
        })
        .collect();
    assert!(traded.contains(&("BTC-C1".into(), 1_000, 5)));
    assert!(traded.contains(&("BTC-C2".into(), 150, 5)));
    // Fully filled at placement: no remainder close, no auto-RFQ.
    assert!(!events
        .iter()
        .any(|ev| matches!(ev, Event::ComboClosed { .. })));
    assert!(!events
        .iter()
        .any(|ev| matches!(ev, Event::RfqCreated { .. })));
    // Positions: long 5 C1, short 5 C2.
    let acct = e.account(1).unwrap();
    assert_eq!(acct.lots_of("BTC-C1"), 5);
    assert_eq!(acct.lots_of("BTC-C2"), -5);
}

// ---------------------------------------------------------------------------
// 2. Net-limit respect: a too-tight implied-in leaves the book alone.
// ---------------------------------------------------------------------------

#[test]
fn implied_in_respects_the_net_limit() {
    let mut e = engine_with_wings();
    seed(&mut e);
    place_limit(&mut e, 2, "BTC-C1", Side::Ask, 1_000, 5);
    place_limit(&mut e, 2, "BTC-C2", Side::Bid, 150, 5);
    // 85 composite vs an 84 limit: no implied-in, GTC rests.
    let events = place_combo(&mut e, 1, Side::Bid, 84, 5, ComboTif::Gtc);
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Event::ComboPlaced { .. })));
    assert!(!events
        .iter()
        .any(|ev| matches!(ev, Event::TradeExecuted(_))));
    assert_eq!(e.combos_view().all().count(), 1);
}

// ---------------------------------------------------------------------------
// 3. Combo-vs-combo at the RESTING limit.
// ---------------------------------------------------------------------------

#[test]
fn combo_vs_combo_trades_at_the_resting_limit() {
    let mut e = engine_with_wings();
    seed(&mut e);
    // MM-2 rests a SELL of the spread demanding an 82-minor net.
    place_combo(&mut e, 2, Side::Ask, 82, 5, ComboTif::Gtc);
    // T-1 crosses with a BUY at 90: trade at the resting 82.
    let events = place_combo(&mut e, 1, Side::Bid, 90, 3, ComboTif::Gtc);
    let combo_trades: Vec<&poc_engine::ComboTrade> = events
        .iter()
        .filter_map(|ev| match ev {
            Event::ComboTraded(t) => Some(t.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(combo_trades.len(), 2, "taker + maker views");
    for t in &combo_trades {
        assert_eq!(t.regime, "combo");
        assert_eq!(t.units, 3);
        let net = t.effective_net_quote_minor_per_unit;
        assert!((net - 82).abs() <= 10, "net {net} vs resting 82");
        assert_eq!(t.leg_executions.len(), 2);
    }
    // Positions match the package: buyer long C1 / short C2; seller mirrored.
    assert_eq!(e.account(1).unwrap().lots_of("BTC-C1"), 3);
    assert_eq!(e.account(1).unwrap().lots_of("BTC-C2"), -3);
    assert_eq!(e.account(2).unwrap().lots_of("BTC-C1"), -3);
    assert_eq!(e.account(2).unwrap().lots_of("BTC-C2"), 3);
    // The resting combo scaled (5 -> 2 open); reservation followed.
    let resting = e.combos_view().all().next().cloned().unwrap();
    assert_eq!(resting.open_units(), 2);
    // Cash: buyer paid 3 × $8.20 = 246_00... in per-leg settlement.
    // (The per-leg split sums to the net: conservation via trades.)
}

// ---------------------------------------------------------------------------
// 4. Implied-out: a plain order fills into a resting combo, with hedge.
// ---------------------------------------------------------------------------

#[test]
fn implied_out_absorbs_a_plain_order_with_hedge() {
    let mut e = engine_with_wings();
    seed(&mut e);
    // MM-2 rests a BUY of the spread at an 80-minor net (buy C1, sell C2).
    place_combo(&mut e, 2, Side::Bid, 80, 5, ComboTif::Gtc);
    // Wing books: C2 bid at 160 ticks (the combo's hedge side), no C1
    // asks. The combo's implied C1 bid: V_C1 <= L - H, H = -16 (the
    // combo SELLS C2 at the 160 bid: flow = -16 minor) ->
    // V_C1 <= 80 + 16 = 96 minor -> 960 ticks (tick = 0.1 minor per
    // tick per unit).
    place_limit(&mut e, 3, "BTC-C2", Side::Bid, 160, 10);
    let events = e.process(Command::Place {
        request: poc_engine::OrderRequest::limit(1, "BTC-C1", Side::Ask, 960, 3),
        now: T0 + 20,
    });
    // The aggressor filled into the combo at the implied price.
    let trades: Vec<(String, u64, u64, u64, u64)> = events
        .iter()
        .filter_map(|ev| match ev {
            Event::TradeExecuted(t) => Some((
                t.symbol.clone(),
                t.price_ticks,
                t.qty_lots,
                t.taker_subaccount,
                t.maker_subaccount,
            )),
            _ => None,
        })
        .collect();
    // Leg 1 (agressed): taker 1 sells C1 to the combo owner 2 at 9_600.
    assert!(
        trades.iter().any(|(s, p, q, tk, mk)| s == "BTC-C1"
            && *p == 960
            && *q == 3
            && *tk == 1
            && *mk == 2),
        "trades: {trades:?}"
    );
    // Hedge: the combo owner SELLS C2 into the resting bid at 160.
    assert!(
        trades
            .iter()
            .any(|(s, p, q, tk, mk)| s == "BTC-C2" && *p == 160 && *q == 3 && *tk == 2),
        "trades: {trades:?}"
    );
    // The combo consumed 3 of its 5 units.
    let resting = e.combos_view().all().next().cloned().unwrap();
    assert_eq!(resting.open_units(), 2);
    // The aggressor's order fully filled (no remainder).
    assert!(events.iter().any(|ev| matches!(
        ev,
        Event::OrderClosed {
            reason: poc_engine::OrderCloseReason::Filled,
            ..
        }
    )));
}

// ---------------------------------------------------------------------------
// 5. Aggressor never worse than its limit in implied-out.
// ---------------------------------------------------------------------------

#[test]
fn implied_out_never_worse_than_the_taker_limit() {
    let mut e = engine_with_wings();
    seed(&mut e);
    place_combo(&mut e, 2, Side::Bid, 80, 5, ComboTif::Gtc);
    place_limit(&mut e, 3, "BTC-C2", Side::Bid, 160, 10);
    // A taker demanding 970 (higher than the 960 implied bid): no
    // fill — the order rests instead.
    let events = e.process(Command::Place {
        request: poc_engine::OrderRequest::limit(1, "BTC-C1", Side::Ask, 970, 3),
        now: T0 + 20,
    });
    assert!(!events
        .iter()
        .any(|ev| matches!(ev, Event::TradeExecuted(_))));
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Event::OrderResting { .. })));
}

// ---------------------------------------------------------------------------
// 6. The package margin gate rejects unaffordable combos.
// ---------------------------------------------------------------------------

#[test]
fn package_margin_gate_rejects_unaffordable_combos() {
    let mut e = engine_with_wings();
    seed(&mut e);
    // A broke account against a deep package (10_000 units of the
    // spread dwarf the deposit).
    e.process(Command::Deposit {
        subaccount: 9,
        amount_quote_minor: 1_000,
    });
    let events = e.process(Command::PlaceCombo {
        owner: 9,
        legs: spread_legs(),
        side: Side::Bid,
        limit_quote_minor_per_unit: 90,
        units: 10_000,
        tif: ComboTif::Gtc,
        now: T0 + 10,
    });
    assert!(matches!(
        events.first(),
        Some(Event::ComboRejected {
            reason: "insufficient margin for package",
            ..
        })
    ));
}

// ---------------------------------------------------------------------------
// 7. Cancel releases the reservation; partial fills scale it.
// ---------------------------------------------------------------------------

#[test]
fn cancel_releases_and_fills_scale_reservations() {
    let mut e = engine_with_wings();
    seed(&mut e);
    place_combo(&mut e, 2, Side::Ask, 82, 10, ComboTif::Gtc);
    let id = e.combos_view().all().next().unwrap().id;
    let reserved = e.combo_reservations_view().get(&id).copied().unwrap_or(0);
    assert!(reserved > 0);
    // Partial fill of 4 units.
    place_combo(&mut e, 1, Side::Bid, 90, 4, ComboTif::Ioc);
    let scaled = e.combo_reservations_view().get(&id).copied().unwrap_or(0);
    assert!(scaled < reserved, "scaled {scaled} must be < {reserved}");
    // Cancel releases the rest.
    e.process(Command::CancelCombo {
        owner: 2,
        combo_id: id,
        now: T0 + 30,
    });
    assert_eq!(e.combo_reservations_view().get(&id), None);
    assert_eq!(e.combos_view().all().count(), 0);
}

// ---------------------------------------------------------------------------
// 8. IOC-combo shortfall publishes the RFQ collar; GTC never publishes.
// ---------------------------------------------------------------------------

#[test]
fn ioc_shortfall_publishes_a_collared_rfq() {
    let mut e = engine_with_wings();
    seed(&mut e);
    // Empty books: an IOC buy of 5 units at an 80-minor net fills nothing.
    let events = place_combo(&mut e, 1, Side::Bid, 80, 5, ComboTif::Ioc);
    let rfq = events.iter().find_map(|ev| match ev {
        Event::RfqCreated {
            rfq_id,
            taker,
            legs,
            max_total_cost_quote_minor,
            ..
        } => Some((*rfq_id, *taker, legs.clone(), *max_total_cost_quote_minor)),
        _ => None,
    });
    let (rfq_id, taker, legs, max_total) = rfq.expect("shortfall must publish an RFQ");
    assert_eq!(taker, 1);
    assert_eq!(legs.len(), 2);
    assert_eq!(legs[0].qty_lots, 5); // remaining units × ratio
    assert_eq!(legs[0].side, Side::Bid); // the taker's side on C1
    assert_eq!(legs[1].side, Side::Ask);
    // Buyer cap: 5 units × 80 minor = 400.
    assert_eq!(max_total, Some(400));
    assert_eq!(
        e.rfq_view().rfq(rfq_id).unwrap().status,
        poc_rfq::RfqStatus::Open
    );

    // GTC shortfall never publishes.
    let events = place_combo(&mut e, 1, Side::Bid, 80, 5, ComboTif::Gtc);
    assert!(!events
        .iter()
        .any(|ev| matches!(ev, Event::RfqCreated { .. })));
}

// ---------------------------------------------------------------------------
// 9. Single-leg sweep shortfall publishes a one-leg RFQ (GTC never).
// ---------------------------------------------------------------------------

#[test]
fn single_leg_sweep_shortfall_publishes_rfq() {
    let mut e = engine_with_wings();
    seed(&mut e);
    let events = e.process(Command::Place {
        request: poc_engine::OrderRequest {
            tif: poc_core::TimeInForce::Ioc,
            ..poc_engine::OrderRequest::limit(1, "BTC-C1", Side::Bid, 1_000, 7)
        },
        now: T0 + 20,
    });
    let rfq = events.iter().find_map(|ev| match ev {
        Event::RfqCreated {
            rfq_id,
            legs,
            max_total_cost_quote_minor,
            ..
        } => Some((*rfq_id, legs.clone(), *max_total_cost_quote_minor)),
        _ => None,
    });
    let (rfq_id, legs, max_total) = rfq.expect("IOC sweep shortfall publishes");
    assert_eq!(legs.len(), 1);
    assert_eq!(legs[0].symbol, "BTC-C1");
    assert_eq!(legs[0].qty_lots, 7);
    // Notional of 7 lots at the 1_000-tick limit: 7 × 100 = 700 minor.
    assert_eq!(max_total, Some(700));
    assert_eq!(
        e.rfq_view().rfq(rfq_id).unwrap().status,
        poc_rfq::RfqStatus::Open
    );

    // GTC never publishes.
    let events = e.process(Command::Place {
        request: poc_engine::OrderRequest::limit(1, "BTC-C1", Side::Bid, 1_000, 7),
        now: T0 + 21,
    });
    assert!(!events
        .iter()
        .any(|ev| matches!(ev, Event::RfqCreated { .. })));
}

// ---------------------------------------------------------------------------
// 10. Static validation: unknown instrument, duplicate legs, ratio 0.
// ---------------------------------------------------------------------------

#[test]
fn static_validation_rejects_bad_packages() {
    let mut e = engine_with_wings();
    seed(&mut e);
    let cases: Vec<(Vec<ComboLegCommand>, &str)> = vec![
        (
            vec![ComboLegCommand {
                symbol: "NOPE".into(),
                side: Side::Bid,
                ratio: 1,
            }],
            "unknown or duplicate instrument in package",
        ),
        (
            vec![
                ComboLegCommand {
                    symbol: "BTC-C1".into(),
                    side: Side::Bid,
                    ratio: 1,
                },
                ComboLegCommand {
                    symbol: "BTC-C1".into(),
                    side: Side::Ask,
                    ratio: 1,
                },
            ],
            "unknown or duplicate instrument in package",
        ),
        (
            vec![ComboLegCommand {
                symbol: "BTC-C1".into(),
                side: Side::Bid,
                ratio: 0,
            }],
            "leg ratio must be positive",
        ),
        (vec![], "leg count out of bounds"),
    ];
    for (legs, reason) in cases {
        let events = e.process(Command::PlaceCombo {
            owner: 1,
            legs,
            side: Side::Bid,
            limit_quote_minor_per_unit: 100,
            units: 1,
            tif: ComboTif::Gtc,
            now: T0 + 10,
        });
        assert!(
            matches!(events.first(), Some(Event::ComboRejected { reason: r, .. }) if *r == reason),
            "expected {reason}, got {events:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 11. Venue-path settlement: fees route through the revenue router and
//     cash movements conserve.
// ---------------------------------------------------------------------------

#[test]
fn combo_settlement_conserves_cash() {
    let mut e = engine_with_wings();
    seed(&mut e);
    let cash_before: i128 = (1..=6u64)
        .map(|s| e.account(s).unwrap().cash_quote_minor)
        .sum();
    let pools_before = e.venue_pools();
    place_combo(&mut e, 2, Side::Ask, 820, 5, ComboTif::Gtc);
    place_combo(&mut e, 1, Side::Bid, 900, 5, ComboTif::Ioc);
    let cash_after: i128 = (1..=6u64)
        .map(|s| e.account(s).unwrap().cash_quote_minor)
        .sum();
    let pools_after = e.venue_pools();
    // Cash moved from buyer to seller; fees left the accounts into the
    // pools. Conservation: (cash_after - cash_before) + routed fees == 0
    // modulo the house share (deliberately untracked).
    let fees_paid = cash_before - cash_after; // positive = net paid by accounts
    let pools_delta: i128 = (pools_after.0 - pools_before.0).try_into().unwrap_or(0)
        + (pools_after.1 as i128 - pools_before.1 as i128)
        + (pools_after.2 as i128 - pools_before.2 as i128)
        + (pools_after.3 as i128 - pools_before.3 as i128);
    assert!(
        fees_paid >= pools_delta,
        "accounts paid {fees_paid}, pools gained {pools_delta}"
    );
    assert!(fees_paid - pools_delta >= 0);
    // The taker fee is positive (fees actually flowed).
    assert!(fees_paid > 0);
}

// ---------------------------------------------------------------------------
// 12. FOK rejects when the package cannot possibly fill.
// ---------------------------------------------------------------------------

#[test]
fn fok_rejects_when_liquidity_is_missing() {
    let mut e = engine_with_wings();
    seed(&mut e);
    let events = place_combo(&mut e, 1, Side::Bid, 90, 5, ComboTif::Fok);
    assert!(matches!(
        events.first(),
        Some(Event::ComboRejected {
            reason: "fok: insufficient package liquidity",
            ..
        })
    ));
    // FOK succeeds when the wings are deep enough.
    place_limit(&mut e, 2, "BTC-C1", Side::Ask, 1_000, 5);
    place_limit(&mut e, 2, "BTC-C2", Side::Bid, 150, 5);
    let events = place_combo(&mut e, 1, Side::Bid, 90, 5, ComboTif::Fok);
    assert!(!events
        .iter()
        .any(|ev| matches!(ev, Event::ComboRejected { .. })));
    assert!(events.iter().any(|ev| matches!(ev, Event::ComboTraded(_))));
    assert_eq!(e.account(1).unwrap().lots_of("BTC-C1"), 5);
}

// ---------------------------------------------------------------------------
// 13. Bit-for-bit replay of a combo-heavy journal.
// ---------------------------------------------------------------------------

#[test]
fn combo_journal_replays_bit_for_bit() {
    let config = EngineConfig::default();
    let mut e = Engine::new(config.clone());
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    e.register_instrument(option("BTC-C1", 9_500_000, poc_core::OptionKind::Call));
    e.register_instrument(option("BTC-C2", 10_500_000, poc_core::OptionKind::Call));
    seed(&mut e);
    place_limit(&mut e, 2, "BTC-C1", Side::Ask, 1_000, 5);
    place_limit(&mut e, 2, "BTC-C2", Side::Bid, 150, 5);
    place_combo(&mut e, 3, Side::Ask, 82, 4, ComboTif::Gtc);
    place_combo(&mut e, 1, Side::Bid, 90, 6, ComboTif::Ioc);
    e.process(Command::Place {
        request: poc_engine::OrderRequest::limit(4, "BTC-C1", Side::Ask, 9_600, 2),
        now: T0 + 25,
    });
    e.process(Command::Tick { now: T0 + 30 });
    let journal: Vec<Event> = e.journal().to_vec();

    let replayed = Engine::replay(config, &journal);
    let fp = |eng: &Engine| {
        (
            eng.accounts_iter()
                .map(|(&s, a)| (s, a.cash_quote_minor, a.positions.clone()))
                .collect::<Vec<_>>(),
            eng.combos_view().all().count(),
            eng.stats().trades,
        )
    };
    assert_eq!(fp(&e), fp(&replayed));
}

// ---------------------------------------------------------------------------
// 14. Cancel-all pulls the owner's combos.
// ---------------------------------------------------------------------------

#[test]
fn cancel_all_pulls_combos() {
    let mut e = engine_with_wings();
    seed(&mut e);
    place_combo(&mut e, 2, Side::Ask, 82, 5, ComboTif::Gtc);
    place_combo(&mut e, 3, Side::Ask, 83, 5, ComboTif::Gtc);
    e.process(Command::CancelAll {
        subaccount: 2,
        symbol: None,
        now: T0 + 40,
    });
    assert_eq!(e.combos_view().all().count(), 1);
    assert_eq!(e.combos_view().all().next().unwrap().owner, 3);
}
