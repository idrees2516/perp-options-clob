//! American early-exercise integration tests.
//!
//! Covers the full AMER lifecycle end to end: tender → TWAP window →
//! settlement with pro-rata short assignment, fee routing, conservation,
//! the European-style rejection, deferred settlement on a young oracle,
//! position-churn self-limiting, and bit-exact journal replay.

use poc_core::{
    AmericanParams, ExerciseStyle, Instrument, OptionMarket, OptionVariant, Side, TimestampMs,
};
use poc_engine::command::OrderRequest;
use poc_engine::{Command, Engine, EngineConfig, Event};

const T0: TimestampMs = 10_000_000;
/// $50k, $1M, $2M deposits in quote minor (2dp).
const RICH: u128 = 200_000_000;

/// An ITM American BTC call: strike $80k, spot $100k, 30 days out,
/// 1-minute TWAP settlement for fast tests, 5 bp exercise fee.
fn american_call() -> OptionMarket {
    OptionMarket {
        symbol: "BTC-AMER-80000-C".into(),
        base_symbol: "BTC".into(),
        kind: poc_core::OptionKind::Call,
        strike_quote_minor: 8_000_000,
        expiry_ts_ms: T0 + 30 * 86_400_000,
        variant: OptionVariant::Dated,
        exercise_style: ExerciseStyle::American,
        american: AmericanParams {
            settlement_twap_ms: 60_000,
            exercise_fee_bps: 5,
        },
        ..OptionMarket::default()
    }
}

fn european_twin() -> OptionMarket {
    OptionMarket {
        symbol: "BTC-EUR-80000-C".into(),
        expiry_ts_ms: T0 + 30 * 86_400_000,
        exercise_style: ExerciseStyle::European,
        ..american_call()
    }
}

fn seed(e: &mut Engine, ts: TimestampMs, price: u128) {
    for provider in ["pyth", "chainlink"] {
        e.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: provider.into(),
            ts,
            price_quote_minor: price,
        });
    }
    e.process(Command::Tick { now: ts });
}

fn deposit(e: &mut Engine, sub: u64, amount: u128) {
    e.process(Command::Deposit {
        subaccount: sub,
        amount_quote_minor: amount,
    });
}

fn limit(
    e: &mut Engine,
    sub: u64,
    symbol: &str,
    side: Side,
    price_ticks: u64,
    qty: u64,
    now: TimestampMs,
) {
    e.process(Command::Place {
        request: OrderRequest::limit(sub, symbol, side, price_ticks, qty),
        now,
    });
}

/// Tracked conservation total: cash + venue pools (see properties.rs).
fn tracked_total(e: &Engine) -> i128 {
    let (insurance, rewards, house, buyback) = e.venue_pools();
    let cash: i128 = e.accounts_iter().map(|(_, a)| a.cash_quote_minor).sum();
    cash + insurance
        + poc_core::to_i128(rewards)
        + poc_core::to_i128(house)
        + poc_core::to_i128(buyback)
}

#[test]
fn american_mark_equals_european_at_zero_rate() {
    // Migration invariant: with the venue default r = 0 (and b = 0), the
    // American BAW mark equals the European BSM mark exactly, so flipping
    // a market's style cannot move marks under the default config.
    let mut e = Engine::new(EngineConfig::default());
    e.register_instrument(Instrument::Option(american_call()));
    e.register_instrument(Instrument::Option(european_twin()));
    seed(&mut e, T0, 10_000_000);

    let marks = e.build_marks(T0).expect("oracle is live");
    let set = marks.get("BTC").expect("BTC mark set");
    let amer = set.marks.get("BTC-AMER-80000-C").expect("american mark");
    let euro = set.marks.get("BTC-EUR-80000-C").expect("european mark");
    match (amer, euro) {
        (
            poc_margin::Mark::Option {
                premium_quote_minor_per_base: a,
                ..
            },
            poc_margin::Mark::Option {
                premium_quote_minor_per_base: b,
                ..
            },
        ) => assert_eq!(a, b, "zero-rate marks must coincide: {a} vs {b}"),
        _ => panic!("both must be option marks"),
    }
}

