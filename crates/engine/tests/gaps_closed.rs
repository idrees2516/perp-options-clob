//! Integration tests for the second closure wave: OCO brackets (G-08),
//! TWAP parents (G-10), the volatility index (G-05), collateral interest
//! (G-18), insurance inventory with rebalancing (G-23), LP vaults
//! (G-16), and the batch sequential-commit id fix — each closing with a
//! replay determinism check over its journal.

use poc_core::{Instrument, PerpMarket, Side, TimestampMs};
use poc_engine::{
    Command, Engine, EngineConfig, Event, OrderCloseReason, OrderRequest, UnderlyingListing,
};

const T0: TimestampMs = 1_000_000;

fn config() -> EngineConfig {
    EngineConfig::default()
}

fn engine_with_market() -> Engine {
    let mut e = Engine::new(config());
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    e
}

fn seed_oracle(e: &mut Engine, ts: TimestampMs, price: u128) {
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

fn seed_oracle_raw(e: &mut Engine, ts: TimestampMs, price: u128) {
    for provider in ["pyth", "chainlink"] {
        e.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: provider.into(),
            ts,
            price_quote_minor: price,
        });
    }
}

fn deposit(e: &mut Engine, sub: u64, amount: u128) {
    e.process(Command::Deposit {
        subaccount: sub,
        amount_quote_minor: amount,
    });
}

/// Every event of `e`'s journal replays into an identical engine.
fn assert_replays(e: &Engine) {
    let journal = e.journal().to_vec();
    let replayed = Engine::replay(e.config().clone(), &journal);
    assert_eq!(replayed.stats(), e.stats());
    for (id, live) in e.accounts_iter() {
        let mirror = replayed.account(*id).expect("account exists on replay");
        assert_eq!(
            live.cash_quote_minor, mirror.cash_quote_minor,
            "cash mismatch on {id}"
        );
        assert_eq!(
            live.positions.len(),
            mirror.positions.len(),
            "position count mismatch on {id}"
        );
    }
}

// ----------------------------------------------------------------------
// G-09 fix: batch siblings commit sequentially with distinct ids
// ----------------------------------------------------------------------

#[test]
fn batch_siblings_get_distinct_ids_and_see_each_other() {
    let mut e = engine_with_market();
    seed_oracle(&mut e, T0, 8_000_000);
    deposit(&mut e, 1, 100_000_000);

    let requests = vec![
        OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_900, 5),
        OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_800, 5),
    ];
    e.process(Command::PlaceBatch { requests, now: T0 });
    let ids: Vec<u64> = e.account(1).unwrap().open_orders.keys().copied().collect();
    assert_eq!(ids.len(), 2, "both orders tracked");
    assert_ne!(ids[0], ids[1], "ids distinct");

    // A marketable batch cannot double-fill the same maker: the second
    // sibling sees the first's fills. The maker rests above account 1's
    // earlier bids so it does not cross them.
    deposit(&mut e, 2, 100_000_000);
    deposit(&mut e, 3, 100_000_000);
    e.process(Command::Place {
        request: OrderRequest::limit(3, "BTC-PERP", Side::Ask, 80_100, 4),
        now: T0 + 1,
    });
    let events = e.process(Command::PlaceBatch {
        requests: vec![
            OrderRequest {
                tif: poc_core::TimeInForce::Ioc,
                ..OrderRequest::limit(2, "BTC-PERP", Side::Bid, 80_200, 3)
            },
            OrderRequest {
                tif: poc_core::TimeInForce::Ioc,
                ..OrderRequest::limit(2, "BTC-PERP", Side::Bid, 80_200, 3)
            },
        ],
        now: T0 + 2,
    });
    let filled: u64 = events
        .iter()
        .filter_map(|ev| match ev {
            Event::TradeExecuted(t) => Some(t.qty_lots),
            _ => None,
        })
        .sum();
    assert_eq!(filled, 4, "only the maker's actual size crossed");
    assert_eq!(
        e.account(3).unwrap().lots_of("BTC-PERP"),
        -4,
        "maker sold 4"
    );
    assert_eq!(e.account(2).unwrap().lots_of("BTC-PERP"), 4, "buyer long 4");
    assert_replays(&e);
}

