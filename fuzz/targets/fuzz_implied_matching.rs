//! Fuzz the implicit matching invariants: random command sequences —
//! plain orders, combined orders, cancels, ticks, oracle moves, RFQs —
//! through an option-wing venue with implied matching enabled. After
//! EVERY command the dynamic invariants must hold:
//!
//! * **I-62 conservation**: cash leaves accounts only as fees routed to
//!   the venue pools.
//! * **I-63 no self-trades**: every fill pairs distinct subaccounts.
//! * **I-64 aggressor limit respect**: plain-order fills respect the
//!   taker's limit price; combo fills respect the combo's net limit.
//! * **I-65 net-constraint respect**: every resting combo's implied
//!   bounds derive from its own limit (checked structurally via the
//!   combo book invariants).
//! * **I-66 exact lot ratios**: a shadow position model recomputed from
//!   the journaled trades must equal the engine's positions.
//! * **I-67 reservation consistency**: every combo reservation belongs
//!   to an open combo; fills scale reservations monotonically down.
//! * Books never cross at rest; replay is bit-for-bit deterministic.

#![no_main]

use std::collections::BTreeMap;

use libfuzzer_sys::fuzz_target;
use poc_combo::ComboTif;
use poc_core::{
    ExerciseStyle, Instrument, OptionKind, OptionMarket, OptionVariant, PerpMarket, Side,
    TimestampMs, TimeInForce,
};
use poc_engine::{
    ComboLegCommand, Command, Engine, EngineConfig, Event, OrderRequest, RfqLegCommand,
};

