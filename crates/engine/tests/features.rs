//! Integration tests for the G-03/06/07/09/12/17/19/34 gap-register
//! implementations: auctions, icebergs, trailing stops, batch/amend,
//! multi-collateral, auto-listing, everlasting rebase, and the
//! replay-determinism of all the new events.

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

fn deposit(e: &mut Engine, sub: u64, amount: u128) {
    e.process(Command::Deposit {
        subaccount: sub,
        amount_quote_minor: amount,
    });
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

fn place(
    e: &mut Engine,
    sub: u64,
    side: Side,
    price: u64,
    qty: u64,
    now: TimestampMs,
) -> Vec<Event> {
    e.process(Command::Place {
        request: OrderRequest::limit(sub, "BTC-PERP", side, price, qty),
        now,
    })
}

// ----------------------------------------------------------------------
// G-12: auctions
// ----------------------------------------------------------------------

#[test]
fn auction_accumulates_then_uncrosses_at_uniform_price() {
    let mut e = engine_with_market();
    seed_oracle(&mut e, T0, 8_000_000);
    deposit(&mut e, 1, 100_000_000);
    deposit(&mut e, 2, 100_000_000);
    deposit(&mut e, 3, 100_000_000);

    let events = e.process(Command::BeginAuction {
        symbol: "BTC-PERP".into(),
        uncross_at: T0 + 10_000,
        now: T0,
    });
    assert!(matches!(events[0], Event::AuctionOpened { .. }));

    // Crossing orders rest during the auction (no matching).
    place(&mut e, 2, Side::Bid, 80_100, 5, T0 + 1);
    place(&mut e, 3, Side::Bid, 80_000, 3, T0 + 2);
    place(&mut e, 1, Side::Ask, 80_050, 4, T0 + 3);
    assert!(e.book("BTC-PERP").is_some_and(|b| b.auction_mode()));
    assert!(e
        .book("BTC-PERP")
        .is_some_and(|b| b.best_bid() == Some(80_100)));
    assert!(e
        .book("BTC-PERP")
        .is_some_and(|b| b.best_ask() == Some(80_050)));
    assert_eq!(e.stats().trades, 0);

    // Market orders are refused mid-auction.
    let events = e.process(Command::Place {
        request: OrderRequest::market(2, "BTC-PERP", Side::Bid, 1),
        now: T0 + 4,
    });
    assert!(matches!(events[0], Event::OrderRejection(_)));

    // Uncross: min(bids >= 80_050 = 5, asks <= 80_100 = 4) = 4 lots at a
    // price in {80_050, 80_100}; band midpoint 80_075 ties toward 80_050.
    let events = e.process(Command::Tick { now: T0 + 10_000 });
    assert!(events.iter().any(|ev| matches!(
        ev,
        Event::AuctionUncrossed {
            clearing_price_ticks: Some(80_050),
            matched_lots: 4,
            ..
        }
    )));
    let trades = events
        .iter()
        .filter(|ev| matches!(ev, Event::TradeExecuted(_)))
        .count();
    assert_eq!(trades, 1, "one bid-ask pairing prints 4 lots");
    assert_eq!(e.stats().trades, 1);
    // Continuous trading restored; the book is no longer crossed.
    assert!(!e.book("BTC-PERP").is_some_and(|b| b.auction_mode()));
    // The leftover 1-lot bid rests on; the filled ask left the book.
    assert!(e.account(2).is_some_and(|a| a.open_orders.len() == 1));
    assert!(e.account(1).is_some_and(|a| a.open_orders.is_empty()));
    assert_eq!(e.account(1).unwrap().lots_of("BTC-PERP"), -4);
    assert_eq!(e.account(2).unwrap().lots_of("BTC-PERP"), 4);
}

// ----------------------------------------------------------------------
// G-06: icebergs through the engine
// ----------------------------------------------------------------------

#[test]
fn iceberg_rests_hidden_and_reveals_in_slices() {
    let mut e = engine_with_market();
    seed_oracle(&mut e, T0, 8_000_000);
    deposit(&mut e, 1, 100_000_000);
    deposit(&mut e, 2, 100_000_000);

    let mut request = OrderRequest::limit(1, "BTC-PERP", Side::Ask, 80_000, 10);
    request.display_lots = Some(3);
    e.process(Command::Place { request, now: T0 });

    let (_, asks) = e.book("BTC-PERP").unwrap().depth(5);
    assert_eq!(asks[0].total_qty_lots, 3, "depth shows the slice only");

    // Taker lifts one slice: 3 lots trade, 7 stay hidden.
    let events = place(&mut e, 2, Side::Bid, 80_000, 10, T0 + 1);
    let traded: u64 = events
        .iter()
        .filter_map(|ev| match ev {
            Event::TradeExecuted(t) => Some(t.qty_lots),
            _ => None,
        })
        .sum();
    assert_eq!(traded, 3);
    let (_, asks) = e.book("BTC-PERP").unwrap().depth(5);
    assert_eq!(asks[0].total_qty_lots, 3, "a fresh slice was revealed");
    assert_eq!(e.account(1).unwrap().lots_of("BTC-PERP"), -3);
    assert_eq!(e.account(2).unwrap().lots_of("BTC-PERP"), 3);
}

// ----------------------------------------------------------------------
// G-07: trailing stops
// ----------------------------------------------------------------------

fn trailing_request(sub: u64, offset: u64) -> OrderRequest {
    OrderRequest {
        subaccount: sub,
        symbol: "BTC-PERP".into(),
        side: Side::Ask,
        order_type: poc_core::OrderType::TrailingStopMarket {
            offset_ticks: offset,
        },
        price_ticks: None,
        qty_lots: 5,
        tif: poc_core::TimeInForce::Gtc,
        post_only: false,
        reduce_only: false,
        stp: poc_core::SelfTradePrevention::CancelNewest,
        display_lots: None,
        oco_group: None,
        client_ts: 0,
    }
}

#[test]
fn trailing_stop_tracks_extreme_and_fires_on_retrace() {
    let mut e = engine_with_market();
    seed_oracle(&mut e, T0, 8_000_000);
    deposit(&mut e, 1, 100_000_000);
    deposit(&mut e, 2, 100_000_000);
    place(&mut e, 1, Side::Bid, 80_000, 10, T0); // resting bid to hit

    // Sell trailer: $200 offset (2 ticks at $100).
    e.process(Command::Place {
        request: trailing_request(2, 2),
        now: T0,
    });

    // Rise to 80_500: the extreme (high) follows upward (journaled).
    seed_oracle(&mut e, T0 + 10_000, 8_050_000);
    assert!(e
        .journal()
        .iter()
        .any(|ev| matches!(ev, Event::TrailingUpdated { .. })));

    // Retrace to 80_200: trigger = 80_500 − 200 = 80_300 ≥ mark → fires.
    let _events = e.process(Command::Tick { now: T0 + 20_000 });
    // Feed the mark first (the tick plan reads the oracle at this ts).
    for provider in ["pyth", "chainlink"] {
        e.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: provider.into(),
            ts: T0 + 20_000,
            price_quote_minor: 8_020_000,
        });
    }
    let events = e.process(Command::Tick { now: T0 + 20_000 });
    let traded: u64 = events
        .iter()
        .filter_map(|ev| match ev {
            Event::TradeExecuted(t) => Some(t.qty_lots),
            _ => None,
        })
        .sum();
    assert_eq!(traded, 5, "the trailer fired into the resting bid");
    assert_eq!(e.account(2).unwrap().lots_of("BTC-PERP"), -5);
}