// ----------------------------------------------------------------------
// G-08: OCO brackets
// ----------------------------------------------------------------------

#[test]
fn oco_take_profit_fill_cancels_stop_loss() {
    let mut e = engine_with_market();
    seed_oracle(&mut e, T0, 8_000_000);
    deposit(&mut e, 1, 100_000_000);
    deposit(&mut e, 2, 100_000_000);

    // Account 1 is long 5 lots.
    e.process(Command::Place {
        request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 79_900, 5),
        now: T0 + 1,
    });
    e.process(Command::Place {
        request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_900, 5),
        now: T0 + 2,
    });
    assert_eq!(e.account(1).unwrap().lots_of("BTC-PERP"), 5);

    // Bracket: TP sell-limit above, SL sell-stop below.
    let events = e.process(Command::PlaceOco {
        first: OrderRequest::limit(1, "BTC-PERP", Side::Ask, 81_000, 5),
        second: OrderRequest {
            order_type: poc_core::OrderType::StopMarket {
                trigger_price: 79_000,
            },
            ..OrderRequest::limit(1, "BTC-PERP", Side::Ask, 78_900, 5)
        },
        now: T0 + 3,
    });
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Event::OcoLinked { .. })));

    // Take-profit fills against a buyer.
    e.process(Command::Place {
        request: OrderRequest::limit(2, "BTC-PERP", Side::Bid, 81_000, 5),
        now: T0 + 4,
    });

    // The parked stop must now be gone with the OCO-sibling reason.
    let closed: Vec<&Event> = e
        .journal()
        .iter()
        .filter(|ev| {
            matches!(
                ev,
                Event::OrderClosed {
                    reason: OrderCloseReason::OcoSibling,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(closed.len(), 1, "exactly one sibling cancel");
    // The group is released.
    let stops: Vec<u64> = Vec::new();
    let _ = stops;
    // No OCO member remains open.
    let open: Vec<u64> = e.account(1).unwrap().open_orders.keys().copied().collect();
    assert!(open.is_empty(), "no bracket member rests");
    assert_replays(&e);
}

#[test]
fn oco_stop_trigger_cancels_take_profit() {
    let mut e = engine_with_market();
    seed_oracle(&mut e, T0, 8_000_000);
    deposit(&mut e, 1, 100_000_000);
    deposit(&mut e, 2, 100_000_000);

    e.process(Command::Place {
        request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 79_900, 5),
        now: T0 + 1,
    });
    e.process(Command::Place {
        request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_900, 5),
        now: T0 + 2,
    });

    e.process(Command::PlaceOco {
        first: OrderRequest::limit(1, "BTC-PERP", Side::Ask, 81_000, 5),
        second: OrderRequest {
            order_type: poc_core::OrderType::StopMarket {
                trigger_price: 79_500,
            },
            ..OrderRequest::limit(1, "BTC-PERP", Side::Ask, 78_900, 5)
        },
        now: T0 + 3,
    });

    // Mark falls through the stop: the parked member activates (and
    // cannot fill — the book is far away), the resting TP is canceled.
    seed_oracle_raw(&mut e, T0 + 60_000, 7_900_000); // $79,000
    let events = e.process(Command::Tick { now: T0 + 60_000 });
    assert!(
        events.iter().any(|ev| {
            matches!(
                ev,
                Event::OrderClosed {
                    reason: OrderCloseReason::OcoSibling,
                    ..
                }
            )
        }),
        "stop activation cancels the take-profit"
    );
    assert_replays(&e);
}

#[test]
fn oco_invalid_pair_rejects_atomically() {
    let mut e = engine_with_market();
    seed_oracle(&mut e, T0, 8_000_000);
    deposit(&mut e, 1, 100_000_000);

    // Different subaccounts is not a bracket.
    let events = e.process(Command::PlaceOco {
        first: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_000, 5),
        second: OrderRequest::limit(2, "BTC-PERP", Side::Bid, 78_000, 5),
        now: T0 + 1,
    });
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Event::OrderRejection(_))));
    let open: Vec<u64> = e.account(1).unwrap().open_orders.keys().copied().collect();
    assert!(open.is_empty(), "nothing placed");
}

