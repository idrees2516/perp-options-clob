//! The implied-matching property suite: seeded random command sequences
//! (the same generator the `fuzz_implied_matching` cargo-fuzz target
//! runs) with the implicit-matching invariants asserted after every
//! command. Runs in the normal test profile — no nightly required.
//!
//! Invariants (see docs/INVARIANTS.md I-62..I-67):
//! conservation, no self-trades, aggressor limit respect, combo-book
//! structure, exact lot ratios via a trade-derived position model,
//! reservation consistency, non-crossed books, replay determinism.

use std::collections::BTreeMap;

use poc_combo::ComboTif;
use poc_core::{
    ExerciseStyle, Instrument, OptionKind, OptionMarket, OptionVariant, PerpMarket, Side,
    TimeInForce, TimestampMs,
};
use poc_engine::{ComboLegCommand, Command, Engine, EngineConfig, Event, OrderRequest};

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

type Positions = BTreeMap<(u64, String), i64>;

fn apply_trade_to_model(model: &mut Positions, t: &poc_engine::Trade) {
    let maker_delta = i64::try_from(t.qty_lots).unwrap_or(i64::MAX) * t.maker_side.sign();
    *model
        .entry((t.maker_subaccount, t.symbol.clone()))
        .or_insert(0) += maker_delta;
    let taker_delta =
        i64::try_from(t.qty_lots).unwrap_or(i64::MAX) * t.maker_side.opposite().sign();
    *model
        .entry((t.taker_subaccount, t.symbol.clone()))
        .or_insert(0) += taker_delta;
}