// ----------------------------------------------------------------------
// G-09: batch + amend
// ----------------------------------------------------------------------

#[test]
fn batch_places_and_amend_reduces_in_place() {
    let mut e = engine_with_market();
    seed_oracle(&mut e, T0, 8_000_000);
    deposit(&mut e, 1, 100_000_000);

    let requests = vec![
        OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_900, 5),
        OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_800, 5),
    ];
    let events = e.process(Command::PlaceBatch { requests, now: T0 });
    let resting = events
        .iter()
        .filter(|ev| matches!(ev, Event::OrderResting { .. }))
        .count();
    assert_eq!(resting, 2);
    let ids: Vec<u64> = e.account(1).unwrap().open_orders.keys().copied().collect();

    // Size reduction at the same price: in place, priority kept.
    let events = e.process(Command::Amend {
        subaccount: 1,
        order_id: ids[0],
        new_price_ticks: None,
        new_open_lots: Some(2),
        now: T0 + 1,
    });
    assert!(matches!(events[0], Event::OrderAmended(_)));
    assert_eq!(e.account(1).unwrap().open_orders[&ids[0]].open_lots, 2);
    assert_eq!(
        e.book("BTC-PERP")
            .unwrap()
            .get(ids[0])
            .map(|r| r.order.open_qty()),
        Some(2)
    );

    // Price change: cancel-and-replace (new id, back of the level).
    let events = e.process(Command::Amend {
        subaccount: 1,
        order_id: ids[0],
        new_price_ticks: Some(79_950),
        new_open_lots: None,
        now: T0 + 2,
    });
    assert!(events.iter().any(|ev| matches!(
        ev,
        Event::OrderClosed {
            reason: OrderCloseReason::Canceled,
            ..
        }
    )));
    assert!(!e.account(1).unwrap().open_orders.contains_key(&ids[0]));

    // Batch cancel the rest.
    let remaining: Vec<u64> = e.account(1).unwrap().open_orders.keys().copied().collect();
    let events = e.process(Command::CancelBatch {
        subaccount: 1,
        order_ids: remaining,
        now: T0 + 3,
    });
    assert!(events.iter().all(|ev| matches!(
        ev,
        Event::OrderClosed {
            reason: OrderCloseReason::Canceled,
            ..
        }
    )));
    assert!(e.account(1).unwrap().open_orders.is_empty());
}