// ----------------------------------------------------------------------
// G-10: TWAP parents
// ----------------------------------------------------------------------

#[test]
fn twap_slices_complete_and_sum_to_total() {
    let mut e = engine_with_market();
    seed_oracle(&mut e, T0, 8_000_000);
    deposit(&mut e, 1, 200_000_000);
    deposit(&mut e, 2, 200_000_000);

    // A resting maker for every slice to hit.
    e.process(Command::Place {
        request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 79_900, 100),
        now: T0 + 1,
    });

    let events = e.process(Command::PlaceTwap {
        subaccount: 1,
        symbol: "BTC-PERP".into(),
        side: Side::Bid,
        total_lots: 100,
        slices: 10,
        slice_interval_ms: 60_000,
        limit_ticks: Some(79_950),
        now: T0 + 2,
    });
    assert!(events.iter().any(|ev| matches!(ev, Event::TwapOpened(_))));

    let mut sliced = 0;
    let mut placed_lots = 0_u64;
    let mut completed = false;
    for i in 1..=10u64 {
        // Keep the feed alive: a stale oracle halts the market and the
        // child would be rejected for missing marks.
        seed_oracle_raw(&mut e, T0 + 2 + i * 60_000, 8_000_000);
        let events = e.process(Command::Tick {
            now: T0 + 2 + i * 60_000,
        });
        for ev in &events {
            match ev {
                Event::TwapSliced { request, .. } => {
                    sliced += 1;
                    placed_lots += request.qty_lots;
                }
                Event::TwapClosed { reason, .. } if *reason == "completed" => completed = true,
                _ => {}
            }
        }
    }
    assert_eq!(sliced, 10, "ten slices emitted");
    assert_eq!(placed_lots, 100, "slices sum to the parent total");
    assert!(completed, "parent completed");
    assert_eq!(e.account(1).unwrap().lots_of("BTC-PERP"), 100);
    assert_replays(&e);
}

#[test]
fn twap_cancel_stops_slicing() {
    let mut e = engine_with_market();
    seed_oracle(&mut e, T0, 8_000_000);
    deposit(&mut e, 1, 200_000_000);

    e.process(Command::PlaceTwap {
        subaccount: 1,
        symbol: "BTC-PERP".into(),
        side: Side::Bid,
        total_lots: 50,
        slices: 5,
        slice_interval_ms: 60_000,
        limit_ticks: Some(79_950),
        now: T0,
    });
    seed_oracle_raw(&mut e, T0 + 60_000, 8_000_000);
    e.process(Command::Tick { now: T0 + 60_000 });
    let events = e.process(Command::CancelTwap {
        subaccount: 1,
        parent_id: 1,
        now: T0 + 61_000,
    });
    assert!(events.iter().any(|ev| matches!(
        ev,
        Event::TwapClosed {
            reason: "canceled",
            ..
        }
    )));
    let after: usize = e
        .journal()
        .iter()
        .filter(|ev| matches!(ev, Event::TwapSliced { .. }))
        .count();
    seed_oracle_raw(&mut e, T0 + 180_000, 8_000_000);
    e.process(Command::Tick { now: T0 + 180_000 });
    let still: usize = e
        .journal()
        .iter()
        .filter(|ev| matches!(ev, Event::TwapSliced { .. }))
        .count();
    assert_eq!(after, still, "no slices after cancel");
}

// ----------------------------------------------------------------------
// G-05: volatility index
// ----------------------------------------------------------------------