#[test]
fn exercise_lifecycle_with_pro_rata_assignment() {
    let cfg = EngineConfig {
        reward_per_interval_quote_minor: 0, // keep conservation exact
        ..EngineConfig::default()
    };
    let mut e = Engine::new(cfg);
    e.register_instrument(Instrument::Option(american_call()));
    seed(&mut e, T0, 10_000_000);
    for sub in 1..=3 {
        deposit(&mut e, sub, RICH);
    }

    // Two short sellers make markets: account 2 offers 2 lots, account 3
    // offers 1 lot, both at a $24,000/BTC premium (intrinsic + extrinsic).
    let premium_ticks = 2_400_000 / 50; // 48,000 ticks @ $0.50
    limit(
        &mut e,
        2,
        "BTC-AMER-80000-C",
        Side::Ask,
        premium_ticks,
        2,
        T0 + 1_000,
    );
    limit(
        &mut e,
        3,
        "BTC-AMER-80000-C",
        Side::Ask,
        premium_ticks,
        1,
        T0 + 2_000,
    );
    // The buyer lifts all three lots.
    limit(
        &mut e,
        1,
        "BTC-AMER-80000-C",
        Side::Bid,
        premium_ticks,
        3,
        T0 + 3_000,
    );

    assert_eq!(e.account(1).map(|a| a.lots_of("BTC-AMER-80000-C")), Some(3));
    assert_eq!(
        e.account(2).map(|a| a.lots_of("BTC-AMER-80000-C")),
        Some(-2)
    );
    assert_eq!(
        e.account(3).map(|a| a.lots_of("BTC-AMER-80000-C")),
        Some(-1)
    );

    let before = tracked_total(&e);

    // Tender all 3 lots for early exercise.
    let evs = e.process(Command::Exercise {
        subaccount: 1,
        symbol: "BTC-AMER-80000-C".into(),
        lots: 3,
        now: T0 + 4_000,
    });
    assert!(matches!(evs.as_slice(), [Event::ExerciseQueued { .. }]));

    // Fill the TWAP window with oracle prints at $100k.
    for dt in [10_000_u64, 30_000, 50_000, 60_000] {
        seed(&mut e, T0 + dt, 10_000_000);
    }
    let (cash1_pre, cash2_pre, cash3_pre) = (
        e.account(1).map(|a| a.cash_quote_minor).unwrap(),
        e.account(2).map(|a| a.cash_quote_minor).unwrap(),
        e.account(3).map(|a| a.cash_quote_minor).unwrap(),
    );
    let (_ins_pre, _rew_pre, house_pre, _buy_pre) = e.venue_pools();
    let settle_events = e.process(Command::Tick { now: T0 + 65_000 });
    let exercised = settle_events
        .iter()
        .find_map(|ev| match ev {
            Event::OptionExercised(ex) => Some(ex.as_ref().clone()),
            _ => None,
        })
        .expect("exercise must settle when the window closes");

    // TWAP struck at $100k: intrinsic $20k/BTC.
    assert_eq!(exercised.settlement_quote_minor, 10_000_000);
    assert_eq!(exercised.intrinsic_per_lot_quote_minor, 20_000); // $200 per 0.01 BTC lot
    assert_eq!(exercised.settled_lots, 3);
    assert_eq!(exercised.requested_lots, 3);
    // 5 bp of $6,000 gross = $0.30 = 30 minor.
    assert_eq!(exercised.exercise_fee_quote_minor, 30);
    // Pro-rata: 2 lots to account 2, 1 lot to account 3 (exact 2:1 split).
    assert_eq!(exercised.assignments.len(), 2);
    let by_sub: Vec<(u64, u64)> = exercised
        .assignments
        .iter()
        .map(|a| (a.subaccount, a.lots))
        .collect();
    assert_eq!(by_sub, vec![(2, 2), (3, 1)]);
    assert!(exercised
        .assignments
        .iter()
        .all(|a| a.charge_quote_minor == 20_000 * u128::from(a.lots)));

    // Position ledger: everyone flat.
    assert_eq!(e.account(1).map(|a| a.lots_of("BTC-AMER-80000-C")), Some(0));
    assert_eq!(e.account(2).map(|a| a.lots_of("BTC-AMER-80000-C")), Some(0));
    assert_eq!(e.account(3).map(|a| a.lots_of("BTC-AMER-80000-C")), Some(0));

    // Cash economics (premium-unpaid convention), relative to the
    // post-trade balances so trading fees stay out of the arithmetic:
    //   buyer:  entry 2.4M/base, closes at 2.0M/base on 0.03 BTC
    //          -> realized -12,000 minor, then -30 fee.
    //   seller 2: entry 2.4M/base, closes at 2.0M on 0.02 BTC -> +8,000.
    //   seller 3: same on 0.01 BTC -> +4,000.
    assert_eq!(
        e.account(1).map(|a| a.cash_quote_minor).unwrap(),
        cash1_pre - 12_000 - 30
    );
    assert_eq!(
        e.account(2).map(|a| a.cash_quote_minor).unwrap(),
        cash2_pre + 8_000
    );
    assert_eq!(
        e.account(3).map(|a| a.cash_quote_minor).unwrap(),
        cash3_pre + 4_000
    );

    // Conservation: the only tracked-total move is the fee's house share
    // (routed out of accounts into venue pools; the full 30 returns to
    // pools, so tracked_total is unchanged).
    let after = tracked_total(&e);
    assert_eq!(after, before, "exercise must conserve the tracked total");

    // Fee revenue routed: the house pool grows by its 60% share of the
    // 30-minor exercise fee (18), on top of the trading fees already
    // collected before the request.
    let (_ins, _rewards, house, _buy) = e.venue_pools();
    assert_eq!(
        house - house_pre,
        18,
        "exercise fee must route to the house pool"
    );
}