// ----------------------------------------------------------------------
// G-17: multi-collateral
// ----------------------------------------------------------------------

#[test]
fn collateral_counts_toward_margin_with_haircut() {
    let mut cfg = config();
    cfg.collateral = vec![poc_engine::CollateralCurrency::btc()];
    let mut e = Engine::new(cfg);
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    seed_oracle(&mut e, T0, 8_000_000); // BTC at $80,000

    // 0.5 BTC of collateral: $40,000 gross, $32,000 after the 20% haircut.
    e.process(Command::DepositCollateral {
        subaccount: 1,
        currency: "BTC".into(),
        amount_minor: 50_000_000, // 8dp
        now: T0,
    });
    deposit(&mut e, 1, 1_000_000); // $10,000 quote
    assert_eq!(e.collateral_balance(1, "BTC"), 50_000_000);

    let summary = e.margin_summary_of(1).unwrap();
    assert_eq!(
        summary.equity_quote_minor,
        1_000_000 + 3_200_000,
        "haircut-adjusted collateral value joins equity"
    );

    // Full withdrawal is fine on a flat account.
    let events = e.process(Command::WithdrawCollateral {
        subaccount: 1,
        currency: "BTC".into(),
        amount_minor: 50_000_000,
        now: T0 + 1,
    });
    assert!(matches!(events[0], Event::CollateralMoved(_)));
    assert_eq!(e.collateral_balance(1, "BTC"), 0);
    assert_eq!(
        e.margin_summary_of(1).unwrap().equity_quote_minor,
        1_000_000
    );

    // Convert BTC → USD at the oracle price.
    e.process(Command::DepositCollateral {
        subaccount: 1,
        currency: "BTC".into(),
        amount_minor: 10_000_000, // 0.1 BTC = $8,000
        now: T0 + 2,
    });
    let events = e.process(Command::ConvertCollateral {
        subaccount: 1,
        from: "BTC".into(),
        to: "USD".into(),
        from_amount_minor: 10_000_000,
        now: T0 + 3,
    });
    assert!(matches!(events[0], Event::CollateralConversion(_)));
    assert_eq!(e.collateral_balance(1, "BTC"), 0);
    assert_eq!(
        e.account(1).unwrap().cash_quote_minor,
        1_000_000 + 800_000 // $10k + $8k
    );
}

// ----------------------------------------------------------------------
// G-34 + G-03: auto-listing + everlasting rebase
// ----------------------------------------------------------------------