fn fingerprint(e: &Engine) -> Vec<(u64, i128, usize, usize)> {
    e.accounts_iter()
        .map(|(&s, a)| {
            (
                s,
                a.cash_quote_minor,
                a.positions.len(),
                a.open_orders.len(),
            )
        })
        .collect()
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

/// Run one seeded scenario; returns () or panics with the invariant
/// that broke.
fn run_scenario(seed: u64) {
    let mut rng = seed | 1;
    let config = EngineConfig::default();
    let mut e = Engine::new(config.clone());
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    e.register_instrument(call("BTC-C1", 9_500_000));
    e.register_instrument(call("BTC-C2", 10_500_000));
    let mut shadow = Engine::new(config);
    shadow.register_instrument(Instrument::Perp(PerpMarket::default()));
    shadow.register_instrument(call("BTC-C1", 9_500_000));
    shadow.register_instrument(call("BTC-C2", 10_500_000));

    let t0: TimestampMs = 1_000_000;
    let prologue = |eng: &mut Engine| {
        for provider in ["pyth", "chainlink"] {
            eng.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: t0,
                price_quote_minor: 10_000_000,
            });
        }
        eng.process(Command::Tick { now: t0 });
        for sub in 1..=5u64 {
            eng.process(Command::Deposit {
                subaccount: sub,
                amount_quote_minor: 10_000_000_000,
            });
        }
    };
    prologue(&mut e);
    prologue(&mut shadow);

    let mut model: Positions = BTreeMap::new();
    let mut baseline = tracked_total(&e);
    let mut lots_traded: u64 = 0;
    let symbols = ["BTC-PERP", "BTC-C1", "BTC-C2"];

    for step in 0..96u64 {
        let now = t0 + 100 + step * 10;
        let cmd = match next(&mut rng) % 10 {
            0 | 1 => Command::Place {
                request: OrderRequest {
                    tif: if next(&mut rng) % 4 == 0 {
                        TimeInForce::Ioc
                    } else if next(&mut rng) % 8 == 0 {
                        TimeInForce::Fok
                    } else {
                        TimeInForce::Gtc
                    },
                    ..OrderRequest::limit(
                        1 + next(&mut rng) % 5,
                        symbols[(next(&mut rng) % 3) as usize],
                        if next(&mut rng) % 2 == 0 {
                            Side::Bid
                        } else {
                            Side::Ask
                        },
                        100 + next(&mut rng) % 5_000,
                        1 + next(&mut rng) % 4,
                    )
                },
                now,
            },
            2 => Command::Cancel {
                subaccount: 1 + next(&mut rng) % 5,
                order_id: next(&mut rng) % 40,
                now,
            },
            3 => Command::CancelAll {
                subaccount: 1 + next(&mut rng) % 5,
                symbol: None,
                now,
            },
            4 | 5 | 6 => {
                let two_legs = next(&mut rng) % 2 == 0;
                let mut legs = vec![ComboLegCommand {
                    symbol: symbols[1 + (next(&mut rng) % 2) as usize].to_string(),
                    side: Side::Bid,
                    ratio: 1 + (next(&mut rng) % 2) as u32,
                }];
                if two_legs {
                    legs.push(ComboLegCommand {
                        symbol: "BTC-PERP".to_string(),
                        side: Side::Ask,
                        ratio: 1,
                    });
                }
                Command::PlaceCombo {
                    owner: 1 + next(&mut rng) % 5,
                    legs,
                    side: if next(&mut rng) % 2 == 0 {
                        Side::Bid
                    } else {
                        Side::Ask
                    },
                    limit_quote_minor_per_unit: 50 + (next(&mut rng) % 200) as i128,
                    units: 1 + next(&mut rng) % 4,
                    tif: match next(&mut rng) % 4 {
                        0 => ComboTif::Ioc,
                        1 => ComboTif::Fok,
                        _ => ComboTif::Gtc,
                    },
                    now,
                }
            }
            7 => Command::CancelCombo {
                owner: 1 + next(&mut rng) % 5,
                combo_id: next(&mut rng) % 20,
                now,
            },
            8 => Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: if next(&mut rng) % 2 == 0 {
                    "pyth".into()
                } else {
                    "chainlink".into()
                },
                ts: now,
                price_quote_minor: u128::from(9_500_000 + next(&mut rng) % 1_000_000),
            },
            _ => Command::Tick { now },
        };
        let events = e.process(cmd.clone());
        shadow.process(cmd);

        for ev in &events {
            if let Event::TradeExecuted(t) = ev {
                assert_ne!(t.taker_subaccount, t.maker_subaccount, "self-trade: {t:?}");
                apply_trade_to_model(&mut model, t);
                lots_traded += t.qty_lots;
            }
            apply_non_trade_movers(&mut model, ev);
        }
        // The trade-derived position model equals the engine's state.
        for (&(sub, ref symbol), &expected) in &model {
            let actual = e.account(sub).map(|a| a.lots_of(symbol)).unwrap_or(0);
            assert_eq!(
                actual, expected,
                "seed {seed} step {step}: sub {sub} {symbol}"
            );
        }

        // I-62: conservation up to per-lot rounding dust (I-5 identity).
        let total = tracked_total(&e);
        let delta = (total - baseline).abs();
        assert!(
            delta <= i128::from(10 * lots_traded + 64),
            "seed {seed} step {step}: conservation broke by {delta}"
        );
        baseline = total;

        // Books never cross at rest.
        for symbol in e.instruments().keys() {
            if let Some(book) = e.book(symbol) {
                if let (Some(b), Some(a)) = (book.best_bid(), book.best_ask()) {
                    assert!(b < a, "seed {seed} step {step}: crossed {symbol}");
                }
            }
        }
        // Combo book structure + reservation consistency.
        e.combos_view()
            .invariants_hold()
            .expect("combo book invariants");
        for combo in e.combos_view().all() {
            assert!(
                e.combo_reservations_view().contains_key(&combo.id),
                "seed {seed} step {step}: combo {} without reservation",
                combo.id
            );
        }
    }
    assert_eq!(
        fingerprint(&e),
        fingerprint(&shadow),
        "seed {seed}: replay divergence"
    );
}

#[test]
fn implied_matching_properties_hold_across_seeds() {
    // 32 deterministic seeds (the fuzz target explores the same space).
    for seed in 1..=32_u64 {
        run_scenario(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    }
}

#[test]
fn implied_matching_properties_hold_on_edge_seeds() {
    // Degenerate seeds that once produced empty data vectors.
    run_scenario(1);
    run_scenario(u64::MAX);
    run_scenario(0xAAAA_AAAA_AAAA_AAAA);
}
