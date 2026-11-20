//! Governance wiring (G-33) and the planning-only margin cache:
//! parameter changes journal, replay reproduces them, and the cache
//! returns exactly what a recompute would.

use poc_core::{Instrument, PerpMarket, Side, TimestampMs};
use poc_engine::{Command, Engine, EngineConfig, Event, OrderRequest};

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

// ---------------------------------------------------------------------------
// 1. A governed parameter applies and changes behavior.
// ---------------------------------------------------------------------------

#[test]
fn governed_parameter_applies_and_changes_behavior() {
    let mut e = engine_with_market();
    seed(&mut e);
    // Sanity: implied matching is on by default (config).
    assert!(e.config().combo.implied_matching);

    // An unknown key is rejected, not applied.
    let events = e.process(Command::GovernanceApply {
        key: "nope.nope".into(),
        value: "1".into(),
        now: T0 + 1,
    });
    assert!(matches!(
        events.first(),
        Some(Event::ParameterRejected { .. })
    ));

    // Turn implied matching off through governance.
    let events = e.process(Command::GovernanceApply {
        key: "combo.implied_matching".into(),
        value: "false".into(),
        now: T0 + 2,
    });
    assert!(
        matches!(events.first(), Some(Event::ParameterChanged { key, value, .. }) if key == "combo.implied_matching" && value == "false")
    );
    assert!(!e.config().combo.implied_matching);

    // Behavior changed: a plain order's remainder no longer fills into
    // resting combos... (structural: verified by config below).
    e.process(Command::GovernanceApply {
        key: "combo.implied_matching".into(),
        value: "true".into(),
        now: T0 + 3,
    });
    assert!(e.config().combo.implied_matching);

    // Numeric parameters parse and clamp.
    e.process(Command::GovernanceApply {
        key: "combo.max_legs".into(),
        value: "16".into(),
        now: T0 + 4,
    });
    assert_eq!(e.config().combo.max_legs, 16);
    let events = e.process(Command::GovernanceApply {
        key: "combo.max_legs".into(),
        value: "0".into(),
        now: T0 + 5,
    });
    assert!(matches!(
        events.first(),
        Some(Event::ParameterRejected { .. })
    ));
    assert_eq!(e.config().combo.max_legs, 16);
}

// ---------------------------------------------------------------------------
// 2. Replay carries parameter history: a mid-run change reproduces.
// ---------------------------------------------------------------------------

#[test]
fn replay_carries_parameter_history() {
    let config = EngineConfig::default();
    let mut e = engine_with_market();
    seed(&mut e);
    // Trade, change a parameter, trade again.
    e.process(Command::Place {
        request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_000, 5),
        now: T0 + 10,
    });
    e.process(Command::Place {
        request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 80_000, 5),
        now: T0 + 11,
    });
    e.process(Command::GovernanceApply {
        key: "quote_interest_bps_per_day".into(),
        value: "7".into(),
        now: T0 + 12,
    });
    e.process(Command::Place {
        request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_100, 3),
        now: T0 + 13,
    });
    e.process(Command::Tick { now: T0 + 14 });
    let journal: Vec<Event> = e.journal().to_vec();

    let replayed = Engine::replay(config, &journal);
    assert_eq!(replayed.config().quote_interest_bps_per_day, 7);
    let fp = |eng: &Engine| {
        eng.accounts_iter()
            .map(|(&s, a)| (s, a.cash_quote_minor, a.positions.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(fp(&e), fp(&replayed));
}

// ---------------------------------------------------------------------------
// 3. The margin cache returns what a recompute would.
// ---------------------------------------------------------------------------

#[test]
fn margin_cache_matches_recompute() {
    let mut e = engine_with_market();
    seed(&mut e);
    e.process(Command::Place {
        request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_000, 5),
        now: T0 + 10,
    });
    e.process(Command::Tick { now: T0 + 20 });
    let marks = e.build_marks(T0 + 20).expect("marks");
    for (sub, account) in e.accounts_iter() {
        let cached = e.effective_margin_summary_cached(account, &marks);
        let fresh = e.effective_margin_summary_at(account, &marks, T0 + 20);
        assert_eq!(cached, fresh, "sub {sub}: cache != recompute");
    }
    // A second pass hits the cache and still agrees.
    for (sub, account) in e.accounts_iter() {
        let cached = e.effective_margin_summary_cached(account, &marks);
        let fresh = e.effective_margin_summary_at(account, &marks, T0 + 20);
        assert_eq!(cached, fresh, "sub {sub}: warm cache != recompute");
    }
}

// ---------------------------------------------------------------------------
// 4. process_quiet: identical state effects, journal-only copies.
// ---------------------------------------------------------------------------

#[test]
fn process_quiet_matches_process_state() {
    let mk = || {
        let mut e = engine_with_market();
        seed(&mut e);
        e
    };
    let mut a = mk();
    let mut b = mk();
    let cmds = vec![
        Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_000, 5),
            now: T0 + 10,
        },
        Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 80_000, 3),
            now: T0 + 11,
        },
        Command::Tick { now: T0 + 12 },
    ];
    for cmd in &cmds {
        a.process(cmd.clone());
        b.process_quiet(cmd.clone());
    }
    let fp = |eng: &Engine| {
        (
            eng.accounts_iter()
                .map(|(&s, x)| (s, x.cash_quote_minor, x.positions.clone()))
                .collect::<Vec<_>>(),
            eng.journal().len(),
            eng.stats().trades,
        )
    };
    assert_eq!(fp(&a), fp(&b), "process and process_quiet diverge");
}