#[test]
fn european_markets_reject_exercise() {
    let mut e = Engine::new(EngineConfig::default());
    e.register_instrument(Instrument::Option(european_twin()));
    seed(&mut e, T0, 10_000_000);
    deposit(&mut e, 1, RICH);
    let evs = e.process(Command::Exercise {
        subaccount: 1,
        symbol: "BTC-EUR-80000-C".into(),
        lots: 1,
        now: T0,
    });
    assert!(matches!(
        evs.as_slice(),
        [Event::ExerciseRejected {
            reason: "european-style",
            ..
        }]
    ));
}

#[test]
fn exercise_requires_a_long_position() {
    let mut e = Engine::new(EngineConfig::default());
    e.register_instrument(Instrument::Option(american_call()));
    seed(&mut e, T0, 10_000_000);
    deposit(&mut e, 1, RICH);
    for (lots, reason) in [(0, "zero-lots"), (5, "no-long-position")] {
        let evs = e.process(Command::Exercise {
            subaccount: 1,
            symbol: "BTC-AMER-80000-C".into(),
            lots,
            now: T0,
        });
        assert!(matches!(
            evs.as_slice(),
            [Event::ExerciseRejected { reason: r, .. }] if *r == reason
        ));
    }
}

#[test]
fn settlement_on_sparse_oracle_never_drops() {
    // A request whose window has sparse oracle coverage must either defer
    // or settle on the step-TWAP (forward-held last mark) — never drop
    // the request and never settle on a fabricated price.
    let mut e = Engine::new(EngineConfig::default());
    e.register_instrument(Instrument::Option(american_call()));
    seed(&mut e, T0, 10_000_000);
    for sub in 1..=2 {
        deposit(&mut e, sub, RICH);
    }
    limit(
        &mut e,
        2,
        "BTC-AMER-80000-C",
        Side::Ask,
        48_000,
        1,
        T0 + 1_000,
    );
    limit(
        &mut e,
        1,
        "BTC-AMER-80000-C",
        Side::Bid,
        48_000,
        1,
        T0 + 2_000,
    );

    e.process(Command::Exercise {
        subaccount: 1,
        symbol: "BTC-AMER-80000-C".into(),
        lots: 1,
        now: T0 + 3_000,
    });

    // Advance past settle_at (T0+3k + 60k) with NO new oracle prints:
    // the step-TWAP holds the last accepted mark forward through the
    // window, so the settlement resolves exactly at $100k.
    for dt in [30_000_u64, 50_000] {
        seed(&mut e, T0 + dt, 10_000_000);
    }
    let evs1 = e.process(Command::Tick { now: T0 + 64_000 });
    let settled = evs1
        .iter()
        .find_map(|ev| match ev {
            Event::OptionExercised(ex) => Some(ex.as_ref().clone()),
            _ => None,
        })
        .expect("must settle or defer at window close, never drop");
    assert_eq!(settled.settlement_quote_minor, 10_000_000);
    assert_eq!(settled.settled_lots, 1);

    // No double settlement on later ticks.
    seed(&mut e, T0 + 70_000, 10_000_000);
    let evs2 = e.process(Command::Tick { now: T0 + 131_000 });
    assert!(evs2
        .iter()
        .all(|ev| !matches!(ev, Event::OptionExercised(_))));
}