#[test]
fn auto_listing_lists_grid_and_rebases_everlasting() {
    let mut cfg = config();
    let mut listing = UnderlyingListing::btc_default();
    // Small grid for the test: $2,000 spacing, 1+1 strikes, no auctions.
    listing.strike_spacing_quote_minor = 200_000;
    listing.strike_steps = 1;
    listing.tenors_ms = vec![7 * 24 * 60 * 60 * 1000];
    listing.everlasting_strikes = 1;
    listing.rebase_band_bps = 1_000; // 10%
    listing.auction_ms = 0;
    listing.template.tick_size_quote_minor = 10;
    listing.template.lot_size_base_minor = 1_000;
    listing.template.price_band_bps = 50_000; // wide band for the test
    cfg.listing.underlyings.push(listing);
    cfg.impact_notional_quote_minor = 0;

    let mut e = Engine::new(cfg);
    e.register_instrument(Instrument::Perp(PerpMarket::default())); // creates the BTC oracle
    seed_oracle_raw(&mut e, T0, 8_000_000); // $80,000

    let events = e.process(Command::Tick { now: T0 });
    let listed = events
        .iter()
        .filter(|ev| matches!(ev, Event::MarketListed { .. }))
        .count();
    assert!(
        listed >= 4,
        "dated + everlasting grids listed, got {listed}"
    );
    assert!(e.instruments().contains_key("BTC-EVER-80000-C"));
    assert!(e.instruments().contains_key("BTC-EVER-80000-P"));

    // Buy the everlasting call through a resting maker.
    deposit(&mut e, 1, 200_000_000);
    deposit(&mut e, 2, 200_000_000);
    e.process(Command::Place {
        request: OrderRequest::limit(2, "BTC-EVER-80000-C", Side::Ask, 300, 5),
        now: T0 + 1,
    });
    let events = e.process(Command::Place {
        request: OrderRequest::limit(1, "BTC-EVER-80000-C", Side::Bid, 300, 5),
        now: T0 + 2,
    });
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Event::TradeExecuted(_))));
    assert_eq!(e.account(1).unwrap().lots_of("BTC-EVER-80000-C"), 5);

    // Spot jumps 15% to 92k: the 80k everlasting strike is out of band.
    seed_oracle_raw(&mut e, T0 + 60_000, 9_200_000);
    let events = e.process(Command::Tick { now: T0 + 60_000 });
    let migrated = events
        .iter()
        .filter(|ev| matches!(ev, Event::PositionMigrated { .. }))
        .count();
    assert_eq!(migrated, 2, "both the long and the short migrated to 92k");
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Event::OptionDelisted { symbol } if symbol == "BTC-EVER-80000-C")));
    assert_eq!(e.account(1).unwrap().lots_of("BTC-EVER-80000-C"), 0);
    assert_eq!(e.account(1).unwrap().lots_of("BTC-EVER-92000-C"), 5);
    assert!(e.instruments().contains_key("BTC-EVER-92000-C"));
}

// ----------------------------------------------------------------------
// G-19: bankrupt liquidation: collateral first, iterative ADL after
// ----------------------------------------------------------------------