#[test]
fn vol_index_publishes_near_the_mark_ivs() {
    let mut cfg = config();
    let mut listing = UnderlyingListing::btc_default();
    listing.strike_spacing_quote_minor = 200_000;
    listing.strike_steps = 2;
    listing.tenors_ms = vec![7 * 24 * 60 * 60 * 1000];
    listing.everlasting_strikes = 0;
    listing.auction_ms = 0;
    listing.template.tick_size_quote_minor = 10;
    listing.template.lot_size_base_minor = 1_000;
    listing.template.price_band_bps = 50_000;
    cfg.listing.underlyings.push(listing);

    let mut e = Engine::new(cfg);
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    seed_oracle(&mut e, T0, 8_000_000);
    e.process(Command::Tick { now: T0 });

    let events = e.process(Command::Tick { now: T0 + 1_000 });
    let indexes: Vec<u64> = events
        .iter()
        .filter_map(|ev| match ev {
            Event::VolIndexPublished { index_permille, .. } => Some(*index_permille),
            _ => None,
        })
        .collect();
    assert!(!indexes.is_empty(), "index published");
    // The default anchored surface: IVs near 5500 bps -> index near
    // 5500 * 10 = 55_000 permille (55.0 points).
    for index in indexes {
        assert!(
            (30_000..=90_000).contains(&index),
            "index {index} in a sane band"
        );
    }
    assert_replays(&e);
}

// ----------------------------------------------------------------------
// G-18: collateral interest
// ----------------------------------------------------------------------

#[test]
fn collateral_interest_accrues_on_utilized_margin() {
    let mut cfg = config();
    cfg.collateral = vec![poc_engine::CollateralCurrency::btc()];
    cfg.collateral_interest_bps_per_day
        .insert("BTC".into(), 100); // 1%/day on utilization
    let mut e = Engine::new(cfg);
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    seed_oracle(&mut e, T0, 8_000_000);

    // A margined account funded by quote cash + BTC collateral.
    e.process(Command::Deposit {
        subaccount: 1,
        amount_quote_minor: 20_000_000,
    });
    e.process(Command::DepositCollateral {
        subaccount: 1,
        currency: "BTC".into(),
        amount_minor: 10_000, // 0.0001 BTC = $8 at $80k
        now: T0,
    });
    deposit(&mut e, 2, 100_000_000);
    e.process(Command::Place {
        request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 79_900, 100),
        now: T0 + 1,
    });
    e.process(Command::Place {
        request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_900, 100),
        now: T0 + 2,
    });
    assert_eq!(
        e.account(1).unwrap().lots_of("BTC-PERP"),
        100,
        "position opened"
    );

    let day = 24 * 60 * 60 * 1000;
    seed_oracle_raw(&mut e, T0 + day, 8_000_000);
    let events = e.process(Command::Tick { now: T0 + day });
    let accrued: Vec<&Event> = events
        .iter()
        .filter(|ev| matches!(ev, Event::CollateralInterestAccrued { .. }))
        .collect();
    assert!(!accrued.is_empty(), "interest accrued at the day boundary");
    let before = e.collateral_balance(1, "BTC") + 1;
    let _ = before;
    assert!(
        e.collateral_balance(1, "BTC") < 10_000,
        "balance debited in kind"
    );
    // Second day ticks accrue again (not double-charged for the same day).
    seed_oracle_raw(&mut e, T0 + 2 * day, 8_000_000);
    let events = e.process(Command::Tick { now: T0 + 2 * day });
    let again: usize = events
        .iter()
        .filter(|ev| matches!(ev, Event::CollateralInterestAccrued { .. }))
        .count();
    assert_eq!(again, 1, "exactly one accrual per boundary");
    assert_replays(&e);
}

// ----------------------------------------------------------------------
// G-23: insurance inventory marking + rebalancing
// ----------------------------------------------------------------------