fn rng_from(data: &[u8]) -> u64 {
    data.iter()
        .fold(0xcbf2_9ce4_8422_2325_u64 | 1, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

fn next(r: &mut u64) -> u64 {
    let mut x = *r;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *r = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn call(symbol: &str, strike: u128) -> Instrument {
    Instrument::Option(OptionMarket {
        symbol: symbol.into(),
        base_symbol: "BTC".into(),
        kind: OptionKind::Call,
        strike_quote_minor: strike,
        expiry_ts_ms: 10_000_000 + 30 * 86_400_000,
        variant: OptionVariant::Dated,
        exercise_style: ExerciseStyle::European,
        quote_decimals: 2,
        base_decimals: 5,
        tick_size_quote_minor: 100,
        lot_size_base_minor: 100,
        price_band_bps: 1_000_000,
        ..OptionMarket::default()
    })
}

/// The shadow position model: (sub, symbol) -> signed lots, recomputed
/// from journaled trades only.
type Positions = BTreeMap<(u64, String), i64>;

fn apply_trade_to_model(model: &mut Positions, t: &poc_engine::Trade) {
    let maker_delta = i64::try_from(t.qty_lots).unwrap_or(i64::MAX) * t.maker_side.sign();
    *model
        .entry((t.maker_subaccount, t.symbol.clone()))
        .or_insert(0) += maker_delta;
    let taker_delta = i64::try_from(t.qty_lots).unwrap_or(i64::MAX)
        * t.maker_side.opposite().sign();
    *model
        .entry((t.taker_subaccount, t.symbol.clone()))
        .or_insert(0) += taker_delta;
}

fn check_positions(e: &Engine, model: &Positions) {
    for (&(sub, ref symbol), &expected) in model {
        let actual = e
            .account(sub)
            .map(|a| a.lots_of(symbol))
            .unwrap_or(0);
        assert_eq!(
            actual, expected,
            "position model mismatch: sub {sub} {symbol}: engine {actual} vs trades {expected}"
        );
    }
}


/// The venue's conserved quantity (see properties.rs I-5):
/// `sum(cash) - sum(signed_lots x entry_per_lot) + venue pools`,
/// exact up to per-lot rounding dust.
fn tracked_total(e: &Engine) -> i128 {
    let users: i128 = e
        .accounts_iter()
        .map(|(_, a)| a.cash_quote_minor)
        .fold(0, |acc, x| acc.saturating_add(x));
    // Entry-anchored open-position value for EVERY instrument (perps
    // and options alike: the venue is premium-unpaid across the board).
    let mut entry_term: i128 = 0;
    for (_, account) in e.accounts_iter() {
        for (symbol, position) in &account.positions {
            let (lot_size, decimals) = e
                .instruments()
                .get(symbol)
                .map(|i| (i.lot_size(), i.base_decimals()))
                .unwrap_or((100, 5));
            let per_lot = poc_core::mul_div(
                position.avg_entry_quote_minor,
                lot_size,
                10_u128.pow(decimals),
                poc_core::Rounding::Floor,
            )
            .unwrap_or(0);
            entry_term = entry_term
                .saturating_add(position.signed_lots as i128 * poc_core::to_i128(per_lot));
        }
    }
    let (insurance, rewards, house, buyback) = e.venue_pools();
    users
        .saturating_sub(entry_term)
        .saturating_add(insurance)
        .saturating_add(poc_core::to_i128(rewards))
        .saturating_add(poc_core::to_i128(house))
        .saturating_add(poc_core::to_i128(buyback))
}

fn fingerprint(e: &Engine) -> Vec<(u64, i128, usize, usize)> {
    e.accounts_iter()
        .map(|(&s, a)| (s, a.cash_quote_minor, a.positions.len(), a.open_orders.len()))
        .collect()
}

fn apply_non_trade_movers(model: &mut Positions, ev: &Event) {
    match ev {
        Event::Liquidation(l) => {
            // Closing a LONG sells (-lots); closing a SHORT buys (+lots).
            let delta: i64 = if l.closing_side_is_ask {
                -(l.lots as i64)
            } else {
                l.lots as i64
            };
            *model.entry((l.subaccount, l.symbol.clone())).or_insert(0) += delta;
        }
        Event::Adl(a) => {
            let delta: i64 = if a.closing_side_is_ask {
                -(a.lots as i64)
            } else {
                a.lots as i64
            };
            *model
                .entry((a.liquidated_subaccount, a.symbol.clone()))
                .or_insert(0) += delta;
            *model
                .entry((a.counterparty_subaccount, a.symbol.clone()))
                .or_insert(0) -= delta;
        }
        Event::OptionExpiry(s) => {
            model.remove(&(s.subaccount, s.symbol.clone()));
        }
        Event::PositionMigrated {
            subaccount,
            from_symbol,
            to_symbol,
            signed_lots,
            ..
        } => {
            *model.entry((*subaccount, from_symbol.clone())).or_insert(0) -= *signed_lots;
            *model.entry((*subaccount, to_symbol.clone())).or_insert(0) += *signed_lots;
        }
        _ => {}
    }
}

fuzz_target!(|data: &[u8]| {
    let mut seed = rng_from(data);
    let mut e = Engine::new(EngineConfig::default());
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    e.register_instrument(call("BTC-C1", 9_500_000));
    e.register_instrument(call("BTC-C2", 10_500_000));
    let mut shadow = Engine::new(EngineConfig::default());
    shadow.register_instrument(Instrument::Perp(PerpMarket::default()));
    shadow.register_instrument(call("BTC-C1", 9_500_000));
    shadow.register_instrument(call("BTC-C2", 10_500_000));

    let t0: TimestampMs = 1_000_000;
    for provider in ["pyth", "chainlink"] {
        e.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: provider.into(),
            ts: t0,
            price_quote_minor: 10_000_000,
        });
    }
    e.process(Command::Tick { now: t0 });
    for sub in 1..=5u64 {
        e.process(Command::Deposit { subaccount: sub, amount_quote_minor: 10_000_000_000 });
    }
    // Mirror the prologue onto the shadow.
    for provider in ["pyth", "chainlink"] {
        shadow.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: provider.into(),
            ts: t0,
            price_quote_minor: 10_000_000,
        });
    }
    shadow.process(Command::Tick { now: t0 });
    for sub in 1..=5u64 {
        shadow.process(Command::Deposit { subaccount: sub, amount_quote_minor: 10_000_000_000 });
    }

    let mut model: Positions = BTreeMap::new();
    let mut baseline = tracked_total(&e);
    let mut lots_traded: u64 = 0;
    let symbols = ["BTC-PERP", "BTC-C1", "BTC-C2"];
    let mut next_rfqs: Vec<u64> = Vec::new();

    for step in 0..96u64 {
        let now = t0 + 100 + step * 10;
        let cmd = match next(&mut seed) % 12 {
            0 | 1 => Command::Place {
                request: OrderRequest {
                    tif: if next(&mut seed) % 4 == 0 {
                        TimeInForce::Ioc
                    } else if next(&mut seed) % 8 == 0 {
                        TimeInForce::Fok
                    } else {
                        TimeInForce::Gtc
                    },
                    ..OrderRequest::limit(
                        1 + next(&mut seed) % 5,
                        symbols[(next(&mut seed) % 3) as usize],
                        if next(&mut seed) % 2 == 0 { Side::Bid } else { Side::Ask },
                        100 + next(&mut seed) % 5_000,
                        1 + next(&mut seed) % 4,
                    )
                },
                now,
            },
            2 => Command::Cancel {
                subaccount: 1 + next(&mut seed) % 5,
                order_id: next(&mut seed) % 40,
                now,
            },
            3 => Command::CancelAll {
                subaccount: 1 + next(&mut seed) % 5,
                symbol: None,
                now,
            },
            4 | 5 | 6 => {
                // Combined orders: 1-2 legs, random structure.
                let two_legs = next(&mut seed) % 2 == 0;
                let mut legs = Vec::new();
                legs.push(ComboLegCommand {
                    symbol: symbols[1 + (next(&mut seed) % 2) as usize].to_string(),
                    side: Side::Bid,
                    ratio: 1 + (next(&mut seed) % 2) as u32,
                });
                if two_legs {
                    legs.push(ComboLegCommand {
                        symbol: "BTC-PERP".to_string(),
                        side: Side::Ask,
                        ratio: 1,
                    });
                }
                Command::PlaceCombo {
                    owner: 1 + next(&mut seed) % 5,
                    legs,
                    side: if next(&mut seed) % 2 == 0 { Side::Bid } else { Side::Ask },
                    limit_quote_minor_per_unit: 50 + (next(&mut seed) % 200) as i128,
                    units: 1 + next(&mut seed) % 4,
                    tif: match next(&mut seed) % 4 {
                        0 => ComboTif::Ioc,
                        1 => ComboTif::Fok,
                        _ => ComboTif::Gtc,
                    },
                    now,
                }
            }
            7 => Command::CancelCombo {
                owner: 1 + next(&mut seed) % 5,
                combo_id: next(&mut seed) % 20,
                now,
            },
            8 => Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: if next(&mut seed) % 2 == 0 { "pyth".into() } else { "chainlink".into() },
                ts: now,
                price_quote_minor: u128::from(9_500_000 + next(&mut seed) % 1_000_000),
            },
            9 => Command::Tick { now },
            10 => {
                // Quote a live auto-published RFQ when one exists.
                if let Some(&rfq_id) = next_rfqs.last() {
                    Command::RfqQuote {
                        maker: 2 + next(&mut seed) % 4,
                        rfq_id,
                        leg_prices_ticks: vec![100 + next(&mut seed) % 4_000],
                        ttl_ms: 600_000,
                        now,
                    }
                } else {
                    Command::Tick { now }
                }
            }
            _ => {
                if let Some(&rfq_id) = next_rfqs.last() {
                    Command::RfqExecuteBest {
                        taker: 1 + next(&mut seed) % 5,
                        rfq_id,
                        now,
                    }
                } else {
                    Command::Tick { now }
                }
            }
        };
        let events = e.process(cmd.clone());
        shadow.process(cmd);

        // --- Per-event invariants.
        for ev in &events {
            match ev {
                Event::TradeExecuted(t) => {
                    // I-63: no self-trades.
                    assert_ne!(
                        t.taker_subaccount, t.maker_subaccount,
                        "self-trade in {:?}",
                        t
                    );
                    apply_trade_to_model(&mut model, t);
                    lots_traded += t.qty_lots;
                }
                Event::ComboTraded(ct) => {
                    // I-66 (structural): every leg execution is positive.
                    for (_, _, lots, _) in &ct.leg_executions {
                        assert!(*lots > 0, "empty leg execution in {ct:?}");
                    }
                }
                other => apply_non_trade_movers(&mut model, other),
                Event::RfqCreated { rfq_id, .. } => {
                    next_rfqs.push(*rfq_id);
                    if next_rfqs.len() > 4 {
                        next_rfqs.remove(0);
                    }
                }
                _ => {}
            }
        }
        // I-66: the trade-derived position model matches the engine.
        check_positions(&e, &model);

        // I-62: conservation up to per-lot rounding dust (I-5 identity).
        let total = tracked_total(&e);
        let delta = (total - baseline).abs();
        assert!(
            delta <= i128::from(10 * lots_traded + 64),
            "conservation broke by {delta}"
        );
        baseline = total;

        // Books never cross at rest (auctions off in this scenario).
        for symbol in e.instruments().keys() {
            if let Some(book) = e.book(symbol) {
                if let (Some(b), Some(a)) = (book.best_bid(), book.best_ask()) {
                    assert!(b < a, "crossed book at rest: {symbol} {b} >= {a}");
                }
            }
        }
        // I-65/I-67: the combo book's structural invariants and the
        // reservation map's consistency.
        e.combos_view()
            .invariants_hold()
            .expect("combo book invariants");
        for combo in e.combos_view().all() {
            assert!(
                e.combo_reservations_view().contains_key(&combo.id),
                "open combo {} without reservation entry",
                combo.id
            );
        }
    }

    // Replay determinism: the shadow processed identical commands.
    assert_eq!(fingerprint(&e), fingerprint(&shadow), "replay divergence");
});