#[test]
fn bankrupt_account_liquidates_collateral_then_adls_in_rounds() {
    let mut cfg = config();
    cfg.collateral = vec![poc_engine::CollateralCurrency::btc()];
    cfg.insurance_seed_quote_minor = 0; // fund exhausted from genesis
    cfg.adl_max_rounds = 4;
    cfg.adl_round_bps = 5_000; // 50% of each counterparty per round
    let mut e = Engine::new(cfg);
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    seed_oracle(&mut e, T0, 8_000_000);

    // Degen: $2,000 cash + 0.1 BTC collateral, long the whole book.
    deposit(&mut e, 1, 200_000);
    e.process(Command::DepositCollateral {
        subaccount: 1,
        currency: "BTC".into(),
        amount_minor: 10_000_000, // 0.1 BTC = $8,000 gross
        now: T0,
    });
    deposit(&mut e, 2, 100_000_000);
    deposit(&mut e, 3, 100_000_000);
    place(&mut e, 2, Side::Ask, 79_900, 200, T0 + 1);
    place(&mut e, 3, Side::Ask, 79_950, 200, T0 + 2);
    let _ = place(&mut e, 1, Side::Bid, 79_950, 400, T0 + 3);
    assert_eq!(e.account(1).unwrap().lots_of("BTC-PERP"), 400);

    // Crash to $40,000: the degen is bankrupt and the fund is empty.
    seed_oracle_raw(&mut e, T0 + 100_000, 4_000_000);
    let events = e.process(Command::Tick { now: T0 + 100_000 });

    assert!(events
        .iter()
        .any(|ev| matches!(ev, Event::CollateralConversion(_))));
    let adls: Vec<&Event> = events
        .iter()
        .filter(|ev| matches!(ev, Event::Adl(_)))
        .collect();
    assert!(!adls.is_empty(), "bankrupt + empty fund forces ADL");
    for adl in &adls {
        if let Event::Adl(exec) = adl {
            assert!(exec.lots <= 200, "rounds cap per-counterparty closures");
            assert_eq!(exec.liquidated_subaccount, 1);
        }
    }
    // The degen's long was cut hard (flat or on its way there).
    let lots = e.account(1).unwrap().lots_of("BTC-PERP");
    assert!(lots < 400, "closures reduced the long, got {lots}");
}

// ----------------------------------------------------------------------
// Replay determinism with all the new event kinds
// ----------------------------------------------------------------------

#[test]
fn new_features_replay_bit_for_bit() {
    fn script(cfg: EngineConfig) -> Engine {
        let mut e = Engine::new(cfg);
        e.register_instrument(Instrument::Perp(PerpMarket::default()));
        seed_oracle(&mut e, T0, 8_000_000);
        deposit(&mut e, 1, 100_000_000);
        deposit(&mut e, 2, 100_000_000);
        e.process(Command::DepositCollateral {
            subaccount: 1,
            currency: "BTC".into(),
            amount_minor: 5_000_000,
            now: T0,
        });

        // Auction cycle.
        e.process(Command::BeginAuction {
            symbol: "BTC-PERP".into(),
            uncross_at: T0 + 5_000,
            now: T0,
        });
        place(&mut e, 1, Side::Bid, 80_000, 5, T0 + 1);
        place(&mut e, 2, Side::Ask, 79_950, 4, T0 + 2);
        e.process(Command::Tick { now: T0 + 5_000 });

        // Iceberg + batch + amend + convert.
        let mut iceberg = OrderRequest::limit(2, "BTC-PERP", Side::Ask, 80_200, 9);
        iceberg.display_lots = Some(2);
        e.process(Command::Place {
            request: iceberg,
            now: T0 + 6_000,
        });
        place(&mut e, 1, Side::Bid, 80_200, 4, T0 + 7_000);
        let requests = vec![
            OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_800, 3),
            OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_700, 3),
        ];
        e.process(Command::PlaceBatch {
            requests,
            now: T0 + 8_000,
        });
        let ids: Vec<u64> = e.account(1).unwrap().open_orders.keys().copied().collect();
        if let Some(&id) = ids.first() {
            e.process(Command::Amend {
                subaccount: 1,
                order_id: id,
                new_price_ticks: None,
                new_open_lots: Some(1),
                now: T0 + 9_000,
            });
        }
        e.process(Command::ConvertCollateral {
            subaccount: 1,
            from: "BTC".into(),
            to: "USD".into(),
            from_amount_minor: 1_000_000,
            now: T0 + 9_500,
        });
        e
    }

    let mut cfg = config();
    cfg.collateral = vec![poc_engine::CollateralCurrency::btc()];
    let e = script(cfg.clone());
    let e2 = script(cfg);

    assert_eq!(e.journal(), e2.journal());
    let e3 = Engine::replay(config(), e.journal());
    assert_eq!(e3.stats(), e.stats());
    assert_eq!(
        e3.account(1).map(|a| a.cash_quote_minor),
        e.account(1).map(|a| a.cash_quote_minor)
    );
    assert_eq!(
        e3.collateral_balance(1, "BTC"),
        e.collateral_balance(1, "BTC")
    );
}