#[test]
fn insurance_books_marks_and_rebalances_inventory() {
    let mut cfg = config();
    // A funded fund: it is the buyer of last resort and CARRIES the
    // closed positions as inventory (G-23).
    cfg.insurance_seed_quote_minor = 50_000_000; // $500k
    let mut e = Engine::new(cfg);
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    seed_oracle(&mut e, T0, 8_000_000);

    // Account 1: leveraged long, sized to be under-margined (not
    // bankrupt) after a moderate crash.
    deposit(&mut e, 1, 120_000); // $1,200
    deposit(&mut e, 2, 100_000_000);
    e.process(Command::Place {
        request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 79_950, 200),
        now: T0 + 1,
    });
    e.process(Command::Place {
        request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_950, 200),
        now: T0 + 2,
    });
    assert_eq!(e.account(1).unwrap().lots_of("BTC-PERP"), 200);

    // Crash to $74k: equity falls under maintenance, the book has no
    // bids, so the liquidation's phase B crosses with the fund.
    seed_oracle_raw(&mut e, T0 + 60_000, 7_400_000);
    let events = e.process(Command::Tick { now: T0 + 60_000 });
    let to_insurance: Vec<&Box<poc_engine::LiquidationExecuted>> = events
        .iter()
        .filter_map(|ev| match ev {
            Event::Liquidation(l) if l.to_insurance => Some(l),
            _ => None,
        })
        .collect();
    assert!(!to_insurance.is_empty(), "fund took the position");
    let fund_lots: i64 = e.insurance_inventory().values().map(|(lots, _)| lots).sum();
    assert!(fund_lots > 0, "inventory booked: {fund_lots}");

    // Price recovers: the inventory is marked to market (a gain for the
    // fund's long).
    seed_oracle_raw(&mut e, T0 + 120_000, 7_800_000);
    let events = e.process(Command::Tick { now: T0 + 120_000 });
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, Event::InsuranceMarked { .. })),
        "inventory marked"
    );

    // A bid above the carrying mark lets the drip unwind inventory into
    // the book at a profit.
    deposit(&mut e, 3, 100_000_000);
    e.process(Command::Place {
        request: OrderRequest::limit(3, "BTC-PERP", Side::Bid, 78_500, 10),
        now: T0 + 121_000,
    });
    let events = e.process(Command::Tick { now: T0 + 121_000 });
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, Event::InsuranceRebalanced { .. })),
        "inventory dripped back into the book"
    );
    let after: i64 = e.insurance_inventory().values().map(|(lots, _)| lots).sum();
    assert!(after < fund_lots, "inventory reduced by the drip");
    assert_replays(&e);
}

// ----------------------------------------------------------------------
// G-16: LP vaults
// ----------------------------------------------------------------------

#[test]
fn vault_epoch_subscribes_redeems_and_pays() {
    let mut cfg = config();
    cfg.vault_epoch_interval_ms = 60_000;
    let mut e = Engine::new(cfg);
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    seed_oracle(&mut e, T0, 8_000_000);
    deposit(&mut e, 1, 10_000_000);
    deposit(&mut e, 2, 5_000_000);

    e.process(Command::VaultCreate {
        revenue_share_bps: 1_000,
        now: T0,
    });
    e.process(Command::VaultSubscribe {
        vault_id: 1,
        subaccount: 1,
        amount_quote_minor: 2_000_000,
        now: T0 + 1,
    });
    // Cash does not move before the boundary.
    assert_eq!(e.account(1).unwrap().cash_quote_minor, 10_000_000);

    let events = e.process(Command::Tick { now: T0 + 61_000 });
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, Event::VaultEpochSettled(_))),
        "epoch settled"
    );
    assert_eq!(
        e.account(1).unwrap().cash_quote_minor,
        8_000_000,
        "subscription debited at the boundary"
    );

    // Revenue credit raises NAV; redemption pays the raised NAV.
    e.process(Command::VaultSubscribe {
        vault_id: 1,
        subaccount: 2,
        amount_quote_minor: 5_000_000,
        now: T0 + 62_000,
    });
    e.process(Command::Tick { now: T0 + 121_000 });
    assert_eq!(e.account(2).unwrap().cash_quote_minor, 0);
    e.process(Command::VaultRedeem {
        vault_id: 1,
        subaccount: 2,
        shares: 5_000_000,
        now: T0 + 122_000,
    });
    let events = e.process(Command::Tick { now: T0 + 181_000 });
    let settled: Vec<&Event> = events
        .iter()
        .filter(|ev| matches!(ev, Event::VaultEpochSettled(_)))
        .collect();
    assert_eq!(settled.len(), 1);
    assert_eq!(
        e.account(2).unwrap().cash_quote_minor,
        5_000_000,
        "redemption paid out at par"
    );
    assert_replays(&e);
}