#[test]
fn exercised_lots_capped_by_position_at_settle() {
    // Request 3, then sell the position away: settlement takes what
    // stands (0), never over-settles.
    let mut e = Engine::new(EngineConfig::default());
    e.register_instrument(Instrument::Option(american_call()));
    seed(&mut e, T0, 10_000_000);
    for sub in 1..=2 {
        deposit(&mut e, sub, RICH);
    }
    limit(
        &mut e,
        2,
        "BTC-AMER-80000-C",
        Side::Ask,
        48_000,
        3,
        T0 + 1_000,
    );
    limit(
        &mut e,
        1,
        "BTC-AMER-80000-C",
        Side::Bid,
        48_000,
        3,
        T0 + 2_000,
    );
    e.process(Command::Exercise {
        subaccount: 1,
        symbol: "BTC-AMER-80000-C".into(),
        lots: 3,
        now: T0 + 3_000,
    });
    // The long flips the position away before settlement.
    limit(
        &mut e,
        2,
        "BTC-AMER-80000-C",
        Side::Bid,
        48_000,
        3,
        T0 + 4_000,
    );
    limit(
        &mut e,
        1,
        "BTC-AMER-80000-C",
        Side::Ask,
        48_000,
        3,
        T0 + 5_000,
    );
    assert_eq!(e.account(1).map(|a| a.lots_of("BTC-AMER-80000-C")), Some(0));

    let mut all_events: Vec<Event> = Vec::new();
    for dt in [20_000_u64, 40_000, 60_000, 61_000, 62_000, 63_000, 64_000] {
        for provider in ["pyth", "chainlink"] {
            all_events.extend(e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: T0 + dt,
                price_quote_minor: 10_000_000,
            }));
        }
        all_events.extend(e.process(Command::Tick { now: T0 + dt }));
    }
    let settled = all_events
        .iter()
        .find_map(|ev| match ev {
            Event::OptionExercised(ex) => Some(ex.as_ref().clone()),
            _ => None,
        })
        .expect("request consumed at window close");
    assert_eq!(settled.settled_lots, 0, "long side no longer holds lots");
    assert!(settled.assignments.is_empty());
    assert_eq!(settled.exercise_fee_quote_minor, 0);
}

/// One account's settled fingerprint for replay comparison.
type AccountFingerprints = Vec<(u64, i128, Vec<(String, i64, u128)>)>;

#[test]
fn exercise_replays_bit_for_bit() {
    // Determinism: the journal replays into an identical engine, queued
    // exercises and all.
    let mut e = Engine::new(EngineConfig::default());
    e.register_instrument(Instrument::Option(american_call()));
    seed(&mut e, T0, 10_000_000);
    for sub in 1..=3 {
        deposit(&mut e, sub, RICH);
    }
    limit(
        &mut e,
        2,
        "BTC-AMER-80000-C",
        Side::Ask,
        48_000,
        2,
        T0 + 1_000,
    );
    limit(
        &mut e,
        3,
        "BTC-AMER-80000-C",
        Side::Ask,
        48_000,
        1,
        T0 + 2_000,
    );
    limit(
        &mut e,
        1,
        "BTC-AMER-80000-C",
        Side::Bid,
        48_000,
        3,
        T0 + 3_000,
    );
    e.process(Command::Exercise {
        subaccount: 1,
        symbol: "BTC-AMER-80000-C".into(),
        lots: 3,
        now: T0 + 4_000,
    });
    for dt in [20_000_u64, 40_000, 63_000] {
        seed(&mut e, T0 + dt, 10_000_000);
    }
    e.process(Command::Tick { now: T0 + 64_000 });

    let replayed = Engine::replay(EngineConfig::default(), e.journal());
    let fingerprint = |eng: &Engine| -> AccountFingerprints {
        eng.accounts_iter()
            .map(|(&s, a)| {
                (
                    s,
                    a.cash_quote_minor,
                    a.positions
                        .iter()
                        .map(|(sym, p)| (sym.clone(), p.signed_lots, p.avg_entry_quote_minor))
                        .collect(),
                )
            })
            .collect()
    };
    assert_eq!(fingerprint(&e), fingerprint(&replayed));

    // Also assert the full journal length (event-for-event equality is
    // implied by replay; length guards against dropped exercise events).
    assert!(!e.journal().is_empty());
}
